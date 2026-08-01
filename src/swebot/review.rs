//! PR-review capability: reviews the pull requests Symphony's own coding agents open
//! (branch name matching `issue-<identifier>`, the same convention
//! `synthesize_repo_hooks`/`open_pull_request` produce -- not every PR a human might
//! open by hand) and posts an approve/request-changes/comment verdict.
//!
//! Never merges: a human always does that (confirmed product decision -- see
//! README.md "SweBot"). Idempotent per commit: `GithubRepoHost::has_reviewed_sha`
//! skips a PR whose current head has already been reviewed, so a poll cycle only
//! reviews an unreviewed PR once and re-reviews only after the branch actually moves.

use super::{PERSONA, extract_json_block, git, run_turn_collect_text};
use crate::agent::AgentBackend;
use crate::config::EffectiveConfig;
use crate::repo_host::{GithubRepoHost, extract_closes_issue_number};
use crate::tracker::TrackerAdapter;

pub async fn poll_once(
    cfg: &EffectiveConfig,
    host: &GithubRepoHost,
    backend: &dyn AgentBackend,
    tracker: &dyn TrackerAdapter,
) -> Result<(), String> {
    let repo = cfg
        .repo
        .as_ref()
        .ok_or("swebot.review.enabled but no repo: block resolved")?;
    let prs = host.list_open_symphony_prs().await?;

    for pr in prs {
        if host.has_reviewed_sha(pr.number, &pr.head_sha).await? {
            continue;
        }

        let scratch = tempfile::tempdir().map_err(|e| e.to_string())?;
        git::clone_branch(repo, scratch.path(), &pr.head_ref).await?;
        let diff = git::diff_against(scratch.path(), &repo.default_branch).await?;

        let issue_context = match extract_closes_issue_number(&pr.body) {
            Some(n) => match tracker.fetch_issues_by_ids(&[n.to_string()]).await {
                Ok(issues) if !issues.is_empty() => format!(
                    "The original ticket, #{n}: \"{}\"\n{}",
                    issues[0].title,
                    issues[0]
                        .description
                        .as_deref()
                        .unwrap_or("(no description)"),
                ),
                _ => format!("(Referenced issue #{n} could not be fetched from the tracker.)"),
            },
            None => {
                "(No 'Closes #N' reference found in the PR body -- reviewing on the diff alone.)"
                    .to_string()
            }
        };

        let prompt = format!(
            "{PERSONA}\n\nReview this pull request.\n\n{issue_context}\n\nDiff against \
             {}:\n{diff}\n\nYou have Bash access to run tests/lints in the checked-out repo \
             at {} -- judge the code, don't fix it (file-editing tools are disabled for \
             this session). Check: correctness against the original ticket's acceptance \
             criteria, security (input handling, secrets, auth/authz boundaries), \
             performance (obvious inefficiency, unbounded loops/queries), and whether it \
             matches this project's own conventions and includes tests. \
             request_changes means something genuinely fails one of these; approve means \
             \"I'd merge this,\" not \"nothing's obviously on fire.\"\n\n\
             End your response with exactly one fenced ```json block: \
             {{\"verdict\": \"approve\"|\"request_changes\"|\"comment\", \
             \"summary\": \"<your reasoning, written for the human who'll read this review>\"}}.",
            repo.default_branch,
            scratch.path().display(),
        );

        let mut session = backend
            .start_session(
                scratch.path(),
                &pr.number.to_string(),
                &format!("PR #{}", pr.number),
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        let raw = match run_turn_collect_text(session.as_mut(), &prompt).await {
            Ok(text) => text,
            Err(e) => {
                session.stop().await;
                return Err(format!("PR #{}: {e}", pr.number));
            }
        };
        session.stop().await;

        let parsed = extract_json_block(&raw)
            .map_err(|e| format!("PR #{}: {e} (raw response: {raw})", pr.number))?;
        let verdict = parsed
            .get("verdict")
            .and_then(|v| v.as_str())
            .unwrap_or("comment");
        let summary = parsed
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("(no summary provided)");
        let event = match verdict {
            "approve" => "APPROVE",
            "request_changes" => "REQUEST_CHANGES",
            _ => "COMMENT",
        };

        host.post_pr_review(pr.number, &pr.head_sha, event, summary)
            .await?;
        tracing::info!(pr = pr.number, url = %pr.html_url, verdict, "swebot: posted PR review");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, RepoConfig};
    use crate::repo_host::GithubRepoHost;
    use crate::swebot::test_support::FakeBackend;
    use crate::tracker::local::LocalTrackerAdapter;
    use serde_json::json;
    use std::process::Command;
    use tempfile::tempdir;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A real local git repo with `main` plus an `issue-42` branch one commit ahead,
    /// so `git::clone_branch`/`git::diff_against` (real `git` subprocesses, not
    /// mockable via wiremock) have something genuine to operate on.
    fn real_repo_with_a_ticket_branch() -> tempfile::TempDir {
        let origin = tempdir().unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(origin.path())
                .status()
                .unwrap();
        };
        git(&["init", "--initial-branch=main"]);
        std::fs::write(origin.path().join("app.rs"), "fn main() {}\n").unwrap();
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "seed",
        ]);
        git(&["checkout", "-b", "issue-42"]);
        std::fs::write(
            origin.path().join("app.rs"),
            "fn main() { println!(\"hello\"); }\n",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "add greeting",
        ]);
        git(&["checkout", "main"]);
        origin
    }

    fn test_cfg(repo_path: &std::path::Path) -> config::EffectiveConfig {
        let yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
            "tracker:\n  kind: local\nrepo:\n  url: {:?}\n  default_branch: main\n",
            repo_path.display()
        ))
        .unwrap();
        config::resolve(&yaml, std::path::Path::new(".")).unwrap()
    }

    fn test_host(server: &MockServer) -> GithubRepoHost {
        unsafe {
            std::env::set_var("SYMPHONY_TEST_REVIEW_TOKEN", "t");
        }
        GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_REVIEW_TOKEN".to_string()),
            pull_request: false,
        })
        .unwrap()
        .with_base_url_for_test(&server.uri())
    }

    #[tokio::test]
    async fn reviews_an_unreviewed_symphony_pr_and_posts_the_mapped_verdict() {
        let origin = real_repo_with_a_ticket_branch();
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/name/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "number": 42, "html_url": "https://github.com/owner/name/pull/42",
                "body": "no closes reference here",
                "head": {"ref": "issue-42", "sha": "fakesha123"}
            })]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/pulls/42/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/name/pulls/42/reviews"))
            .and(body_string_contains("APPROVE"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
            .mount(&server)
            .await;

        let cfg = test_cfg(origin.path());
        let host = test_host(&server);
        let backend = FakeBackend::with_response(
            "```json\n{\"verdict\": \"approve\", \"summary\": \"Looks correct and well-tested.\"}\n```",
        );
        let tracker_dir = tempdir().unwrap();
        let tracker_provider: serde_yaml::Value =
            serde_yaml::from_str(&format!("dir: {:?}", tracker_dir.path())).unwrap();
        let tracker =
            LocalTrackerAdapter::new(&tracker_provider, std::path::Path::new(".")).unwrap();

        poll_once(&cfg, &host, &backend, &tracker).await.unwrap();

        let prompts = backend.prompts_seen.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].contains("println"),
            "prompt should include the actual diff content: {}",
            prompts[0]
        );
    }

    #[tokio::test]
    async fn skips_a_pr_already_reviewed_at_its_current_head_sha() {
        let origin = real_repo_with_a_ticket_branch();
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/name/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "number": 42, "html_url": "https://github.com/owner/name/pull/42",
                "body": "", "head": {"ref": "issue-42", "sha": "fakesha123"}
            })]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/pulls/42/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "body": "<!-- swebot:reviewed:fakesha123 -->\nAlready reviewed."
            })]))
            .mount(&server)
            .await;
        // No POST mock -- if the driver tried to review again, the call would have
        // nothing to match.

        let cfg = test_cfg(origin.path());
        let host = test_host(&server);
        let backend = FakeBackend::with_response("should not be called");
        let tracker_dir = tempdir().unwrap();
        let tracker_provider: serde_yaml::Value =
            serde_yaml::from_str(&format!("dir: {:?}", tracker_dir.path())).unwrap();
        let tracker =
            LocalTrackerAdapter::new(&tracker_provider, std::path::Path::new(".")).unwrap();

        poll_once(&cfg, &host, &backend, &tracker).await.unwrap();

        assert!(backend.prompts_seen.lock().unwrap().is_empty());
    }
}
