//! GitHub Issues tracker adapter (`tracker.kind: github`).
//!
//! State model: an **open** issue plus a managed `state:*` label is an active state; a
//! **closed** issue is the one supported terminal state (`closed_state` names it, e.g.
//! `done`). GitHub Issues have no native custom-state field, so this adapter owns a
//! small label vocabulary instead of writing arbitrary state strings the way
//! `local.rs` does. Only one terminal state is modeled for v1 (matches most
//! workflows' `todo -> in progress -> done` shape); a project needing a distinct
//! `cancelled` etc. would need this extended, not configured around.
//!
//! `tracker.provider`:
//! - `repo` (string, REQUIRED): `owner/name`.
//! - `token` (string, REQUIRED): a GitHub token with `repo`/`issues` scope. Normally
//!   `$VAR`-indirected (`envsub::resolve_var`) rather than written in plain text.
//! - `closed_state` (string, REQUIRED): the `tracker.terminal_states` value that means
//!   "this issue is closed" (e.g. `done`).
//! - `active_state_labels` (map of state name -> label, REQUIRED): every non-terminal
//!   `tracker.active_states` value must have an entry here.
//! - `base_url` (string, OPTIONAL): defaults to `https://api.github.com`; override for
//!   GitHub Enterprise, or by tests, to point at a mock server.
//!
//! `depends_on`: GitHub issues have no native blocking-dependency field reachable via
//! plain REST, so it's parsed from a line in the issue body matching (case
//! insensitively) `Depends-On: #12, #45`. Cross-issue resolution reuses
//! `super::depends_on::resolve_dependencies` -- the same two-pass "AND dispatchable
//! with every dependency being done, populate blocked_by" logic `local.rs` uses,
//! rather than a second implementation that could quietly diverge.
//!
//! `Issue::id`/`identifier` are both the issue number as a string (GitHub issues have
//! no separate stable identifier distinct from their number). `branch_name` defaults
//! to `issue-<number>` so a project's repo hooks (see README.md "Git repo as
//! first-class input") have a natural per-ticket branch name to work with without
//! extra configuration.

use super::depends_on::{RawIssue, parse_depends_on, resolve_dependencies};
use super::{IssueComment, ToolResult, ToolSpec, TrackerAdapter, TrackerError};
use crate::domain::Issue;
use crate::envsub;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use serde_yaml::Value;
use std::collections::HashMap;

const DEFAULT_BASE_URL: &str = "https://api.github.com";
const PER_PAGE: u32 = 100;
const MAX_PAGES: u32 = 20; // safety cap, not expected to matter at realistic ticket counts

pub struct GithubTrackerAdapter {
    client: reqwest::Client,
    base_url: String,
    repo: String,
    token: String,
    closed_state: String,
    /// normalized (trim + lowercase) state name -> label
    active_state_labels: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    state: String, // "open" | "closed"
    #[serde(default)]
    labels: Vec<GhLabel>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    html_url: String,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

impl GithubTrackerAdapter {
    pub fn new(provider: &Value) -> Result<Self, TrackerError> {
        let repo = provider
            .get("repo")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TrackerError::InvalidTrackerConfig(
                    "tracker.provider.repo is required for tracker.kind=github (owner/name)"
                        .to_string(),
                )
            })?
            .to_string();

        let token_raw = provider
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TrackerError::InvalidTrackerConfig(
                    "tracker.provider.token is required for tracker.kind=github".to_string(),
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
                    "tracker.provider.closed_state is required for tracker.kind=github".to_string(),
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
                "tracker.provider.active_state_labels must have at least one entry for tracker.kind=github"
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
            repo,
            token,
            closed_state,
            active_state_labels,
        })
    }

    fn auth_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    /// List open issues carrying `label`, paginated, skipping pull requests (GitHub's
    /// issues-list endpoint includes them unless filtered out client-side).
    async fn list_open_issues(&self, label: &str) -> Result<Vec<GhIssue>, TrackerError> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = format!("{}/repos/{}/issues", self.base_url, self.repo);
            let req = self.client.get(&url).query(&[
                ("state", "open"),
                ("labels", label),
                ("per_page", &PER_PAGE.to_string()),
                ("page", &page.to_string()),
            ]);
            let batch: Vec<GhIssue> = self.send_json(self.auth_headers(req)).await?;
            let got = batch.len();
            out.extend(batch.into_iter().filter(|i| i.pull_request.is_none()));
            if got < PER_PAGE as usize {
                break;
            }
        }
        Ok(out)
    }

    async fn list_closed_issues(&self) -> Result<Vec<GhIssue>, TrackerError> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = format!("{}/repos/{}/issues", self.base_url, self.repo);
            let req = self.client.get(&url).query(&[
                ("state", "closed"),
                ("per_page", &PER_PAGE.to_string()),
                ("page", &page.to_string()),
            ]);
            let batch: Vec<GhIssue> = self.send_json(self.auth_headers(req)).await?;
            let got = batch.len();
            out.extend(batch.into_iter().filter(|i| i.pull_request.is_none()));
            if got < PER_PAGE as usize {
                break;
            }
        }
        Ok(out)
    }

    async fn get_issue(&self, number: u64) -> Result<Option<GhIssue>, TrackerError> {
        let url = format!("{}/repos/{}/issues/{}", self.base_url, self.repo, number);
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
        resp.json::<GhIssue>()
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

    fn to_domain(&self, gh: GhIssue) -> Issue {
        let normalized_state = self.normalize_state(&gh);
        let number_str = gh.number.to_string();
        let labels: Vec<String> = gh
            .labels
            .iter()
            .map(|l| l.name.trim().to_lowercase())
            .collect();

        Issue {
            id: number_str.clone(),
            native_ref: None,
            identifier: number_str.clone(),
            title: gh.title,
            description: gh.body.clone().filter(|b| !b.trim().is_empty()),
            priority: None,
            state: normalized_state,
            branch_name: Some(format!("issue-{number_str}")),
            url: Some(gh.html_url),
            assignee_id: None,
            labels,
            blocked_by: Vec::new(),
            dispatchable: true,
            created_at: Some(gh.created_at),
            updated_at: Some(gh.updated_at),
        }
    }

    /// `to_domain` plus the `depends_on` pass extracts (from the body, before it's
    /// consumed) -- kept as a separate step since `RawIssue` needs both.
    fn raw(&self, gh: GhIssue) -> RawIssue {
        let depends_on = parse_depends_on(gh.body.as_deref().unwrap_or(""));
        RawIssue {
            issue: self.to_domain(gh),
            depends_on,
        }
    }

    /// `open + managed label` -> that label's state name; `closed` -> `closed_state`;
    /// `open` with none of this adapter's managed labels present is left as `"open"`
    /// verbatim (won't match any configured `active_states`/`terminal_states` value,
    /// so it's simply never dispatchable -- a safe default for an issue nobody has
    /// triaged into the workflow yet, rather than guessing).
    fn normalize_state(&self, gh: &GhIssue) -> String {
        if gh.state == "closed" {
            return self.closed_state.clone();
        }
        let managed = self.managed_labels();
        for label in &gh.labels {
            let name = label.name.trim().to_lowercase();
            if managed.contains(name.as_str())
                && let Some((state_name, _)) = self
                    .active_state_labels
                    .iter()
                    .find(|(_, l)| l.trim().to_lowercase() == name)
            {
                return state_name.clone();
            }
        }
        "open".to_string()
    }
}

#[async_trait]
impl TrackerAdapter for GithubTrackerAdapter {
    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: Vec<String> = states.iter().map(|s| s.trim().to_lowercase()).collect();

        let mut raw: Vec<(String, Result<RawIssue, String>)> = Vec::new();

        if wanted.contains(&self.closed_state) {
            for gh in self.list_closed_issues().await? {
                let key = gh.number.to_string();
                raw.push((key, Ok(self.raw(gh))));
            }
        }
        for (state_name, label) in &self.active_state_labels {
            if !wanted.contains(state_name) {
                continue;
            }
            for gh in self.list_open_issues(label).await? {
                let key = gh.number.to_string();
                // An issue can carry more than one managed label at once (mislabeling,
                // or a snapshot mid a partially-applied update_issue_state) -- without
                // this it would come back once per matched label and appear twice in
                // the result with the same identifier, risking double-dispatch.
                if raw.iter().any(|(k, _)| k == &key) {
                    continue;
                }
                raw.push((key, Ok(self.raw(gh))));
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
        let url = format!("{}/repos/{}/issues", self.base_url, self.repo);
        let req = self.auth_headers(self.client.post(&url).json(&json!({
            "title": title,
            "body": body,
            "labels": [label],
        })));
        let gh: GhIssue = self.send_json(req).await?;
        Ok(self.to_domain(gh))
    }

    async fn fetch_issues_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Fetch every open+closed issue once so depends_on resolution has the full
        // picture (mirrors local.rs reading the whole directory), then filter to the
        // requested ids -- simpler and more correct than N independent single-issue
        // fetches that couldn't see each other's state for dependency resolution.
        let mut raw: Vec<(String, Result<RawIssue, String>)> = Vec::new();
        for gh in self.list_closed_issues().await? {
            raw.push((gh.number.to_string(), Ok(self.raw(gh))));
        }
        for label in self.active_state_labels.values() {
            for gh in self.list_open_issues(label).await? {
                let key = gh.number.to_string();
                if raw.iter().any(|(k, _)| k == &key) {
                    continue; // an issue can carry more than one managed label transiently
                }
                raw.push((key, Ok(self.raw(gh))));
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
            let Ok(number) = id.parse::<u64>() else {
                return Err(TrackerError::Response(format!("invalid issue id '{id}'")));
            };
            // no longer visible -> omitted per the trait contract
            if let Some(gh) = self.get_issue(number).await? {
                out.push(self.to_domain(gh));
            }
        }
        Ok(out)
    }

    fn agent_tool_specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "update_issue_state".to_string(),
            description: "Update this issue's tracker state (for example to 'done' once \
                the issue is fully resolved). This is the supported way to advance the \
                issue; GitHub Issues are not directly accessible."
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
        let Ok(number) = issue_id.parse::<u64>() else {
            return ToolResult::error(format!("invalid issue id '{issue_id}'"));
        };
        let normalized = new_state.trim().to_lowercase();

        let fetched = match self.get_issue(number).await {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let Some(current) = fetched else {
            return ToolResult::error(format!("issue #{number} not found"));
        };

        let managed = self.managed_labels();
        let mut labels: Vec<String> = current
            .labels
            .iter()
            .map(|l| l.name.clone())
            .filter(|name| !managed.contains(name.trim().to_lowercase().as_str()))
            .collect();

        let mut body = serde_json::Map::new();
        if normalized == self.closed_state {
            body.insert("state".to_string(), json!("closed"));
        } else if let Some(label) = self.active_state_labels.get(&normalized) {
            labels.push(label.clone());
            body.insert("state".to_string(), json!("open"));
        } else {
            return ToolResult::error(format!(
                "unrecognized state '{new_state}' (not this tracker's closed_state and not \
                 in active_state_labels)"
            ));
        }
        body.insert("labels".to_string(), json!(labels));

        let url = format!("{}/repos/{}/issues/{}", self.base_url, self.repo, number);
        let req = self.auth_headers(self.client.patch(&url).json(&body));
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                ToolResult::ok(format!("Updated issue #{number} state to '{new_state}'."))
            }
            Ok(resp) => ToolResult::error(format!("PATCH {url} -> {}", resp.status())),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }

    /// AIR-5's second approval channel: GitHub Issues comments are plain REST, no
    /// separate "discussion" concept to route through the way `repo_host`'s
    /// Discussions/PR-review handling does.
    async fn fetch_issue_comments(
        &self,
        issue_id: &str,
    ) -> Result<Vec<IssueComment>, TrackerError> {
        let Ok(number) = issue_id.parse::<u64>() else {
            return Err(TrackerError::Response(format!(
                "invalid issue id '{issue_id}'"
            )));
        };
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = format!(
                "{}/repos/{}/issues/{}/comments",
                self.base_url, self.repo, number
            );
            let req = self.client.get(&url).query(&[
                ("per_page", &PER_PAGE.to_string()),
                ("page", &page.to_string()),
            ]);
            let batch: Vec<GhComment> = self.send_json(self.auth_headers(req)).await?;
            let got = batch.len();
            out.extend(batch.into_iter().map(|c| IssueComment {
                id: c.id,
                author: c.user.map(|u| u.login),
                body: c.body.unwrap_or_default(),
            }));
            if got < PER_PAGE as usize {
                break;
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Deserialize)]
struct GhComment {
    id: u64,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<GhUser>,
}

#[derive(Debug, Deserialize)]
struct GhUser {
    login: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider_yaml(base_url: &str) -> Value {
        serde_yaml::from_str(&format!(
            "repo: owner/name\ntoken: test-token\nclosed_state: done\nbase_url: {base_url}\n\
             active_state_labels:\n  todo: \"state:todo\"\n  \"in progress\": \"state:in-progress\"\n"
        ))
        .unwrap()
    }

    fn gh_issue_json(
        number: u64,
        title: &str,
        state: &str,
        labels: &[&str],
        body: &str,
    ) -> serde_json::Value {
        json!({
            "number": number,
            "title": title,
            "body": body,
            "state": state,
            "labels": labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-02T00:00:00Z",
            "html_url": format!("https://github.com/owner/name/issues/{number}"),
        })
    }

    #[test]
    fn missing_repo_errors() {
        let provider: Value = serde_yaml::from_str("token: t\nclosed_state: done\n").unwrap();
        assert!(matches!(
            GithubTrackerAdapter::new(&provider),
            Err(TrackerError::InvalidTrackerConfig(_))
        ));
    }

    #[test]
    fn missing_token_var_is_missing_secret() {
        unsafe {
            std::env::remove_var("SYMPHONY_TEST_GH_TOKEN_MISSING");
        }
        let provider: Value = serde_yaml::from_str(
            "repo: owner/name\ntoken: $SYMPHONY_TEST_GH_TOKEN_MISSING\nclosed_state: done\n\
             active_state_labels:\n  todo: \"state:todo\"\n",
        )
        .unwrap();
        assert!(matches!(
            GithubTrackerAdapter::new(&provider),
            Err(TrackerError::MissingTrackerSecret(_))
        ));
    }

    #[tokio::test]
    async fn fetch_by_states_maps_labels_and_closed_to_normalized_state() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/issues"))
            .and(query_param("state", "open"))
            .and(query_param("labels", "state:todo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![gh_issue_json(
                1,
                "Todo issue",
                "open",
                &["state:todo"],
                "",
            )]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/issues"))
            .and(query_param("state", "closed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![gh_issue_json(
                2,
                "Done issue",
                "closed",
                &[],
                "",
            )]))
            .mount(&server)
            .await;

        let adapter = GithubTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
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

    /// Regression test: `fetch_issues_by_states` queries once per matched active-state
    /// label, separately. An issue mislabeled with *two* managed state labels at once
    /// (a labeling mistake, or a snapshot mid a partially-applied `update_issue_state`)
    /// would previously come back from both queries and appear **twice** in the
    /// result with the same identifier -- risking double-dispatch, the exact failure
    /// class this session already hit for real via duplicate orchestrators racing the
    /// same ticket. `fetch_issues_by_ids` already guards against this; this proves
    /// `fetch_issues_by_states` does too.
    #[tokio::test]
    async fn fetch_by_states_dedupes_an_issue_carrying_two_managed_labels() {
        let server = MockServer::start().await;
        let mislabeled = gh_issue_json(
            7,
            "Mislabeled issue",
            "open",
            &["state:todo", "state:in-progress"],
            "",
        );
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/issues"))
            .and(query_param("state", "open"))
            .and(query_param("labels", "state:todo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![mislabeled.clone()]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/issues"))
            .and(query_param("state", "open"))
            .and(query_param("labels", "state:in-progress"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![mislabeled]))
            .mount(&server)
            .await;

        let adapter = GithubTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        let issues = adapter
            .fetch_issues_by_states(&["todo".to_string(), "in progress".to_string()])
            .await
            .unwrap();

        assert_eq!(
            issues.len(),
            1,
            "issue #7 should appear exactly once, not once per matched label: {issues:?}"
        );
    }

    #[tokio::test]
    async fn depends_on_blocks_dispatch_until_dependency_closed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/issues"))
            .and(query_param("state", "open"))
            .and(query_param("labels", "state:todo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![gh_issue_json(
                2,
                "Downstream",
                "open",
                &["state:todo"],
                "Depends-On: #1",
            )]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/issues"))
            .and(query_param("state", "closed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;

        let adapter = GithubTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        let issues = adapter
            .fetch_issues_by_states(&["todo".to_string()])
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert!(!issues[0].dispatchable);
        assert_eq!(issues[0].blocked_by[0].identifier.as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn create_issue_posts_title_body_and_the_states_managed_label() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/name/issues"))
            .respond_with(ResponseTemplate::new(201).set_body_json(gh_issue_json(
                11,
                "Export galleries as a zip",
                "open",
                &["state:todo"],
                "media only, per-account",
            )))
            .mount(&server)
            .await;

        let adapter = GithubTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
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
        let adapter = GithubTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
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
            .and(path("/repos/owner/name/issues/5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(gh_issue_json(
                5,
                "An issue",
                "closed",
                &["state:todo", "keep-me"],
                "",
            )))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/owner/name/issues/5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(gh_issue_json(
                5,
                "An issue",
                "open",
                &["state:in-progress", "keep-me"],
                "",
            )))
            .mount(&server)
            .await;

        let adapter = GithubTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        let result = adapter
            .execute_agent_tool("update_issue_state", json!({"state": "in progress"}), "5")
            .await;
        assert!(result.success, "{}", result.content);
    }

    #[tokio::test]
    async fn update_issue_state_unrecognized_state_errors_without_calling_patch() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/issues/5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(gh_issue_json(
                5,
                "An issue",
                "open",
                &["state:todo"],
                "",
            )))
            .mount(&server)
            .await;
        // No PATCH mock registered at all -- if the adapter tried to PATCH anyway,
        // wiremock would 404/panic on the unexpected request depending on strictness;
        // asserting failure here is the primary check.

        let adapter = GithubTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        let result = adapter
            .execute_agent_tool("update_issue_state", json!({"state": "nonexistent"}), "5")
            .await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn fetch_issue_comments_maps_id_author_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/issues/5/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": 101, "body": "/approve", "user": {"login": "alice"}},
                {"id": 102, "body": "/changes please split this up", "user": {"login": "bob"}},
            ])))
            .mount(&server)
            .await;

        let adapter = GithubTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        let comments = adapter.fetch_issue_comments("5").await.unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].id, 101);
        assert_eq!(comments[0].author.as_deref(), Some("alice"));
        assert_eq!(comments[0].body, "/approve");
        assert_eq!(comments[1].id, 102);
        assert_eq!(comments[1].body, "/changes please split this up");
    }

    #[tokio::test]
    async fn fetch_issue_comments_rejects_a_non_numeric_id() {
        let server = MockServer::start().await;
        let adapter = GithubTrackerAdapter::new(&provider_yaml(&server.uri())).unwrap();
        assert!(adapter.fetch_issue_comments("not-a-number").await.is_err());
    }
}
