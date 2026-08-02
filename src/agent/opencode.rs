//! OpenCode backend: launches the open-source, provider-agnostic `opencode` CLI in
//! headless mode (`opencode run --format json`), one subprocess per turn, continuity
//! across turns via `--session <session_id>` (mirrors the Claude backend's
//! `--resume <session_id>` -- see `claude.rs`).
//!
//! **Why this exists**: this is the "pluggable AI provider" backend. `opencode` itself
//! is provider-agnostic (~75 model providers, Fireworks among them) via its own
//! already-configured provider config -- this backend only ever passes a
//! `provider/model` string (the `opencode.model` front-matter key) through to
//! `--model`. It deliberately does not manage `opencode`'s provider credentials, same
//! posture as the `claude`/`codex` backends not managing those CLIs' own auth: the
//! operator configures a provider in `opencode` once, out of band (its `/connect` TUI
//! flow, or a static `opencode.json` provider block), before pointing Symphony at it.
//! See README.md "Coding-agent backends" for the setup steps.
//!
//! **Verification caveat** (mirrors `codex.rs`'s own): `opencode run --format json`'s
//! exact per-event NDJSON schema is not fully documented publicly as of this writing --
//! only the final non-JSON `{"response": ...}` wrapper is. This module implements the
//! transport contract Symphony controls (subprocess launch, NDJSON framing, per-line
//! timeout, stderr kept separate) and a best-effort guess at field names for session
//! id, turn completion/failure, and token usage, using the same lenient
//! substring-matching approach `codex.rs` uses for its own uncertain schema. Treat the
//! guessed field names as a starting point to verify against a real installed
//! `opencode` build, not as a confirmed contract.
//!
//! **High-trust default posture** (mirrors the Claude backend, Section 10.5 example):
//! passes `--auto` by default, auto-approving every tool call (file edit, bash) the
//! model requests. There is no human present in a headless run to approve anything
//! interactively, and a restrictive default would silently prevent the agent from ever
//! running its own build/tests -- see `claude.rs`'s module doc for the identical lesson
//! learned there first.
//!
//! **Docker mode**: supported, mirroring `claude.rs` exactly (`docker exec` into the
//! per-ticket container instead of spawning `opencode` on the host, same
//! `ContainerKillGuard` cancellation-safety handling). Installing `opencode` natively
//! is the main friction point on Windows -- baking it (plus a static provider config)
//! into the Symphony base image once, per README.md "Docker mode", sidesteps that
//! entirely.

use super::{AgentBackend, AgentError, AgentEvent, AgentSession, TokenUsage, TurnOutcome};
use crate::container::{ContainerHandle, ContainerKillGuard};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

pub struct OpenCodeBackend {
    pub command: String,
    pub model: Option<String>,
    pub extra_args: Vec<String>,
    pub auto_approve: bool,
    pub turn_timeout_ms: u64,
    /// Needed to map a host workspace path to its in-container equivalent in Docker
    /// mode (see README.md "Docker mode"), same reason `ClaudeBackend` keeps this.
    pub workflow_dir: PathBuf,
}

#[async_trait]
impl AgentBackend for OpenCodeBackend {
    async fn start_session(
        &self,
        workspace: &Path,
        _issue_id: &str,
        _title: &str,
        container: Option<&ContainerHandle>,
    ) -> Result<Box<dyn AgentSession>, AgentError> {
        if !workspace.is_dir() {
            return Err(AgentError::InvalidCwd(format!(
                "{workspace:?} is not a directory"
            )));
        }

        let container_workspace_path =
            container.map(|c| c.to_container_path(&self.workflow_dir, workspace));

        Ok(Box::new(OpenCodeSession {
            command: self.command.clone(),
            model: self.model.clone(),
            extra_args: self.extra_args.clone(),
            auto_approve: self.auto_approve,
            turn_timeout_ms: self.turn_timeout_ms,
            workspace: workspace.to_path_buf(),
            container: container.cloned(),
            container_workspace_path,
            session_id: None,
        }))
    }
}

struct OpenCodeSession {
    command: String,
    model: Option<String>,
    extra_args: Vec<String>,
    auto_approve: bool,
    turn_timeout_ms: u64,
    workspace: PathBuf,
    container: Option<ContainerHandle>,
    container_workspace_path: Option<PathBuf>,
    session_id: Option<String>,
}

#[async_trait]
impl AgentSession for OpenCodeSession {
    fn session_id(&self) -> &str {
        self.session_id.as_deref().unwrap_or("")
    }

    async fn run_turn(
        &mut self,
        prompt: &str,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<TurnOutcome, AgentError> {
        let mut args: Vec<String> = vec![
            "run".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ];
        if let Some(model) = &self.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        if self.auto_approve {
            args.push("--auto".to_string());
        }
        if let Some(sid) = &self.session_id {
            args.push("--session".to_string());
            args.push(sid.clone());
        }
        args.extend(self.extra_args.clone());
        // Prompt goes last, as its own argv element (not through a shell), matching
        // `claude.rs`'s `-p <prompt>` handling -- no injection surface from prompt
        // content containing shell metacharacters.
        args.push(prompt.to_string());

        // Same container-vs-host branch as `claude.rs::ClaudeSession::run_turn`: in
        // Docker mode, `docker exec` into the per-ticket container instead of
        // spawning `opencode` directly on the host.
        let mut kill_guard: Option<ContainerKillGuard> = None;
        let mut cmd = match (&self.container, &self.container_workspace_path) {
            (Some(container), Some(container_workspace)) => {
                let mut c = Command::new("docker");
                c.arg("exec")
                    .arg("-w")
                    .arg(container_workspace.to_string_lossy().to_string())
                    .arg(&container.name)
                    .arg(&self.command)
                    .args(&args);
                kill_guard = Some(ContainerKillGuard::armed(
                    container.name.clone(),
                    self.command.clone(),
                ));
                c
            }
            _ => {
                let mut c = Command::new(&self.command);
                c.args(&args).current_dir(&self.workspace);
                c
            }
        };
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            AgentError::NotFound(format!("failed to launch '{}': {e}", self.command))
        })?;

        let stdout = child.stdout.take().expect("piped stdout");
        let mut lines = BufReader::new(stdout).lines();

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!(target: "opencode_stderr", "{line}");
                }
            });
        }

        let _ = events.send(AgentEvent::new("turn_started"));

        let mut outcome: Option<TurnOutcome> = None;
        loop {
            let next = tokio::time::timeout(
                Duration::from_millis(self.turn_timeout_ms),
                lines.next_line(),
            )
            .await;
            let line = match next {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => break, // stdout EOF
                Ok(Err(e)) => {
                    let _ = child.kill().await;
                    if let Some(guard) = &kill_guard {
                        guard.kill_now().await;
                    }
                    return Err(AgentError::ResponseError(e.to_string()));
                }
                Err(_) => {
                    let _ = child.kill().await;
                    if let Some(guard) = &kill_guard {
                        guard.kill_now().await;
                    }
                    return Err(AgentError::TurnTimeout(format!(
                        "no output for {}ms",
                        self.turn_timeout_ms
                    )));
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => {
                    if let Some(o) = self.handle_message(&v, &events) {
                        outcome = Some(o);
                    }
                }
                Err(_) => {
                    let _ = events.send(AgentEvent::new("malformed").with_message(truncate(&line)));
                }
            }
        }

        let status = child
            .wait()
            .await
            .map_err(|e| AgentError::ProcessExit(e.to_string()))?;

        // The subprocess (host `opencode`, or the `docker exec` client) exited on its
        // own by this point -- whatever it was attached to inside a container is
        // already gone too, so disarm rather than firing a pointless `pkill`. Mirrors
        // `claude.rs`'s identical reasoning.
        if let Some(guard) = &mut kill_guard {
            guard.disarm();
        }

        match outcome {
            Some(o) => Ok(o),
            None if status.success() => Ok(TurnOutcome::Completed { usage: None }),
            None => Ok(TurnOutcome::Failed {
                reason: format!("subprocess exited with {status} and no result event"),
            }),
        }
    }

    async fn stop(self: Box<Self>) {
        // Each turn is a self-contained subprocess (like the Claude backend); nothing
        // persistent to tear down.
    }
}

impl OpenCodeSession {
    /// Handle one parsed NDJSON line. Returns `Some(outcome)` only for a
    /// terminal (completed/failed) event. See the module doc's verification caveat:
    /// the field names matched here are a best-effort guess, not a confirmed schema.
    fn handle_message(
        &mut self,
        v: &Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Option<TurnOutcome> {
        if self.session_id.is_none()
            && let Some(sid) = shallow_find_str(v, &["sessionid", "session_id"])
        {
            self.session_id = Some(sid);
            let _ = events.send(AgentEvent::new("session_started"));
        }

        // Empty/absent `type` mirrors `claude.rs`'s own `"" => malformed` arm: valid
        // JSON that doesn't look like a real event at all.
        let raw_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if raw_type.is_empty() {
            let _ = events.send(AgentEvent::new("malformed"));
            return None;
        }
        let msg_type = raw_type.to_lowercase();

        if msg_type.contains("error") || v.get("error").is_some() {
            let reason = v
                .get("error")
                .and_then(|e| {
                    e.as_str()
                        .map(String::from)
                        .or_else(|| e.get("message").and_then(|m| m.as_str()).map(String::from))
                })
                .unwrap_or_else(|| format!("opencode reported type='{msg_type}'"));
            let _ = events.send(AgentEvent::new("turn_failed").with_message(reason.clone()));
            return Some(TurnOutcome::Failed { reason });
        }

        if msg_type.contains("finish") || msg_type.contains("complete") || msg_type.contains("done")
        {
            let usage = extract_usage_leniently(v);
            let mut e = AgentEvent::new("turn_completed");
            if let Some(u) = usage.clone() {
                e = e.with_usage(u);
            }
            let _ = events.send(e);
            return Some(TurnOutcome::Completed { usage });
        }

        if msg_type.contains("tool") {
            if let Some(name) = shallow_find_str(v, &["toolname", "tool_name", "tool"]) {
                let _ = events.send(AgentEvent::new("tool_call").with_message(name));
            } else {
                // Recognized as a tool event but no name found under any guessed
                // field -- surface the raw type rather than going silent, same
                // reasoning as the unrecognized-type fallback below.
                let _ = events
                    .send(AgentEvent::new("other_message").with_message(raw_type.to_string()));
            }
            return None;
        }

        if let Some(text) = shallow_find_str(v, &["text"]) {
            let _ = events.send(AgentEvent::new("notification").with_message(text));
            return None;
        }

        // Recognized (non-empty `type`) but not one of the categories matched above --
        // mirrors `claude.rs`'s `other => other_message` arm. Surfacing the raw type
        // string here (rather than a content-less "notification") matters more for
        // this backend than for Claude's: the whole event vocabulary is a best-effort
        // guess (see module doc), so losing visibility into *what* showed up makes it
        // harder to ever tighten the guess against a real installed `opencode` build.
        let _ = events.send(AgentEvent::new("other_message").with_message(raw_type.to_string()));
        None
    }
}

/// Look for a string field named (case-insensitively, after stripping `_`) like one of
/// `keys`, at the top level of `v`, one level inside `properties`/`info`/`session`/
/// `message`/`part`/`parts` (common envelope shapes for event-bus-style CLIs), or one
/// level into an array found at any of those spots (e.g. a `parts: [...]` list --
/// mirrors `claude.rs`'s own `content[]` block array for the same reason: a message's
/// text/tool-call is plausibly carried as one of several ordered parts, not a single
/// flat field). Deliberately bounded rather than fully recursive, unlike
/// `extract_usage_leniently` below, since this is used for free-text/name fields where
/// unbounded recursion risks picking up an unrelated nested string.
fn shallow_find_str(v: &Value, keys: &[&str]) -> Option<String> {
    fn matches(key: &str, candidates: &[&str]) -> bool {
        let normalized = key.to_lowercase().replace('_', "");
        candidates.iter().any(|c| normalized == c.replace('_', ""))
    }
    fn scan_object(obj: &Value, keys: &[&str]) -> Option<String> {
        let map = obj.as_object()?;
        for (k, val) in map {
            if matches(k, keys)
                && let Some(s) = val.as_str()
            {
                return Some(s.to_string());
            }
        }
        None
    }
    fn scan_any(v: &Value, keys: &[&str]) -> Option<String> {
        match v {
            Value::Array(items) => items.iter().find_map(|item| scan_object(item, keys)),
            _ => scan_object(v, keys),
        }
    }
    scan_object(v, keys).or_else(|| {
        ["properties", "info", "session", "message", "part", "parts"]
            .iter()
            .find_map(|nested| v.get(nested).and_then(|n| scan_any(n, keys)))
    })
}

/// Extract token usage leniently from common field names, searching arbitrarily deep
/// (unlike `shallow_find_str`) since usage blocks are commonly nested a variable number
/// of levels deep in practice -- same approach and same reasoning as
/// `codex.rs::extract_usage_leniently`, duplicated rather than shared because the two
/// backends' schemas are independent guesses that may not stay aligned.
fn extract_usage_leniently(v: &Value) -> Option<TokenUsage> {
    fn find_u64(v: &Value, needles: &[&str]) -> Option<u64> {
        match v {
            Value::Object(map) => {
                for (k, val) in map {
                    let lower = k.to_lowercase();
                    if needles.iter().any(|n| lower.contains(n))
                        && let Some(n) = val.as_u64()
                    {
                        return Some(n);
                    }
                    if let Some(n) = find_u64(val, needles) {
                        return Some(n);
                    }
                }
                None
            }
            Value::Array(items) => items.iter().find_map(|v| find_u64(v, needles)),
            _ => None,
        }
    }
    // Covers both Anthropic-style (input_tokens/output_tokens) and OpenAI-style
    // (prompt_tokens/completion_tokens) naming, since OpenCode aggregates across
    // providers and may normalize to either.
    let input = find_u64(
        v,
        &["input_token", "inputtoken", "prompt_token", "prompttoken"],
    );
    let output = find_u64(
        v,
        &[
            "output_token",
            "outputtoken",
            "completion_token",
            "completiontoken",
        ],
    );
    match (input, output) {
        (None, None) => None,
        (i, o) => {
            let i = i.unwrap_or(0);
            let o = o.unwrap_or(0);
            Some(TokenUsage {
                input_tokens: i,
                output_tokens: o,
                total_tokens: i + o,
            })
        }
    }
}

fn truncate(s: &str) -> String {
    const MAX: usize = 500;
    if s.len() > MAX {
        format!("{}...", &s[..MAX])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn new_session() -> OpenCodeSession {
        OpenCodeSession {
            command: "opencode".to_string(),
            model: None,
            extra_args: Vec::new(),
            auto_approve: true,
            turn_timeout_ms: 1_000,
            workspace: PathBuf::from("."),
            container: None,
            container_workspace_path: None,
            session_id: None,
        }
    }

    #[test]
    fn captures_session_id_from_top_level_field() {
        let mut session = new_session();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let v = json!({"type": "message", "sessionID": "abc123"});
        session.handle_message(&v, &tx);
        assert_eq!(session.session_id.as_deref(), Some("abc123"));
        let evt = rx.try_recv().unwrap();
        assert_eq!(evt.event, "session_started");
    }

    #[test]
    fn captures_session_id_nested_under_properties() {
        let mut session = new_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        let v = json!({"type": "session.updated", "properties": {"session_id": "nested-1"}});
        session.handle_message(&v, &tx);
        assert_eq!(session.session_id.as_deref(), Some("nested-1"));
    }

    #[test]
    fn finish_type_yields_completed_outcome_with_usage() {
        let mut session = new_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        let v = json!({
            "type": "turn.finish",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let outcome = session.handle_message(&v, &tx);
        match outcome {
            Some(TurnOutcome::Completed { usage: Some(u) }) => {
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.output_tokens, 5);
                assert_eq!(u.total_tokens, 15);
            }
            other => panic!("expected Completed with usage, got {other:?}"),
        }
    }

    #[test]
    fn openai_style_usage_field_names_are_also_recognized() {
        let mut session = new_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        let v = json!({
            "type": "done",
            "usage": {"prompt_tokens": 7, "completion_tokens": 3}
        });
        let outcome = session.handle_message(&v, &tx);
        match outcome {
            Some(TurnOutcome::Completed { usage: Some(u) }) => {
                assert_eq!(u.total_tokens, 10);
            }
            other => panic!("expected Completed with usage, got {other:?}"),
        }
    }

    #[test]
    fn error_type_yields_failed_outcome() {
        let mut session = new_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        let v = json!({"type": "turn.error", "error": {"message": "boom"}});
        let outcome = session.handle_message(&v, &tx);
        match outcome {
            Some(TurnOutcome::Failed { reason }) => assert_eq!(reason, "boom"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn error_field_without_error_type_still_fails() {
        let mut session = new_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        let v = json!({"type": "message", "error": "something broke"});
        let outcome = session.handle_message(&v, &tx);
        assert!(matches!(outcome, Some(TurnOutcome::Failed { .. })));
    }

    #[test]
    fn tool_type_emits_tool_call_event_and_no_outcome() {
        let mut session = new_session();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let v = json!({"type": "tool.call", "tool": "bash"});
        let outcome = session.handle_message(&v, &tx);
        assert!(outcome.is_none());
        let evt = rx.try_recv().unwrap();
        assert_eq!(evt.event, "tool_call");
        assert_eq!(evt.message.as_deref(), Some("bash"));
    }

    #[test]
    fn plain_text_message_emits_notification() {
        let mut session = new_session();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let v = json!({"type": "message", "text": "hello there"});
        let outcome = session.handle_message(&v, &tx);
        assert!(outcome.is_none());
        let evt = rx.try_recv().unwrap();
        assert_eq!(evt.event, "notification");
        assert_eq!(evt.message.as_deref(), Some("hello there"));
    }

    #[test]
    fn unrecognized_type_surfaces_as_other_message_with_raw_type() {
        let mut session = new_session();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let v = json!({"type": "something.unknown"});
        let outcome = session.handle_message(&v, &tx);
        assert!(outcome.is_none());
        let evt = rx.try_recv().unwrap();
        assert_eq!(evt.event, "other_message");
        assert_eq!(evt.message.as_deref(), Some("something.unknown"));
    }

    #[test]
    fn missing_type_field_is_malformed() {
        let mut session = new_session();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let v = json!({"foo": "bar"});
        let outcome = session.handle_message(&v, &tx);
        assert!(outcome.is_none());
        let evt = rx.try_recv().unwrap();
        assert_eq!(evt.event, "malformed");
    }

    #[test]
    fn text_nested_inside_a_parts_array_is_still_found() {
        let mut session = new_session();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let v = json!({
            "type": "message.part.updated",
            "part": [{"type": "tool"}, {"type": "text", "text": "from a part"}]
        });
        let outcome = session.handle_message(&v, &tx);
        assert!(outcome.is_none());
        let evt = rx.try_recv().unwrap();
        assert_eq!(evt.event, "notification");
        assert_eq!(evt.message.as_deref(), Some("from a part"));
    }
}
