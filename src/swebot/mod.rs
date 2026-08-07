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
//! All capabilities are read-only with respect to the repo's own code: every SweBot
//! session is started under `ToolPolicy::SWEBOT` (AIR-2, see `agent::ToolPolicy`'s doc
//! comment) -- file-mutating tools disallowed, translated into `--disallowedTools` for
//! `claude` and an `{"edit":"deny"}` permission config for `opencode` -- so SweBot
//! answers, drafts, and reviews, but never edits the repo directly. That stays the
//! coding agent's job, gated by the normal ticket-dispatch flow. The backend SweBot
//! runs on is `swebot.backend` when set, else `agent.backend`; `codex` is unsupported
//! and refused rather than run unrestricted (see `build_restricted_backend`).
//!
//! Chat mode (`chat`) adds a unified Q&A + ticket-drafting conversation surface and
//! browser chat UI; when it's enabled for a GitHub repo it owns that repo's
//! Discussions (and the standalone `qa`/`drafting` loops below skip GitHub -- see
//! `run`), while GitLab keeps the standalone loops.

pub mod chat;
pub mod drafting;
mod git;
pub mod qa;
pub mod review;

use crate::agent::claude::ClaudeBackend;
use crate::agent::opencode::OpenCodeBackend;
use crate::agent::{AgentBackend, AgentEvent, AgentSession, TurnOutcome};
use crate::config::{AgentBackendKind, EffectiveConfig, RepoProvider};
use crate::repo_host::{self, DiscussionHost, DiscussionThread, RepoHost};
use crate::tracker::TrackerAdapter;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

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

/// Marks a mid-turn "still working" notice (`chat::github::deliver_notices`) as
/// SweBot's own -- deliberately *not* an `answered_marker`, since a notice hasn't
/// answered anything yet and must not advance the thread's answered-marker before
/// the real reply lands. Without some marker at all, `is_swebot_reply` can't tell a
/// notice apart from a human comment, so the very next `ingest` poll enqueues
/// SweBot's own "Still working on that..." text as a fresh question and answers it
/// -- an observed-in-production feedback loop of SweBot replying to its own notices
/// forever.
const NOTICE_MARKER: &str = "<!-- swebot:notice -->";

/// Whether `body` is SweBot's own post -- a reply (carries an `answered_marker`) or a
/// "still working" notice (carries `NOTICE_MARKER`) -- rather than a human comment.
/// Matters because SweBot's own reply necessarily gets a *higher* `database_id` than
/// the marker value it embeds -- without excluding it (and its notices), the "find
/// the newest comment past the last marker" scan below would treat SweBot's own
/// just-posted text as a fresh unanswered question on the very next poll, an
/// infinite reply-to-itself loop.
fn is_swebot_reply(body: &str) -> bool {
    body.contains("<!-- swebot:answered:") || body.contains(NOTICE_MARKER)
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
///
/// AIR-7: moved to `crate::review_rubric` so the in-cycle Reviewer pipeline stage can
/// hold code to the exact same persona/rubric text as this PR-review path -- re-exported
/// here so every existing `use super::PERSONA` import (`qa.rs`, `drafting.rs`,
/// `review.rs`, `chat/worker.rs`) keeps working unchanged.
pub use crate::review_rubric::PERSONA;

/// A restricted `AgentBackend` for SweBot's sessions, built from whichever backend
/// `cfg.swebot_backend()` resolves to (`swebot.backend` override, else `agent.backend`)
/// -- same command/model/timeout as ticket dispatch, no MCP tool wiring. "Restricted"
/// here means every session this backend starts must be started with
/// `ToolPolicy::SWEBOT` (AIR-2: file-mutating tools denied, Bash/Read stay free) -- the
/// backend itself no longer bakes that in (see `ToolPolicy`'s doc comment for why this
/// moved: the same translation now also drives pipeline roles' `tools.allow_edits`).
/// SweBot's drivers parse each turn's text/JSON response themselves and act via
/// `RepoHost`/`TrackerAdapter` directly, rather than routing through an agent-invoked
/// tool call the way ticket dispatch's `update_issue_state` does (that plumbing exists
/// for a long tool-using coding session; SweBot's turns are single-shot conversational
/// exchanges, so parsing the final response is enough).
///
/// Returns `None` for `codex`, which SweBot doesn't support yet -- `swebot::run`
/// refuses to start rather than silently running an unrestricted skeleton (`codex.rs`'s
/// `start_session` would also refuse a `ToolPolicy::SWEBOT` session on its own, but
/// failing at SweBot startup gives a clearer error than failing on the first poll).
fn build_restricted_backend(cfg: &EffectiveConfig) -> Option<Box<dyn AgentBackend>> {
    Some(match cfg.swebot_backend() {
        AgentBackendKind::Claude => Box::new(ClaudeBackend {
            command: cfg.claude.command.clone(),
            extra_args: cfg.claude.args.clone(),
            model: cfg.claude.model.clone(),
            permission_mode: cfg.claude.permission_mode.clone(),
            turn_timeout_ms: cfg.claude.turn_timeout_ms,
            mcp_wiring: None,
            workflow_dir: cfg.workflow_dir.clone(),
        }),
        AgentBackendKind::OpenCode => Box::new(OpenCodeBackend {
            command: cfg.opencode.command.clone(),
            model: cfg.opencode.model.clone(),
            extra_args: cfg.opencode.args.clone(),
            auto_approve: cfg.opencode.auto_approve,
            turn_timeout_ms: cfg.opencode.turn_timeout_ms,
            mcp_wiring: None,
            workflow_dir: cfg.workflow_dir.clone(),
        }),
        AgentBackendKind::Codex => {
            tracing::error!(
                "swebot is not supported with the codex backend yet -- set agent.backend \
                 or swebot.backend to claude or opencode; SweBot will not start"
            );
            return None;
        }
    })
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
pub(crate) fn extract_json_block(text: &str) -> Result<serde_json::Value, String> {
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

/// Every fenced ```json ... ``` block in `text`, each parsed independently -- a
/// ticket-drafting turn may propose *several* tickets in one response (one block
/// per ticket), and `extract_json_block`'s `rfind` only ever sees the last one,
/// silently dropping every ticket but the last (the bug this exists to fix). Falls
/// back to `extract_json_block`'s single bare-JSON behavior when no fence is found
/// at all. A block that fails to parse is skipped rather than failing the whole
/// turn -- one malformed ticket shouldn't sink the others.
fn extract_json_blocks(text: &str) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(idx) = rest.find("```json") {
        rest = &rest[idx + "```json".len()..];
        let candidate = rest.trim_start();
        let mut de = serde_json::Deserializer::from_str(candidate);
        if let Ok(v) = serde::Deserialize::deserialize(&mut de) {
            out.push(v);
        }
    }
    if out.is_empty()
        && let Ok(v) = extract_json_block(text)
    {
        out.push(v);
    }
    out
}

/// Entry point spawned by `orchestrator::run` when `cfg.swebot.enabled`. Loops
/// forever at the same cadence as ticket dispatch (`polling.interval_ms`); each
/// capability's own poll failure is logged and doesn't stop the others or the next
/// cycle.
pub async fn run(cfg: EffectiveConfig, tracker: Arc<dyn TrackerAdapter>) {
    // SweBot posts (review) authenticate as `swebot.token` when set -- a separate
    // GitHub identity from `repo.token` (the coding agent's own, used for branch
    // pushes and `open_pull_request`). Falls back to `repo.token` when `swebot.token`
    // isn't configured. See `config::EffectiveConfig::swebot_repo_config`'s doc
    // comment for why this split exists: GitHub rejects a PR review from the same
    // account that authored the PR.
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
    // Q&A/drafting poll the real code host's own Discussions/labels. PR/MR review
    // always uses `host` directly below too.
    let (qa_selector, drafting_selector) = match host.provider_kind() {
        RepoProvider::Github => (
            cfg.swebot.qa_discussion_category.clone(),
            cfg.swebot.drafting_discussion_category.clone(),
        ),
        RepoProvider::Gitlab => (
            cfg.swebot.qa_label.clone(),
            cfg.swebot.drafting_label.clone(),
        ),
    };
    let Some(backend) = build_restricted_backend(&cfg) else {
        return;
    };
    let shared_clone_dir = cfg.workspace_root.join(".swebot-shared-clone");

    // Ownership split for GitHub Discussions: when chat mode's web surface is enabled
    // (`swebot.chat.enabled`), its GitHub connector owns that repo's Discussions, so
    // the standalone qa/drafting loops below skip GitHub to avoid double-answering.
    // GitLab is never owned by chat, so it keeps the standalone loops. Review always
    // runs here regardless.
    let chat_owns_discussions =
        cfg.swebot.chat.enabled && matches!(swebot_repo.provider, RepoProvider::Github);
    let run_standalone_qa_drafting = !chat_owns_discussions;
    let interval = Duration::from_millis(cfg.poll_interval_ms);

    tracing::info!("swebot starting");
    loop {
        if run_standalone_qa_drafting {
            if let Err(e) = git::ensure_shared_clone(&swebot_repo, &shared_clone_dir).await {
                tracing::warn!(error = %e, "swebot: failed to refresh shared clone; Q&A/drafting will retry next cycle");
            } else {
                let discussion_host: &dyn DiscussionHost = host.as_ref();
                if let Err(e) = qa::poll_once(
                    discussion_host,
                    &qa_selector,
                    backend.as_ref(),
                    &shared_clone_dir,
                )
                .await
                {
                    tracing::warn!(error = %e, "swebot: Q&A poll failed");
                }
                if let Err(e) = drafting::poll_once(
                    &cfg,
                    discussion_host,
                    &drafting_selector,
                    backend.as_ref(),
                    &shared_clone_dir,
                    tracker.as_ref(),
                )
                .await
                {
                    tracing::warn!(error = %e, "swebot: drafting poll failed");
                }
            }
        }
        if cfg.swebot.review_enabled
            && let Err(e) =
                review::poll_once(&cfg, host.as_ref(), backend.as_ref(), tracker.as_ref()).await
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
    use crate::agent::{AgentBackend, AgentError, AgentEvent, AgentSession, ToolPolicy, TurnOutcome};
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
            _tool_policy: &ToolPolicy,
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

    fn backend_test_cfg(yaml_extra: &str) -> EffectiveConfig {
        unsafe {
            std::env::set_var("SYMPHONY_TEST_BACKEND_TOKEN", "t");
        }
        let yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/owner/name.git\n  \
             token: $SYMPHONY_TEST_BACKEND_TOKEN\nswebot:\n  enabled: true\n{yaml_extra}"
        ))
        .unwrap();
        crate::config::resolve(&yaml, std::path::Path::new(".")).unwrap()
    }

    /// The default (no `swebot.backend`) resolves to the global `agent.backend` value.
    #[test]
    fn swebot_backend_defaults_to_agent_backend() {
        let cfg = backend_test_cfg("");
        assert_eq!(cfg.swebot_backend(), AgentBackendKind::Claude);
    }

    #[test]
    fn swebot_backend_override_wins_over_agent_backend() {
        let cfg = backend_test_cfg("  backend: opencode\n");
        assert_eq!(cfg.swebot_backend(), AgentBackendKind::OpenCode);
    }

    #[test]
    fn swebot_backend_override_parses_unknown_values_as_claude_like_agent_backend() {
        let cfg = backend_test_cfg("  backend: not-a-real-backend\n");
        assert_eq!(cfg.swebot_backend(), AgentBackendKind::Claude);
    }

    /// The actual `--disallowedTools`/`OPENCODE_PERMISSION` translation moved to
    /// `ToolPolicy` (AIR-2, see `agent::claude`/`agent::opencode`'s own unit tests for
    /// that) -- `build_restricted_backend` itself is now just "construct the right
    /// backend kind, no MCP wiring"; the restriction is applied by every SweBot call
    /// site passing `&ToolPolicy::SWEBOT` to `start_session` (see `qa.rs`/`drafting.rs`/
    /// `review.rs`/`chat::worker`).
    #[test]
    fn restricted_backend_defaults_to_claude() {
        let cfg = backend_test_cfg("");
        let backend = build_restricted_backend(&cfg).unwrap();
        let cb = backend
            .as_any()
            .and_then(|a| a.downcast_ref::<ClaudeBackend>())
            .expect("default should build a ClaudeBackend");
        assert!(cb.mcp_wiring.is_none());
    }

    #[test]
    fn restricted_backend_opencode_override_builds_open_code_backend() {
        let cfg = backend_test_cfg("  backend: opencode\n");
        let backend = build_restricted_backend(&cfg).unwrap();
        assert!(
            backend
                .as_any()
                .and_then(|a| a.downcast_ref::<OpenCodeBackend>())
                .is_some(),
            "swebot.backend: opencode should build an OpenCodeBackend"
        );
    }

    #[test]
    fn restricted_backend_codex_is_not_supported() {
        let cfg = backend_test_cfg("  backend: codex\n");
        assert_eq!(cfg.swebot_backend(), AgentBackendKind::Codex);
        assert!(build_restricted_backend(&cfg).is_none());
    }

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

    #[test]
    fn is_swebot_reply_detects_marker_carriers_but_not_human_comments() {
        assert!(is_swebot_reply(
            "<!-- swebot:answered:10 -->\nSee src/dedup.rs"
        ));
        assert!(!is_swebot_reply(
            "thanks, but what about videos specifically?"
        ));
    }

    /// Regression test for a real bug found running this live: GitHub Discussions
    /// store the opening question in `body`, not as a `comment`. The chat GitHub
    /// connector honors that by enqueuing the body (remote id 0) whenever no marker
    /// exists yet -- the direct successor of the removed `next_to_answer` sentinel.
    #[test]
    fn last_answered_marker_distinguishes_none_from_some_zero() {
        // No marker at all: the body is still unanswered.
        assert_eq!(last_answered_marker(&thread("ignored", vec![])), None);
        // Marker 0: the body itself was already answered.
        let t = thread("ignored", vec![comment(30, &answered_marker(0), "swebot")]);
        assert_eq!(last_answered_marker(&t), Some(0));
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

    /// Regression test for the reported bug: a response drafting several tickets at
    /// once put multiple ```json blocks in one message, and `extract_json_block`'s
    /// `rfind` only ever surfaced the last one, so only the last ticket got created.
    #[test]
    fn extract_json_blocks_parses_every_fenced_block_not_just_the_last() {
        let text = "Here are three tickets.\n\n\
                     ```json\n{\"ready\": true, \"title\": \"one\"}\n```\n\n\
                     ```json\n{\"ready\": true, \"title\": \"two\"}\n```\n\n\
                     ```json\n{\"ready\": true, \"title\": \"three\"}\n```\n";
        let v = extract_json_blocks(text);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0]["title"], serde_json::json!("one"));
        assert_eq!(v[1]["title"], serde_json::json!("two"));
        assert_eq!(v[2]["title"], serde_json::json!("three"));
    }

    #[test]
    fn extract_json_blocks_falls_back_to_bare_json_when_no_fence_is_found() {
        let v = extract_json_blocks("{\"ready\": true, \"title\": \"x\"}");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["title"], serde_json::json!("x"));
    }

    #[test]
    fn extract_json_blocks_returns_empty_on_malformed_output() {
        assert!(extract_json_blocks("just some prose, no JSON at all").is_empty());
    }
}
