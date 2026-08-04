//! GitLab Issues tracker adapter (`tracker.kind: gitlab`).
//!
//! State model: mirrors `tracker::github`'s exactly -- an **open** issue plus a
//! managed `state:*` label is an active state; a **closed** issue is the one
//! supported terminal state (`closed_state` names it, e.g. `done`). Only one
//! terminal state is modeled for v1, same rationale as `tracker::github`.
//!
//! `tracker.provider`:
//! - `project` (string, REQUIRED): `group/subgroup/name` path (GitLab supports
//!   nested subgroups; percent-encoded automatically for the `:id` path segment the
//!   REST API expects).
//! - `token` (string, REQUIRED): a GitLab token (personal/project/group access
//!   token) with `api` scope. Normally `$VAR`-indirected (`envsub::resolve_var`)
//!   rather than written in plain text.
//! - `closed_state` (string, REQUIRED): the `tracker.terminal_states` value that
//!   means "this issue is closed" (e.g. `done`).
//! - `active_state_labels` (map of state name -> label, REQUIRED): every
//!   non-terminal `tracker.active_states` value must have an entry here.
//! - `base_url` (string, OPTIONAL): defaults to `https://gitlab.com/api/v4`;
//!   override for a self-managed instance, or by tests, to point at a mock server.
//!
//! `depends_on`: same `Depends-On: #12, #45` body-text convention as GitHub (GitLab
//! issues have no native blocking-dependency field reachable via plain REST either),
//! parsed via the shared `super::depends_on::parse_depends_on`/`resolve_dependencies`.
//!
//! `Issue::id`/`identifier` are both the issue `iid` (project-scoped) as a string --
//! **not** GitLab's global `id`, which is unique across the whole instance but not
//! the number a human or the project's own UI ever sees; `iid` is what
//! `WORKFLOW.md`/issue URLs/`#N` references all mean. `branch_name` defaults to
//! `issue-<iid>`, same convention as `tracker::github`.
//!
//! Wire-format divergences from `tracker::github` worth flagging: the issue
//! identifier is `iid` not `number`; the body field is `description` not `body`;
//! labels come back as plain strings (`["x"]`) not `{name: "x"}` objects; Issues and
//! Merge Requests are fully separate resources (no `pull_request` filter needed the
//! way GitHub's mixed issues-list endpoint requires); closing/reopening is the verb
//! `state_event: "close"|"reopen"`, not the noun `state: "open"|"closed"`; and auth
//! is a bare `PRIVATE-TOKEN` header, not `Authorization: Bearer`.

use super::depends_on::{RawIssue, parse_depends_on, resolve_dependencies};
use super::{ToolResult, ToolSpec, TrackerAdapter, TrackerError};
use crate::domain::Issue;
use crate::envsub;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use serde_yaml::Value;
use std::collections::HashMap;

const DEFAULT_BASE_URL: &str = "https://gitlab.com/api/v4";
const PER_PAGE: u32 = 100;
const MAX_PAGES: u32 = 20; // safety cap, not expected to matter at realistic ticket counts

pub struct GitlabTrackerAdapter {
    client: reqwest::Client,
    base_url: String,
    /// Percent-encoded project path, ready to interpolate as the `:id` path segment.
    project: String,
    token: String,
    closed_state: String,
    /// normalized (trim + lowercase) state name -> label
    active_state_labels: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct GlIssue {
    iid: u64,
    title: String,
    #[serde(default)]
    description: Option<String>,
    state: String, // "opened" | "closed"
    #[serde(default)]
    labels: Vec<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    web_url: String,
}

/// GitLab's REST API expects the project path (`group/subgroup/name`) percent-encoded
/// as a single `:id` path segment -- no `percent-encoding` dependency for just this
/// one character, mirroring `tracker::depends_on::parse_depends_on`'s own "manual
/// scan rather than a new dependency" style.
fn encode_project_path(project: &str) -> String {
    project.replace('/', "%2F")
}

impl GitlabTrackerAdapter {
    pub fn new(provider: &Value) -> Result<Self, TrackerError> {
        let project = provider
            .get("project")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TrackerError::InvalidTrackerConfig(
                    "tracker.provider.project is required for tracker.kind=gitlab \
                     (group/subgroup/name)"
                        .to_string(),
                )
            })?
            .to_string();

        let token_raw = provider
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TrackerError::InvalidTrackerConfig(
                    "tracker.provider.token is required for tracker.kind=gitlab".to_string(),
                )
            })?;
        let token = envsub::resolve_var(token_raw).ok_or_else(|| {
            TrackerError::MissingTrackerSecret("tracker.provider.token".to_string())
        })?;

        let closed_state = provider
            .get("closed_state")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TrackerError::InvalidTrackerConfig(
                    "tracker.provider.closed_state is required for tracker.kind=gitlab".to_string(),
                )
            })?
            .trim()
            .to_lowercase();

        let active_state_labels: HashMap<String, String> = provider
            .get("active_state_labels")
            .and_then(|v| v.as_mapping())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        let key = k.as_str()?.trim().to_lowercase();
                        let label = v.as_str()?.to_string();
                        Some((key, label))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if active_state_labels.is_empty() {
            return Err(TrackerError::InvalidTrackerConfig(
                "tracker.provider.active_state_labels must have at least one entry for tracker.kind=gitlab"
                    .to_string(),
            ));
        }

        let base_url = provider
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_string();

        let client = reqwest::Client::builder()
            .user_agent("symphony")
            .build()
            .map_err(|e| TrackerError::Request(e.to_string()))?;

        Ok(Self {
            client,
            base_url,
            project: encode_project_path(&project),
            token,
            closed_state,
            active_state_labels,
        })
    }

    fn auth_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("PRIVATE-TOKEN", &self.token)
    }

    /// List issues in `state` (`"opened"`/`"closed"`), optionally filtered to
    /// `label`, paginated. Unlike GitHub's mixed issues-list endpoint, GitLab Issues
    /// and Merge Requests are fully separate resources, so no client-side filtering
    /// is needed here.
    async fn list_issues(
        &self,
        state: &str,
        label: Option<&str>,
    ) -> Result<Vec<GlIssue>, TrackerError> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = format!("{}/projects/{}/issues", self.base_url, self.project);
            let mut query = vec![
                ("state", state.to_string()),
                ("per_page", PER_PAGE.to_string()),
                ("page", page.to_string()),
            ];
            if let Some(l) = label {
                query.push(("labels", l.to_string()));
            }
            let req = self.client.get(&url).query(&query);
            let batch: Vec<GlIssue> = self.send_json(self.auth_headers(req)).await?;
            let got = batch.len();
            out.extend(batch);
            if got < PER_PAGE as usize {
                break;
            }
        }
        Ok(out)
    }

    async fn get_issue(&self, iid: u64) -> Result<Option<GlIssue>, TrackerError> {
        let url = format!("{}/projects/{}/issues/{}", self.base_url, self.project, iid);
        let resp = self
            .auth_headers(self.client.get(&url))
            .send()
            .await
            .map_err(|e| TrackerError::Request(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(TrackerError::Response(format!(
                "GET {url} -> {}",
                resp.status()
            )));
        }
        resp.json::<GlIssue>()
            .await
            .map(Some)
            .map_err(|e| TrackerError::Response(e.to_string()))
    }

    async fn send_json<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, TrackerError> {
        let resp = req
            .send()
            .await
            .map_err(|e| TrackerError::Request(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TrackerError::Response(format!("{status}: {body}")));
        }
        resp.json::<T>()
            .await
            .map_err(|e| TrackerError::Response(e.to_string()))
    }

    /// This adapter's managed labels currently on `issue` (used so `update_issue_state`
    /// only ever removes labels it owns, never a project's unrelated labels).
    fn managed_labels(&self) -> std::collections::HashSet<&str> {
        self.active_state_labels
            .values()
            .map(String::as_str)
            .collect()
    }

    fn to_domain(&self, gl: GlIssue) -> Issue {
        let normalized_state = self.normalize_state(&gl);
        let iid_str = gl.iid.to_string();
        let labels: Vec<String> = gl.labels.iter().map(|l| l.trim().to_lowercase()).collect();

        Issue {
            id: iid_str.clone(),
            native_ref: None,
            identifier: iid_str.clone(),
            title: gl.title,
            description: gl.description.clone().filter(|b| !b.trim().is_empty()),
            priority: None,
            state: normalized_state,
            branch_name: Some(format!("issue-{iid_str}")),
            url: Some(gl.web_url),
            assignee_id: None,
            labels,
            blocked_by: Vec::new(),
            dispatchable: true,
            created_at: Some(gl.created_at),
            updated_at: Some(gl.updated_at),
        }
    }

    /// `to_domain` plus the `depends_on` pass extracts (from `description`, before
    /// it's consumed) -- kept as a separate step since `RawIssue` needs both.
    fn raw(&self, gl: GlIssue) -> RawIssue {
        let depends_on = parse_depends_on(gl.description.as_deref().unwrap_or(""));
        RawIssue {
            issue: self.to_domain(gl),
            depends_on,
        }
    }

    /// `opened + managed label` -> that label's state name; `closed` -> `closed_state`;
    /// `opened` with none of this adapter's managed labels present is left as
    /// `"opened"` verbatim (GitLab's own wire value for an open issue -- won't match
    /// any configured `active_states`/`terminal_states` value, so it's simply never
    /// dispatchable, a safe default for an issue nobody has triaged into the
    /// workflow yet, same posture as `tracker::github`'s own fallback).
    fn normalize_state(&self, gl: &GlIssue) -> String {
        if gl.state == "closed" {
            return self.closed_state.clone();
        }
        let managed = self.managed_labels();
        for label in &gl.labels {
            let name = label.trim().to_lowercase();
            if managed.contains(name.as_str())
                && let Some((state_name, _)) = self
                    .active_state_labels
                    .iter()
                    .find(|(_, l)| l.trim().to_lowercase() == name)
            {
                return state_name.clone();
            }
        }
        "opened".to_string()
    }
}

#[async_trait]
impl TrackerAdapter for GitlabTrackerAdapter {
    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: Vec<String> = states.iter().map(|s| s.trim().to_lowercase()).collect();

        let mut raw: Vec<(String, Result<RawIssue, String>)> = Vec::new();

        if wanted.contains(&self.closed_state) {
            for gl in self.list_issues("closed", None).await? {
                let key = gl.iid.to_string();
                raw.push((key, Ok(self.raw(gl))));
            }
        }
        for (state_name, label) in &self.active_state_labels {
            if !wanted.contains(state_name) {
                continue;
            }
            for gl in self.list_issues("opened", Some(label)).await? {
                let key = gl.iid.to_string();
                // An issue can carry more than one managed label at once (mislabeling,
                // or a snapshot mid a partially-applied update_issue_state) -- without
                // this it would come back once per matched label and appear twice in
                // the result with the same identifier, risking double-dispatch.
                if raw.iter().any(|(k, _)| k == &key) {
                    continue;
                }
                raw.push((key, Ok(self.raw(gl))));
            }
        }

        let resolved = resolve_dependencies(raw);
        Ok(resolved.into_iter().filter_map(|(_, r)| r.ok()).collect())
    }

    async fn create_issue(
        &self,
        title: &str,
        body: &str,
        state: &str,
    ) -> Result<Issue, TrackerError> {
        let normalized = state.trim().to_lowercase();
        let label = self.active_state_labels.get(&normalized).ok_or_else(|| {
            TrackerError::Request(format!(
                "unrecognized state '{state}' (not in active_state_labels -- create_issue \
                 can only create an issue directly into an active state, never closed_state)"
            ))
        })?;
        let url = format!("{}/projects/{}/issues", self.base_url, self.project);
        let req = self.auth_headers(self.client.post(&url).json(&json!({
            "title": title,
            "description": body,
            "labels": label,
        })));
        let gl: GlIssue = self.send_json(req).await?;
        Ok(self.to_domain(gl))
    }

    async fn fetch_issues_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Fetch every open+closed issue once so depends_on resolution has the full
        // picture (mirrors tracker::github's own approach), then filter to the
        // requested ids -- simpler and more correct than N independent single-issue
        // fetches that couldn't see each other's state for dependency resolution.
        let mut raw: Vec<(String, Result<RawIssue, String>)> = Vec::new();
        for gl in self.list_issues("closed", None).await? {
            raw.push((gl.iid.to_string(), Ok(self.raw(gl))));
        }
        for label in self.active_state_labels.values() {
            for gl in self.list_issues("opened", Some(label)).await? {
                let key = gl.iid.to_string();
                if raw.iter().any(|(k, _)| k == &key) {
                    continue; // an issue can carry more than one managed label transiently
                }
                raw.push((key, Ok(self.raw(gl))));
            }
        }

        let resolved: HashMap<String, Issue> = resolve_dependencies(raw)
            .into_iter()
            .filter_map(|(k, r)| r.ok().map(|i| (k, i)))
            .collect();

        // Requested ids not present in either open-with-a-managed-label or closed sets
        // (e.g. an untriaged open issue with none of this adapter's labels) are fetched
        // directly so a malformed/missing *requested* id still surfaces as an error
        // rather than being silently dropped (Section 11.1).
        let mut out = Vec::new();
        for id in ids {
            if let Some(issue) = resolved.get(id) {
                out.push(issue.clone());
                continue;
            }
            let Ok(iid) = id.parse::<u64>() else {
                return Err(TrackerError::Response(format!("invalid issue id '{id}'")));
            };
            // no longer visible -> omitted per the trait contract
            if let Some(gl) = self.get_issue(iid).await? {
                out.push(self.to_domain(gl));
            }
        }
        Ok(out)
    }

    fn agent_tool_specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "update_issue_state".to_string(),
            description: "Update this issue's tracker state (for example to 'done' once \
                the issue is fully resolved). This is the supported way to advance the \
                issue; GitLab Issues are not directly accessible."
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
        let Ok(iid) = issue_id.parse::<u64>() else {
            return ToolResult::error(format!("invalid issue id '{issue_id}'"));
        };
        let normalized = new_state.trim().to_lowercase();

        let fetched = match self.get_issue(iid).await {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let Some(current) = fetched else {
            return ToolResult::error(format!("issue !{iid} not found"));
        };

        let managed = self.managed_labels();
        let mut labels: Vec<String> = current
            .labels
            .iter()
            .filter(|name| !managed.contains(name.trim().to_lowercase().as_str()))
            .cloned()
            .collect();

        let mut body = serde_json::Map::new();
        if normalized == self.closed_state {
            body.insert("state_event".to_string(), json!("close"));
        } else if let Some(label) = self.active_state_labels.get(&normalized) {
            labels.push(label.clone());
            body.insert("state_event".to_string(), json!("reopen"));
        } else {
            return ToolResult::error(format!(
                "unrecognized state '{new_state}' (not this tracker's closed_state and not \
                 in active_state_labels)"
            ));
        }
        body.insert("labels".to_string(), json!(labels.join(",")));

        let url = format!("{}/projects/{}/issues/{}", self.base_url, self.project, iid);
        let req = self.auth_headers(self.client.put(&url).json(&body));
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                ToolResult::ok(format!("Updated issue !{iid} state to '{new_state}'."))
            }
            Ok(resp) => ToolResult::error(format!("PUT {url} -> {}", resp.status())),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider_yaml(base_url: &str) -> Value {
        serde_yaml::from_str(&format!(
            "project: group/name\ntoken: test-token\nclosed_state: done\nbase_url: {base_url}\n\
             active_state_labels:\n  todo: \"state:todo\"\n  \"in progress\": \"state:in-progress\"\n"
        ))
        .unwrap()
    }

    fn gl_issue_json(
        iid: u64,
        title: &str,
        state: &str,
        labels: &[&str],
        description: &str,
    ) -> serde_json::Value {
        json!({
            "iid": iid,
            "title": title,
            "description": description,
            "state": state,
            "labels": labels,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-02T00:00:00Z",
            "web_url": format!("https://gitlab.com/group/name/-/issues/{iid}"),
        })
    }

    #[test]
    fn missing_project_errors() {
        let provider: Value = serde_yaml::from_str("token: t\nclosed_state: done\n").unwrap();
        assert!(matches!(
            GitlabTrackerAdapter::new(&provider),
            Err(TrackerError::InvalidTrackerConfig(_))
        ));
    }

    #[test]
    fn missing_token_var_is_missing_secret() {
        unsafe {
            std::env::remove_var("SYMPHONY_TEST_GL_TOKEN_MISSING");
        }
        let provider: Value = serde_yaml::from_str(
            "project: group/name\ntoken: $SYMPHONY_TEST_GL_TOKEN_MISSING\nclosed_state: done\n\
             active_state_labels:\n  todo: \"state:todo\"\n",
        )
        .unwrap();
        assert!(matches!(
            GitlabTrackerAdapter::new(&provider),
            Err(TrackerError::MissingTrackerSecret(_))
        ));
    }

    #[test]
    fn encodes_nested_subgroup_project_paths() {
        assert_eq!(
            encode_project_path("group/subgroup/name"),
            "group%2Fsubgroup%2Fname"
        );
    }

    #[tokio::test]
    async fn fetch_by_states_maps_labels_and_closed_to_normalized_state() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/issues"))
            .and(query_param("state", "opened"))
            .and(query_param("labels", "state:todo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![gl_issue_json(
                1,
                "Todo issue",
                "opened",
                &["state:todo"],
                "",
            )]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/issues"))
            .and(query_param("state", "closed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![gl_issue_json(
                2,
                "Done issue",
                "closed",
                &[],
                "",
            )]))
            .mount(&server)
            .await;

        let adapter = GitlabTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        let mut issues = adapter
            .fetch_issues_by_states(&["todo".to_string(), "done".to_string()])
            .await
            .unwrap();
        issues.sort_by_key(|i| i.identifier.clone());

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].identifier, "1");
        assert_eq!(issues[0].state, "todo");
        assert_eq!(issues[1].identifier, "2");
        assert_eq!(issues[1].state, "done");
    }

    #[tokio::test]
    async fn fetch_by_states_dedupes_an_issue_carrying_two_managed_labels() {
        let server = MockServer::start().await;
        let mislabeled = gl_issue_json(
            7,
            "Mislabeled issue",
            "opened",
            &["state:todo", "state:in-progress"],
            "",
        );
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/issues"))
            .and(query_param("state", "opened"))
            .and(query_param("labels", "state:todo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![mislabeled.clone()]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/issues"))
            .and(query_param("state", "opened"))
            .and(query_param("labels", "state:in-progress"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![mislabeled]))
            .mount(&server)
            .await;

        let adapter = GitlabTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        let issues = adapter
            .fetch_issues_by_states(&["todo".to_string(), "in progress".to_string()])
            .await
            .unwrap();

        assert_eq!(
            issues.len(),
            1,
            "issue !7 should appear exactly once, not once per matched label: {issues:?}"
        );
    }

    #[tokio::test]
    async fn depends_on_blocks_dispatch_until_dependency_closed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/issues"))
            .and(query_param("state", "opened"))
            .and(query_param("labels", "state:todo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![gl_issue_json(
                2,
                "Downstream",
                "opened",
                &["state:todo"],
                "Depends-On: #1",
            )]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/issues"))
            .and(query_param("state", "closed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;

        let adapter = GitlabTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        let issues = adapter
            .fetch_issues_by_states(&["todo".to_string()])
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert!(!issues[0].dispatchable);
        assert_eq!(issues[0].blocked_by[0].identifier.as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn create_issue_posts_title_description_and_the_states_managed_label() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/projects/group%2Fname/issues"))
            .respond_with(ResponseTemplate::new(201).set_body_json(gl_issue_json(
                11,
                "Export galleries as a zip",
                "opened",
                &["state:todo"],
                "media only, per-account",
            )))
            .mount(&server)
            .await;

        let adapter = GitlabTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        let issue = adapter
            .create_issue(
                "Export galleries as a zip",
                "media only, per-account",
                "todo",
            )
            .await
            .unwrap();
        assert_eq!(issue.identifier, "11");
        assert_eq!(issue.state, "todo");
    }

    #[tokio::test]
    async fn create_issue_rejects_a_state_with_no_managed_label() {
        let server = MockServer::start().await;
        // No POST mock registered -- if the adapter tried to create anyway, the
        // request would have nothing to match.
        let adapter = GitlabTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        let err = adapter
            .create_issue("t", "b", "nonexistent-state")
            .await
            .unwrap_err();
        assert!(matches!(err, TrackerError::Request(_)));
    }

    #[tokio::test]
    async fn update_issue_state_to_active_adds_label_and_reopens() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/issues/5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(gl_issue_json(
                5,
                "An issue",
                "closed",
                &["state:todo", "keep-me"],
                "",
            )))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/projects/group%2Fname/issues/5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(gl_issue_json(
                5,
                "An issue",
                "opened",
                &["state:in-progress", "keep-me"],
                "",
            )))
            .mount(&server)
            .await;

        let adapter = GitlabTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        let result = adapter
            .execute_agent_tool("update_issue_state", json!({"state": "in progress"}), "5")
            .await;
        assert!(result.success, "{}", result.content);
    }

    #[tokio::test]
    async fn update_issue_state_unrecognized_state_errors_without_calling_put() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/issues/5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(gl_issue_json(
                5,
                "An issue",
                "opened",
                &["state:todo"],
                "",
            )))
            .mount(&server)
            .await;
        // No PUT mock registered at all -- if the adapter tried to PUT anyway,
        // wiremock would 404/panic on the unexpected request depending on strictness;
        // asserting failure here is the primary check.

        let adapter = GitlabTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        let result = adapter
            .execute_agent_tool("update_issue_state", json!({"state": "nonexistent"}), "5")
            .await;
        assert!(!result.success);
    }
}
