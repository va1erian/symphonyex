//! GitLab repo-host operations: merge-request automation (`repo.pull_request: true`)
//! and, for SweBot (README.md "SweBot"), label-filtered Issues (Q&A/drafting) and
//! MR review.
//!
//! GitLab's REST v4 API covers everything needed here directly -- unlike GitHub
//! Discussions, there's no GraphQL-only resource to work around (see
//! `super::github`'s own module doc comment for that contrast).
//!
//! Review semantics genuinely diverge from GitHub's and are handled entirely inside
//! this module (never leaking into `swebot::review`, which only sees the
//! provider-agnostic `ReviewVerdict`/`ReviewOutcome`):
//! - No formal "request changes" review state on GitLab core -- posted as a plainly
//!   prefixed MR discussion note instead.
//! - Approve is two calls: `POST .../approve` (which carries no text field) plus a
//!   marker note, so `has_reviewed_sha`'s marker scan (over Notes -- GitLab has no
//!   separate "reviews" resource the way GitHub does) has something to find.
//! - GitLab's "prevent approval by author" project setting only blocks the *approve*
//!   endpoint, not plain notes -- narrower than GitHub's blanket self-review block,
//!   so a request-changes/comment verdict never needs the fallback below. Detected by
//!   HTTP status class (4xx) on the approve call, not error-message substring
//!   matching -- GitLab's exact wording is less version-stable than GitHub's.

use super::{
    DiscussionComment, DiscussionHost, DiscussionThread, RepoHost, ReviewOutcome, ReviewVerdict,
    SymphonyPullRequest,
};
use crate::config::{RepoConfig, RepoProvider};
use crate::tracker::{ToolResult, ToolSpec};
use crate::workspace;
use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

#[derive(Debug)]
pub struct GitlabRepoHost {
    client: reqwest::Client,
    base_url: String,
    /// Percent-encoded project path, ready to interpolate as the `:id` path segment.
    project: String,
    token: String,
    default_branch: String,
    /// `repo.evidence` -- whether `attach_evidence` should be offered alongside
    /// `open_pull_request` (see `RepoConfig::evidence`'s own doc comment).
    evidence: bool,
}

/// GitLab's REST API expects the project path (`group/subgroup/name`) percent-encoded
/// as a single `:id` path segment -- no `percent-encoding` dependency for just this
/// one character, same convention `tracker::gitlab`'s own copy of this helper uses
/// (kept independent rather than shared -- see this module's own doc comment on
/// staying independent from `tracker::gitlab`).
fn encode_project_path(project: &str) -> String {
    project.replace('/', "%2F")
}

/// Parse `(origin, project_path)` out of a GitLab repo URL, HTTPS or SSH, with or
/// without a trailing `.git` -- `origin` is `scheme://host` (used to derive a default
/// API base URL for a self-managed instance when `repo.api_base_url` isn't set),
/// `project_path` is the `group/subgroup/name` path GitLab's API expects. Unlike
/// `github::parse_github_owner_repo`, this accepts *any* host: `repo.provider:
/// gitlab` is already explicit, so there's no need to sniff a specific hostname the
/// way GitHub detection does (and self-managed GitLab is commonly at an arbitrary
/// hostname anyway).
pub fn parse_gitlab_url(url: &str) -> Option<(String, String)> {
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        let scheme = if url.starts_with("https://") {
            "https"
        } else {
            "http"
        };
        let mut parts = rest.splitn(2, '/');
        let host = parts.next()?;
        let path = parts.next()?.trim_end_matches('/').trim_end_matches(".git");
        if host.is_empty() || path.is_empty() {
            return None;
        }
        Some((format!("{scheme}://{host}"), path.to_string()))
    } else if let Some(rest) = url.strip_prefix("git@") {
        let mut parts = rest.splitn(2, ':');
        let host = parts.next()?;
        let path = parts.next()?.trim_end_matches('/').trim_end_matches(".git");
        if host.is_empty() || path.is_empty() {
            return None;
        }
        // SSH is for git operations only; the REST API is always reached over HTTPS.
        Some((format!("https://{host}"), path.to_string()))
    } else {
        None
    }
}

/// Whether `url` parses as a GitLab repo URL at all (used by `config::resolve`'s
/// `repo.pull_request`/`swebot.enabled` validation -- it only needs the yes/no, not
/// the parsed pieces `GitlabRepoHost::new` needs).
pub fn parse_gitlab_project_path(url: &str) -> Option<String> {
    parse_gitlab_url(url).map(|(_, path)| path)
}

impl GitlabRepoHost {
    pub fn new(repo: &RepoConfig) -> Result<Self, String> {
        let (origin, project_path) = parse_gitlab_url(&repo.url).ok_or_else(|| {
            format!(
                "repo.url '{}' could not be parsed as a GitLab repo URL",
                repo.url
            )
        })?;
        let base_url = repo
            .api_base_url
            .clone()
            .unwrap_or_else(|| format!("{origin}/api/v4"));
        let token_env = repo
            .token_env
            .as_ref()
            .ok_or("repo.token is required for repo.pull_request")?;
        let token = std::env::var(token_env)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("env var '{token_env}' (repo.token) is unset or empty"))?;
        let client = reqwest::Client::builder()
            .user_agent("symphony")
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            base_url,
            project: encode_project_path(&project_path),
            token,
            default_branch: repo.default_branch.clone(),
            evidence: repo.evidence,
        })
    }

    /// The GitLab *origin* (`scheme://host`), derived from `base_url` (the REST API
    /// root, always ending in `/api/v4` -- see `GitlabRepoHost::new`) by stripping that
    /// suffix. Needed only for building a `-/raw/` content URL, which lives under the
    /// plain web origin, not the API root.
    fn origin(&self) -> &str {
        self.base_url.trim_end_matches("/api/v4")
    }

    fn auth_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("PRIVATE-TOKEN", &self.token)
    }

    async fn find_open_mr(&self, source_branch: &str) -> Result<Option<u64>, String> {
        let url = format!("{}/projects/{}/merge_requests", self.base_url, self.project);
        let req = self
            .client
            .get(&url)
            .query(&[("state", "opened"), ("source_branch", source_branch)]);
        let resp = self
            .auth_headers(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("GET {url} -> {status}: {text}"));
        }
        let mrs: Vec<GlMergeRequest> = resp.json().await.map_err(|e| e.to_string())?;
        Ok(mrs.first().map(|m| m.iid))
    }

    async fn create_mr(&self, source_branch: &str, title: &str, body: &str) -> ToolResult {
        let url = format!("{}/projects/{}/merge_requests", self.base_url, self.project);
        let req = self.client.post(&url).json(&json!({
            "title": title,
            "description": body,
            "source_branch": source_branch,
            "target_branch": self.default_branch,
        }));
        match self.auth_headers(req).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json::<GlMergeRequest>().await {
                Ok(mr) => ToolResult::ok(format!("Opened merge request: {}", mr.web_url)),
                Err(e) => {
                    ToolResult::error(format!("created MR but failed to parse response: {e}"))
                }
            },
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                ToolResult::error(format!(
                    "POST {url} -> {status}: {text} (has this branch been pushed yet?)"
                ))
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }

    async fn update_mr(&self, iid: u64, title: &str, body: &str) -> ToolResult {
        let url = format!(
            "{}/projects/{}/merge_requests/{iid}",
            self.base_url, self.project
        );
        let req = self
            .client
            .put(&url)
            .json(&json!({"title": title, "description": body}));
        match self.auth_headers(req).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json::<GlMergeRequest>().await {
                Ok(mr) => ToolResult::ok(format!("Updated merge request: {}", mr.web_url)),
                Err(e) => {
                    ToolResult::error(format!("updated MR but failed to parse response: {e}"))
                }
            },
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                ToolResult::error(format!("PUT {url} -> {status}: {text}"))
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }

    /// GitLab counterpart of `github::GithubRepoHost::attach_evidence` -- same
    /// content-hashed repo path (so a plain create always succeeds, no existing-blob
    /// `sha`/lock-token juggling), same markdown-snippet return shape, but via GitLab's
    /// Repository Files API (`POST .../repository/files/:file_path`, `encoding: base64`)
    /// and a `-/raw/<branch>/<path>` URL under the plain web origin instead of a
    /// separate raw-content host.
    async fn attach_evidence(
        &self,
        workspace_dir: &Path,
        image_path: &str,
        caption: &str,
        issue_id: &str,
    ) -> ToolResult {
        let full_path = workspace_dir.join(image_path);
        let bytes = match tokio::fs::read(&full_path).await {
            Ok(b) => b,
            Err(e) => {
                return ToolResult::error(format!(
                    "could not read image at '{image_path}' (resolved to {}): {e}",
                    full_path.display()
                ));
            }
        };
        let file_name = Path::new(image_path)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("evidence.png");
        let digest = <sha2::Sha256 as sha2::Digest>::digest(&bytes);
        let short_hash = digest
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let branch = format!("issue-{}", workspace::derive_workspace_key(issue_id));
        let repo_path = format!(
            ".symphony/evidence/{}/{short_hash}-{file_name}",
            workspace::derive_workspace_key(issue_id)
        );
        let url = format!(
            "{}/projects/{}/repository/files/{}",
            self.base_url,
            self.project,
            encode_project_path(&repo_path)
        );
        let content_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let req = self.client.post(&url).json(&json!({
            "branch": branch,
            "content": content_b64,
            "encoding": "base64",
            "commit_message": format!("Attach evidence: {caption}"),
        }));
        match self.auth_headers(req).send().await {
            Ok(resp) if resp.status().is_success() => {
                let raw_url = format!(
                    "{}/{}/-/raw/{branch}/{repo_path}",
                    self.origin(),
                    self.project.replace("%2F", "/")
                );
                ToolResult::ok(format!(
                    "Evidence uploaded. Paste this into the merge request body to embed it:\n\n![{caption}]({raw_url})"
                ))
            }
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                ToolResult::error(format!(
                    "POST {url} -> {status}: {text} (has this branch been pushed yet?)"
                ))
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }

    /// Same marker convention `github::GithubRepoHost::review_marker` uses -- scanned
    /// here over MR Notes (GitLab has no separate "reviews" resource the way GitHub
    /// does, so Notes is the only place to look).
    fn review_marker(head_sha: &str) -> String {
        format!("<!-- swebot:reviewed:{head_sha} -->")
    }

    /// Post a Notes-based review comment, with the SHA-specific marker prefixed so
    /// `has_reviewed_sha` can find it later.
    async fn post_review_note(
        &self,
        mr_iid: u64,
        head_sha: &str,
        body: &str,
    ) -> Result<(), String> {
        let marked = format!("{}\n{body}", Self::review_marker(head_sha));
        let url = format!(
            "{}/projects/{}/merge_requests/{mr_iid}/notes",
            self.base_url, self.project
        );
        let req = self.client.post(&url).json(&json!({"body": marked}));
        match self.auth_headers(req).send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                Err(format!("POST {url} -> {status}: {text}"))
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

#[async_trait]
impl RepoHost for GitlabRepoHost {
    fn provider_kind(&self) -> RepoProvider {
        RepoProvider::Gitlab
    }

    fn agent_tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![ToolSpec {
            name: "open_pull_request".to_string(),
            description: "Open (or update, if one already exists) a merge request for \
                this ticket's branch, with a title and a body describing the work and \
                the rationale behind it. This is the way to submit work for review -- \
                call it once your branch has been pushed and you're satisfied with the \
                change. Include 'Closes #<issue-number>' in the body so the tracker \
                issue closes automatically when this merge request is merged; do not \
                call update_issue_state with a terminal/'done' state yourself in this \
                workflow -- closing is a side effect of merge, not something to do \
                before anyone has reviewed the code. After this succeeds, if the \
                project's tracker config offers a non-terminal state for this (e.g. \
                'in review'), call update_issue_state with that -- otherwise the \
                tracker keeps treating this ticket as active work still in progress \
                and you'll keep being redispatched to it with nothing new to do."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Merge request title"},
                    "body": {
                        "type": "string",
                        "description": "Merge request body: what changed, why, and 'Closes #N'"
                    }
                },
                "required": ["title", "body"]
            }),
        }];
        if self.evidence {
            specs.push(ToolSpec {
                name: "attach_evidence".to_string(),
                description: "Upload an image file you already saved somewhere inside this \
                    workspace (e.g. a screenshot of the app running, taken with a headless \
                    browser) to this merge request's branch, and get back a markdown image \
                    snippet. Paste that snippet into the merge request body \
                    (open_pull_request's 'body' argument) so a reviewer can see the \
                    evidence directly instead of taking your word for it. Call this before \
                    open_pull_request so the snippet is ready to include; calling \
                    open_pull_request again afterwards with an updated body works too."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "image_path": {
                            "type": "string",
                            "description": "Path to the image file, relative to this workspace's root"
                        },
                        "caption": {
                            "type": "string",
                            "description": "Short caption describing what the image shows"
                        }
                    },
                    "required": ["image_path", "caption"]
                }),
            });
        }
        specs
    }

    async fn execute_agent_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
        issue_id: &str,
        workspace_dir: &Path,
    ) -> ToolResult {
        match name {
            "open_pull_request" => {
                let Some(title) = arguments.get("title").and_then(|v| v.as_str()) else {
                    return ToolResult::error("missing required argument 'title'");
                };
                let Some(body) = arguments.get("body").and_then(|v| v.as_str()) else {
                    return ToolResult::error("missing required argument 'body'");
                };
                let head = format!("issue-{}", workspace::derive_workspace_key(issue_id));

                match self.find_open_mr(&head).await {
                    Ok(Some(iid)) => self.update_mr(iid, title, body).await,
                    Ok(None) => self.create_mr(&head, title, body).await,
                    Err(e) => ToolResult::error(e),
                }
            }
            "attach_evidence" if self.evidence => {
                let Some(image_path) = arguments.get("image_path").and_then(|v| v.as_str()) else {
                    return ToolResult::error("missing required argument 'image_path'");
                };
                let Some(caption) = arguments.get("caption").and_then(|v| v.as_str()) else {
                    return ToolResult::error("missing required argument 'caption'");
                };
                self.attach_evidence(workspace_dir, image_path, caption, issue_id)
                    .await
            }
            _ => ToolResult::error(format!("unsupported tool '{name}'")),
        }
    }

    async fn list_open_symphony_prs(&self) -> Result<Vec<SymphonyPullRequest>, String> {
        let url = format!("{}/projects/{}/merge_requests", self.base_url, self.project);
        let req = self
            .client
            .get(&url)
            .query(&[("state", "opened"), ("per_page", "100")]);
        let resp = self
            .auth_headers(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("GET {url} -> {status}: {text}"));
        }
        let all: Vec<GlMergeRequestFull> = resp.json().await.map_err(|e| e.to_string())?;
        Ok(all
            .into_iter()
            .filter(|mr| mr.source_branch.starts_with("issue-"))
            .map(|mr| SymphonyPullRequest {
                number: mr.iid,
                html_url: mr.web_url,
                head_ref: mr.source_branch,
                head_sha: mr.sha,
                body: mr.description.unwrap_or_default(),
            })
            .collect())
    }

    async fn has_reviewed_sha(&self, pr_number: u64, head_sha: &str) -> Result<bool, String> {
        let marker = Self::review_marker(head_sha);
        let url = format!(
            "{}/projects/{}/merge_requests/{pr_number}/notes",
            self.base_url, self.project
        );
        let req = self.client.get(&url).query(&[("per_page", "100")]);
        let resp = self
            .auth_headers(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("GET {url} -> {status}: {text}"));
        }
        let notes: Vec<GlNote> = resp.json().await.map_err(|e| e.to_string())?;
        Ok(notes.iter().any(|n| n.body.contains(&marker)))
    }

    /// See this module's own doc comment for why GitLab's review semantics diverge
    /// from GitHub's: no formal request-changes state (posted as a plain prefixed
    /// note), approve is two calls (the approve endpoint itself, which carries no
    /// text, plus a marker note), and the self-approval restriction only blocks the
    /// approve endpoint (detected by HTTP status class, not error-message text).
    async fn post_pr_review(
        &self,
        pr_number: u64,
        head_sha: &str,
        verdict: ReviewVerdict,
        summary: &str,
    ) -> Result<ReviewOutcome, String> {
        match verdict {
            ReviewVerdict::Approve => {
                let url = format!(
                    "{}/projects/{}/merge_requests/{pr_number}/approve",
                    self.base_url, self.project
                );
                let req = self.client.post(&url).json(&json!({"sha": head_sha}));
                match self.auth_headers(req).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        self.post_review_note(pr_number, head_sha, summary).await?;
                        Ok(ReviewOutcome {
                            posted_as: ReviewVerdict::Approve,
                        })
                    }
                    Ok(resp) if resp.status().is_client_error() => {
                        let note = format!(
                            "(SweBot would approve this, but GitLab rejected the approval -- \
                             likely the \"prevent approval by author\" project setting, since \
                             the coding agent and SweBot are using the same token here. \
                             Posting as a comment instead. Set `swebot.token` to a separate \
                             GitLab identity to avoid this.)\n\n{summary}"
                        );
                        self.post_review_note(pr_number, head_sha, &note).await?;
                        Ok(ReviewOutcome {
                            posted_as: ReviewVerdict::Comment,
                        })
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        Err(format!("POST {url} -> {status}: {text}"))
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
            ReviewVerdict::RequestChanges => {
                let note = format!("**Requesting changes:**\n\n{summary}");
                self.post_review_note(pr_number, head_sha, &note).await?;
                Ok(ReviewOutcome {
                    posted_as: ReviewVerdict::RequestChanges,
                })
            }
            ReviewVerdict::Comment => {
                self.post_review_note(pr_number, head_sha, summary).await?;
                Ok(ReviewOutcome {
                    posted_as: ReviewVerdict::Comment,
                })
            }
        }
    }
}

#[async_trait]
impl DiscussionHost for GitlabRepoHost {
    /// Issues carrying `selector` (a label, e.g. `"swebot::question"`), open, newest
    /// 25 -- same "not expected to matter at realistic conversation volumes"
    /// pragmatism `github::GithubRepoHost::list_swebot_threads` documents. Each
    /// issue's Notes become `comments`, filtered to `system: false` (GitLab's Notes
    /// list includes system-generated notes like "label added", which GitHub's
    /// GraphQL Discussions query has no analog of needing to filter). GitLab notes
    /// have no reply-nesting the way GitHub Discussions do, so -- unlike
    /// `list_swebot_threads` there -- there's no `replies` flattening step needed:
    /// they're already a flat chronological list.
    async fn list_swebot_threads(&self, selector: &str) -> Result<Vec<DiscussionThread>, String> {
        let url = format!("{}/projects/{}/issues", self.base_url, self.project);
        let req = self.client.get(&url).query(&[
            ("labels", selector),
            ("state", "opened"),
            ("per_page", "25"),
        ]);
        let resp = self
            .auth_headers(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("GET {url} -> {status}: {text}"));
        }
        let issues: Vec<GlIssueFull> = resp.json().await.map_err(|e| e.to_string())?;

        let mut threads = Vec::with_capacity(issues.len());
        for issue in issues {
            let notes_url = format!(
                "{}/projects/{}/issues/{}/notes",
                self.base_url, self.project, issue.iid
            );
            let req = self.client.get(&notes_url).query(&[("per_page", "100")]);
            let resp = self
                .auth_headers(req)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("GET {notes_url} -> {status}: {text}"));
            }
            let notes: Vec<GlNote> = resp.json().await.map_err(|e| e.to_string())?;
            let comments = notes
                .into_iter()
                .filter(|n| !n.system)
                .map(|n| DiscussionComment {
                    database_id: n.id,
                    body: n.body,
                    author_login: n.author.map(|a| a.username),
                })
                .collect();
            threads.push(DiscussionThread {
                id: issue.iid.to_string(),
                number: issue.iid,
                title: issue.title,
                body: issue.description.unwrap_or_default(),
                url: issue.web_url,
                comments,
            });
        }
        Ok(threads)
    }

    /// Reply to a label-filtered issue. Returns the new note's numeric id (as a
    /// string, matching `DiscussionComment::database_id`'s convention).
    async fn post_discussion_comment(&self, thread_id: &str, body: &str) -> Result<String, String> {
        let url = format!(
            "{}/projects/{}/issues/{thread_id}/notes",
            self.base_url, self.project
        );
        let req = self.client.post(&url).json(&json!({"body": body}));
        let resp = self
            .auth_headers(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("POST {url} -> {status}: {text}"));
        }
        let note: GlNote = resp.json().await.map_err(|e| e.to_string())?;
        Ok(note.id.to_string())
    }

    /// Close the source idea issue once ticket drafting has promoted it to a real
    /// tracker issue (see `swebot::drafting`'s `close_thread` call) -- keeps
    /// `list_swebot_threads`'s label-filtered polling query from accumulating stale,
    /// already-drafted idea issues forever.
    async fn close_thread(&self, thread_id: &str) -> Result<(), String> {
        let url = format!(
            "{}/projects/{}/issues/{thread_id}",
            self.base_url, self.project
        );
        let req = self.client.put(&url).json(&json!({"state_event": "close"}));
        match self.auth_headers(req).send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                Err(format!("PUT {url} -> {status}: {text}"))
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GlMergeRequest {
    iid: u64,
    web_url: String,
}

#[derive(Debug, Deserialize)]
struct GlMergeRequestFull {
    iid: u64,
    web_url: String,
    #[serde(default)]
    description: Option<String>,
    source_branch: String,
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GlIssueFull {
    iid: u64,
    title: String,
    #[serde(default)]
    description: Option<String>,
    web_url: String,
}

#[derive(Debug, Deserialize)]
struct GlNote {
    id: u64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    system: bool,
    author: Option<GlNoteAuthor>,
}

#[derive(Debug, Deserialize)]
struct GlNoteAuthor {
    username: String,
}

// RepoConfig has no api-base-url-for-test override distinct from `repo.api_base_url`
// itself -- tests set `api_base_url` directly on the `RepoConfig` they build, unlike
// `GithubRepoHost`'s `with_base_url_for_test` (GitHub's base URL isn't derived from
// config at all, so it needed a test-only backdoor; GitLab's already is).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_and_ssh_gitlab_urls_including_nested_subgroups() {
        assert_eq!(
            parse_gitlab_url("https://gitlab.example.com/group/subgroup/name.git"),
            Some((
                "https://gitlab.example.com".to_string(),
                "group/subgroup/name".to_string()
            ))
        );
        assert_eq!(
            parse_gitlab_url("https://gitlab.com/group/name"),
            Some(("https://gitlab.com".to_string(), "group/name".to_string()))
        );
        assert_eq!(
            parse_gitlab_url("git@gitlab.example.com:group/name.git"),
            Some((
                "https://gitlab.example.com".to_string(),
                "group/name".to_string()
            ))
        );
    }

    fn set_token(name: &str, value: &str) {
        unsafe {
            std::env::set_var(name, value);
        }
    }

    #[test]
    fn new_derives_api_base_url_from_the_repo_url_when_unset() {
        set_token("SYMPHONY_TEST_GL_HOST_1", "t");
        let host = GitlabRepoHost::new(&RepoConfig {
            url: "https://gitlab.example.com/group/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_GL_HOST_1".to_string()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(host.base_url, "https://gitlab.example.com/api/v4");
        assert_eq!(host.project, "group%2Fname");
    }

    #[test]
    fn new_prefers_an_explicit_api_base_url_override() {
        set_token("SYMPHONY_TEST_GL_HOST_2", "t");
        let host = GitlabRepoHost::new(&RepoConfig {
            url: "https://gitlab.example.com/group/name.git".to_string(),
            api_base_url: Some("https://gitlab.example.com/gitlab/api/v4".to_string()),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_GL_HOST_2".to_string()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(host.base_url, "https://gitlab.example.com/gitlab/api/v4");
    }

    #[test]
    fn new_requires_the_token_env_var_to_be_set() {
        unsafe {
            std::env::remove_var("SYMPHONY_TEST_GL_HOST_MISSING");
        }
        let err = GitlabRepoHost::new(&RepoConfig {
            url: "https://gitlab.com/group/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_GL_HOST_MISSING".to_string()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.contains("SYMPHONY_TEST_GL_HOST_MISSING"));
    }

    fn host(server: &MockServer, token_var: &str) -> GitlabRepoHost {
        set_token(token_var, "t");
        GitlabRepoHost::new(&RepoConfig {
            url: "https://gitlab.com/group/name.git".to_string(),
            api_base_url: Some(server.uri()),
            default_branch: "main".to_string(),
            token_env: Some(token_var.to_string()),
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn opens_a_new_mr_when_none_exists() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/merge_requests"))
            .and(query_param("source_branch", "issue-42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/projects/group%2Fname/merge_requests"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "iid": 7,
                "web_url": "https://gitlab.com/group/name/-/merge_requests/7"
            })))
            .mount(&server)
            .await;

        let h = host(&server, "SYMPHONY_TEST_GL_OPEN_MR");
        let result = h
            .execute_agent_tool(
                "open_pull_request",
                json!({"title": "t", "body": "b"}),
                "42",
                std::path::Path::new("."),
            )
            .await;
        assert!(result.success, "{}", result.content);
        assert!(result.content.contains("merge_requests/7"));
    }

    #[tokio::test]
    async fn updates_the_existing_mr_instead_of_creating_a_duplicate() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/merge_requests"))
            .and(query_param("source_branch", "issue-42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "iid": 9,
                "web_url": "https://gitlab.com/group/name/-/merge_requests/9"
            })]))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/projects/group%2Fname/merge_requests/9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "iid": 9,
                "web_url": "https://gitlab.com/group/name/-/merge_requests/9"
            })))
            .mount(&server)
            .await;

        let h = host(&server, "SYMPHONY_TEST_GL_UPDATE_MR");
        let result = h
            .execute_agent_tool(
                "open_pull_request",
                json!({"title": "t2", "body": "b2"}),
                "42",
                std::path::Path::new("."),
            )
            .await;
        assert!(result.success, "{}", result.content);
        assert!(result.content.contains("Updated"));
    }

    #[tokio::test]
    async fn attach_evidence_uploads_the_file_and_returns_a_markdown_image_snippet() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/projects/group%2Fname/repository/files/.*$"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"file_path": "x"})))
            .mount(&server)
            .await;

        set_token("SYMPHONY_TEST_GL_EVIDENCE_1", "t");
        let h = GitlabRepoHost::new(&RepoConfig {
            url: "https://gitlab.com/group/name.git".to_string(),
            api_base_url: Some(server.uri()),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_GL_EVIDENCE_1".to_string()),
            pull_request: true,
            evidence: true,
            ..Default::default()
        })
        .unwrap();

        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("screenshot.png"), b"fake-png-bytes").unwrap();

        let result = h
            .execute_agent_tool(
                "attach_evidence",
                json!({"image_path": "screenshot.png", "caption": "App running"}),
                "42",
                workspace.path(),
            )
            .await;
        assert!(result.success, "{}", result.content);
        assert!(result.content.contains("![App running]"));
        assert!(result.content.contains("/group/name/-/raw/issue-42/"));
    }

    #[tokio::test]
    async fn attach_evidence_is_not_offered_when_repo_evidence_is_off() {
        set_token("SYMPHONY_TEST_GL_EVIDENCE_2", "t");
        let h = GitlabRepoHost::new(&RepoConfig {
            url: "https://gitlab.com/group/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_GL_EVIDENCE_2".to_string()),
            pull_request: true,
            evidence: false,
            ..Default::default()
        })
        .unwrap();

        assert!(
            !h.agent_tool_specs()
                .iter()
                .any(|s| s.name == "attach_evidence")
        );
    }

    #[tokio::test]
    async fn lists_only_symphony_authored_open_mrs() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/merge_requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                json!({
                    "iid": 5, "web_url": "https://gitlab.com/group/name/-/merge_requests/5",
                    "description": "Closes #12", "source_branch": "issue-12", "sha": "abc123"
                }),
                json!({
                    "iid": 6, "web_url": "https://gitlab.com/group/name/-/merge_requests/6",
                    "description": null, "source_branch": "a-human-branch", "sha": "def456"
                }),
            ]))
            .mount(&server)
            .await;

        let prs = host(&server, "SYMPHONY_TEST_GL_LIST_MRS")
            .list_open_symphony_prs()
            .await
            .unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 5);
        assert_eq!(prs[0].head_sha, "abc123");
    }

    #[tokio::test]
    async fn has_reviewed_sha_matches_only_the_exact_commit_marker() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/merge_requests/5/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "id": 1, "body": "<!-- swebot:reviewed:abc123 -->\nLooks good.", "system": false
            })]))
            .mount(&server)
            .await;

        let h = host(&server, "SYMPHONY_TEST_GL_REVIEWED");
        assert!(h.has_reviewed_sha(5, "abc123").await.unwrap());
        assert!(!h.has_reviewed_sha(5, "a-newer-sha").await.unwrap());
    }

    #[tokio::test]
    async fn approve_posts_the_approve_call_and_a_marker_note() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/projects/group%2Fname/merge_requests/5/approve"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/projects/group%2Fname/merge_requests/5/notes"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
            .mount(&server)
            .await;

        let outcome = host(&server, "SYMPHONY_TEST_GL_APPROVE")
            .post_pr_review(5, "abc123", ReviewVerdict::Approve, "Looks correct.")
            .await
            .unwrap();
        assert_eq!(outcome.posted_as, ReviewVerdict::Approve);
    }

    #[tokio::test]
    async fn approve_falls_back_to_a_comment_on_a_4xx_from_the_approve_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/projects/group%2Fname/merge_requests/5/approve"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "message": "403 Forbidden"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/projects/group%2Fname/merge_requests/5/notes"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
            .mount(&server)
            .await;

        let outcome = host(&server, "SYMPHONY_TEST_GL_APPROVE_FALLBACK")
            .post_pr_review(5, "abc123", ReviewVerdict::Approve, "Looks correct.")
            .await
            .unwrap();
        assert_eq!(outcome.posted_as, ReviewVerdict::Comment);
    }

    #[tokio::test]
    async fn request_changes_posts_a_plain_prefixed_note_not_an_approve_call() {
        let server = MockServer::start().await;
        // No approve mock -- if the driver called it for a RequestChanges verdict,
        // the call would have nothing to match.
        Mock::given(method("POST"))
            .and(path("/projects/group%2Fname/merge_requests/5/notes"))
            .and(body_string_contains("Requesting changes"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
            .mount(&server)
            .await;

        let outcome = host(&server, "SYMPHONY_TEST_GL_REQUEST_CHANGES")
            .post_pr_review(5, "abc123", ReviewVerdict::RequestChanges, "Fix the bug.")
            .await
            .unwrap();
        assert_eq!(outcome.posted_as, ReviewVerdict::RequestChanges);
    }

    #[tokio::test]
    async fn lists_swebot_threads_filtered_by_label_with_notes_as_comments() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/issues"))
            .and(query_param("labels", "swebot::question"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "iid": 1, "title": "How does X work?", "description": "explain X please",
                "web_url": "https://gitlab.com/group/name/-/issues/1"
            })]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects/group%2Fname/issues/1/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                json!({"id": 100, "body": "added label", "system": true}),
                json!({"id": 101, "body": "tell me more", "system": false,
                       "author": {"username": "alice"}}),
            ]))
            .mount(&server)
            .await;

        let threads = host(&server, "SYMPHONY_TEST_GL_THREADS")
            .list_swebot_threads("swebot::question")
            .await
            .unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(
            threads[0].comments.len(),
            1,
            "system notes should be filtered out"
        );
        assert_eq!(threads[0].comments[0].database_id, 101);
        assert_eq!(
            threads[0].comments[0].author_login.as_deref(),
            Some("alice")
        );
    }

    #[tokio::test]
    async fn post_discussion_comment_returns_the_new_notes_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/projects/group%2Fname/issues/1/notes"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": 99, "body": "here's the answer", "system": false
            })))
            .mount(&server)
            .await;

        let id = host(&server, "SYMPHONY_TEST_GL_POST_COMMENT")
            .post_discussion_comment("1", "here's the answer")
            .await
            .unwrap();
        assert_eq!(id, "99");
    }

    #[tokio::test]
    async fn close_thread_sends_the_close_state_event() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/projects/group%2Fname/issues/1"))
            .and(body_string_contains("close"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        host(&server, "SYMPHONY_TEST_GL_CLOSE_THREAD")
            .close_thread("1")
            .await
            .unwrap();
    }

    use wiremock::matchers::{body_string_contains, method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};
}
