//! GitHub Discussions connector: SweBot's Q&A and ticket-drafting surface, folded
//! into the chat pipeline. Replaces the old standalone `qa.rs`/`drafting.rs` drivers
//! -- a discussion thread becomes a `github` conversation, each new human comment is
//! `ingest`ed as a `pending` user message, the worker answers it like any other chat
//! message, and `deliver` posts the reply back as a discussion comment.
//!
//! The `<!-- swebot:answered:<databaseId> -->` marker scheme (`swebot::answered_marker`
//! & friends) is preserved end-to-end, for two reasons:
//!
//! - **Idempotent ingest/migration**: any comment SweBot (old standalone or chat)
//!   has ever answered carries a marker, so `ingest` never re-enqueues it -- even the
//!   first chat run after upgrading from the pre-chat SweBot picks up exactly where
//!   the old loop left off.
//! - **Cross-process dedupe**: markers live in the platform's own data, not the
//!   store, so if delivery and ingest ever race (reply posted but poll saw the
//!   thread before the marker landed), the store's `remote_message_id` idempotency
//!   key is the second, backstop layer.
//!
//! Delivery is eventual, keyed off the store: as soon as a reply is `sent`, the next
//! `deliver` poll posts it (with the marker of the human comment it answers, so the
//! thread advances correctly). An in-flight turn's "still working" system notice is
//! also delivered, carrying `NOTICE_MARKER` instead of an `answered_marker` -- distinct
//! from a real reply's marker so it never advances the thread's answered-marker before
//! the real reply lands, but still recognizable as SweBot's own post so the next
//! `ingest` poll doesn't mistake it for a fresh human comment and answer it.

use super::connector::ChatConnector;
use super::store::{ChatStore, STATUS_NOTICE_DONE};
use crate::repo_host::DiscussionHost;
use crate::repo_host::github::GithubRepoHost;
use crate::swebot::{NOTICE_MARKER, answered_marker, is_swebot_reply, last_answered_marker};

/// Connector name + conversations server-wide key for discussions.
pub const CONNECTOR_NAME: &str = "github";

pub struct GitHubChatConnector {
    host: GithubRepoHost,
    qa_category: String,
    drafting_category: String,
}

impl GitHubChatConnector {
    pub fn new(host: GithubRepoHost, qa_category: String, drafting_category: String) -> Self {
        Self {
            host,
            qa_category,
            drafting_category,
        }
    }
}

#[async_trait::async_trait]
impl ChatConnector for GitHubChatConnector {
    fn name(&self) -> &str {
        CONNECTOR_NAME
    }

    async fn ingest(&self, store: &ChatStore) -> Result<(), String> {
        self.ingest_category(store, &self.qa_category).await?;
        self.ingest_category(store, &self.drafting_category).await?;
        Ok(())
    }

    async fn deliver(&self, store: &ChatStore) -> Result<(), String> {
        deliver_notices(self, store).await?;
        deliver_replies(self, store).await?;
        Ok(())
    }
}

impl GitHubChatConnector {
    async fn ingest_category(&self, store: &ChatStore, category: &str) -> Result<(), String> {
        let threads = self.host.list_swebot_threads(category).await?;
        for thread in threads {
            let last = last_answered_marker(&thread);
            let conv = store.get_or_create_remote_conversation(
                CONNECTOR_NAME,
                &thread.id,
                "github",
                &thread.title,
            )?;
            // The opening body ("remote id 0") is the original question; enqueue it
            // until any marker shows it has been answered.
            if last.is_none() {
                store.upsert_remote_user_message(conv, 0, &thread.body)?;
            }
            for comment in &thread.comments {
                // SweBot's own replies (marker carriers and all) are never re-fed as
                // questions; they're what the markers are derived from.
                if is_swebot_reply(&comment.body) {
                    continue;
                }
                // Anything at or before the last answered marker was already handled.
                if last.is_some_and(|l| comment.database_id <= l) {
                    continue;
                }
                store.upsert_remote_user_message(conv, comment.database_id, &comment.body)?;
            }
        }
        Ok(())
    }
}

/// Post undelivered "still working" notices as marker-less comments (marker-less so
/// they never advance the thread's answered-marker before the actual reply lands).
async fn deliver_notices(connector: &GitHubChatConnector, store: &ChatStore) -> Result<(), String> {
    for notice in store.undelivered_system_notices(CONNECTOR_NAME)? {
        let Some(conv) = store.conversation(notice.conversation_id)? else {
            continue;
        };
        let Some(thread_id) = conv.remote_id else {
            continue;
        };
        // NOTICE_MARKER (not `answered_marker`) so this never advances the thread's
        // answered-marker before the real reply lands, but still marks the comment as
        // SweBot's own -- otherwise the very next `ingest_category` poll finds this
        // "still working" text with no author identity to check and no marker to
        // recognize, treats it as a fresh human comment, and answers it: an
        // infinite loop of SweBot replying to its own notices (see NOTICE_MARKER's
        // doc comment -- this is exactly the bug it was added to fix).
        let comment = format!("{NOTICE_MARKER}\n{}", notice.body);
        let comment_id = connector
            .host
            .post_discussion_comment(&thread_id, &comment)
            .await?;
        store.mark_delivered(notice.id, &comment_id)?;
        // It's on GitHub now; drop the local "active" flag so nothing else tries to
        // deliver it again (or reads it as an in-flight turn).
        store.set_message_status(notice.id, STATUS_NOTICE_DONE)?;
        tracing::debug!(
            discussion = thread_id,
            "swebot chat: delivered notice to discussion"
        );
    }
    Ok(())
}

/// Post undelivered replies as comments carrying the answered marker, then transition
/// their conversation's local notices to done (the marker now covers what the notice
/// stood in for on GitHub too).
async fn deliver_replies(connector: &GitHubChatConnector, store: &ChatStore) -> Result<(), String> {
    for reply in store.undelivered_assistant_sent(CONNECTOR_NAME)? {
        let Some(conv) = store.conversation(reply.conversation_id)? else {
            continue;
        };
        let Some(thread_id) = conv.remote_id else {
            continue;
        };
        // The marker is the remote id of the human message this reply answers -- the
        // databaseId of the comment (or 0 for the opening body). With the marker
        // embedded, `next_to_answer` on the next poll treats that message as handled.
        let marker_id = match reply.reply_to {
            Some(user_msg_id) => store
                .message(user_msg_id)?
                .and_then(|m| m.remote_message_id)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0),
            None => 0,
        };
        let comment = format!("{}\n{}", answered_marker(marker_id), reply.body);
        let comment_id = connector
            .host
            .post_discussion_comment(&thread_id, &comment)
            .await?;
        store.mark_delivered(reply.id, &comment_id)?;
        store.resolve_system_notices(reply.conversation_id)?;
        if let Err(e) = connector
            .host
            .mark_discussion_comment_as_answer(&comment_id)
            .await
        {
            // Non-fatal: only Q&A-category discussions actually support this
            // mutation; the reply itself already posted, which is the part that
            // matters (mirrors the old qa/drafting drivers).
            tracing::debug!(discussion = thread_id, error = %e, "swebot chat: could not mark comment as answer (ignored)");
        }
        tracing::info!(discussion = thread_id, url = %conv.title, "swebot chat: delivered reply to discussion");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, RepoConfig};
    use crate::swebot::chat::store::{ChatStore, ROLE_ASSISTANT, STATUS_PROCESSED, STATUS_SENT};
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_cfg() -> config::EffectiveConfig {
        unsafe {
            std::env::set_var("SYMPHONY_TEST_GH_CHAT_TOKEN", "t");
        }
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/owner/name.git\n  \
             token: $SYMPHONY_TEST_GH_CHAT_TOKEN\nswebot:\n  enabled: true\n  \
             chat:\n    enabled: true\n",
        )
        .unwrap();
        config::resolve(&yaml, std::path::Path::new(".")).unwrap()
    }

    fn test_host(server: &MockServer) -> GithubRepoHost {
        GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_GH_CHAT_TOKEN".to_string()),
            pull_request: false,
            provider: crate::config::RepoProvider::Github,
            api_base_url: None,
        })
        .unwrap()
        .with_base_url_for_test(&server.uri())
    }

    fn test_connector(server: &MockServer) -> GitHubChatConnector {
        let cfg = test_cfg();
        GitHubChatConnector::new(
            test_host(server),
            cfg.swebot.qa_discussion_category,
            cfg.swebot.drafting_discussion_category,
        )
    }

    fn response_thread(body: &str, comments: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "repository": {
                    "discussions": {
                        "nodes": [{
                            "id": "D_1", "number": 1, "title": "How does auth work?",
                            "body": body, "url": "https://github.com/owner/name/discussions/1",
                            "category": {"name": "Q&A"},
                            "comments": {"nodes": comments}
                        }]
                    }
                }
            }
        }))
    }

    fn comment(database_id: u64, body: &str, login: &str) -> serde_json::Value {
        json!({
            "databaseId": database_id,
            "body": body,
            "author": {"login": login},
            "replies": {"nodes": []}
        })
    }

    async fn mock_post_comment(server: &MockServer, comment_id: &str) {
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("addDiscussionComment"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"addDiscussionComment": {"comment": {"id": comment_id}}}
            })))
            .mount(server)
            .await;
    }

    async fn mock_mark_answer(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("markDiscussionCommentAsAnswer"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"markDiscussionCommentAsAnswer": {"clientMutationId": null}}
            })))
            .mount(server)
            .await;
    }

    fn store() -> (ChatStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = ChatStore::open(dir.path().join("chat.db")).unwrap();
        (s, dir)
    }

    #[tokio::test]
    async fn ingest_enqueues_opening_body_and_new_comments_idempotently() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("discussions"))
            .respond_with(response_thread(
                "how does auth work?",
                json!([comment(10, "any docs?", "alice")]),
            ))
            .mount(&server)
            .await;

        let connector = test_connector(&server);
        let (store, _d) = store();
        connector.ingest(&store).await.unwrap();

        // Opening body + the one new comment both land in the store.
        let conv = store.list_conversations().unwrap()[0].id;
        let msgs = store.messages_of_conversation(conv, 0).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].remote_message_id.as_deref(), Some("0"));
        assert_eq!(msgs[1].remote_message_id.as_deref(), Some("10"));

        // One turn per conversation: only the oldest (the body) is pending at once.
        let pending = store.pending_user_messages(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].remote_message_id.as_deref(), Some("0"));
        store
            .set_message_status(pending[0].id, STATUS_PROCESSED)
            .unwrap();
        let pending = store.pending_user_messages(10).unwrap();
        assert_eq!(pending[0].remote_message_id.as_deref(), Some("10"));

        // A second ingest must not duplicate anything (idempotency via the unique
        // (conversation_id, remote_message_id) key).
        connector.ingest(&store).await.unwrap();
        assert_eq!(store.messages_of_conversation(conv, 0).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn ingest_skips_swebot_replies_and_previously_answered_comments() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("discussions"))
            .respond_with(response_thread(
                "how does auth work?",
                json!([
                    comment(10, "nevermind, found it", "alice"),
                    comment(11, "<!-- swebot:answered:10 -->\nSee src/auth.rs", "swebot"),
                    comment(12, "what about videos?", "alice")
                ]),
            ))
            .mount(&server)
            .await;

        let connector = test_connector(&server);
        let (store, _d) = store();
        connector.ingest(&store).await.unwrap();

        // Comment 10 is at/below the marker, the marker-carrier comment 11 is a SweBot
        // reply, and the opening body isn't re-enqueued once any marker exists (the
        // conversation is past its opening), so only the follow-up at 12 lands.
        let conv = store.list_conversations().unwrap()[0].id;
        let msgs = store.messages_of_conversation(conv, 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].remote_message_id.as_deref(), Some("12"));

        let pending = store.pending_user_messages(10).unwrap();
        assert_eq!(pending[0].remote_message_id.as_deref(), Some("12"));
    }

    #[tokio::test]
    async fn deliver_posts_the_reply_with_the_answered_marker_and_marks_it() {
        let server = MockServer::start().await;
        mock_post_comment(&server, "C_99").await;
        mock_mark_answer(&server).await;

        let connector = test_connector(&server);
        let (store, _d) = store();
        let conv = store
            .get_or_create_remote_conversation(CONNECTOR_NAME, "D_1", "github", "t")
            .unwrap();
        // The human comment (remote id 10) ingested earlier, then answered by a
        // now-sent assistant reply linked to it.
        store
            .upsert_remote_user_message(conv, 10, "what about auth?")
            .unwrap();
        let user_id = store.pending_user_messages(10).unwrap()[0].id;
        let assistant_id = store
            .insert_message(
                conv,
                ROLE_ASSISTANT,
                "Auth uses OAuth.",
                STATUS_SENT,
                &json!({}),
                Some(user_id),
            )
            .unwrap();

        connector.deliver(&store).await.unwrap();

        // The reply is marked delivered against the posted comment's node id.
        let delivered = store.message(assistant_id).unwrap().unwrap();
        assert_eq!(delivered.remote_message_id.as_deref(), Some("C_99"));
        // And the comment body really carried the answered comment's remote id.
        let jobs = server.received_requests().await.unwrap();
        let add = jobs
            .iter()
            .find(|r| String::from_utf8_lossy(&r.body).contains("addDiscussionComment"))
            .expect("an addDiscussionComment mutation");
        let body = String::from_utf8_lossy(&add.body).to_string();
        assert!(body.contains("swebot:answered:10"));
        assert!(body.contains("Auth uses OAuth."));
    }

    #[tokio::test]
    async fn deliver_posts_system_notices_with_the_notice_marker_not_an_answered_marker() {
        let server = MockServer::start().await;
        mock_post_comment(&server, "C_77").await;

        let connector = test_connector(&server);
        let (store, _d) = store();
        let conv = store
            .get_or_create_remote_conversation(CONNECTOR_NAME, "D_1", "github", "t")
            .unwrap();
        let notice_id = store.insert_system_notice(conv, "still working").unwrap();

        connector.deliver(&store).await.unwrap();

        let notice = store.message(notice_id).unwrap().unwrap();
        assert_eq!(notice.remote_message_id.as_deref(), Some("C_77"));
        // Notices must not advance the answered marker...
        let jobs = server.received_requests().await.unwrap();
        let body = jobs
            .iter()
            .filter_map(|r| {
                let b = String::from_utf8_lossy(&r.body);
                b.contains("addDiscussionComment").then_some(b.to_string())
            })
            .next()
            .unwrap();
        assert!(body.contains("still working"));
        assert!(!body.contains("swebot:answered:"));
        // ...but must carry NOTICE_MARKER, so ingest recognizes it as SweBot's own post
        // rather than a fresh human comment (see the regression test below).
        assert!(body.contains(NOTICE_MARKER));
    }

    /// Regression test for a real bug found running this live: a delivered "still
    /// working" notice carried no marker at all, so the very next `ingest` poll found
    /// it in `thread.comments` with nothing to distinguish it from a human comment,
    /// enqueued it as a fresh question, and answered it -- an infinite loop of SweBot
    /// replying to its own notices, visible as alternating "Still working..." /
    /// substantive-reply comments forever in a real GitHub Discussion.
    #[tokio::test]
    async fn ingest_never_re_enqueues_a_delivered_notice_as_a_question() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("discussions"))
            .respond_with(response_thread(
                "how does auth work?",
                json!([
                    comment(
                        10,
                        &format!("{NOTICE_MARKER}\nStill working on that — checking the code."),
                        "swebot"
                    ),
                    comment(11, &format!("{}\nAuth uses OAuth.", answered_marker(0)), "swebot"),
                ]),
            ))
            .mount(&server)
            .await;

        let connector = test_connector(&server);
        let (store, _d) = store();
        connector.ingest(&store).await.unwrap();

        // Nothing left to answer: the opening body was answered (marker 0), and the
        // notice at comment 10 must never have been enqueued as a question.
        let conv = store.list_conversations().unwrap()[0].id;
        let msgs = store.messages_of_conversation(conv, 0).unwrap();
        assert!(msgs.is_empty(), "expected no enqueued messages, got {msgs:?}");
    }
}
