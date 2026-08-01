//! GitHub pull-request automation (`repo.pull_request: true`).
//!
//! Deliberately independent of `tracker::TrackerAdapter`: a pull request is a
//! property of `repo:` (the code host), not `tracker:` (the issue board). They
//! usually point at the same GitHub repo in practice, but nothing requires that --
//! `repo.token` is already kept separate from the tracker's own token for the same
//! reason (see `config::RepoConfig::token_env`'s doc comment).
//!
//! Mirrors `tracker::github`'s reqwest/bearer-auth/error-handling shape but isn't
//! shared code with it: the two adapt genuinely different resources (issues vs.
//! pull requests) and staying independent keeps each one simple to read on its own.

use crate::config::RepoConfig;
use crate::tracker::{ToolResult, ToolSpec};
use crate::workspace;
use serde::Deserialize;
use serde_json::json;

const DEFAULT_BASE_URL: &str = "https://api.github.com";

#[derive(Debug)]
pub struct GithubRepoHost {
    client: reqwest::Client,
    base_url: String,
    owner: String,
    repo: String,
    token: String,
    default_branch: String,
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
        })
    }

    fn auth_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    pub fn agent_tool_specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
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
        }]
    }

    pub async fn execute_agent_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
        issue_id: &str,
    ) -> ToolResult {
        if name != "open_pull_request" {
            return ToolResult::error(format!("unsupported tool '{name}'"));
        }
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
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn set_token(name: &str, value: &str) {
        unsafe {
            std::env::set_var(name, value);
        }
    }

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
        })
        .unwrap()
        .with_base_url_for_test(&server.uri());

        let result = host
            .execute_agent_tool(
                "open_pull_request",
                json!({"title": "t", "body": "b"}),
                "42",
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
        })
        .unwrap()
        .with_base_url_for_test(&server.uri());

        let result = host
            .execute_agent_tool(
                "open_pull_request",
                json!({"title": "t2", "body": "b2"}),
                "42",
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
        })
        .unwrap()
        .with_base_url_for_test(&server.uri());

        let result = host
            .execute_agent_tool(
                "open_pull_request",
                json!({"title": "t", "body": "b"}),
                "42",
            )
            .await;
        assert!(!result.success);
        assert!(result.content.contains("No commits"));
    }

    #[test]
    fn new_requires_a_github_url() {
        set_token("SYMPHONY_TEST_REPO_HOST_TOKEN_4", "t");
        let err = GithubRepoHost::new(&RepoConfig {
            url: "https://gitlab.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_REPO_HOST_TOKEN_4".to_string()),
            pull_request: false,
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
        })
        .unwrap_err();
        assert!(err.contains("SYMPHONY_TEST_REPO_HOST_TOKEN_MISSING"));
    }
}
