//! Ticket-drafting capability: turns a rough idea posted in the repo's `Ideas`-
//! category GitHub Discussions (`swebot.drafting.discussion_category`, default
//! `"Ideas"`), an issue label on GitLab (`swebot.drafting.label`, default
//! `"swebot::idea"`), or a board on `crate::board`'s local bulletin board when
//! `swebot.board.enabled`, into a properly scoped issue through a clarifying
//! dialogue. `selector` below is whichever of those `swebot::mod::run` resolved for
//! the current `host`; this module itself stays agnostic to which one it got.
//!
//! Ends by creating a **new** issue via `TrackerAdapter::create_issue` rather than
//! rewriting the source thread into one -- the source stays the messy conversational
//! space, the tracker's Issues stays the clean actionable backlog Symphony's own
//! dispatch loop watches, and a half-drafted idea is never sitting in the tracker
//! looking dispatchable when it isn't. On GitLab (where the source thread is itself
//! a tracker Issue) and on the local bulletin board, the source thread is closed
//! once promoted (`DiscussionHost::close_thread`) to keep that separation from
//! collapsing back into one undifferentiated list.
//!
//! Each poll cycle starts a *fresh* `claude` session rather than resuming a previous
//! one (a human's next reply may come hours later, well past a SweBot restart) --
//! the full discussion transcript, reconstructed from the host's own stored comments
//! and sent as the prompt every time, carries the conversation's state instead. No
//! `--resume` needed, and nothing to persist beyond the same `swebot:answered`
//! marker `qa.rs` already uses.

use super::{
    PERSONA, answered_marker, extract_json_block, next_to_answer, run_turn_collect_text,
    transcript_up_to,
};
use crate::agent::AgentBackend;
use crate::config::EffectiveConfig;
use crate::repo_host::DiscussionHost;
use crate::tracker::TrackerAdapter;
use std::path::Path;

pub async fn poll_once(
    cfg: &EffectiveConfig,
    host: &dyn DiscussionHost,
    selector: &str,
    backend: &dyn AgentBackend,
    shared_clone_dir: &Path,
    tracker: &dyn TrackerAdapter,
) -> Result<(), String> {
    let threads = host.list_swebot_threads(selector).await?;

    for thread in threads {
        let Some(marker_id) = next_to_answer(&thread) else {
            continue; // nothing new since SweBot's last reply, or already handed off to an issue
        };
        let transcript = transcript_up_to(&thread, marker_id);

        let prompt = format!(
            "{PERSONA}\n\nHelp turn this idea (\"{}\") into a properly \
             scoped ticket. Ask only the clarifying questions that actually change scope \
             -- acceptance criteria, edge cases, what's explicitly out of scope -- not a \
             generic checklist. You have read access to the repo, checked out at {}, so \
             ground questions in what's actually there rather than guessing.\n\n\
             Original idea and discussion so far:\n{transcript}\n\n\
             End your response with exactly one fenced ```json block: either \
             {{\"ready\": false, \"reply\": \"<your clarifying question>\"}} if you need \
             more information, or {{\"ready\": true, \"title\": \"<ticket title>\", \
             \"body\": \"<full ticket body: what, why, acceptance criteria>\"}} once you \
             have enough to write a properly scoped ticket.",
            thread.title,
            shared_clone_dir.display(),
        );

        let mut session = backend
            .start_session(
                shared_clone_dir,
                &thread.number.to_string(),
                &thread.title,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        let raw = match run_turn_collect_text(session.as_mut(), &prompt).await {
            Ok(text) => text,
            Err(e) => {
                session.stop().await;
                return Err(format!("discussion #{}: {e}", thread.number));
            }
        };
        session.stop().await;

        let parsed = match extract_json_block(&raw) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!(
                    "discussion #{}: {e} (raw response: {raw})",
                    thread.number
                ));
            }
        };
        let ready = parsed
            .get("ready")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let marker = answered_marker(marker_id);

        if !ready {
            let reply = parsed
                .get("reply")
                .and_then(|v| v.as_str())
                .unwrap_or("Could you say a bit more about what you're looking for?");
            let comment = format!("{marker}\n{reply}");
            host.post_discussion_comment(&thread.id, &comment).await?;
            tracing::info!(
                discussion = thread.number,
                url = %thread.url,
                "swebot: asked a clarifying question"
            );
            continue;
        }

        let title = parsed
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(&thread.title)
            .to_string();
        let body = parsed
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or(&thread.body)
            .to_string();

        // The project's own configured "starting" active state -- the same
        // vocabulary `tracker.active_states` already uses. First entry by
        // convention (mirrors how a project orders todo before in-progress etc.).
        let Some(initial_state) = cfg.active_states.first() else {
            return Err(format!(
                "discussion #{}: tracker.active_states is empty, nowhere to create the \
                 drafted issue into",
                thread.number
            ));
        };

        let issue = tracker
            .create_issue(&title, &body, initial_state)
            .await
            .map_err(|e| format!("discussion #{}: create_issue failed: {e}", thread.number))?;

        let comment = format!(
            "{marker}\nDrafted and created: **{}** ({})\n\nSymphony will pick this up on \
             its next poll.",
            title,
            issue.url.as_deref().unwrap_or(&issue.identifier),
        );
        let comment_id = host.post_discussion_comment(&thread.id, &comment).await?;
        tracing::info!(
            discussion = thread.number,
            issue = %issue.identifier,
            "swebot: drafted and created issue"
        );
        if let Err(e) = host.mark_discussion_comment_as_answer(&comment_id).await {
            tracing::debug!(discussion = thread.number, error = %e, "swebot: could not mark comment as answer (ignored)");
        }
        // Best-effort, same tolerance as mark_discussion_comment_as_answer above: a
        // no-op on GitHub (Discussions were never closed by this flow), but on GitLab
        // this keeps the label-filtered polling query (list_swebot_threads) from
        // accumulating stale, already-drafted idea issues forever -- the link to the
        // new ticket is already in the comment just posted above.
        if let Err(e) = host.close_thread(&thread.id).await {
            tracing::debug!(discussion = thread.number, error = %e, "swebot: could not close source thread (ignored)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, RepoConfig, RepoProvider};
    use crate::repo_host::github::GithubRepoHost;
    use crate::repo_host::gitlab::GitlabRepoHost;
    use crate::swebot::test_support::FakeBackend;
    use crate::tracker::local::LocalTrackerAdapter;
    use serde_json::json;
    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_cfg() -> config::EffectiveConfig {
        unsafe {
            std::env::set_var("SYMPHONY_TEST_DRAFTING_TOKEN", "t");
        }
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            "tracker:\n  kind: local\n  active_states: [\"todo\"]\nrepo:\n  \
             url: https://github.com/owner/name.git\n  token: $SYMPHONY_TEST_DRAFTING_TOKEN\n\
             swebot:\n  enabled: true\n",
        )
        .unwrap();
        config::resolve(&yaml, std::path::Path::new(".")).unwrap()
    }

    fn test_host(server: &MockServer) -> GithubRepoHost {
        GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_DRAFTING_TOKEN".to_string()),
            pull_request: false,
            ..Default::default()
        })
        .unwrap()
        .with_base_url_for_test(&server.uri())
    }

    #[tokio::test]
    async fn drafts_and_creates_an_issue_when_ready() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(wiremock::matchers::body_string_contains("discussions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "discussions": {
                            "nodes": [{
                                "id": "D_1", "number": 1, "title": "archive download feature",
                                "body": "export pictures as a zip",
                                "url": "https://github.com/owner/name/discussions/1",
                                "category": {"name": "Ideas"},
                                "comments": {"nodes": []}
                            }]
                        }
                    }
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(wiremock::matchers::body_string_contains(
                "addDiscussionComment",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"addDiscussionComment": {"comment": {"id": "C_1"}}}
            })))
            .mount(&server)
            .await;

        let cfg = test_cfg();
        let host = test_host(&server);
        let backend = FakeBackend::with_response(
            "```json\n{\"ready\": true, \"title\": \"Export as zip\", \
             \"body\": \"acceptance criteria here\"}\n```",
        );
        let tracker_dir = tempdir().unwrap();
        let tracker_provider: serde_yaml::Value =
            serde_yaml::from_str(&format!("dir: {:?}", tracker_dir.path())).unwrap();
        let tracker =
            LocalTrackerAdapter::new(&tracker_provider, std::path::Path::new(".")).unwrap();
        let dir = tempfile::tempdir().unwrap();

        poll_once(
            &cfg,
            &host,
            &cfg.swebot.drafting_discussion_category,
            &backend,
            dir.path(),
            &tracker,
        )
        .await
        .unwrap();

        let created = tracker
            .fetch_issues_by_states(&["todo".to_string()])
            .await
            .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].title, "Export as zip");
    }

    fn test_gitlab_host(server: &MockServer) -> GitlabRepoHost {
        unsafe {
            std::env::set_var("SYMPHONY_TEST_DRAFTING_GITLAB_TOKEN", "t");
        }
        GitlabRepoHost::new(&RepoConfig {
            url: "https://gitlab.com/owner/name.git".to_string(),
            provider: RepoProvider::Gitlab,
            api_base_url: Some(server.uri()),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_DRAFTING_GITLAB_TOKEN".to_string()),
            ..Default::default()
        })
        .unwrap()
    }

    /// GitLab-specific: once an idea is promoted to a real tracker issue, the source
    /// labeled issue must be closed (`RepoHost::close_thread`) -- a no-op on GitHub,
    /// but on GitLab this is what keeps `list_swebot_threads`'s label-filtered query
    /// from re-processing the same already-drafted idea forever.
    #[tokio::test]
    async fn gitlab_closes_the_source_issue_after_drafting_and_creating_a_ticket() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/owner%2Fname/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "iid": 3, "title": "archive download feature",
                "description": "export pictures as a zip",
                "web_url": "https://gitlab.com/owner/name/-/issues/3"
            })]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects/owner%2Fname/issues/3/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/projects/owner%2Fname/issues/3/notes"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 50})))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/projects/owner%2Fname/issues/3"))
            .and(wiremock::matchers::body_string_contains("close"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let yaml: serde_yaml::Value = serde_yaml::from_str(
            "tracker:\n  kind: local\n  active_states: [\"todo\"]\nrepo:\n  provider: gitlab\n  \
             url: https://gitlab.com/owner/name.git\n  token: $SYMPHONY_TEST_DRAFTING_GITLAB_TOKEN\n\
             swebot:\n  enabled: true\n",
        )
        .unwrap();
        unsafe {
            std::env::set_var("SYMPHONY_TEST_DRAFTING_GITLAB_TOKEN", "t");
        }
        let cfg = config::resolve(&yaml, std::path::Path::new(".")).unwrap();

        let host = test_gitlab_host(&server);
        let backend = FakeBackend::with_response(
            "```json\n{\"ready\": true, \"title\": \"Export as zip\", \
             \"body\": \"acceptance criteria here\"}\n```",
        );
        let tracker_dir = tempdir().unwrap();
        let tracker_provider: serde_yaml::Value =
            serde_yaml::from_str(&format!("dir: {:?}", tracker_dir.path())).unwrap();
        let tracker =
            LocalTrackerAdapter::new(&tracker_provider, std::path::Path::new(".")).unwrap();
        let dir = tempfile::tempdir().unwrap();

        poll_once(
            &cfg,
            &host,
            &cfg.swebot.drafting_label,
            &backend,
            dir.path(),
            &tracker,
        )
        .await
        .unwrap();
    }
}
