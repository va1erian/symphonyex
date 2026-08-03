//! SweBot: answers questions and drafts tickets, and reviews the pull/merge requests
//! Symphony's coding agents open. Works against either GitHub or GitLab (see
//! `config::SwebotConfig`'s doc comment for how the Q&A/drafting conversational
//! surface differs per provider), keyed off `repo:` rather than the tracker so it
//! works the same regardless of `tracker.kind`.
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
use crate::config::{EffectiveConfig, RepoProvider};
use crate::repo_host::{self, DiscussionHost, DiscussionThread, RepoHost};
use crate::tracker::TrackerAdapter;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Which conversational surface a given run talks to for Q&A/drafting: either the
/// real code host's own Discussions/labels, or (when `swebot.board.enabled`)
/// `crate::board`'s local bulletin board instead -- see
/// `config::SwebotConfig::board_enabled`'s doc comment for why a project would pick
/// the latter. PR/MR review always goes through the real `RepoHost` regardless (see
/// `run` below) -- a merge request is inherently a property of the actual code host,
/// not something the board has any notion of.
enum ConversationHost {
    Board(crate::board::BulletinBoardHost),
    Repo(Arc<dyn RepoHost>),
}

#[async_trait]
impl DiscussionHost for ConversationHost {
    async fn list_swebot_threads(&self, selector: &str) -> Result<Vec<DiscussionThread>, String> {
        match self {
            ConversationHost::Board(b) => b.list_swebot_threads(selector).await,
            ConversationHost::Repo(r) => r.list_swebot_threads(selector).await,
        }
    }

    async fn post_discussion_comment(&self, thread_id: &str, body: &str) -> Result<String, String> {
        match self {
            ConversationHost::Board(b) => b.post_discussion_comment(thread_id, body).await,
            ConversationHost::Repo(r) => r.post_discussion_comment(thread_id, body).await,
        }
    }

    async fn mark_discussion_comment_as_answer(&self, comment_id: &str) -> Result<(), String> {
        match self {
            ConversationHost::Board(b) => b.mark_discussion_comment_as_answer(comment_id).await,
            ConversationHost::Repo(r) => r.mark_discussion_comment_as_answer(comment_id).await,
        }
    }

    async fn close_thread(&self, thread_id: &str) -> Result<(), String> {
        match self {
            ConversationHost::Board(b) => b.close_thread(thread_id).await,
            ConversationHost::Repo(r) => r.close_thread(thread_id).await,
        }
    }
}

/// `<!-- swebot:answered:<id> -->`, embedded in every SweBot reply. `id` is a real
/// comment `database_id` in the normal case, or the reserved sentinel `0` when
/// SweBot answered the discussion's own *opening body* -- GitHub Discussions store
/// the opening question in `body`, not as a `comment`, so a fresh discussion with
/// zero replies has an empty `comments` list and there's nothing else to key a
/// marker off until a real comment exists. `0` is safe as a sentinel: real GitHub
/// comment ids are always positive.
fn answered_marker(id: u64) -> String {
    format!("<!-- swebot:answered:{id} -->")
}

/// Whether `body` is SweBot's own reply (carries an `answered_marker`) rather than a
/// human comment. Matters because SweBot's own reply necessarily gets a *higher*
/// `database_id` than the marker value it embeds -- without excluding it, the "find
/// the newest comment past the last marker" scan below would treat SweBot's own
/// just-posted reply as a fresh unanswered question on the very next poll, an
/// infinite reply-to-itself loop.
fn is_swebot_reply(body: &str) -> bool {
    body.contains("<!-- swebot:answered:")
}

/// The highest `answered_marker` id found anywhere in the thread, or `None` if
/// SweBot has never answered anything in it yet -- not even the opening body.
/// Distinguishing `None` from `Some(0)` matters: `Some(0)` means "the body itself
/// was already answered, don't answer it again," not "nothing has happened yet."
fn last_answered_marker(thread: &DiscussionThread) -> Option<u64> {
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
}

/// The marker id of the next thing in `thread` SweBot should respond to, if
/// anything: a real (non-SweBot) comment past the last answered marker, or -- if
/// nothing has ever been answered and no qualifying comment exists yet -- the
/// sentinel `0` for the discussion's own opening body. `None` means there's nothing
/// new since SweBot's last reply.
fn next_to_answer(thread: &DiscussionThread) -> Option<u64> {
    let last = last_answered_marker(thread);
    if let Some(c) = thread
        .comments
        .iter()
        .filter(|c| !is_swebot_reply(&c.body) && last.is_none_or(|l| c.database_id > l))
        .max_by_key(|c| c.database_id)
    {
        return Some(c.database_id);
    }
    if last.is_none() {
        return Some(0);
    }
    None
}

/// Everything in `thread` up to and including `marker_id`, as one prompt-ready
/// block: the opening body is always included first (it's the original question and
/// always relevant context, even once real comments exist), followed by comments
/// with `database_id <= marker_id` (naturally none, for the `marker_id == 0`
/// body-only case, since real comment ids are always positive).
fn transcript_up_to(thread: &DiscussionThread, marker_id: u64) -> String {
    let mut parts = vec![format!("(opening post) {}", thread.body)];
    parts.extend(
        thread
            .comments
            .iter()
            .filter(|c| c.database_id <= marker_id)
            .map(|c| {
                format!(
                    "{}: {}",
                    c.author_login.as_deref().unwrap_or("someone"),
                    c.body
                )
            }),
    );
    parts.join("\n\n")
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
/// and act via `RepoHost`/`TrackerAdapter` directly, rather than routing
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
    let candidate = match text.rfind("```json") {
        Some(start) => text[start + "```json".len()..].trim_start(),
        None => text.trim(),
    };
    // Deliberately *not* string-searching for a closing ``` fence: a drafted ticket
    // or review summary can easily contain a markdown code block of its own inside
    // a JSON string value (a real response hit this live -- a ticket body quoting a
    // code snippet), and the naive "find the next ```" approach truncates the JSON
    // right there, mid-string. A streaming deserializer parses exactly one JSON
    // value and stops the moment it's structurally complete, ignoring whatever
    // trailing text follows (a closing fence, more prose, anything) -- so an
    // embedded ``` inside a properly JSON-escaped string is never mistaken for the
    // end of the block.
    let mut de = serde_json::Deserializer::from_str(candidate);
    serde::Deserialize::deserialize(&mut de)
        .map_err(|e| format!("could not parse a JSON object from the response: {e}"))
}

/// Entry point spawned by `orchestrator::run` when `cfg.swebot.enabled`. Loops
/// forever at the same cadence as ticket dispatch (`polling.interval_ms`); each
/// capability's own poll failure is logged and doesn't stop the other two or the
/// next cycle.
pub async fn run(cfg: EffectiveConfig, tracker: Arc<dyn TrackerAdapter>) {
    // SweBot posts (Q&A, drafting, review) authenticate as `swebot.token` when set --
    // a separate GitHub identity from `repo.token` (the coding agent's own, used for
    // branch pushes and `open_pull_request`). Falls back to `repo.token` when
    // `swebot.token` isn't configured, matching the old single-identity behavior. See
    // `config::EffectiveConfig::swebot_repo_config`'s doc comment for why this split
    // exists: GitHub rejects a PR review from the same account that authored the PR.
    let Some(swebot_repo) = cfg.swebot_repo_config() else {
        tracing::error!(
            "swebot.enabled but no repo: block resolved -- config::resolve should have \
             rejected this already"
        );
        return;
    };
    let host: Arc<dyn RepoHost> = match repo_host::build(&swebot_repo) {
        Ok(h) => Arc::from(h),
        Err(e) => {
            tracing::error!(error = %e, "swebot: failed to build repo host, not starting");
            return;
        }
    };
    // Q&A/drafting poll a `ConversationHost` -- the real host's own Discussions/
    // labels, or `crate::board`'s local bulletin board when `swebot.board.enabled`
    // (see `config::SwebotConfig::board_enabled`'s doc comment). PR/MR review always
    // uses `host` directly below, regardless of this choice.
    let (conversation_host, qa_selector, drafting_selector) = if cfg.swebot.board_enabled {
        // Must point at the same directory `status::router` mounts `crate::board`'s
        // own web UI against (`workflow_dir`, not `workspace_root`) -- both read and
        // write the same `board.db`, so a human replying via the UI and SweBot
        // replying via this poll loop see each other's posts.
        let board_host = crate::board::BulletinBoardHost::new(cfg.workflow_dir.clone(), "/board");
        (
            ConversationHost::Board(board_host),
            cfg.swebot.qa_discussion_category.clone(),
            cfg.swebot.drafting_discussion_category.clone(),
        )
    } else {
        let (qa, drafting) = match host.provider_kind() {
            RepoProvider::Github => (
                cfg.swebot.qa_discussion_category.clone(),
                cfg.swebot.drafting_discussion_category.clone(),
            ),
            RepoProvider::Gitlab => (
                cfg.swebot.qa_label.clone(),
                cfg.swebot.drafting_label.clone(),
            ),
        };
        (ConversationHost::Repo(host.clone()), qa, drafting)
    };
    let backend = restricted_backend(&cfg);
    let shared_clone_dir = cfg.workspace_root.join(".swebot-shared-clone");
    let interval = Duration::from_millis(cfg.poll_interval_ms);

    tracing::info!("swebot starting");
    loop {
        if let Err(e) = git::ensure_shared_clone(&swebot_repo, &shared_clone_dir).await {
            tracing::warn!(error = %e, "swebot: failed to refresh shared clone; Q&A/drafting will retry next cycle");
        } else {
            if let Err(e) = qa::poll_once(
                &conversation_host,
                &qa_selector,
                &backend,
                &shared_clone_dir,
            )
            .await
            {
                tracing::warn!(error = %e, "swebot: Q&A poll failed");
            }
            if let Err(e) = drafting::poll_once(
                &cfg,
                &conversation_host,
                &drafting_selector,
                &backend,
                &shared_clone_dir,
                tracker.as_ref(),
            )
            .await
            {
                tracing::warn!(error = %e, "swebot: drafting poll failed");
            }
        }
        if cfg.swebot.review_enabled
            && let Err(e) = review::poll_once(&cfg, host.as_ref(), &backend, tracker.as_ref()).await
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

    fn comment(database_id: u64, body: &str, author: &str) -> crate::repo_host::DiscussionComment {
        crate::repo_host::DiscussionComment {
            database_id,
            body: body.to_string(),
            author_login: Some(author.to_string()),
        }
    }

    fn thread(body: &str, comments: Vec<crate::repo_host::DiscussionComment>) -> DiscussionThread {
        DiscussionThread {
            id: "D_1".to_string(),
            number: 1,
            title: "t".to_string(),
            body: body.to_string(),
            url: "https://github.com/owner/name/discussions/1".to_string(),
            comments,
        }
    }

    #[test]
    fn last_answered_marker_finds_the_highest_marker_across_any_comment() {
        let t = thread(
            "b",
            vec![
                comment(10, "a human question", "alice"),
                comment(
                    11,
                    &format!("{}\nhere's the answer", answered_marker(10)),
                    "swebot",
                ),
            ],
        );
        assert_eq!(last_answered_marker(&t), Some(10));
    }

    #[test]
    fn last_answered_marker_is_none_for_a_thread_with_no_swebot_reply_yet() {
        let t = thread("b", vec![comment(5, "first question, unanswered", "alice")]);
        assert_eq!(last_answered_marker(&t), None);
    }

    /// Regression test for a real bug found running this live: GitHub Discussions
    /// store the opening question in `body`, not as a `comment` -- a fresh
    /// discussion with zero replies has an *empty* `comments` list, so the original
    /// "only ever look at comments" logic found nothing to answer and silently did
    /// nothing on every poll.
    #[test]
    fn next_to_answer_answers_the_opening_body_when_there_are_no_comments_yet() {
        let t = thread("how does the dedup logic work?", vec![]);
        assert_eq!(next_to_answer(&t), Some(0));
        assert!(transcript_up_to(&t, 0).contains("how does the dedup logic work?"));
    }

    #[test]
    fn next_to_answer_does_not_re_answer_the_body_once_marked() {
        let t = thread(
            "how does the dedup logic work?",
            vec![comment(
                100,
                &format!("{}\nSee src/dedup.rs", answered_marker(0)),
                "swebot",
            )],
        );
        assert_eq!(next_to_answer(&t), None);
    }

    #[test]
    fn next_to_answer_picks_up_a_real_reply_after_the_body_was_already_answered() {
        let t = thread(
            "how does the dedup logic work?",
            vec![
                comment(
                    100,
                    &format!("{}\nSee src/dedup.rs", answered_marker(0)),
                    "swebot",
                ),
                comment(101, "thanks, but what about videos specifically?", "alice"),
            ],
        );
        assert_eq!(next_to_answer(&t), Some(101));
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

    /// Regression test for a real bug found running this live: a drafted ticket body
    /// containing its own markdown code fence (e.g. quoting a code snippet) made the
    /// old "find the next ```" logic truncate the JSON mid-string, right at that
    /// *inner* fence rather than the real closing one.
    #[test]
    fn extract_json_block_handles_an_embedded_code_fence_inside_a_string_value() {
        let text = "Some analysis.\n\n```json\n{\"ready\": true, \"title\": \"x\", \
                     \"body\": \"See:\\n\\n```rust\\nfn f() {}\\n```\\n\\nDone.\"}\n```\n";
        let v = extract_json_block(text).unwrap();
        assert_eq!(v["ready"], serde_json::json!(true));
        assert!(v["body"].as_str().unwrap().contains("fn f() {}"));
    }
}
