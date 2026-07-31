use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Normalized schedulable work item (Section 4.1.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Issue {
    pub id: String,
    #[serde(default)]
    pub native_ref: Option<serde_json::Value>,
    pub identifier: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    pub state: String,
    #[serde(default)]
    pub branch_name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub assignee_id: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub blocked_by: Vec<BlockerRef>,
    pub dispatchable: bool,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

impl Issue {
    /// Normalized state comparison: trim + lowercase (Section 4.2).
    pub fn normalized_state(&self) -> String {
        self.state.trim().to_lowercase()
    }

    /// issue_routable(issue): dispatchable is true and all required_labels match (Section 8.2).
    pub fn is_routable(&self, required_labels: &[String]) -> bool {
        if !self.dispatchable {
            return false;
        }
        required_labels.iter().all(|required| {
            let required = required.trim().to_lowercase();
            !required.is_empty() && self.labels.iter().any(|l| l == &required)
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlockerRef {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub identifier: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

/// Parsed WORKFLOW.md payload (Section 4.1.2).
#[derive(Debug, Clone)]
pub struct WorkflowDefinition {
    pub config: serde_yaml::Value,
    pub prompt_template: String,
}
