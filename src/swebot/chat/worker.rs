//! Chat worker: the single authority that turns pending user messages into SweBot
//! replies. One task per project, polled on `swebot.chat.poll_interval_ms`.
//!
//! Turn flow per message:
//!
//! 1. Claim the oldest `pending` user message per conversation (`processing`).
//! 2. Build a prompt from the conversation's transcript + the shared repo clone.
//! 3. Insert an empty assistant message (`streaming`) and run the turn **streaming**
//!    (`run_turn_streaming`): each `AgentEvent` text chunk is flushed into the store
//!    (~150ms cadence), so the web UI shows the reply appearing rather than a blank
//!    window. If the first text hasn't arrived within
//!    `swebot.chat.first_text_deadline_ms` (default 5000) the worker inserts a system
//!    notice ("still working…") so the user is *told* a slow turn is happening instead
//!    of staring at a spinner -- the must-notify rule; the notice resolves to
//!    `notice-done` once the reply lands.
//! 4. Drafting: if the response's trailing ```json block says `ready`, either create
//!    the issue immediately (`auto_create_issue`, default) or stash the draft in the
//!    message `meta` and wait for a "create it" confirmation. A confirmation is
//!    detected on its own next turn (see `is_confirmation` + `create_from_pending_draft`).
//!
//! Runs connector-agnostic: `connectors` are polled for ingest/deliver around the
//! processing, and the worker itself never knows which platform produced a message.

use super::connector::ChatConnector;
use super::store::{
    ChatStore, MessageRow, ROLE_ASSISTANT, ROLE_SYSTEM, STATUS_FAILED, STATUS_NOTICE_ERROR,
    STATUS_PROCESSED, STATUS_PROCESSING, STATUS_SENT, STATUS_STREAMING,
};
use crate::agent::{AgentBackend, AgentEvent, AgentSession, TurnOutcome};
use crate::config::EffectiveConfig;
use crate::swebot::{PERSONA, extract_json_block};
use crate::tracker::TrackerAdapter;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Placeholder shown while an assistant message streams its first text.
const NOTICE_BODY: &str = "Still working on that — checking the code; I'll reply shortly.";

pub async fn run_loop(
    cfg: EffectiveConfig,
    backend: Box<dyn AgentBackend>,
    tracker: Arc<dyn TrackerAdapter>,
    store: ChatStore,
    connectors: Vec<Box<dyn ChatConnector>>,
) {
    // Requeue anything a previous run crashed mid-claim, then start (the store's
    // `open` does this too; belt and suspenders across task restart boundaries).
    if let Err(e) = store.requeue_stale_processing() {
        tracing::warn!(error = %e, "chat: stale-claim requeue failed");
    }

    // Delivery runs on its *own* loop, concurrently with processing: a non-web
    // connector (GitHub) must be able to post a slow turn's "still working" notice
    // *while* the turn is still running, which is impossible if deliver only ever
    // runs after the worker returns. The two loops only touch disjoint rows (the
    // worker writes streaming/sent statuses; delivery marks `remote_message_id`),
    // so WAL SQLite handles the concurrency.
    let connectors = Arc::new(connectors);
    let deliver_connectors = connectors.clone();
    let deliver_store = store.clone();
    tokio::spawn(async move {
        let deliver_interval_ms = cfg.swebot.chat.poll_interval_ms.max(100);
        let mut interval = tokio::time::interval(Duration::from_millis(deliver_interval_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            for connector in deliver_connectors.iter() {
                if let Err(e) = connector.deliver(&deliver_store).await {
                    tracing::warn!(connector = connector.name(), error = %e, "chat: connector deliver failed");
                }
            }
        }
    });

    let interval_ms = cfg.swebot.chat.poll_interval_ms.max(100);
    let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        for connector in connectors.iter() {
            if let Err(e) = connector.ingest(&store).await {
                tracing::warn!(connector = connector.name(), error = %e, "chat: connector ingest failed");
            }
        }
        if let Err(e) = process_cycle(&cfg, backend.as_ref(), tracker.as_ref(), &store).await {
            tracing::warn!(error = %e, "chat: processing cycle failed");
        }
    }
}

/// Refresh the shared clone (best-effort -- talk from whatever's checked out
/// otherwise) and answer every claimable pending user message.
async fn process_cycle(
    cfg: &EffectiveConfig,
    backend: &dyn AgentBackend,
    tracker: &dyn TrackerAdapter,
    store: &ChatStore,
) -> Result<(), String> {
    refresh_shared_clone(cfg).await;

    let limit = cfg.swebot.chat.max_concurrent_replies.max(1) as usize;
    let pending = store
        .pending_user_messages(limit)
        .map_err(|e| format!("pending query failed: {e}"))?;
    for msg in pending {
        store
            .set_message_status(msg.id, STATUS_PROCESSING)
            .map_err(|e| format!("claim failed: {e}"))?;
        match respond_to(cfg, backend, tracker, store, &msg).await {
            Ok(()) => {
                store
                    .set_message_status(msg.id, STATUS_PROCESSED)
                    .map_err(|e| format!("finalize failed: {e}"))?;
            }
            Err(e) => {
                tracing::warn!(message = msg.id, error = %e, "chat: failed to answer message");
                store
                    .set_message_status(msg.id, STATUS_FAILED)
                    .map_err(|err| format!("mark-failed: {err}"))?;
                // A final error notice -- NOT `notice-active` (or the web UI's typing
                // banner would read it as an in-flight turn and never clear), and NOT
                // `notice-done` (which the GitHub connector suppresses as delivered/
                // superseded): the user on a non-web connector must actually see it.
                if let Ok(notice_id) = store.insert_system_notice(
                    msg.conversation_id,
                    &format!("Sorry — I hit an error answering that: {e}"),
                ) {
                    let _ = store.set_message_status(notice_id, STATUS_NOTICE_ERROR);
                }
            }
        }
    }
    Ok(())
}

async fn refresh_shared_clone(cfg: &EffectiveConfig) {
    // Chat's own clone dir -- the one shared checkout all conversations ground their
    // answers on (PR review uses its own per-PR scratch clones, so nothing else
    // touches this directory).
    let dir = cfg.workspace_root.join(".swebot-chat-clone");
    let Some(repo) = cfg.swebot_repo_config() else {
        return;
    };
    if let Err(e) = crate::swebot::git::ensure_shared_clone(&repo, &dir).await {
        tracing::warn!(error = %e, "chat: shared clone refresh failed; answering from stale checkout");
    }
}

async fn respond_to(
    cfg: &EffectiveConfig,
    backend: &dyn AgentBackend,
    tracker: &dyn TrackerAdapter,
    store: &ChatStore,
    msg: &MessageRow,
) -> Result<(), String> {
    // A "create it" reply (when auto_create_issue is off) is handled as an action,
    // not another conversation turn.
    if is_confirmation(&msg.body) && create_from_pending_draft(cfg, tracker, store, msg).await? {
        return Ok(());
    }

    let clone_dir = cfg.workspace_root.join(".swebot-chat-clone");
    let history = store
        .messages_of_conversation(msg.conversation_id, 0)
        .map_err(|e| format!("history query failed: {e}"))?;
    let transcript = build_transcript(&history, cfg.swebot.chat.max_history_messages.max(1));
    let prompt = format!(
        "{PERSONA}\n\n{}\n\nConversation so far:\n{transcript}",
        unified_instructions(&clone_dir),
    );

    let mut session = backend
        .start_session(
            &clone_dir,
            &format!("chat-{}", msg.id),
            &format!("chat-{}", msg.id),
            None,
        )
        .await
        .map_err(|e| format!("session startup failed: {e}"))?;

    let assistant_id = store
        .insert_message(
            msg.conversation_id,
            ROLE_ASSISTANT,
            "",
            STATUS_STREAMING,
            &json!({}),
            Some(msg.id),
        )
        .map_err(|e| format!("failed to open assistant message: {e}"))?;

    let raw = run_turn_streaming(
        session.as_mut(),
        &prompt,
        store,
        msg.conversation_id,
        assistant_id,
        cfg.swebot.chat.first_text_deadline_ms,
    )
    .await;

    let raw = match raw {
        Ok(text) => text,
        Err(e) => {
            session.stop().await;
            let _ = store.set_message_status(assistant_id, STATUS_FAILED);
            return Err(e);
        }
    };
    session.stop().await;

    store
        .set_message_status(assistant_id, STATUS_SENT)
        .map_err(|e| e.to_string())?;

    // The streaming flusher only writes every ~150ms, so the final chunk may not have
    // been flushed cleanly -- persist the canonical final text now.
    store
        .set_message_body(assistant_id, &raw)
        .map_err(|e| e.to_string())?;

    // Drafting decision from the response's trailing JSON block, if present.
    if let Ok(parsed) = extract_json_block(&raw) {
        if let Some(ready) = parsed.get("ready").and_then(|r| r.as_bool()) {
            if ready {
                let title = parsed
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("Drafted ticket")
                    .to_string();
                let body = parsed
                    .get("body")
                    .and_then(|b| b.as_str())
                    .unwrap_or_default()
                    .to_string();
                return finish_draft(
                    cfg,
                    tracker,
                    store,
                    assistant_id,
                    &strip_json_block(&raw),
                    &title,
                    &body,
                )
                .await;
            }
            // Clarifying question / needs more info: make the actual question the
            // visible body (drop the JSON scaffold).
            if let Some(reply) = parsed.get("reply").and_then(|r| r.as_str())
                && !reply.trim().is_empty()
            {
                store
                    .set_message_body(assistant_id, reply.trim())
                    .map_err(|e| e.to_string())?;
            } else {
                store
                    .set_message_body(assistant_id, &strip_json_block(&raw))
                    .map_err(|e| e.to_string())?;
            }
        } else {
            store
                .set_message_body(assistant_id, &strip_json_block(&raw))
                .map_err(|e| e.to_string())?;
        }
    } else {
        store
            .set_message_body(assistant_id, &raw)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Record a ready-to-file draft: create the issue immediately (`auto_create_issue`)
/// or stash the draft in the assistant message's `meta` and ask the user to confirm.
/// (System notices were already resolved by the streaming turn's collector, so
/// there's nothing conversation-wide left to do here.)
async fn finish_draft(
    cfg: &EffectiveConfig,
    tracker: &dyn TrackerAdapter,
    store: &ChatStore,
    assistant_id: i64,
    reply_body: &str,
    title: &str,
    body: &str,
) -> Result<(), String> {
    if cfg.swebot.chat.auto_create_issue {
        let initial = cfg.active_states.first().ok_or_else(|| {
            "tracker.active_states is empty -- nowhere to create the drafted issue into".to_string()
        })?;
        let issue = tracker
            .create_issue(title, body, initial)
            .await
            .map_err(|e| format!("create_issue failed: {e}"))?;
        let ref_str = issue.url.as_deref().unwrap_or(&issue.identifier);
        let footer = format!(
            "\n\nDrafted and created: **{title}** ({ref_str}) — Symphony will pick it up on \
             its next poll."
        );
        store
            .set_message_body(assistant_id, &format!("{reply_body}{footer}"))
            .map_err(|e| e.to_string())?;
        store
            .set_message_meta(
                assistant_id,
                &json!({
                    "kind": "draft",
                    "ready": true,
                    "title": title,
                    "body": body,
                    "issue_url": issue.url,
                    "issue_id": issue.identifier,
                    "created": true,
                }),
            )
            .map_err(|e| e.to_string())?;
    } else {
        let footer = "\n\nI'm ready to file this ticket. Reply \"create it\" to confirm.";
        store
            .set_message_body(assistant_id, &format!("{reply_body}{footer}"))
            .map_err(|e| e.to_string())?;
        store
            .set_message_meta(
                assistant_id,
                &json!({
                    "kind": "draft",
                    "ready": true,
                    "title": title,
                    "body": body,
                    "created": false,
                }),
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// If there's an un-created draft waiting in this conversation and `msg` is a
/// confirmation, file it and post the confirmation reply. Returns whether the
/// message was consumed as a confirmation (so the caller skips a normal answer).
async fn create_from_pending_draft(
    cfg: &EffectiveConfig,
    tracker: &dyn TrackerAdapter,
    store: &ChatStore,
    msg: &MessageRow,
) -> Result<bool, String> {
    let Some((draft_msg_id, meta)) = store
        .pending_draft_for(msg.conversation_id)
        .map_err(|e| format!("draft lookup failed: {e}"))?
    else {
        return Ok(false);
    };
    let title = meta
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("Drafted ticket");
    let body = meta
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or_default();
    let initial = cfg.active_states.first().ok_or_else(|| {
        "tracker.active_states is empty -- nowhere to create the drafted issue".to_string()
    })?;
    let issue = tracker
        .create_issue(title, body, initial)
        .await
        .map_err(|e| format!("create_issue failed: {e}"))?;
    let ref_str = issue.url.as_deref().unwrap_or(&issue.identifier);
    let reply = format!(
        "Done — created **{title}** ({ref_str}). Symphony will pick it up on its next poll."
    );
    store
        .insert_message(
            msg.conversation_id,
            ROLE_ASSISTANT,
            &reply,
            STATUS_SENT,
            &json!({
                "kind": "draft-confirm",
                "issue_url": issue.url,
                "issue_id": issue.identifier,
                "created": true,
            }),
            Some(msg.id), // links the reply to the confirmation message for marker delivery
        )
        .map_err(|e| e.to_string())?;
    let mut new_meta = meta.clone();
    new_meta["created"] = json!(true);
    store
        .set_message_meta(draft_msg_id, &new_meta)
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// The words that confirm a pending draft. Keep narrow -- a confirmation is asked
/// for explicitly ("reply \"create it\" to confirm"), so only explicit accepts count.
/// Deliberately excludes bare generic acknowledgements like "yes"/"confirm"/"go
/// ahead": a pending draft stays un-created until the next assistant reply, so a
/// user's next short answer to some unrelated follow-up question (which could
/// easily be "yes") must not be mistaken for filing an old draft.
fn is_confirmation(body: &str) -> bool {
    let t = body.trim().to_lowercase().replace('.', "");
    matches!(
        t.as_str(),
        "create"
            | "create it"
            | "file it"
            | "file the ticket"
            | "yes, create it"
            | "yes create it"
            | "yes, file it"
    )
}

/// Run one turn, streaming text chunks into the store as they arrive, and return the
/// final text. The `<first_text_deadline_ms>` budget is the must-notify rule: if the
/// first text still hasn't arrived when it expires, insert the "still working"
/// system notice (resolved once the reply lands) instead of leaving the conversation
/// silent.
async fn run_turn_streaming(
    session: &mut dyn AgentSession,
    prompt: &str,
    store: &ChatStore,
    conversation_id: i64,
    assistant_id: i64,
    first_text_deadline_ms: u64,
) -> Result<String, String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let store2 = store.clone();
    let collector = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(first_text_deadline_ms);
        let mut notice_sent = false;
        let mut last_message: Option<String> = None;
        let mut last_flush = tokio::time::Instant::now() - Duration::from_secs(1);
        loop {
            let until_deadline: Option<tokio::time::Sleep> = if notice_sent {
                None
            } else {
                Some(tokio::time::sleep(
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                ))
            };
            let received = if let Some(sleep) = until_deadline {
                tokio::select! {
                    maybe = rx.recv() => maybe,
                    _ = sleep => {
                        let _ = store2.insert_system_notice(conversation_id, NOTICE_BODY);
                        notice_sent = true;
                        continue;
                    }
                }
            } else {
                rx.recv().await
            };
            match received {
                Some(event) => {
                    if let Some(text) = event.message
                        && !text.trim().is_empty()
                    {
                        last_message = Some(text.clone());
                        let now = tokio::time::Instant::now();
                        if now.duration_since(last_flush) >= Duration::from_millis(150) {
                            last_flush = now;
                            let _ = store2.set_message_body(assistant_id, &text);
                        }
                    }
                }
                None => break,
            }
        }
        if let Some(m) = &last_message {
            let _ = store2.set_message_body(assistant_id, m);
        }
        let _ = store2.resolve_system_notices(conversation_id);
        let _ = store2.set_message_status(assistant_id, STATUS_SENT);
        last_message
    });

    // Await the collector *before* inspecting the outcome: `run_turn` returning
    // early (timeout/spawn failure) drops its `tx`, which is the only thing that
    // ends the collector's recv loop, so awaiting here guarantees its final writes
    // (body flush, SENT status, notice resolution) have landed before the caller
    // decides what to do next -- otherwise the detached collector would race the
    // caller and could, e.g., flip a failed message back to `sent`.
    let outcome = session.run_turn(prompt, tx).await;
    let last_message = collector
        .await
        .map_err(|e| format!("chat streaming collector failed: {e}"))?;
    match outcome {
        Ok(TurnOutcome::Completed { .. }) => {
            last_message.ok_or_else(|| "turn completed with no text response".to_string())
        }
        Ok(TurnOutcome::Failed { reason }) => Err(reason),
        Err(e) => Err(e.to_string()),
    }
}

/// The shared instruction block for chat turns: one prompt covering both Q&A and
/// collaborative ticket drafting (the unified interface).
fn unified_instructions(clone_dir: &Path) -> String {
    format!(
        "You are chatting with a developer through a unified Q&A and ticket-drafting \
         assistant for this repo, checked out at {} (you have read access; \
         editing tools are disabled, so explore with Bash/Read freely and never edit).\n\
         Answer questions directly and concretely, citing specific files/lines; say \
         plainly when something isn't knowable from the code alone rather than guessing. \
         Keep answers tight -- this is a chat, not a report.\n\
         When the user asks you to draft or file a ticket, drive a short scoping dialogue: \
         ask only the clarifying questions that actually change scope (acceptance \
         criteria, edge cases, what's out of scope), then -- once you have enough to \
         write a properly scoped ticket -- END your response with exactly one fenced \
         ```json block:\n\
         {{\"ready\": true, \"title\": \"<ticket title>\", \"body\": \"<full ticket body: what, why, acceptance criteria>\"}}\n\
         While you're answering a question you don't need the JSON block at all -- plain \
         text only.",
        clone_dir.display(),
    )
}

/// The newest `max` non-empty messages as a prompt-ready transcript.
fn build_transcript(history: &[MessageRow], max: usize) -> String {
    let bounded: Vec<&MessageRow> = history
        .iter()
        .filter(|m| !m.body.trim().is_empty())
        .collect::<Vec<_>>();
    let start = bounded.len().saturating_sub(max);
    bounded[start..]
        .iter()
        .map(|m| {
            let role = match m.role.as_str() {
                ROLE_ASSISTANT => "swebot",
                ROLE_SYSTEM => "system",
                _ => "user",
            };
            truncate_body(&m.body)
                .lines()
                .map(|line| format!("{role}: {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Bound each message's size so a long pasted dump can't blow up the prompt.
fn truncate_body(body: &str) -> String {
    const MAX_CHARS: usize = 4_000;
    let s: String = body.chars().take(MAX_CHARS).collect();
    if s.chars().count() < body.chars().count() {
        format!("{s}\n…[truncated]")
    } else {
        s
    }
}

/// Cut a trailing fenced ```json … ``` block (and surrounding whitespace) out of a
/// turn's raw response so the JSON scaffold never shows up in the chat UI. A bare
/// JSON object (model skipped the fence) is left alone -- it's short and inline.
fn strip_json_block(text: &str) -> String {
    match text.rfind("```json") {
        Some(start) => text[..start].trim_end().to_string(),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::{ROLE_USER, STATUS_PENDING};
    use super::*;
    use crate::agent::{AgentBackend, AgentError, AgentSession, TurnOutcome};
    use crate::container::ContainerHandle;
    use crate::swebot::test_support::FakeBackend;
    use async_trait::async_trait;
    use std::path::Path;

    fn test_cfg(yaml_extra: &str) -> EffectiveConfig {
        unsafe {
            std::env::set_var("SYMPHONY_TEST_CHAT_TOKEN", "t");
        }
        let yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
            "tracker:\n  kind: local\n  active_states: [todo]\n  terminal_states: [done]\n\
             repo:\n  url: https://github.com/owner/name.git\n  \
             token: $SYMPHONY_TEST_CHAT_TOKEN\nswebot:\n  enabled: true\n  \
             chat:\n    enabled: true\n{yaml_extra}"
        ))
        .unwrap();
        crate::config::resolve(&yaml, std::path::Path::new(".")).unwrap()
    }

    fn test_tracker(dir: &Path) -> Box<dyn TrackerAdapter> {
        let provider: serde_yaml::Value = serde_yaml::from_str(&format!(
            "dir: {}",
            dir.to_str().unwrap().replace('\\', "/")
        ))
        .unwrap();
        crate::tracker::build("local", &provider, std::path::Path::new(".")).unwrap()
    }

    fn chat_store() -> (ChatStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = ChatStore::open(tmp.path().join("chat.db")).unwrap();
        (store, tmp)
    }

    /// A store with one conversation holding a single pending user message. The
    /// tempdir is returned alongside so the SQLite file stays alive for the test.
    fn seeded(conversation_body: &str) -> (ChatStore, tempfile::TempDir) {
        let (store, tmp) = chat_store();
        let conv = store
            .create_conversation("test", None, "u", "t")
            .expect("conv");
        store
            .insert_message(
                conv,
                ROLE_USER,
                conversation_body,
                STATUS_PENDING,
                &json!({}),
                None,
            )
            .expect("msg");
        (store, tmp)
    }

    fn one_pending(store: &ChatStore) -> MessageRow {
        store.pending_user_messages(10).unwrap().remove(0)
    }

    fn last_assistant(store: &ChatStore, conv: i64) -> MessageRow {
        store
            .messages_of_conversation(conv, 0)
            .unwrap()
            .into_iter()
            .rev()
            .find(|m| m.role == ROLE_ASSISTANT)
            .expect("an assistant message")
    }

    #[tokio::test]
    async fn plain_answer_posts_the_text_as_a_sent_assistant_message() {
        let cfg = test_cfg("");
        let (store, _tmp) = seeded("how does auth work?");
        let tracker_dir = tempfile::tempdir().unwrap();
        let tracker = test_tracker(tracker_dir.path());
        let backend = FakeBackend::with_response("Auth uses OAuth, see src/auth.rs.");
        let msg = one_pending(&store);
        respond_to(&cfg, &backend, tracker.as_ref(), &store, &msg)
            .await
            .unwrap();

        let assistant = last_assistant(&store, msg.conversation_id);
        assert_eq!(assistant.body, "Auth uses OAuth, see src/auth.rs.");
        assert_eq!(assistant.status, STATUS_SENT);
        assert_eq!(assistant.reply_to, Some(msg.id));
    }

    #[tokio::test]
    async fn ready_draft_auto_creates_an_issue_and_appends_the_footer() {
        let cfg = test_cfg("");
        let (store, _tmp) = seeded("draft a ticket for rate limiting");
        let tracker_dir = tempfile::tempdir().unwrap();
        let tracker = test_tracker(tracker_dir.path());
        let backend = FakeBackend::with_response(
            "Here's a scoped ticket.\n\n```json\n{\"ready\": true, \"title\": \"Add rate limiting\", \"body\": \"Why: abuse. What: ...\"}\n```\n",
        );
        let msg = one_pending(&store);
        respond_to(&cfg, &backend, tracker.as_ref(), &store, &msg)
            .await
            .unwrap();

        let assistant = last_assistant(&store, msg.conversation_id);
        assert!(assistant.body.contains("Drafted and created"));
        assert!(!assistant.body.contains("```json"));
        assert_eq!(assistant.meta["created"], true);
        assert_eq!(assistant.meta["title"], "Add rate limiting");
        // The issue really landed in the local tracker dir.
        assert_eq!(std::fs::read_dir(tracker_dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn clarifying_question_posts_the_reply_and_creates_no_issue() {
        let cfg = test_cfg("");
        let (store, _tmp) = seeded("draft a ticket for rate limiting");
        let tracker_dir = tempfile::tempdir().unwrap();
        let tracker = test_tracker(tracker_dir.path());
        let backend = FakeBackend::with_response(
            "A scope question first.\n\n```json\n{\"ready\": false, \"reply\": \"Does rate limiting apply per-user or globally?\"}\n```\n",
        );
        let msg = one_pending(&store);
        respond_to(&cfg, &backend, tracker.as_ref(), &store, &msg)
            .await
            .unwrap();

        let assistant = last_assistant(&store, msg.conversation_id);
        assert!(assistant.body.contains("per-user or globally"));
        assert!(!assistant.body.contains("```json"));
        assert_eq!(std::fs::read_dir(tracker_dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_confirmation_files_the_stashed_draft_without_running_a_turn() {
        let cfg = test_cfg("    auto_create_issue: false\n");
        let (store, _tmp) = seeded("draft a ticket for rate limiting");
        let tracker_dir = tempfile::tempdir().unwrap();
        let tracker = test_tracker(tracker_dir.path());
        let backend = FakeBackend::with_response(
            "Draft:\n\n```json\n{\"ready\": true, \"title\": \"Add rate limiting\", \"body\": \"Why: abuse.\"}\n```\n",
        );
        let msg = one_pending(&store);
        respond_to(&cfg, &backend, tracker.as_ref(), &store, &msg)
            .await
            .unwrap();
        store.set_message_status(msg.id, STATUS_PROCESSED).unwrap();
        // No issue yet -- just a stashed draft awaiting confirmation.
        assert_eq!(std::fs::read_dir(tracker_dir.path()).unwrap().count(), 0);

        // The user confirms: the draft is filed, without another model turn.
        let confirm_id = store
            .insert_message(
                msg.conversation_id,
                ROLE_USER,
                "create it",
                STATUS_PENDING,
                &json!({}),
                None,
            )
            .unwrap();
        let confirm_msg = store
            .pending_user_messages(10)
            .unwrap()
            .iter()
            .find(|m| m.id == confirm_id)
            .unwrap()
            .clone();
        respond_to(&cfg, &backend, tracker.as_ref(), &store, &confirm_msg)
            .await
            .unwrap();

        assert_eq!(std::fs::read_dir(tracker_dir.path()).unwrap().count(), 1);
        assert!(
            store
                .pending_draft_for(msg.conversation_id)
                .unwrap()
                .is_none()
        );
        let msgs = store
            .messages_of_conversation(msg.conversation_id, 0)
            .unwrap();
        assert!(
            msgs.iter()
                .any(|m| m.role == ROLE_ASSISTANT && m.body.contains("Done — created"))
        );
    }

    /// A backend whose turn stays silent past the notice deadline, then streams its
    /// text -- exercises the must-notify rule for real.
    struct SlowBackend {
        first_text_delay_ms: u64,
        body: String,
    }

    #[async_trait]
    impl AgentBackend for SlowBackend {
        async fn start_session(
            &self,
            _workspace: &Path,
            _issue_id: &str,
            _title: &str,
            _container: Option<&ContainerHandle>,
        ) -> Result<Box<dyn AgentSession>, AgentError> {
            Ok(Box::new(SlowSession {
                delay_ms: self.first_text_delay_ms,
                body: self.body.clone(),
            }))
        }
    }

    struct SlowSession {
        delay_ms: u64,
        body: String,
    }

    #[async_trait]
    impl AgentSession for SlowSession {
        fn session_id(&self) -> &str {
            "slow"
        }

        async fn run_turn(
            &mut self,
            _prompt: &str,
            events: mpsc::UnboundedSender<AgentEvent>,
        ) -> Result<TurnOutcome, AgentError> {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            let _ = events.send(AgentEvent::new("notification").with_message(self.body.clone()));
            Ok(TurnOutcome::Completed { usage: None })
        }

        async fn stop(self: Box<Self>) {}
    }

    #[tokio::test]
    async fn a_slow_turn_posts_the_still_working_notice_before_its_first_text() {
        let cfg = test_cfg("    first_text_deadline_ms: 100\n");
        let (store, _tmp) = seeded("deep question");
        let tracker_dir = tempfile::tempdir().unwrap();
        let tracker = test_tracker(tracker_dir.path());
        let backend = SlowBackend {
            first_text_delay_ms: 400,
            body: "finally".to_string(),
        };
        let msg = one_pending(&store);
        respond_to(&cfg, &backend, tracker.as_ref(), &store, &msg)
            .await
            .unwrap();

        let msgs = store
            .messages_of_conversation(msg.conversation_id, 0)
            .unwrap();
        let notice = msgs.iter().find(|m| m.role == ROLE_SYSTEM).unwrap();
        assert_eq!(notice.body, NOTICE_BODY);
        let assistant = msgs.iter().find(|m| m.role == ROLE_ASSISTANT).unwrap();
        assert_eq!(assistant.body, "finally");
        assert_eq!(assistant.status, STATUS_SENT);
    }

    #[test]
    fn strip_json_block_removes_only_the_trailing_fence() {
        let raw = "Here's the plan.\n\n```json\n{\"ready\": true, \"title\": \"t\"}\n```\n";
        assert_eq!(strip_json_block(raw), "Here's the plan.");
        assert_eq!(strip_json_block("no json here"), "no json here");
    }

    #[test]
    fn is_confirmation_accepts_only_the_expected_phrases() {
        assert!(is_confirmation("create it"));
        assert!(is_confirmation(" Yes, create it."));
        assert!(!is_confirmation("create a ticket about X"));
        assert!(!is_confirmation("what is the auth flow?"));
    }

    #[test]
    fn is_confirmation_rejects_bare_generic_acknowledgements() {
        // "yes"/"confirm"/"go ahead" alone are too generic: a user's short reply to
        // some unrelated follow-up question must not be mistaken for confirming a
        // still-pending draft from earlier in the conversation.
        assert!(!is_confirmation("yes"));
        assert!(!is_confirmation("confirm"));
        assert!(!is_confirmation("go ahead"));
    }
}
