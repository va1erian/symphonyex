//! Agent Runner Protocol abstraction (Section 10): a coding-agent backend that Symphony
//! launches inside a per-issue workspace and streams turn events from.
//!
//! Three backends ship: [`claude`] (Claude Code CLI headless mode; the default and the
//! well-exercised path), [`codex`] (Codex app-server; a best-effort JSON-RPC skeleton
//! — see that module's docs for its verification caveat), and [`opencode`] (the
//! open-source, provider-agnostic `opencode` CLI — the "pluggable AI provider" path,
//! e.g. for Fireworks AI; see that module's docs for its own verification caveat).

pub mod claude;
pub mod codex;
pub mod opencode;

use crate::container::ContainerHandle;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::path::Path;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// Emitted runtime event (Section 10.4). `event` uses the spec's vocabulary where it
/// applies (`session_started`, `turn_completed`, `turn_failed`, `notification`, ...).
#[derive(Debug, Clone)]
pub struct AgentEvent {
    pub event: String,
    pub timestamp: DateTime<Utc>,
    pub message: Option<String>,
    pub usage: Option<TokenUsage>,
}

impl AgentEvent {
    pub fn new(event: impl Into<String>) -> Self {
        Self {
            event: event.into(),
            timestamp: Utc::now(),
            message: None,
            usage: None,
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn with_usage(mut self, usage: TokenUsage) -> Self {
        self.usage = Some(usage);
        self
    }
}

#[derive(Debug)]
pub enum TurnOutcome {
    /// `usage` is informational; the orchestrator's authoritative token accounting comes
    /// from `AgentEvent::usage` on the `turn_completed` event.
    #[allow(dead_code)]
    Completed {
        usage: Option<TokenUsage>,
    },
    Failed {
        reason: String,
    },
}

/// RECOMMENDED normalized error categories (Section 10.6).
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("codex_not_found: {0}")]
    NotFound(String),
    #[error("invalid_workspace_cwd: {0}")]
    InvalidCwd(String),
    #[error("response_timeout: {0}")]
    ResponseTimeout(String),
    #[error("turn_timeout: {0}")]
    TurnTimeout(String),
    #[error("port_exit: {0}")]
    ProcessExit(String),
    #[error("response_error: {0}")]
    ResponseError(String),
    /// A backend was asked to start a session under a `ToolPolicy` it cannot honor
    /// (AIR-2: `codex` has no known native tool-denial mechanism yet). Refusing to
    /// start is the correct posture -- silently running unrestricted would defeat the
    /// whole point of a role's tool restriction (e.g. a Reviewer that can edit files).
    #[error("unsupported_tool_policy: {0}")]
    UnsupportedToolPolicy(String),
}

/// Backend-agnostic tool restriction a session starts under (roadmap §4: "a Reviewer
/// that can edit files is not a reviewer"). Originally SweBot's own hardcoded
/// `--disallowedTools`/`OPENCODE_PERMISSION` construction (see `swebot::mod` history);
/// generalized here (AIR-2) so both SweBot and pipeline roles (`src/roles/`) drive the
/// same mechanism. Each `AgentBackend` translates this into its own native denial
/// mechanism in `start_session` -- `claude.rs` appends `--disallowedTools`, `opencode.rs`
/// sets `OPENCODE_PERMISSION` -- and `codex.rs` returns
/// `AgentError::UnsupportedToolPolicy` for any restricted policy rather than silently
/// running unrestricted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPolicy {
    pub allow_edits: bool,
    pub allow_commands: bool,
}

impl Default for ToolPolicy {
    /// The high-trust posture every backend already defaults to (Section 10.5 example):
    /// nothing denied.
    fn default() -> Self {
        Self {
            allow_edits: true,
            allow_commands: true,
        }
    }
}

impl ToolPolicy {
    /// SweBot's own restriction (`allow_edits: false`, unchanged from before this
    /// refactor): file-mutating tools denied, Bash/Read stay free so it can still
    /// explore the repo and run tests during review.
    pub const SWEBOT: ToolPolicy = ToolPolicy {
        allow_edits: false,
        allow_commands: true,
    };

    pub fn is_restricted(&self) -> bool {
        !self.allow_edits || !self.allow_commands
    }
}

#[async_trait]
pub trait AgentSession: Send {
    fn session_id(&self) -> &str;

    /// Run one turn to completion (or timeout/failure), forwarding events to `events`.
    async fn run_turn(
        &mut self,
        prompt: &str,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<TurnOutcome, AgentError>;

    /// Stop the session. Best-effort; errors are not actionable by the caller.
    async fn stop(self: Box<Self>);
}

#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Start (or prepare to start) a session rooted at `workspace`. `issue_id` is the
    /// opaque dispatch id (used to bind any provider-native tool wiring to this issue);
    /// `title` is used for turn/session titling where the backend supports it (Section
    /// 10.2). `container` is `Some` in Docker mode (see README.md) -- backends that
    /// don't support it (currently: Codex) may ignore it. `tool_policy` (AIR-2) is this
    /// session's tool restriction, translated into whichever native denial mechanism
    /// this backend has -- see `ToolPolicy`'s doc comment.
    async fn start_session(
        &self,
        workspace: &Path,
        issue_id: &str,
        title: &str,
        container: Option<&ContainerHandle>,
        tool_policy: &ToolPolicy,
    ) -> Result<Box<dyn AgentSession>, AgentError>;

    /// Downcast hook, mainly so tests can inspect which concrete backend a factory
    /// selected (e.g. `swebot::build_restricted_backend`'s dispatch). Defaults to
    /// not-downcastable; concrete backends that warrant inspection override it.
    /// `allow(dead_code)` because in non-test builds it's only ever called through
    /// trait-object dispatch from `#[cfg(test)]` code.
    #[allow(dead_code)]
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
}
