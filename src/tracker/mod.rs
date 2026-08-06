//! Issue tracker adapter boundary (Section 11): a small portable read kernel.

pub mod depends_on;
pub mod github;
pub mod gitlab;
pub mod local;

use crate::domain::Issue;
use async_trait::async_trait;
use thiserror::Error;

/// RECOMMENDED adapter error categories (Section 11.4).
#[derive(Debug, Error, Clone)]
pub enum TrackerError {
    #[error("unsupported_tracker_kind: {0}")]
    UnsupportedTrackerKind(String),
    #[error("invalid_tracker_config: {0}")]
    InvalidTrackerConfig(String),
    /// Section 15.3: an adapter with a secret (e.g. `github`'s API token) that's
    /// missing/empty. The bundled `local` adapter has no secrets and never returns this.
    #[error("missing_tracker_secret: {0}")]
    MissingTrackerSecret(String),
    #[error("tracker_request: {0}")]
    Request(String),
    #[error("tracker_response: {0}")]
    Response(String),
}

/// A provider-native agent tool an adapter chooses to expose (Section 10.5, OPTIONAL).
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Result of executing a provider-native tool call. Distinguishes success from failure
/// so callers can return a structured failure to the agent instead of stalling or
/// crashing the session (Section 10.5).
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub success: bool,
    pub content: String,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            success: true,
            content: content.into(),
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            success: false,
            content: content.into(),
        }
    }
}

/// One comment on an issue thread, as read by AIR-5's approval-comment poller
/// (`orchestrator::poll_approval_comments`) -- a human replying `/approve`,
/// `/changes <reason>` or `/reject [reason]` on the issue itself.
#[derive(Debug, Clone)]
pub struct IssueComment {
    /// Provider-native comment id -- monotonically increasing within an issue, used
    /// as the poller's "already scanned up to here" cursor
    /// (`approvals::last_seen_comment_id`). Not necessarily a database id in every
    /// provider, just some stable, orderable identifier.
    pub id: u64,
    pub author: Option<String>,
    pub body: String,
}

#[async_trait]
pub trait TrackerAdapter: Send + Sync {
    /// Fetch normalized issues in the configured scope matching any of `states`
    /// (case-insensitive). Empty input MUST return empty without a provider request.
    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError>;

    /// Fetch current normalized snapshots for opaque dispatch IDs. Empty input MUST
    /// return empty without a provider request. IDs no longer visible are omitted.
    /// A malformed *requested* record MUST fail the whole call (Section 11.1).
    async fn fetch_issues_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError>;

    /// Provider-native agent tools this adapter exposes (Section 10.5, OPTIONAL).
    /// Default: none.
    fn agent_tool_specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    /// Create a new issue in `state` (a value from the same vocabulary
    /// `tracker.active_states`/`update_issue_state` already use, e.g. `"todo"` --
    /// *not* the terminal/closed state, since a freshly created issue is by
    /// definition not yet done). OPTIONAL, like the other agent-tool-adjacent
    /// capabilities on this trait: unimplemented by default. Added for SweBot's
    /// ticket-drafting capability (README.md "SweBot"), but generically useful --
    /// any caller that needs to hand a fully-formed, immediately-dispatchable issue
    /// to a tracker can use it, not just SweBot.
    async fn create_issue(
        &self,
        _title: &str,
        _body: &str,
        _state: &str,
    ) -> Result<Issue, TrackerError> {
        Err(TrackerError::Request(
            "create_issue is not supported by this tracker adapter".to_string(),
        ))
    }

    /// Execute a provider-native tool call host-side, with the adapter's configured
    /// credential, for the given opaque dispatch `issue_id`. Default: unsupported.
    async fn execute_agent_tool(
        &self,
        name: &str,
        _arguments: serde_json::Value,
        _issue_id: &str,
    ) -> ToolResult {
        ToolResult::error(format!("unsupported tool '{name}'"))
    }

    /// Directly set an issue's tracker state, host-side -- not through the agent-tool
    /// round trip `execute_agent_tool("update_issue_state", ...)` uses. Used by the
    /// delivery pipeline (`orchestrator::run_pipeline`) to park an issue in
    /// `pipeline.blocked_state` when a blocking stage fails: that's the orchestrator's
    /// own decision, not the agent's, so it goes through this instead of asking the
    /// agent to call a tool on its own behalf. Default: unsupported, like `create_issue`.
    async fn set_issue_state(&self, _issue_id: &str, _state: &str) -> Result<(), TrackerError> {
        Err(TrackerError::Request(
            "set_issue_state is not supported by this tracker adapter".to_string(),
        ))
    }

    /// Comments on `issue_id`'s own thread, oldest first -- AIR-5's second approval
    /// channel (`orchestrator::poll_approval_comments`) reads these looking for
    /// `/approve`, `/changes <reason>` or `/reject [reason]` so a human never has to
    /// open the dashboard. Default: unsupported adapters report no comments (`Ok(vec![])`)
    /// rather than erroring -- indistinguishable, from the poller's point of view, from
    /// "nothing new since last time," so it can poll every adapter uniformly without
    /// special-casing the ones that don't implement this yet.
    async fn fetch_issue_comments(
        &self,
        _issue_id: &str,
    ) -> Result<Vec<IssueComment>, TrackerError> {
        Ok(Vec::new())
    }
}

/// Construct the configured tracker adapter. Returns `UnsupportedTrackerKind` for any
/// `tracker.kind` this implementation does not ship.
pub fn build(
    kind: &str,
    provider: &serde_yaml::Value,
    workflow_dir: &std::path::Path,
) -> Result<Box<dyn TrackerAdapter>, TrackerError> {
    match kind {
        "local" => Ok(Box::new(local::LocalTrackerAdapter::new(
            provider,
            workflow_dir,
        )?)),
        "github" => Ok(Box::new(github::GithubTrackerAdapter::new(provider)?)),
        "gitlab" => Ok(Box::new(gitlab::GitlabTrackerAdapter::new(provider)?)),
        other => Err(TrackerError::UnsupportedTrackerKind(other.to_string())),
    }
}

pub const SUPPORTED_TRACKER_KINDS: &[&str] = &["local", "github", "gitlab"];
