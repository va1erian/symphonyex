//! Q&A capability: answers questions asked in the repo's `Q&A`-category GitHub
//! Discussions (`swebot.qa.discussion_category`, default `"Q&A"`), or, on GitLab,
//! Issues carrying the `swebot.qa.label` label (default `"swebot::question"`) --
//! see `config::SwebotConfig`'s doc comment for why the two hosts need different
//! conversational surfaces.
//!
//! No local persistence: "which comments has SweBot already answered" is derived
//! entirely from `<!-- swebot:answered:<comment-id> -->` markers embedded in SweBot's
//! own past replies, scanned out of the thread itself on every poll -- so it survives
//! a restart for free.

use super::{PERSONA, answered_marker, next_to_answer, run_turn_collect_text, transcript_up_to};
use crate::agent::AgentBackend;
use crate::config::{EffectiveConfig, RepoProvider};
use crate::repo_host::RepoHost;
use std::path::Path;

pub async fn poll_once(
    cfg: &EffectiveConfig,
    host: &dyn RepoHost,
    backend: &dyn AgentBackend,
    shared_clone_dir: &Path,
) -> Result<(), String> {
    let selector = match host.provider_kind() {
        RepoProvider::Github => &cfg.swebot.qa_discussion_category,
        RepoProvider::Gitlab => &cfg.swebot.qa_label,
    };
    let threads = host.list_swebot_threads(selector).await?;

    for thread in threads {
        let Some(marker_id) = next_to_answer(&thread) else {
            continue; // nothing new since SweBot's last reply
        };
        let transcript = transcript_up_to(&thread, marker_id);

        let prompt = format!(
            "{PERSONA}\n\nAnswer the newest question in this thread (\"{}\"). \
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

        let reply = format!("{}\n{answer}", answered_marker(marker_id));
        let comment_id = host.post_discussion_comment(&thread.id, &reply).await?;
        tracing::info!(discussion = thread.number, url = %thread.url, "swebot: answered discussion");
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
    use crate::repo_host::github::GithubRepoHost;
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
            ..Default::default()
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
                                "body": "", "url": "https://github.com/owner/name/discussions/1",
                                "category": {"name": "Q&A"},
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
                                "body": "", "url": "https://github.com/owner/name/discussions/1",
                                "category": {"name": "Q&A"},
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

    fn test_gitlab_cfg() -> config::EffectiveConfig {
        unsafe {
            std::env::set_var("SYMPHONY_TEST_QA_GITLAB_TOKEN", "t");
        }
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            "tracker:\n  kind: local\nrepo:\n  provider: gitlab\n  \
             url: https://gitlab.com/owner/name.git\n  token: $SYMPHONY_TEST_QA_GITLAB_TOKEN\n\
             swebot:\n  enabled: true\n",
        )
        .unwrap();
        config::resolve(&yaml, std::path::Path::new(".")).unwrap()
    }

    fn test_gitlab_host(server: &MockServer) -> crate::repo_host::gitlab::GitlabRepoHost {
        unsafe {
            std::env::set_var("SYMPHONY_TEST_QA_GITLAB_HOST_TOKEN", "t");
        }
        crate::repo_host::gitlab::GitlabRepoHost::new(&RepoConfig {
            url: "https://gitlab.com/owner/name.git".to_string(),
            provider: crate::config::RepoProvider::Gitlab,
            api_base_url: Some(server.uri()),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_QA_GITLAB_HOST_TOKEN".to_string()),
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn gitlab_answers_the_newest_unanswered_question_and_marks_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/owner%2Fname/issues"))
            .and(wiremock::matchers::query_param(
                "labels",
                "swebot::question",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "iid": 1, "title": "How does auth work?", "description": "",
                "web_url": "https://gitlab.com/owner/name/-/issues/1"
            })]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects/owner%2Fname/issues/1/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "id": 10, "body": "how does auth work?", "system": false,
                "author": {"username": "alice"}
            })]))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/projects/owner%2Fname/issues/1/notes"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 99})))
            .mount(&server)
            .await;

        let cfg = test_gitlab_cfg();
        let host = test_gitlab_host(&server);
        let backend = FakeBackend::with_response("Auth uses OAuth, see src/auth.rs.");
        let dir = tempfile::tempdir().unwrap();

        poll_once(&cfg, &host, &backend, dir.path()).await.unwrap();

        let prompts = backend.prompts_seen.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains("how does auth work?"));
    }

    #[tokio::test]
    async fn gitlab_does_not_re_answer_a_question_already_covered_by_a_marker() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects/owner%2Fname/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
                "iid": 1, "title": "How does auth work?", "description": "",
                "web_url": "https://gitlab.com/owner/name/-/issues/1"
            })]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects/owner%2Fname/issues/1/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                json!({"id": 10, "body": "how does auth work?", "system": false,
                       "author": {"username": "alice"}}),
                json!({"id": 11, "body": "<!-- swebot:answered:10 -->\nSee src/auth.rs.",
                       "system": false, "author": {"username": "swebot"}}),
            ]))
            .mount(&server)
            .await;
        // No POST notes mock registered -- if the driver tried to reply anyway, the
        // call would have nothing to match.

        let cfg = test_gitlab_cfg();
        let host = test_gitlab_host(&server);
        let backend = FakeBackend::with_response("should not be called");
        let dir = tempfile::tempdir().unwrap();

        poll_once(&cfg, &host, &backend, dir.path()).await.unwrap();

        assert!(backend.prompts_seen.lock().unwrap().is_empty());
    }
}
