//! GitHub repo-host operations: pull-request automation (`repo.pull_request: true`)
//! and, for SweBot (README.md "SweBot"), Discussions (Q&A/drafting) and PR review.
//!
//! Mirrors `tracker::github`'s reqwest/bearer-auth/error-handling shape but isn't
//! shared code with it: the two adapt genuinely different resources (issues vs.
//! pull requests/discussions) and staying independent keeps each one simple to read
//! on its own.
//!
//! GitHub Discussions has no REST API -- only GraphQL (`POST /graphql`, single
//! endpoint, query/mutation in the body). `graphql` below is the one shared helper
//! for that; everything else (pull requests, reviews) stays plain REST like the rest
//! of this codebase's GitHub calls.

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

const DEFAULT_BASE_URL: &str = "https://api.github.com";

#[derive(Debug)]
pub struct GithubRepoHost {
    client: reqwest::Client,
    base_url: String,
    owner: String,
    repo: String,
    token: String,
    default_branch: String,
    /// `repo.evidence` -- whether `attach_evidence` should be offered alongside
    /// `open_pull_request` (see `RepoConfig::evidence`'s own doc comment).
    evidence: bool,
}

#[derive(Debug, Deserialize)]
struct GhPullRequest {
    number: u64,
    html_url: String,
}

/// Parse `owner/name` out of a GitHub repo URL, HTTPS or SSH, with or without a
/// trailing `.git`. Returns `None` for anything not on `github.com` (including
/// GitHub Enterprise hosts -- `open_pull_request` doesn't support those today; a
/// project needing that would extend this rather than configure around it).
pub fn parse_github_owner_repo(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.splitn(2, '/');
    let owner = parts.next()?.trim();
    let name = parts.next()?.trim();
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some((owner.to_string(), name.to_string()))
}

/// Fetch a file's raw contents from a GitHub repo at a given ref -- used by the
/// multi-project service (`src/service.rs`) to pull a newly-registered (or
/// periodically refreshed) repo's own `WORKFLOW.md` directly from GitHub, without
/// requiring a local clone. Doesn't need a full `GithubRepoHost` (which mandates a
/// token): public repos need none, so `token_env` is optional here, resolved to its
/// value only at the point of this call and never stored -- same convention as
/// `RepoConfig::token_env` everywhere else in this codebase.
pub async fn fetch_file(
    owner: &str,
    repo: &str,
    branch: &str,
    path: &str,
    token_env: Option<&str>,
) -> anyhow::Result<String> {
    let token = token_env
        .map(|var| {
            std::env::var(var)
                .map_err(|_| anyhow::anyhow!("env var '{var}' (referenced by token_env) is unset"))
        })
        .transpose()?;
    let client = reqwest::Client::builder().user_agent("symphony").build()?;
    let url = format!("{DEFAULT_BASE_URL}/repos/{owner}/{repo}/contents/{path}?ref={branch}");
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github.raw")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = &token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("failed to fetch '{path}' from {owner}/{repo}@{branch}: {status} {body}");
    }
    Ok(resp.text().await?)
}

impl GithubRepoHost {
    pub fn new(repo: &RepoConfig) -> Result<Self, String> {
        let (owner, name) = parse_github_owner_repo(&repo.url)
            .ok_or_else(|| format!("repo.url '{}' is not a github.com URL", repo.url))?;
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
            base_url: DEFAULT_BASE_URL.to_string(),
            owner,
            repo: name,
            token,
            default_branch: repo.default_branch.clone(),
            evidence: repo.evidence,
        })
    }

    fn auth_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    async fn find_open_pr(&self, head: &str) -> Result<Option<u64>, String> {
        let url = format!("{}/repos/{}/{}/pulls", self.base_url, self.owner, self.repo);
        let head_param = format!("{}:{head}", self.owner);
        let req = self
            .client
            .get(&url)
            .query(&[("state", "open"), ("head", head_param.as_str())]);
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
        let prs: Vec<GhPullRequest> = resp.json().await.map_err(|e| e.to_string())?;
        Ok(prs.first().map(|p| p.number))
    }

    async fn create_pr(&self, head: &str, title: &str, body: &str) -> ToolResult {
        let url = format!("{}/repos/{}/{}/pulls", self.base_url, self.owner, self.repo);
        let req = self.client.post(&url).json(&json!({
            "title": title,
            "body": body,
            "head": head,
            "base": self.default_branch,
        }));
        match self.auth_headers(req).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json::<GhPullRequest>().await {
                Ok(pr) => ToolResult::ok(format!("Opened pull request: {}", pr.html_url)),
                Err(e) => {
                    ToolResult::error(format!("created PR but failed to parse response: {e}"))
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

    async fn update_pr(&self, number: u64, title: &str, body: &str) -> ToolResult {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}",
            self.base_url, self.owner, self.repo
        );
        let req = self
            .client
            .patch(&url)
            .json(&json!({"title": title, "body": body}));
        match self.auth_headers(req).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json::<GhPullRequest>().await {
                Ok(pr) => ToolResult::ok(format!("Updated pull request: {}", pr.html_url)),
                Err(e) => {
                    ToolResult::error(format!("updated PR but failed to parse response: {e}"))
                }
            },
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                ToolResult::error(format!("PATCH {url} -> {status}: {text}"))
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }

    /// Upload a file the agent already produced somewhere inside its own workspace
    /// (`image_path`, resolved against `workspace_dir` -- the workspace path exactly as
    /// seen by this process, see `RepoHost::execute_agent_tool`'s doc comment) to this
    /// repo's ticket branch via the Contents API, and hand back a ready-to-paste
    /// markdown image snippet pointing at the committed file's `raw.githubusercontent.com`
    /// URL. The repo path is content-hashed (`sha2`, already a dependency) rather than
    /// reusing `image_path`'s own name verbatim: the Contents API's create-or-update
    /// call requires the *existing* file's blob `sha` to update it, which would mean an
    /// extra round-trip (or a stale-sha race) just to re-attach a same-named screenshot
    /// across retries -- hashing the content into the path means every distinct
    /// screenshot gets its own path and a plain create always succeeds, while an
    /// identical retry harmlessly no-ops (GitHub is fine with re-creating a file whose
    /// content already matches).
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
            "{}/repos/{}/{}/contents/{repo_path}",
            self.base_url, self.owner, self.repo
        );
        let content_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let req = self.client.put(&url).json(&json!({
            "message": format!("Attach evidence: {caption}"),
            "content": content_b64,
            "branch": branch,
        }));
        match self.auth_headers(req).send().await {
            Ok(resp) if resp.status().is_success() => {
                let raw_url = format!(
                    "https://raw.githubusercontent.com/{}/{}/{branch}/{repo_path}",
                    self.owner, self.repo
                );
                ToolResult::ok(format!(
                    "Evidence uploaded. Paste this into the pull request body to embed it:\n\n![{caption}]({raw_url})"
                ))
            }
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                ToolResult::error(format!(
                    "PUT {url} -> {status}: {text} (has this branch been pushed yet?)"
                ))
            }
            Err(e) => ToolResult::error(e.to_string()),
        }
    }

    /// Every SweBot-posted review body starts with this (SHA-specific) HTML comment
    /// marker, so `has_reviewed_sha` can tell "already reviewed this exact commit" from
    /// "never reviewed" or "reviewed an earlier commit on this branch" without needing
    /// to know which account posted it (no separate `GET /user` call to resolve "who am
    /// I" -- the marker is self-contained and survives a token rotation).
    fn review_marker(head_sha: &str) -> String {
        format!("<!-- swebot:reviewed:{head_sha} -->")
    }

    /// Post a PR review. `event` must be one of GitHub's own review-event values:
    /// `"APPROVE"`, `"REQUEST_CHANGES"`, or `"COMMENT"`.
    async fn post_review_raw(
        &self,
        pr_number: u64,
        head_sha: &str,
        event: &str,
        summary: &str,
    ) -> Result<(), String> {
        let body = format!("{}\n{summary}", Self::review_marker(head_sha));
        let url = format!(
            "{}/repos/{}/{}/pulls/{pr_number}/reviews",
            self.base_url, self.owner, self.repo
        );
        let req = self
            .client
            .post(&url)
            .json(&json!({"body": body, "event": event}));
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

    // --- Discussions (GraphQL-only; see this module's own doc comment) ---

    async fn graphql<T: serde::de::DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T, String> {
        let url = format!("{}/graphql", self.base_url);
        let req = self
            .client
            .post(&url)
            .json(&json!({"query": query, "variables": variables}));
        let resp = self
            .auth_headers(req)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("POST {url} -> {status}: {text}"));
        }
        let envelope: GraphqlEnvelope<T> =
            serde_json::from_str(&text).map_err(|e| format!("{e}: {text}"))?;
        if let Some(errors) = envelope.errors.filter(|e| !e.is_empty()) {
            let messages: Vec<String> = errors.into_iter().map(|e| e.message).collect();
            return Err(format!("GraphQL errors: {}", messages.join("; ")));
        }
        envelope
            .data
            .ok_or_else(|| "GraphQL response had no data".to_string())
    }
}

#[async_trait]
impl RepoHost for GithubRepoHost {
    fn provider_kind(&self) -> RepoProvider {
        RepoProvider::Github
    }

    fn agent_tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![ToolSpec {
            name: "open_pull_request".to_string(),
            description: "Open (or update, if one already exists) a pull request for this \
                ticket's branch, with a title and a body describing the work and the \
                rationale behind it. This is the way to submit work for review -- call it \
                once your branch has been pushed and you're satisfied with the change. \
                Include 'Closes #<issue-number>' in the body so the tracker issue closes \
                automatically when this PR is merged; do not call update_issue_state with a \
                terminal/'done' state yourself in this workflow -- closing is a side effect \
                of merge, not something to do before anyone has reviewed the code. After \
                this succeeds, if the project's tracker config offers a non-terminal state \
                for this (e.g. 'in review'), call update_issue_state with that -- otherwise \
                the tracker keeps treating this ticket as active work still in progress and \
                you'll keep being redispatched to it with nothing new to do."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Pull request title"},
                    "body": {
                        "type": "string",
                        "description": "Pull request body: what changed, why, and 'Closes #N'"
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
                    browser) to this pull request's branch, and get back a markdown image \
                    snippet. Paste that snippet into the pull request body (open_pull_request's \
                    'body' argument) so a reviewer can see the evidence directly instead of \
                    taking your word for it. Call this before open_pull_request so the \
                    snippet is ready to include; calling open_pull_request again afterwards \
                    with an updated body works too."
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

                match self.find_open_pr(&head).await {
                    Ok(Some(number)) => self.update_pr(number, title, body).await,
                    Ok(None) => self.create_pr(&head, title, body).await,
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
        let url = format!("{}/repos/{}/{}/pulls", self.base_url, self.owner, self.repo);
        let req = self
            .client
            .get(&url)
            .query(&[("state", "open"), ("per_page", "100")]);
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
        let all: Vec<GhPullRequestFull> = resp.json().await.map_err(|e| e.to_string())?;
        Ok(all
            .into_iter()
            .filter(|pr| pr.head.r#ref.starts_with("issue-"))
            .map(|pr| SymphonyPullRequest {
                number: pr.number,
                html_url: pr.html_url,
                head_ref: pr.head.r#ref,
                head_sha: pr.head.sha,
                body: pr.body.unwrap_or_default(),
            })
            .collect())
    }

    async fn has_reviewed_sha(&self, pr_number: u64, head_sha: &str) -> Result<bool, String> {
        let marker = Self::review_marker(head_sha);
        let url = format!(
            "{}/repos/{}/{}/pulls/{pr_number}/reviews",
            self.base_url, self.owner, self.repo
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
        let reviews: Vec<GhReviewBody> = resp.json().await.map_err(|e| e.to_string())?;
        Ok(reviews
            .iter()
            .any(|r| r.body.as_deref().unwrap_or_default().contains(&marker)))
    }

    /// Posts the requested verdict, mapped to GitHub's own review-event vocabulary
    /// (`APPROVE`/`REQUEST_CHANGES`/`COMMENT`). GitHub refuses to let an account
    /// approve its own pull request (422 "Can not approve your own pull request") --
    /// when the same token authenticates both the coding agent (PR author) and
    /// SweBot (reviewer, the default when `swebot.token` isn't set -- see
    /// `config::EffectiveConfig::swebot_repo_config`), an `Approve` verdict always
    /// hits this. Falling back to a plain comment with the same reasoning means the
    /// review still gets posted and recorded (`has_reviewed_sha` becomes true) rather
    /// than retrying and 422ing again on every single poll forever -- a real failure
    /// mode hit running this live: it silently burned a full review turn on every
    /// cycle with nothing ever actually landing. Setting `swebot.token` to a second
    /// GitHub identity avoids hitting this fallback at all.
    async fn post_pr_review(
        &self,
        pr_number: u64,
        head_sha: &str,
        verdict: ReviewVerdict,
        summary: &str,
    ) -> Result<ReviewOutcome, String> {
        let event = match verdict {
            ReviewVerdict::Approve => "APPROVE",
            ReviewVerdict::RequestChanges => "REQUEST_CHANGES",
            ReviewVerdict::Comment => "COMMENT",
        };
        match self
            .post_review_raw(pr_number, head_sha, event, summary)
            .await
        {
            Ok(()) => Ok(ReviewOutcome { posted_as: verdict }),
            Err(e)
                if verdict == ReviewVerdict::Approve
                    && e.contains("Can not approve your own pull request") =>
            {
                let note = format!(
                    "(SweBot would approve this, but GitHub doesn't allow approving your own \
                     pull request -- the coding agent and SweBot are using the same token \
                     here. Posting as a comment instead. Set `swebot.token` to a separate \
                     GitHub identity to avoid this.)\n\n{summary}"
                );
                self.post_review_raw(pr_number, head_sha, "COMMENT", &note)
                    .await?;
                Ok(ReviewOutcome {
                    posted_as: ReviewVerdict::Comment,
                })
            }
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl DiscussionHost for GithubRepoHost {
    /// Discussions in `selector` (a category name, e.g. `"Q&A"`, `"Ideas"`),
    /// newest-updated first, each with up to its first 50 comments. Filtering by
    /// category happens client-side against the category's `name` rather than
    /// resolving a `categoryId` first -- one GraphQL round trip instead of two, and
    /// simpler to read; acceptable at the discussion volumes this is meant for
    /// (mirrors `tracker/github.rs`'s own `MAX_PAGES` "not expected to matter at
    /// realistic counts" pragmatism). Only the first 25 discussions are considered,
    /// same reasoning.
    async fn list_swebot_threads(&self, selector: &str) -> Result<Vec<DiscussionThread>, String> {
        // Fetches each top-level comment's `replies` too, not just `comments` --
        // GitHub's own "Reply" button on a specific comment creates a *threaded*
        // reply, not a new top-level comment, and that's the natural way a human
        // responds to one of SweBot's own questions. Missing this meant a human
        // reply via that (very normal) path was invisible and SweBot looked stuck.
        // Flattened into one list alongside top-level comments below -- ordering by
        // `database_id` (GitHub assigns these sequentially regardless of nesting)
        // is all `next_to_answer`/`transcript_up_to` need, so no separate
        // reply-vs-comment distinction has to survive past this point.
        const QUERY: &str = "query($owner: String!, $name: String!) { \
            repository(owner: $owner, name: $name) { \
              discussions(first: 25, orderBy: {field: UPDATED_AT, direction: DESC}) { \
                nodes { \
                  id number title body url \
                  category { name } \
                  comments(first: 50) { \
                    nodes { \
                      databaseId body author { login } \
                      replies(first: 50) { nodes { databaseId body author { login } } } \
                    } \
                  } \
                } \
              } \
            } \
          }";
        let data: DiscussionsData = self
            .graphql(QUERY, json!({"owner": self.owner, "name": self.repo}))
            .await?;
        Ok(data
            .repository
            .discussions
            .nodes
            .into_iter()
            .filter(|d| d.category.name == selector)
            .map(|d| DiscussionThread {
                id: d.id,
                number: d.number,
                title: d.title,
                body: d.body,
                url: d.url,
                comments: d
                    .comments
                    .nodes
                    .into_iter()
                    .flat_map(|c| {
                        let reply_comments =
                            c.replies.nodes.into_iter().map(|r| DiscussionComment {
                                database_id: r.database_id.unwrap_or(0),
                                body: r.body,
                                author_login: r.author.map(|a| a.login),
                            });
                        std::iter::once(DiscussionComment {
                            database_id: c.database_id.unwrap_or(0),
                            body: c.body,
                            author_login: c.author.map(|a| a.login),
                        })
                        .chain(reply_comments)
                    })
                    .collect(),
            })
            .collect())
    }

    /// Reply to a discussion. Returns the new comment's GraphQL node id.
    async fn post_discussion_comment(&self, thread_id: &str, body: &str) -> Result<String, String> {
        const MUTATION: &str = "mutation($discussionId: ID!, $body: String!) { \
            addDiscussionComment(input: {discussionId: $discussionId, body: $body}) { \
              comment { id } \
            } \
          }";
        let data: AddCommentData = self
            .graphql(MUTATION, json!({"discussionId": thread_id, "body": body}))
            .await?;
        Ok(data.add_discussion_comment.comment.id)
    }

    /// Mark a Q&A-category discussion as answered by `comment_id` (a GraphQL node id,
    /// as returned by `post_discussion_comment`, not the numeric `databaseId`).
    async fn mark_discussion_comment_as_answer(&self, comment_id: &str) -> Result<(), String> {
        const MUTATION: &str = "mutation($id: ID!) { \
            markDiscussionCommentAsAnswer(input: {id: $id}) { clientMutationId } \
          }";
        self.graphql::<serde_json::Value>(MUTATION, json!({"id": comment_id}))
            .await?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct GhPullRequestFull {
    number: u64,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    head: GhPrHead,
}

#[derive(Debug, Deserialize)]
struct GhPrHead {
    #[serde(rename = "ref")]
    r#ref: String,
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GhReviewBody {
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphqlEnvelope<T> {
    // `Option<T>` fields are permitted to be absent from the JSON without needing
    // `#[serde(default)]` (serde special-cases Option), which matters here since
    // `#[serde(default)]` on a generic field makes serde_derive add a `T: Default`
    // bound to the whole impl even though `Option::default()` doesn't actually need
    // one -- a known serde-derive quirk with generic structs.
    data: Option<T>,
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct DiscussionsData {
    repository: DiscussionsRepo,
}

#[derive(Debug, Deserialize)]
struct DiscussionsRepo {
    discussions: DiscussionsConnection,
}

#[derive(Debug, Deserialize)]
struct DiscussionsConnection {
    nodes: Vec<GhDiscussion>,
}

#[derive(Debug, Deserialize)]
struct GhDiscussion {
    id: String,
    number: u64,
    title: String,
    #[serde(default)]
    body: String,
    url: String,
    category: GhCategory,
    comments: GhCommentsConnection,
}

#[derive(Debug, Deserialize)]
struct GhCategory {
    name: String,
}

#[derive(Debug, Default, Deserialize)]
struct GhCommentsConnection {
    nodes: Vec<GhComment>,
}

#[derive(Debug, Deserialize)]
struct GhComment {
    #[serde(rename = "databaseId", default)]
    database_id: Option<u64>,
    #[serde(default)]
    body: String,
    author: Option<GhAuthor>,
    #[serde(default)]
    replies: GhCommentsConnection,
}

#[derive(Debug, Deserialize)]
struct GhAuthor {
    login: String,
}

#[derive(Debug, Deserialize)]
struct AddCommentData {
    #[serde(rename = "addDiscussionComment")]
    add_discussion_comment: AddCommentPayload,
}

#[derive(Debug, Deserialize)]
struct AddCommentPayload {
    comment: AddedComment,
}

#[derive(Debug, Deserialize)]
struct AddedComment {
    id: String,
}

// RepoConfig has no base_url field (only GithubRepoHost does) -- tests (here and in
// mcp.rs, which also needs to point a GithubRepoHost at a wiremock server) build the
// host directly and override base_url after construction instead of threading a
// test-only field through the public config type.
#[cfg(test)]
impl GithubRepoHost {
    pub(crate) fn with_base_url_for_test(mut self, base_url: &str) -> Self {
        self.base_url = base_url.to_string();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_and_ssh_github_urls() {
        assert_eq!(
            parse_github_owner_repo("https://github.com/owner/name.git"),
            Some(("owner".to_string(), "name".to_string()))
        );
        assert_eq!(
            parse_github_owner_repo("https://github.com/owner/name"),
            Some(("owner".to_string(), "name".to_string()))
        );
        assert_eq!(
            parse_github_owner_repo("git@github.com:owner/name.git"),
            Some(("owner".to_string(), "name".to_string()))
        );
        assert_eq!(
            parse_github_owner_repo("https://gitlab.com/owner/name.git"),
            None
        );
    }

    #[tokio::test]
    async fn opens_a_new_pr_when_none_exists() {
        let server = MockServer::start().await;
        set_token("SYMPHONY_TEST_REPO_HOST_TOKEN_1", "t");
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/pulls"))
            .and(query_param("head", "owner:issue-42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/name/pulls"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "number": 7,
                "html_url": "https://github.com/owner/name/pull/7"
            })))
            .mount(&server)
            .await;

        let host = GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_REPO_HOST_TOKEN_1".to_string()),
            pull_request: false,
            ..Default::default()
        })
        .unwrap()
        .with_base_url_for_test(&server.uri());

        let result = host
            .execute_agent_tool(
                "open_pull_request",
                json!({"title": "t", "body": "b"}),
                "42",
                std::path::Path::new("."),
            )
            .await;
        assert!(result.success, "{}", result.content);
        assert!(result.content.contains("pull/7"));
    }

    #[tokio::test]
    async fn updates_the_existing_pr_instead_of_creating_a_duplicate() {
        let server = MockServer::start().await;
        set_token("SYMPHONY_TEST_REPO_HOST_TOKEN_2", "t");
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/pulls"))
            .and(query_param("head", "owner:issue-42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "number": 9,
                "html_url": "https://github.com/owner/name/pull/9"
            })]))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/owner/name/pulls/9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "number": 9,
                "html_url": "https://github.com/owner/name/pull/9"
            })))
            .mount(&server)
            .await;
        // No POST mock registered -- if the client tried to create instead of
        // update, wiremock would have nothing to match and the call would fail.

        let host = GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_REPO_HOST_TOKEN_2".to_string()),
            pull_request: false,
            ..Default::default()
        })
        .unwrap()
        .with_base_url_for_test(&server.uri());

        let result = host
            .execute_agent_tool(
                "open_pull_request",
                json!({"title": "t2", "body": "b2"}),
                "42",
                std::path::Path::new("."),
            )
            .await;
        assert!(result.success, "{}", result.content);
        assert!(result.content.contains("Updated"));
        assert!(result.content.contains("pull/9"));
    }

    #[tokio::test]
    async fn surfaces_github_error_when_branch_has_no_commits() {
        let server = MockServer::start().await;
        set_token("SYMPHONY_TEST_REPO_HOST_TOKEN_3", "t");
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/name/pulls"))
            .respond_with(ResponseTemplate::new(422).set_body_json(json!({
                "message": "Validation Failed",
                "errors": [{"message": "No commits between main and issue-42"}]
            })))
            .mount(&server)
            .await;

        let host = GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_REPO_HOST_TOKEN_3".to_string()),
            pull_request: false,
            ..Default::default()
        })
        .unwrap()
        .with_base_url_for_test(&server.uri());

        let result = host
            .execute_agent_tool(
                "open_pull_request",
                json!({"title": "t", "body": "b"}),
                "42",
                std::path::Path::new("."),
            )
            .await;
        assert!(!result.success);
        assert!(result.content.contains("No commits"));
    }

    #[tokio::test]
    async fn attach_evidence_uploads_the_file_and_returns_a_markdown_image_snippet() {
        let server = MockServer::start().await;
        set_token("SYMPHONY_TEST_REPO_HOST_EVIDENCE_1", "t");
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/repos/owner/name/contents/\.symphony/evidence/.*$",
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"content": {}})))
            .mount(&server)
            .await;

        let host = GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_REPO_HOST_EVIDENCE_1".to_string()),
            pull_request: true,
            evidence: true,
            ..Default::default()
        })
        .unwrap()
        .with_base_url_for_test(&server.uri());

        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("screenshot.png"), b"fake-png-bytes").unwrap();

        let result = host
            .execute_agent_tool(
                "attach_evidence",
                json!({"image_path": "screenshot.png", "caption": "App running"}),
                "42",
                workspace.path(),
            )
            .await;
        assert!(result.success, "{}", result.content);
        assert!(result.content.contains("![App running]"));
        assert!(
            result
                .content
                .contains("raw.githubusercontent.com/owner/name/issue-42/")
        );
    }

    #[tokio::test]
    async fn attach_evidence_is_not_offered_or_executable_when_repo_evidence_is_off() {
        set_token("SYMPHONY_TEST_REPO_HOST_EVIDENCE_2", "t");
        let host = GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_REPO_HOST_EVIDENCE_2".to_string()),
            pull_request: true,
            evidence: false,
            ..Default::default()
        })
        .unwrap();

        assert!(
            !host
                .agent_tool_specs()
                .iter()
                .any(|s| s.name == "attach_evidence")
        );

        let result = host
            .execute_agent_tool(
                "attach_evidence",
                json!({"image_path": "screenshot.png", "caption": "App running"}),
                "42",
                std::path::Path::new("."),
            )
            .await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn attach_evidence_errors_clearly_when_the_image_file_is_missing() {
        set_token("SYMPHONY_TEST_REPO_HOST_EVIDENCE_3", "t");
        let host = GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_REPO_HOST_EVIDENCE_3".to_string()),
            pull_request: true,
            evidence: true,
            ..Default::default()
        })
        .unwrap();

        let workspace = tempfile::tempdir().unwrap();
        let result = host
            .execute_agent_tool(
                "attach_evidence",
                json!({"image_path": "does-not-exist.png", "caption": "App running"}),
                "42",
                workspace.path(),
            )
            .await;
        assert!(!result.success);
        assert!(result.content.contains("does-not-exist.png"));
    }

    #[test]
    fn new_requires_a_github_url() {
        set_token("SYMPHONY_TEST_REPO_HOST_TOKEN_4", "t");
        let err = GithubRepoHost::new(&RepoConfig {
            url: "https://gitlab.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_REPO_HOST_TOKEN_4".to_string()),
            pull_request: false,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.contains("github.com"));
    }

    #[test]
    fn new_requires_the_token_env_var_to_be_set() {
        unsafe {
            std::env::remove_var("SYMPHONY_TEST_REPO_HOST_TOKEN_MISSING");
        }
        let err = GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_REPO_HOST_TOKEN_MISSING".to_string()),
            pull_request: false,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.contains("SYMPHONY_TEST_REPO_HOST_TOKEN_MISSING"));
    }

    fn set_token(name: &str, value: &str) {
        unsafe {
            std::env::set_var(name, value);
        }
    }

    fn host(server: &MockServer, token_var: &str) -> GithubRepoHost {
        set_token(token_var, "t");
        GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some(token_var.to_string()),
            pull_request: false,
            ..Default::default()
        })
        .unwrap()
        .with_base_url_for_test(&server.uri())
    }

    #[tokio::test]
    async fn lists_only_symphony_authored_open_prs() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                json!({
                    "number": 5, "html_url": "https://github.com/owner/name/pull/5",
                    "body": "Closes #12", "head": {"ref": "issue-12", "sha": "abc123"}
                }),
                json!({
                    "number": 6, "html_url": "https://github.com/owner/name/pull/6",
                    "body": null, "head": {"ref": "a-human-branch", "sha": "def456"}
                }),
            ]))
            .mount(&server)
            .await;

        let prs = host(&server, "SYMPHONY_TEST_RH_LIST_PRS")
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
            .and(path("/repos/owner/name/pulls/5/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "body": "<!-- swebot:reviewed:abc123 -->\nLooks good."
            })]))
            .mount(&server)
            .await;

        let h = host(&server, "SYMPHONY_TEST_RH_REVIEWED");
        assert!(h.has_reviewed_sha(5, "abc123").await.unwrap());
        assert!(!h.has_reviewed_sha(5, "a-newer-sha").await.unwrap());
    }

    #[tokio::test]
    async fn post_pr_review_sends_the_marker_and_mapped_event() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/name/pulls/5/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
            .mount(&server)
            .await;

        let result = host(&server, "SYMPHONY_TEST_RH_POST_REVIEW")
            .post_pr_review(
                5,
                "abc123",
                ReviewVerdict::Approve,
                "Looks correct and well-tested.",
            )
            .await;
        assert!(result.is_ok(), "{:?}", result);
        assert_eq!(result.unwrap().posted_as, ReviewVerdict::Approve);
    }

    #[tokio::test]
    async fn post_pr_review_falls_back_to_a_comment_on_self_approval_rejection() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/name/pulls/5/reviews"))
            .and(body_string_contains("APPROVE"))
            .respond_with(ResponseTemplate::new(422).set_body_json(json!({
                "message": "Unprocessable Entity",
                "errors": ["Review Can not approve your own pull request"]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/name/pulls/5/reviews"))
            .and(body_string_contains("COMMENT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
            .mount(&server)
            .await;

        let result = host(&server, "SYMPHONY_TEST_RH_SELF_APPROVE")
            .post_pr_review(5, "abc123", ReviewVerdict::Approve, "Looks correct.")
            .await
            .unwrap();
        assert_eq!(result.posted_as, ReviewVerdict::Comment);
    }

    #[tokio::test]
    async fn lists_discussions_filtered_to_the_requested_category() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "discussions": {
                            "nodes": [
                                {
                                    "id": "D_1", "number": 1, "title": "How does X work?",
                                    "body": "explain X please",
                                    "url": "https://github.com/owner/name/discussions/1",
                                    "category": {"name": "Q&A"},
                                    "comments": {"nodes": []}
                                },
                                {
                                    "id": "D_2", "number": 2, "title": "A feature idea",
                                    "body": "would be nice",
                                    "url": "https://github.com/owner/name/discussions/2",
                                    "category": {"name": "Ideas"},
                                    "comments": {"nodes": [
                                        {"id": "C_1", "databaseId": 111, "body": "tell me more",
                                         "author": {"login": "alice"}}
                                    ]}
                                }
                            ]
                        }
                    }
                }
            })))
            .mount(&server)
            .await;

        let h = host(&server, "SYMPHONY_TEST_RH_DISCUSSIONS");
        let qa = h.list_swebot_threads("Q&A").await.unwrap();
        assert_eq!(qa.len(), 1);
        assert_eq!(qa[0].number, 1);

        let ideas = h.list_swebot_threads("Ideas").await.unwrap();
        assert_eq!(ideas.len(), 1);
        assert_eq!(ideas[0].comments.len(), 1);
        assert_eq!(ideas[0].comments[0].database_id, 111);
        assert_eq!(ideas[0].comments[0].author_login.as_deref(), Some("alice"));
    }

    /// Regression test for a real bug found running this live: GitHub's own "Reply"
    /// button on a comment creates a *threaded* reply, not a new top-level comment --
    /// the natural way a human answers one of SweBot's own questions. The original
    /// query only fetched top-level `comments`, so a threaded reply was invisible
    /// and SweBot looked stuck forever waiting for an answer that had, in fact,
    /// already arrived.
    #[tokio::test]
    async fn threaded_replies_are_flattened_in_alongside_top_level_comments() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "discussions": {
                            "nodes": [{
                                "id": "D_1", "number": 1, "title": "archive download feature",
                                "body": "export pictures as a zip",
                                "url": "https://github.com/owner/name/discussions/1",
                                "category": {"name": "Ideas"},
                                "comments": {"nodes": [{
                                    "databaseId": 100, "body": "which selection?",
                                    "author": {"login": "swebot"},
                                    "replies": {"nodes": [{
                                        "databaseId": 101, "body": "whole archive",
                                        "author": {"login": "alice"}
                                    }]}
                                }]}
                            }]
                        }
                    }
                }
            })))
            .mount(&server)
            .await;

        let h = host(&server, "SYMPHONY_TEST_RH_THREADED_REPLIES");
        let ideas = h.list_swebot_threads("Ideas").await.unwrap();
        assert_eq!(ideas.len(), 1);
        assert_eq!(
            ideas[0].comments.len(),
            2,
            "expected both the comment and its reply"
        );
        assert_eq!(ideas[0].comments[0].database_id, 100);
        assert_eq!(ideas[0].comments[1].database_id, 101);
        assert_eq!(ideas[0].comments[1].body, "whole archive");
        assert_eq!(ideas[0].comments[1].author_login.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn graphql_surfaces_errors_from_the_response_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": null,
                "errors": [{"message": "Could not resolve to a Repository"}]
            })))
            .mount(&server)
            .await;

        let err = host(&server, "SYMPHONY_TEST_RH_GRAPHQL_ERR")
            .list_swebot_threads("Q&A")
            .await
            .unwrap_err();
        assert!(err.contains("Could not resolve to a Repository"));
    }

    #[tokio::test]
    async fn post_discussion_comment_returns_the_new_comments_node_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"addDiscussionComment": {"comment": {"id": "C_99"}}}
            })))
            .mount(&server)
            .await;

        let id = host(&server, "SYMPHONY_TEST_RH_POST_COMMENT")
            .post_discussion_comment("D_1", "here's the answer")
            .await
            .unwrap();
        assert_eq!(id, "C_99");
    }

    use wiremock::matchers::{body_string_contains, method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};
}
