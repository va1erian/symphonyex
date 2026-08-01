//! Local filesystem tracker adapter (`tracker.kind: local`).
//!
//! Not part of the spec's normative tracker set; it exists so Symphony can be run and
//! iterated on without wiring a real issue-tracker credential. Each issue is one
//! Markdown file (front matter + body) in a configured directory. Operators drive the
//! workflow by editing the `state:` field (and creating/removing files) by hand or with
//! their own scripts.
//!
//! `tracker.provider`:
//! - `dir` (path, REQUIRED): directory containing one `*.md` file per issue.
//!
//! Per-file front matter fields:
//! - `identifier` (string, REQUIRED)
//! - `title` (string, REQUIRED)
//! - `state` (string, REQUIRED)
//! - `id` (string, default: filename stem)
//! - `dispatchable` (bool, default: `true`)
//! - `priority` (integer, OPTIONAL)
//! - `labels` (list of strings, OPTIONAL)
//! - `assignee_id`, `url`, `branch_name` (strings, OPTIONAL)
//! - `created_at`, `updated_at` (RFC 3339, OPTIONAL; `updated_at` falls back to file mtime)
//! - `depends_on` (list of `identifier` strings, OPTIONAL): other issues in this same
//!   directory that must reach `state: done` before this one becomes dispatchable.
//!   Resolved on every read (not cached), so it always reflects current state. A
//!   dependency is "satisfied" only when its state is exactly `done` (case-insensitive)
//!   — not any other terminal state — since e.g. a cancelled prerequisite never
//!   delivered what depended work needs. Unsatisfied dependencies both force
//!   `dispatchable: false` (ANDed with the file's own `dispatchable` value) and are
//!   surfaced in `blocked_by` (Section 4.1.1) so logs/status views can show why an
//!   issue isn't running. A `depends_on` entry naming an identifier that doesn't exist
//!   (typo, or its file is malformed) is treated as permanently unsatisfied — fix the
//!   typo/file rather than expecting it to silently unblock.
//!
//! The Markdown body becomes `description` when no explicit `description` field is set.
//! This adapter has no secrets. It exposes one provider-native agent tool,
//! `update_issue_state`, so the coding agent can move an issue forward (e.g. to `done`)
//! without touching tracker files directly (Section 11.5) — see `execute_agent_tool`.

use super::depends_on::{RawIssue, resolve_dependencies};
use super::{ToolResult, ToolSpec, TrackerAdapter, TrackerError};
use crate::domain::Issue;
use crate::envsub;
use crate::frontmatter;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;
use serde_yaml::Value;
use std::path::{Path, PathBuf};

pub struct LocalTrackerAdapter {
    dir: PathBuf,
}

/// `(filename stem, parsed issue or error message)` for one candidate file.
type ParsedEntry = (String, Result<Issue, String>);

impl LocalTrackerAdapter {
    pub fn new(provider: &Value, workflow_dir: &Path) -> Result<Self, TrackerError> {
        let dir_raw = provider
            .get("dir")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TrackerError::InvalidTrackerConfig(
                    "tracker.provider.dir is required for tracker.kind=local".to_string(),
                )
            })?;
        let dir = envsub::resolve_path(dir_raw, workflow_dir);
        Ok(Self { dir })
    }

    fn read_all(&self) -> Result<Vec<ParsedEntry>, TrackerError> {
        if !self.dir.exists() {
            std::fs::create_dir_all(&self.dir).map_err(|e| {
                TrackerError::Request(format!("failed to create tracker dir {:?}: {e}", self.dir))
            })?;
        }
        let entries = std::fs::read_dir(&self.dir)
            .map_err(|e| TrackerError::Request(format!("failed to read {:?}: {e}", self.dir)))?;

        let mut raw = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|e| TrackerError::Request(format!("failed to read dir entry: {e}")))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let key = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            raw.push((key, parse_issue_file(&path)));
        }
        Ok(resolve_dependencies(raw))
    }

    /// Rewrite the `state:` (and `updated_at:`) front-matter fields of the issue file
    /// matching `issue_id`, preserving every other field and the Markdown body.
    fn update_issue_state(&self, issue_id: &str, new_state: &str) -> Result<(), String> {
        let all = self.read_all().map_err(|e| e.to_string())?;
        let path = all
            .into_iter()
            .find_map(|(key, parsed)| match parsed {
                Ok(issue) if issue.id == issue_id => Some(self.dir.join(format!("{key}.md"))),
                _ => None,
            })
            .ok_or_else(|| format!("no issue file found for id '{issue_id}'"))?;

        let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let (mut fm, body) = frontmatter::split(&raw).map_err(|e| e.to_string())?;
        let mapping = fm.as_mapping_mut().ok_or("front matter is not a map")?;
        mapping.insert(
            Value::String("state".to_string()),
            Value::String(new_state.to_string()),
        );
        mapping.insert(
            Value::String("updated_at".to_string()),
            Value::String(Utc::now().to_rfc3339()),
        );

        let yaml = serde_yaml::to_string(&fm).map_err(|e| e.to_string())?;
        let new_contents = if body.is_empty() {
            format!("---\n{yaml}---\n")
        } else {
            format!("---\n{yaml}---\n{body}\n")
        };
        std::fs::write(&path, new_contents).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl TrackerAdapter for LocalTrackerAdapter {
    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: Vec<String> = states.iter().map(|s| s.trim().to_lowercase()).collect();
        let all = self.read_all()?;
        let mut out = Vec::new();
        for (key, parsed) in all {
            match parsed {
                Ok(issue) => {
                    if wanted.contains(&issue.normalized_state()) {
                        out.push(issue);
                    }
                }
                Err(reason) => {
                    tracing::warn!(file = %key, reason = %reason, "skipping malformed issue file");
                }
            }
        }
        Ok(out)
    }

    async fn fetch_issues_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let all = self.read_all()?;
        let mut by_id = std::collections::HashMap::new();
        for (key, parsed) in all {
            match parsed {
                Ok(issue) => {
                    by_id.insert(issue.id.clone(), issue);
                }
                Err(reason) => {
                    if ids.contains(&key) {
                        return Err(TrackerError::Response(format!(
                            "malformed issue file for requested id '{key}': {reason}"
                        )));
                    }
                }
            }
        }
        Ok(ids.iter().filter_map(|id| by_id.get(id).cloned()).collect())
    }

    fn agent_tool_specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "update_issue_state".to_string(),
            description: "Update this issue's tracker state (for example to 'done' once \
                the issue is fully resolved). This is the supported way to advance the \
                issue; the tracker's storage is not directly accessible."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "description": "New state value, e.g. 'done'"
                    }
                },
                "required": ["state"]
            }),
        }]
    }

    async fn execute_agent_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
        issue_id: &str,
    ) -> ToolResult {
        if name != "update_issue_state" {
            return ToolResult::error(format!("unsupported tool '{name}'"));
        }
        let Some(new_state) = arguments.get("state").and_then(|v| v.as_str()) else {
            return ToolResult::error("missing required argument 'state'");
        };
        match self.update_issue_state(issue_id, new_state) {
            Ok(()) => ToolResult::ok(format!("Updated issue state to '{new_state}'.")),
            Err(e) => ToolResult::error(e),
        }
    }
}

fn parse_issue_file(path: &Path) -> Result<RawIssue, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let (fm, body) = frontmatter::split(&raw).map_err(|e| e.to_string())?;

    let id = fm
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        });
    let identifier = fm
        .get("identifier")
        .and_then(|v| v.as_str())
        .ok_or("missing required field 'identifier'")?
        .to_string();
    let title = fm
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or("missing required field 'title'")?
        .to_string();
    let state = fm
        .get("state")
        .and_then(|v| v.as_str())
        .ok_or("missing required field 'state'")?
        .to_string();

    if id.is_empty() || identifier.is_empty() || title.is_empty() || state.is_empty() {
        return Err("id, identifier, title, and state MUST be non-empty".to_string());
    }

    let dispatchable = fm
        .get("dispatchable")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let priority = fm.get("priority").and_then(|v| v.as_i64());

    let mut labels: Vec<String> = fm
        .get("labels")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    labels.sort();
    labels.dedup();

    let description = fm
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(if body.is_empty() { None } else { Some(body) });

    let created_at = fm
        .get("created_at")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let updated_at = fm
        .get("updated_at")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .map(DateTime::<Utc>::from)
        });

    let depends_on: Vec<String> = fm
        .get("depends_on")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    Ok(RawIssue {
        issue: Issue {
            id,
            native_ref: None,
            identifier,
            title,
            description,
            priority,
            state,
            branch_name: fm
                .get("branch_name")
                .and_then(|v| v.as_str())
                .map(String::from),
            url: fm.get("url").and_then(|v| v.as_str()).map(String::from),
            assignee_id: fm
                .get("assignee_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            labels,
            blocked_by: Vec::new(),
            dispatchable,
            created_at,
            updated_at,
        },
        depends_on,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_issue(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    #[tokio::test]
    async fn fetch_by_states_filters_and_normalizes() {
        let dir = tempdir().unwrap();
        write_issue(
            dir.path(),
            "a.md",
            "---\nidentifier: A-1\ntitle: Task A\nstate: Todo\n---\nbody a\n",
        );
        write_issue(
            dir.path(),
            "b.md",
            "---\nidentifier: B-1\ntitle: Task B\nstate: Done\n---\nbody b\n",
        );
        let provider: Value = serde_yaml::from_str(&format!("dir: {:?}", dir.path())).unwrap();
        let adapter = LocalTrackerAdapter::new(&provider, Path::new(".")).unwrap();

        let issues = adapter
            .fetch_issues_by_states(&["todo".to_string()])
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "A-1");
    }

    #[tokio::test]
    async fn empty_states_returns_empty_without_scan() {
        let dir = tempdir().unwrap();
        let provider: Value = serde_yaml::from_str(&format!("dir: {:?}", dir.path())).unwrap();
        let adapter = LocalTrackerAdapter::new(&provider, Path::new(".")).unwrap();
        let issues = adapter.fetch_issues_by_states(&[]).await.unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn fetch_by_ids_fails_on_malformed_requested_record() {
        let dir = tempdir().unwrap();
        write_issue(
            dir.path(),
            "bad.md",
            "---\ntitle: No identifier or state\n---\n",
        );
        let provider: Value = serde_yaml::from_str(&format!("dir: {:?}", dir.path())).unwrap();
        let adapter = LocalTrackerAdapter::new(&provider, Path::new(".")).unwrap();
        let err = adapter
            .fetch_issues_by_ids(&["bad".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(err, TrackerError::Response(_)));
    }

    #[tokio::test]
    async fn update_issue_state_tool_rewrites_state_and_preserves_body() {
        let dir = tempdir().unwrap();
        write_issue(
            dir.path(),
            "c.md",
            "---\nidentifier: C-1\ntitle: Task C\nstate: todo\nlabels: [x]\n---\nDo the thing.\n",
        );
        let provider: Value = serde_yaml::from_str(&format!("dir: {:?}", dir.path())).unwrap();
        let adapter = LocalTrackerAdapter::new(&provider, Path::new(".")).unwrap();

        let specs = adapter.agent_tool_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "update_issue_state");

        let result = adapter
            .execute_agent_tool("update_issue_state", json!({"state": "done"}), "c")
            .await;
        assert!(result.success, "{}", result.content);

        let issues = adapter
            .fetch_issues_by_states(&["done".to_string()])
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "C-1");
        assert_eq!(issues[0].labels, vec!["x".to_string()]);
        assert_eq!(issues[0].description.as_deref(), Some("Do the thing."));
    }

    #[tokio::test]
    async fn unsupported_tool_name_returns_structured_failure() {
        let dir = tempdir().unwrap();
        let provider: Value = serde_yaml::from_str(&format!("dir: {:?}", dir.path())).unwrap();
        let adapter = LocalTrackerAdapter::new(&provider, Path::new(".")).unwrap();
        let result = adapter
            .execute_agent_tool("delete_everything", json!({}), "c")
            .await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn depends_on_blocks_dispatch_until_dependency_is_done() {
        let dir = tempdir().unwrap();
        write_issue(
            dir.path(),
            "upstream.md",
            "---\nidentifier: UP-1\ntitle: Upstream\nstate: todo\n---\n",
        );
        write_issue(
            dir.path(),
            "downstream.md",
            "---\nidentifier: DOWN-1\ntitle: Downstream\nstate: todo\ndepends_on: [UP-1]\n---\n",
        );
        let provider: Value = serde_yaml::from_str(&format!("dir: {:?}", dir.path())).unwrap();
        let adapter = LocalTrackerAdapter::new(&provider, Path::new(".")).unwrap();

        let issues = adapter
            .fetch_issues_by_states(&["todo".to_string()])
            .await
            .unwrap();
        let downstream = issues.iter().find(|i| i.identifier == "DOWN-1").unwrap();
        assert!(!downstream.dispatchable);
        assert_eq!(downstream.blocked_by.len(), 1);
        assert_eq!(downstream.blocked_by[0].identifier.as_deref(), Some("UP-1"));
        assert_eq!(downstream.blocked_by[0].state.as_deref(), Some("todo"));

        // Once the dependency reaches exactly "done", downstream unblocks.
        adapter
            .execute_agent_tool("update_issue_state", json!({"state": "done"}), "upstream")
            .await;
        let issues = adapter
            .fetch_issues_by_states(&["todo".to_string()])
            .await
            .unwrap();
        let downstream = issues.iter().find(|i| i.identifier == "DOWN-1").unwrap();
        assert!(downstream.dispatchable);
        assert!(downstream.blocked_by.is_empty());
    }

    #[tokio::test]
    async fn depends_on_missing_target_stays_blocked() {
        let dir = tempdir().unwrap();
        write_issue(
            dir.path(),
            "downstream.md",
            "---\nidentifier: DOWN-1\ntitle: Downstream\nstate: todo\ndepends_on: [NOPE]\n---\n",
        );
        let provider: Value = serde_yaml::from_str(&format!("dir: {:?}", dir.path())).unwrap();
        let adapter = LocalTrackerAdapter::new(&provider, Path::new(".")).unwrap();

        let issues = adapter
            .fetch_issues_by_states(&["todo".to_string()])
            .await
            .unwrap();
        let downstream = &issues[0];
        assert!(!downstream.dispatchable);
        assert_eq!(downstream.blocked_by[0].state, None);
    }

    #[tokio::test]
    async fn cancelled_dependency_does_not_satisfy_depends_on() {
        let dir = tempdir().unwrap();
        write_issue(
            dir.path(),
            "upstream.md",
            "---\nidentifier: UP-1\ntitle: Upstream\nstate: cancelled\n---\n",
        );
        write_issue(
            dir.path(),
            "downstream.md",
            "---\nidentifier: DOWN-1\ntitle: Downstream\nstate: todo\ndepends_on: [UP-1]\n---\n",
        );
        let provider: Value = serde_yaml::from_str(&format!("dir: {:?}", dir.path())).unwrap();
        let adapter = LocalTrackerAdapter::new(&provider, Path::new(".")).unwrap();

        let issues = adapter
            .fetch_issues_by_states(&["todo".to_string()])
            .await
            .unwrap();
        let downstream = issues.iter().find(|i| i.identifier == "DOWN-1").unwrap();
        assert!(
            !downstream.dispatchable,
            "a cancelled dependency must not satisfy depends_on"
        );
    }
}
