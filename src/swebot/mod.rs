//! SweBot: answers questions and drafts tickets in GitHub Discussions, and reviews
//! the pull requests Symphony's coding agents open. GitHub-specific for v1 (see
//! `config::SwebotConfig`'s doc comment for why), keyed off `repo:` rather than the
//! tracker so it works the same regardless of `tracker.kind`.
//!
//! Runs as an additional task inside the *same* orchestrator process (spawned from
//! `orchestrator::run` when `cfg.swebot.enabled`), sharing its polling cadence and the
//! daemon's existing container/volume/restart lifecycle -- not a second daemon to
//! deploy and keep alive.
//!
//! All three capabilities are read-only with respect to the repo's own code: SweBot's
//! sessions run with `Edit`/`Write`/`NotebookEdit` disallowed (see `restricted_backend`)
//! -- it answers, drafts, and reviews, but it never edits the repo directly. That
//! stays the coding agent's job, gated by the normal ticket-dispatch flow.

pub mod drafting;
mod git;
pub mod qa;
pub mod review;

use crate::agent::claude::ClaudeBackend;
use crate::agent::{AgentEvent, AgentSession, TurnOutcome};
use crate::config::EffectiveConfig;
use crate::repo_host::{DiscussionThread, GithubRepoHost};
use crate::tracker::TrackerAdapter;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// `<!-- swebot:answered:<comment-id> -->`, embedded in every SweBot reply. Comment
/// ids only ever increase, so "the max marker value found anywhere in the thread"
/// is "the last comment SweBot has already responded to" -- no local persistence,
/// no need to know which GitHub account SweBot itself posts as.
fn answered_marker(comment_id: u64) -> String {
    format!("<!-- swebot:answered:{comment_id} -->")
}

/// Whether `body` is SweBot's own reply (carries an `answered_marker`) rather than a
/// human comment. Matters because SweBot's own reply necessarily gets a *higher*
/// `database_id` than the marker value it embeds -- without excluding it here,
/// `qa`/`drafting`'s "find the newest comment past `last_answered_id`" scan would
/// treat SweBot's own just-posted reply as a fresh unanswered question on the very
/// next poll, an infinite reply-to-itself loop.
fn is_swebot_reply(body: &str) -> bool {
    body.contains("<!-- swebot:answered:")
}

/// See `answered_marker`. `0` reads as "nothing answered yet" -- every real GitHub
/// comment id is positive.
fn last_answered_id(thread: &DiscussionThread) -> u64 {
    thread
        .comments
        .iter()
        .filter_map(|c| {
            c.body
                .split("<!-- swebot:answered:")
                .nth(1)
                .and_then(|rest| rest.split(" -->").next())
                .and_then(|id| id.trim().parse::<u64>().ok())
        })
        .max()
        .unwrap_or(0)
}

/// Shared tone/rubric prefix for every SweBot prompt. The user asked for SweBot to be
/// consistently friendly *and* competent -- approachable, but holding a genuinely
/// high bar (especially on security and performance), not a rubber stamp. One
/// constant, not copy-pasted per capability; `qa`/`drafting`/`review` each append
/// their own task-specific instructions after this.
pub const PERSONA: &str = "You are SweBot, this project's software-engineering assistant. \
Be warm and direct: explain *why*, not just *what*, and never be condescending or \
sycophantic. Hold a genuinely high bar, especially on security and performance -- \
answering, drafting, or approving something is a real signal, not a formality.";

const DISALLOWED_TOOLS: &str = "Edit,Write,NotebookEdit";

/// A `ClaudeBackend` configured the same way ticket dispatch's is (same command,
/// model, timeout) but with file-mutating tools explicitly disallowed and no MCP
/// tool wiring -- SweBot's drivers parse each turn's text/JSON response themselves
/// and act via `GithubRepoHost`/`TrackerAdapter` directly, rather than routing
/// through an agent-invoked tool call the way ticket dispatch's `update_issue_state`
/// does (that plumbing exists for a long tool-using coding session; SweBot's turns
/// are single-shot conversational exchanges, so parsing the final response is enough).
fn restricted_backend(cfg: &EffectiveConfig) -> ClaudeBackend {
    let mut extra_args = cfg.claude.args.clone();
    extra_args.push("--disallowedTools".to_string());
    extra_args.push(DISALLOWED_TOOLS.to_string());
    ClaudeBackend {
        command: cfg.claude.command.clone(),
        extra_args,
        model: cfg.claude.model.clone(),
        permission_mode: cfg.claude.permission_mode.clone(),
        turn_timeout_ms: cfg.claude.turn_timeout_ms,
        mcp_wiring: None,
        workflow_dir: cfg.workflow_dir.clone(),
    }
}

/// Run one turn to completion and return its final text response -- SweBot's drivers
/// just need the answer, not the live per-event progress reporting the ticket
/// dispatch's status dashboard cares about.
async fn run_turn_collect_text(
    session: &mut dyn AgentSession,
    prompt: &str,
) -> Result<String, String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let collector = tokio::spawn(async move {
        let mut last_message = None;
        while let Some(event) = rx.recv().await {
            if let Some(msg) = event.message {
                last_message = Some(msg);
            }
        }
        last_message
    });
    let outcome = session
        .run_turn(prompt, tx)
        .await
        .map_err(|e| e.to_string())?;
    let last_message = collector.await.map_err(|e| e.to_string())?;
    match outcome {
        TurnOutcome::Completed { .. } => {
            last_message.ok_or_else(|| "turn completed with no text response".to_string())
        }
        TurnOutcome::Failed { reason } => Err(reason),
    }
}

/// Parse a fenced ```json ... ``` block (or a bare JSON object, if the model skipped
/// the fence) out of a turn's free-text response. `qa`/`drafting`/`review` each ask
/// for a specific shape at the end of their prompt; this just locates and parses it,
/// independent of what shape the caller expects.
fn extract_json_block(text: &str) -> Result<serde_json::Value, String> {
    let candidate = if let Some(start) = text.rfind("```json") {
        let after = &text[start + "```json".len()..];
        let end = after.find("```").ok_or("unterminated ```json block")?;
        after[..end].trim()
    } else {
        text.trim()
    };
    serde_json::from_str(candidate)
        .map_err(|e| format!("could not parse a JSON object from the response: {e}"))
}

/// Entry point spawned by `orchestrator::run` when `cfg.swebot.enabled`. Loops
/// forever at the same cadence as ticket dispatch (`polling.interval_ms`); each
/// capability's own poll failure is logged and doesn't stop the other two or the
/// next cycle.
pub async fn run(cfg: EffectiveConfig, tracker: Arc<dyn TrackerAdapter>) {
    let Some(repo) = cfg.repo.clone() else {
        tracing::error!(
            "swebot.enabled but no repo: block resolved -- config::resolve should have \
             rejected this already"
        );
        return;
    };
    let host = match GithubRepoHost::new(&repo) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "swebot: failed to build GithubRepoHost, not starting");
            return;
        }
    };
    let backend = restricted_backend(&cfg);
    let shared_clone_dir = cfg.workspace_root.join(".swebot-shared-clone");
    let interval = Duration::from_millis(cfg.poll_interval_ms);

    tracing::info!("swebot starting");
    loop {
        if let Err(e) = git::ensure_shared_clone(&repo, &shared_clone_dir).await {
            tracing::warn!(error = %e, "swebot: failed to refresh shared clone; Q&A/drafting will retry next cycle");
        } else {
            if let Err(e) = qa::poll_once(&cfg, &host, &backend, &shared_clone_dir).await {
                tracing::warn!(error = %e, "swebot: Q&A poll failed");
            }
            if let Err(e) =
                drafting::poll_once(&cfg, &host, &backend, &shared_clone_dir, tracker.as_ref())
                    .await
            {
                tracing::warn!(error = %e, "swebot: drafting poll failed");
            }
        }
        if cfg.swebot.review_enabled
            && let Err(e) = review::poll_once(&cfg, &host, &backend, tracker.as_ref()).await
        {
            tracing::warn!(error = %e, "swebot: review poll failed");
        }
        tokio::time::sleep(interval).await;
    }
}

/// A canned-response `AgentBackend`/`AgentSession` pair for exercising `qa`/
/// `drafting`/`review`'s own orchestration logic (skip-vs-act decisions, marker
/// construction, verdict routing) without spawning a real `claude` process.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::agent::{AgentBackend, AgentError, AgentEvent, AgentSession, TurnOutcome};
    use crate::container::ContainerHandle;
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    pub struct FakeBackend {
        pub response: String,
        /// Every prompt a session built from this backend was asked to run, in
        /// order -- lets a test assert on prompt *content* (e.g. "did the review
        /// prompt actually include the diff") without over-specifying the whole
        /// string. `Arc` so each spawned `FakeSession` can append to the same log
        /// the backend itself hands back to the test.
        pub prompts_seen: Arc<Mutex<Vec<String>>>,
    }

    impl FakeBackend {
        pub fn with_response(response: impl Into<String>) -> Self {
            Self {
                response: response.into(),
                prompts_seen: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl AgentBackend for FakeBackend {
        async fn start_session(
            &self,
            _workspace: &Path,
            _issue_id: &str,
            _title: &str,
            _container: Option<&ContainerHandle>,
        ) -> Result<Box<dyn AgentSession>, AgentError> {
            Ok(Box::new(FakeSession {
                response: self.response.clone(),
                prompts_seen: self.prompts_seen.clone(),
            }))
        }
    }

    struct FakeSession {
        response: String,
        prompts_seen: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl AgentSession for FakeSession {
        fn session_id(&self) -> &str {
            "fake-session"
        }

        async fn run_turn(
            &mut self,
            prompt: &str,
            events: mpsc::UnboundedSender<AgentEvent>,
        ) -> Result<TurnOutcome, AgentError> {
            self.prompts_seen.lock().unwrap().push(prompt.to_string());
            let _ =
                events.send(AgentEvent::new("notification").with_message(self.response.clone()));
            Ok(TurnOutcome::Completed { usage: None })
        }

        async fn stop(self: Box<Self>) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_answered_id_finds_the_highest_marker_across_any_comment() {
        let thread = DiscussionThread {
            id: "D_1".to_string(),
            number: 1,
            title: "t".to_string(),
            body: "b".to_string(),
            comments: vec![
                crate::repo_host::DiscussionComment {
                    database_id: 10,
                    body: "a human question".to_string(),
                    author_login: Some("alice".to_string()),
                },
                crate::repo_host::DiscussionComment {
                    database_id: 11,
                    body: format!("{}\nhere's the answer", answered_marker(10)),
                    author_login: Some("swebot".to_string()),
                },
            ],
        };
        assert_eq!(last_answered_id(&thread), 10);
    }

    #[test]
    fn last_answered_id_is_zero_for_a_thread_with_no_swebot_reply_yet() {
        let thread = DiscussionThread {
            id: "D_2".to_string(),
            number: 2,
            title: "t".to_string(),
            body: "b".to_string(),
            comments: vec![crate::repo_host::DiscussionComment {
                database_id: 5,
                body: "first question, unanswered".to_string(),
                author_login: Some("alice".to_string()),
            }],
        };
        assert_eq!(last_answered_id(&thread), 0);
    }

    #[test]
    fn extract_json_block_parses_a_fenced_block() {
        let text = "Here's my answer.\n\n```json\n{\"ready\": true, \"title\": \"x\"}\n```\n";
        let v = extract_json_block(text).unwrap();
        assert_eq!(v["ready"], serde_json::json!(true));
        assert_eq!(v["title"], serde_json::json!("x"));
    }

    #[test]
    fn extract_json_block_falls_back_to_bare_json() {
        let text = "{\"verdict\": \"approve\", \"summary\": \"looks good\"}";
        let v = extract_json_block(text).unwrap();
        assert_eq!(v["verdict"], serde_json::json!("approve"));
    }

    #[test]
    fn extract_json_block_errors_clearly_on_malformed_output() {
        let err = extract_json_block("just some prose, no JSON at all").unwrap_err();
        assert!(err.contains("could not parse"));
    }
}
