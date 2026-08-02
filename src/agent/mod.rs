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
    /// don't support it (currently: Codex) may ignore it.
    async fn start_session(
        &self,
        workspace: &Path,
        issue_id: &str,
        title: &str,
        container: Option<&ContainerHandle>,
    ) -> Result<Box<dyn AgentSession>, AgentError>;
}
