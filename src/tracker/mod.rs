//! Issue tracker adapter boundary (Section 11): a small portable read kernel.

pub mod depends_on;
pub mod github;
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
        other => Err(TrackerError::UnsupportedTrackerKind(other.to_string())),
    }
}

pub const SUPPORTED_TRACKER_KINDS: &[&str] = &["local", "github"];
