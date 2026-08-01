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
        // Written to a file and *referenced* in the prompt, not embedded in it: a
        // real PR's diff hit Windows' ~32K command-line length limit in practice
        // (`claude` is launched with the prompt as a literal argv entry -- see
        // `agent::claude::ClaudeSession::run_turn`), failing with "The filename or
        // extension is too long" (os error 206) before the review could even start.
        // The agent already has Bash/Read access to this same checked-out clone, so
        // pointing it at the file costs nothing it doesn't already have.
        let diff_path = scratch.path().join(".swebot-review.diff");
        tokio::fs::write(&diff_path, &diff)
            .await
            .map_err(|e| format!("PR #{}: failed to write diff file: {e}", pr.number))?;

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
            "{PERSONA}\n\nReview this pull request.\n\n{issue_context}\n\nThe diff against \
             {} is at {} in this checked-out repo (read it with your Read/Bash tools -- it \
             wasn't inlined here since a large diff can exceed the command-line length this \
             prompt is passed with). You also have Bash access to run tests/lints in this \
             checkout at {} -- judge the code, don't fix it (file-editing tools are disabled \
             for this session). Check: correctness against the original ticket's acceptance \
             criteria, security (input handling, secrets, auth/authz boundaries), \
             performance (obvious inefficiency, unbounded loops/queries), and whether it \
             matches this project's own conventions and includes tests. \
             request_changes means something genuinely fails one of these; approve means \
             \"I'd merge this,\" not \"nothing's obviously on fire.\"\n\n\
             End your response with exactly one fenced ```json block: \
             {{\"verdict\": \"approve\"|\"request_changes\"|\"comment\", \
             \"summary\": \"<your reasoning, written for the human who'll read this review>\"}}.",
            repo.default_branch,
            diff_path.display(),
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
        // The diff is written to a file and *referenced* by path, not embedded in
        // the prompt directly (a real diff exceeded Windows' command-line length
        // limit when it was inlined -- see the write-then-reference comment at the
        // call site). Only a structural check here (the actual diff *content* isn't
        // re-checked -- by the time this assertion runs, `poll_once` has already
        // returned and dropped the scratch tempdir the file lived in; the dedicated
        // large-diff test below is what actually exercises the write-then-read path
        // at a size that would have broken the old inlined-prompt approach).
        assert!(
            prompts[0].contains(".swebot-review.diff"),
            "prompt should reference the diff file, not embed diff content: {}",
            prompts[0]
        );
        assert!(!prompts[0].contains("println"));
    }

    /// Regression test for a real bug found running this live: with the diff
    /// embedded directly in the prompt, `claude` (launched with the prompt as a
    /// literal command-line argument -- see `agent::claude::ClaudeSession::run_turn`)
    /// failed outright on a real PR's diff with "The filename or extension is too
    /// long" (Windows' ~32K command-line length limit, os error 206). The prompt
    /// must stay small regardless of diff size now that the diff is written to a
    /// file and only referenced by path.
    #[tokio::test]
    async fn prompt_stays_small_even_for_a_diff_that_would_have_blown_the_command_line_limit() {
        let origin = real_repo_with_a_ticket_branch();
        // Simulate a diff far larger than Windows' ~32K command-line limit by adding
        // a second, much bigger change on the same branch.
        std::process::Command::new("git")
            .args(["checkout", "issue-42"])
            .current_dir(origin.path())
            .status()
            .unwrap();
        std::fs::write(origin.path().join("big.rs"), "x".repeat(200_000)).unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(origin.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "big file",
            ])
            .current_dir(origin.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "main"])
            .current_dir(origin.path())
            .status()
            .unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "number": 43, "html_url": "https://github.com/owner/name/pull/43",
                "body": "", "head": {"ref": "issue-42", "sha": "fakesha456"}
            })]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/name/pulls/43/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/name/pulls/43/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
            .mount(&server)
            .await;

        let cfg = test_cfg(origin.path());
        let host = test_host(&server);
        let backend = FakeBackend::with_response(
            "```json\n{\"verdict\": \"comment\", \"summary\": \"n/a\"}\n```",
        );
        let tracker_dir = tempdir().unwrap();
        let tracker_provider: serde_yaml::Value =
            serde_yaml::from_str(&format!("dir: {:?}", tracker_dir.path())).unwrap();
        let tracker =
            LocalTrackerAdapter::new(&tracker_provider, std::path::Path::new(".")).unwrap();

        poll_once(&cfg, &host, &backend, &tracker).await.unwrap();

        let prompts = backend.prompts_seen.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        // Comfortably under Windows' ~32K command-line limit regardless of how big
        // the underlying diff (200KB+ here) actually is.
        assert!(
            prompts[0].len() < 4_000,
            "prompt should stay small regardless of diff size, was {} bytes",
            prompts[0].len()
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
