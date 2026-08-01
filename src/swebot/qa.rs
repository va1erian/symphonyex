//! Q&A capability: answers questions asked in the repo's `Q&A`-category GitHub
//! Discussions (`swebot.qa.discussion_category`, default `"Q&A"`).
//!
//! No local persistence: "which comments has SweBot already answered" is derived
//! entirely from `<!-- swebot:answered:<comment-id> -->` markers embedded in SweBot's
//! own past replies, scanned out of the thread itself on every poll -- so it survives
//! a restart for free.

use super::{PERSONA, answered_marker, is_swebot_reply, last_answered_id, run_turn_collect_text};
use crate::agent::AgentBackend;
use crate::config::EffectiveConfig;
use crate::repo_host::GithubRepoHost;
use std::path::Path;

pub async fn poll_once(
    cfg: &EffectiveConfig,
    host: &GithubRepoHost,
    backend: &dyn AgentBackend,
    shared_clone_dir: &Path,
) -> Result<(), String> {
    let threads = host
        .list_discussions_in_category(&cfg.swebot.qa_discussion_category)
        .await?;

    for thread in threads {
        let last_answered = last_answered_id(&thread);
        let Some(question) = thread
            .comments
            .iter()
            .filter(|c| c.database_id > last_answered && !is_swebot_reply(&c.body))
            .max_by_key(|c| c.database_id)
        else {
            continue; // nothing new since SweBot's last reply (or this thread has no comments yet)
        };

        let transcript: String = thread
            .comments
            .iter()
            .filter(|c| c.database_id <= question.database_id)
            .map(|c| {
                format!(
                    "{}: {}",
                    c.author_login.as_deref().unwrap_or("someone"),
                    c.body
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let prompt = format!(
            "{PERSONA}\n\nAnswer the newest question in this GitHub Discussion (\"{}\"). \
             You have read access to the repo, checked out at {}. Cite specific files/lines \
             where relevant, and say plainly when something isn't knowable from the code \
             alone rather than guessing.\n\nDiscussion so far:\n{transcript}",
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
        let answer = match run_turn_collect_text(session.as_mut(), &prompt).await {
            Ok(text) => text,
            Err(e) => {
                session.stop().await;
                return Err(format!("discussion #{}: {e}", thread.number));
            }
        };
        session.stop().await;

        let reply = format!("{}\n{answer}", answered_marker(question.database_id));
        let comment_id = host.post_discussion_comment(&thread.id, &reply).await?;
        if let Err(e) = host.mark_discussion_comment_as_answer(&comment_id).await {
            // Non-fatal: only Q&A-category discussions actually support this mutation,
            // and a project may have miscategorized one -- the reply itself already
            // posted, which is the part that matters.
            tracing::debug!(discussion = thread.number, error = %e, "swebot: could not mark comment as answer (ignored)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, RepoConfig};
    use crate::repo_host::GithubRepoHost;
    use crate::swebot::test_support::FakeBackend;
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_cfg() -> config::EffectiveConfig {
        unsafe {
            std::env::set_var("SYMPHONY_TEST_QA_TOKEN", "t");
        }
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/owner/name.git\n  \
             token: $SYMPHONY_TEST_QA_TOKEN\nswebot:\n  enabled: true\n",
        )
        .unwrap();
        config::resolve(&yaml, std::path::Path::new(".")).unwrap()
    }

    fn test_host(server: &MockServer) -> GithubRepoHost {
        GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_QA_TOKEN".to_string()),
            pull_request: false,
        })
        .unwrap()
        .with_base_url_for_test(&server.uri())
    }

    #[tokio::test]
    async fn answers_the_newest_unanswered_question_and_marks_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("discussions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "discussions": {
                            "nodes": [{
                                "id": "D_1", "number": 1, "title": "How does auth work?",
                                "body": "", "category": {"name": "Q&A"},
                                "comments": {"nodes": [
                                    {"databaseId": 10, "body": "how does auth work?",
                                     "author": {"login": "alice"}}
                                ]}
                            }]
                        }
                    }
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("addDiscussionComment"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"addDiscussionComment": {"comment": {"id": "C_99"}}}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("markDiscussionCommentAsAnswer"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"markDiscussionCommentAsAnswer": {"clientMutationId": null}}
            })))
            .mount(&server)
            .await;

        let cfg = test_cfg();
        let host = test_host(&server);
        let backend = FakeBackend::with_response("Auth uses OAuth, see src/auth.rs.");
        let dir = tempfile::tempdir().unwrap();

        poll_once(&cfg, &host, &backend, dir.path()).await.unwrap();

        let prompts = backend.prompts_seen.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains("how does auth work?"));
    }

    #[tokio::test]
    async fn does_not_re_answer_a_question_already_covered_by_a_marker() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("discussions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "discussions": {
                            "nodes": [{
                                "id": "D_1", "number": 1, "title": "How does auth work?",
                                "body": "", "category": {"name": "Q&A"},
                                "comments": {"nodes": [
                                    {"databaseId": 10, "body": "how does auth work?",
                                     "author": {"login": "alice"}},
                                    {"databaseId": 11,
                                     "body": "<!-- swebot:answered:10 -->\nSee src/auth.rs.",
                                     "author": {"login": "swebot"}}
                                ]}
                            }]
                        }
                    }
                }
            })))
            .mount(&server)
            .await;
        // No addDiscussionComment mock registered -- if the driver tried to reply
        // anyway, the call would have nothing to match.

        let cfg = test_cfg();
        let host = test_host(&server);
        let backend = FakeBackend::with_response("should not be called");
        let dir = tempfile::tempdir().unwrap();

        poll_once(&cfg, &host, &backend, dir.path()).await.unwrap();

        assert!(backend.prompts_seen.lock().unwrap().is_empty());
    }
}
