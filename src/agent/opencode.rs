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
//! substring-matching approach `codex.rs` uses for its own uncertain schema.
//!
//! Confirmed against two real Docker-mode runs (see
//! `real_captured_auth_failure_payload_parses_correctly` and
//! `real_step_finish_payload_extracts_token_usage` below) -- `sessionID` (camelCase,
//! not `session_id`) is the real top-level field name; a failing turn looks like
//! `{"type":"error","error":{"data":{"message":"..."}}}}` (the message lives under
//! `error.data.message`, not `error.message`); and a successful step's completion
//! event is `{"type":"step_finish","part":{"tokens":{"input":N,"output":N,...}}}` --
//! matched by the `contains("finish")` check by coincidence (the real type name is
//! `step_finish`, not `finish`/`complete`/`done` as originally guessed), with usage at
//! `part.tokens.{input,output}`, not the `input_tokens`/`output_tokens` flat naming
//! originally guessed (every real turn recorded zero tokens until this was fixed).
//! One structural note from that same real run: `opencode` emits a `step_finish` per
//! *internal* agentic-loop step, not once per `opencode run` invocation -- so a single
//! Symphony turn can emit several `turn_completed` `AgentEvent`s before the subprocess
//! actually exits. This is fine as designed: `TurnOutcome::Completed.usage` is
//! documented as informational only (see `AgentEvent`'s own doc in `mod.rs`); the
//! orchestrator's authoritative token accounting sums every `AgentEvent::usage` as it
//! streams through, not just the last one. Tool-call detection is also confirmed
//! working from that same real run's event log: real `tool_call` events correctly
//! carried the actual tool names used (`read`, `bash`, `glob`, `grep`).
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
//!
//! **Provider-native tracker tools** (Section 10.5, OPTIONAL, see `mcp_config_env`):
//! `opencode` has no per-invocation config flag like `claude`'s `--mcp-config`, only
//! config-file discovery -- but it does read `OPENCODE_CONFIG_CONTENT` (inline config
//! content via an env var) as its own escape hatch for exactly this, and confirmed
//! against opencode's own docs, config layers deep-merge rather than replace each
//! other, so setting it never disturbs the separately-baked-in provider config (the
//! image's own Fireworks setup). `mcp_wiring` carries the same `McpToolWiring` data
//! `ClaudeBackend` uses; when set, `start_session` builds the `OPENCODE_CONFIG_CONTENT`
//! value once and every subsequent turn forwards it as an env var (`-e` in Docker mode,
//! `.env()` on the host), same delivery mechanism `permission_config` already uses.

use super::claude::McpToolWiring;
use super::{AgentBackend, AgentError, AgentEvent, AgentSession, TokenUsage, TurnOutcome};
use crate::container::{self, ContainerHandle, ContainerKillGuard};
use async_trait::async_trait;
use serde_json::{Value, json};
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
    /// When set, restrict this backend's turns by passing an opencode `permission`
    /// config JSON (see `opencode`'s own Permissions docs) as the
    /// `OPENCODE_PERMISSION` env var on every spawned subprocess. `"deny"` rules are
    /// enforced even under `--auto` (which only auto-approves what is *not* already
    /// denied), so this is the mechanism for a read-only restricted session: e.g.
    /// `{"edit":"deny"}` blocks edit/write/patch while leaving bash/read free.
    /// `None` (the default) leaves permissions untouched -- SweBot's restricted
    /// sessions set it, ticket dispatch does not.
    pub permission_config: Option<String>,
    /// Section 10.5's provider-native-tool wiring (`open_pull_request`,
    /// `update_issue_state`) -- the same `McpToolWiring` `ClaudeBackend` uses.
    /// `opencode` supports MCP servers via its own config (`"mcp": {...}`, deep-merged
    /// across config layers, confirmed against opencode's docs -- a project/session
    /// layer adding an `"mcp"` entry never disturbs the separately-baked-in provider
    /// config), so the same data drives both backends; only how it's delivered to the
    /// subprocess differs (see `mcp_config_env`). `None` leaves the tools unwired, as
    /// before this field existed.
    pub mcp_wiring: Option<McpToolWiring>,
    /// Needed to map a host workspace path to its in-container equivalent in Docker
    /// mode (see README.md "Docker mode"), same reason `ClaudeBackend` keeps this.
    pub workflow_dir: PathBuf,
}

#[async_trait]
impl AgentBackend for OpenCodeBackend {
    async fn start_session(
        &self,
        workspace: &Path,
        issue_id: &str,
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

        let mcp_config_env = self
            .mcp_wiring
            .as_ref()
            .map(|wiring| mcp_config_env(wiring, issue_id, container));

        Ok(Box::new(OpenCodeSession {
            command: self.command.clone(),
            model: self.model.clone(),
            extra_args: self.extra_args.clone(),
            auto_approve: self.auto_approve,
            turn_timeout_ms: self.turn_timeout_ms,
            permission_config: self.permission_config.clone(),
            mcp_config_env,
            workspace: workspace.to_path_buf(),
            container: container.cloned(),
            container_workspace_path,
            session_id: None,
        }))
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

struct OpenCodeSession {
    command: String,
    model: Option<String>,
    extra_args: Vec<String>,
    auto_approve: bool,
    turn_timeout_ms: u64,
    permission_config: Option<String>,
    /// `(OPENCODE_CONFIG_CONTENT, <json>)` when `mcp_wiring` is set -- bound once at
    /// `start_session` (per Section 10.5's "one session snapshot" rule, same as
    /// `ClaudeSession::mcp_config_path`), not recomputed per turn.
    mcp_config_env: Option<(String, String)>,
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
        let args = self.run_args(prompt);
        // Everything but the prompt itself (last arg, and often long): enough to
        // reproduce a hung/stalled turn's exact invocation by hand outside Symphony
        // (`opencode <these args> "<the actual prompt>"`) without dumping a
        // potentially huge prompt into every turn's log at debug level. Its *length*
        // is logged unconditionally though (never sensitive on its own) -- an
        // unexpectedly-empty or near-empty prompt is exactly what makes `opencode`
        // print its own CLI usage/help text and exit 1 with no NDJSON at all (no
        // `message` positional given), which otherwise looks identical in the log to
        // any other immediate spawn failure.
        tracing::debug!(
            command = %self.command,
            workspace = %self.workspace.display(),
            args = ?&args[..args.len().saturating_sub(1)],
            prompt_len = prompt.len(),
            "opencode: starting turn"
        );

        // Same container-vs-host branch as `claude.rs::ClaudeSession::run_turn`: in
        // Docker mode, `docker exec` into the per-ticket container instead of
        // spawning `opencode` directly on the host.
        let mut kill_guard: Option<ContainerKillGuard> = None;
        let mut cmd = match (&self.container, &self.container_workspace_path) {
            (Some(container), Some(container_workspace)) => {
                let mut c = Command::new("docker");
                c.args(self.docker_exec_args(container_workspace, &container.name, &args));
                kill_guard = Some(ContainerKillGuard::armed(
                    container.name.clone(),
                    self.command.clone(),
                ));
                c
            }
            _ => {
                let mut c = Command::new(&self.command);
                c.args(&args).current_dir(&self.workspace);
                if let Some((k, v)) = permission_env(self.permission_config.as_deref()) {
                    c.env(k, v);
                }
                if let Some((k, v)) = &self.mcp_config_env {
                    c.env(k, v);
                }
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
            // `handle_message` only ever surfaces a bare `type` string for anything
            // it doesn't specifically recognize (`other_message`/`tool` with no
            // name) -- the full raw line is the only way to actually see what an
            // unrecognized event carried (a tool's arguments, a nested error, an id
            // to correlate against). Opt-in (debug) since a chatty turn can be many
            // lines; `RUST_LOG=debug` (or `RUST_LOG=symphony::agent::opencode=debug`)
            // turns this on when diagnosing a stalled/hung turn after the fact.
            tracing::debug!(target: "opencode_ndjson", "{line}");
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
    /// The argv (everything after the program, and after any `docker exec -w` prefix)
    /// for one `opencode run --format json` turn. Extracted from `run_turn` so tests
    /// can assert flag ordering/env composition without spawning a process.
    fn run_args(&self, prompt: &str) -> Vec<String> {
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
        args
    }

    /// The full `docker exec` argv (everything after `docker`) for one Docker-mode
    /// turn. Extracted from `run_turn` so tests can assert flag ordering without
    /// spawning a process -- this is the exact ordering that matters: `docker exec
    /// [OPTIONS] CONTAINER COMMAND [ARG...]` treats anything after CONTAINER as the
    /// command and its own arguments, not a docker option. A real regression put
    /// `-e` *after* `.arg(&container.name).arg(&self.command).args(&args)`, which
    /// silently handed `-e OPENCODE_CONFIG_CONTENT=...` to `opencode` itself instead
    /// of to `docker exec`; `opencode run` has no `-e` flag, so its yargs parser
    /// rejected it and dumped its own usage/help text with exit 1 and zero NDJSON --
    /// hit live once ticket dispatch started wiring `OPENCODE_CONFIG_CONTENT` through
    /// this path (see `mcp_config_env`).
    fn docker_exec_args(
        &self,
        container_workspace: &Path,
        container_name: &str,
        run_args: &[String],
    ) -> Vec<String> {
        let mut args = vec![
            "exec".to_string(),
            "-w".to_string(),
            container_workspace.to_string_lossy().to_string(),
        ];
        // A container doesn't inherit the `docker` client's process env the way a
        // plain child does, so a restricted session's permission JSON has to be
        // forwarded explicitly via `-e`.
        if let Some((k, v)) = permission_env(self.permission_config.as_deref()) {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }
        if let Some((k, v)) = &self.mcp_config_env {
            args.push("-e".to_string());
            args.push(format!("{k}={v}"));
        }
        args.push(container_name.to_string());
        args.push(self.command.clone());
        args.extend_from_slice(run_args);
        args
    }

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
            // Confirmed against a real `opencode run --format json` failure (invalid
            // provider credentials): `{"type":"error","error":{"name":"UnknownError",
            // "data":{"message":"...","ref":"..."}}}` -- the message lives under
            // `error.data.message`, not `error.message` directly. `error.message` and
            // bare-string `error` are kept as fallbacks for whatever other error shapes
            // exist (still unverified -- see the module doc's caveat).
            let reason = v
                .get("error")
                .and_then(|e| {
                    e.as_str()
                        .map(String::from)
                        .or_else(|| e.get("message").and_then(|m| m.as_str()).map(String::from))
                        .or_else(|| {
                            e.get("data")
                                .and_then(|d| d.get("message"))
                                .and_then(|m| m.as_str())
                                .map(String::from)
                        })
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

/// The `OPENCODE_PERMISSION` env `(key, value)` pair for a permission config, if set
/// -- how a restricted session's deny rules reach the spawned `opencode` subprocess
/// (see `OpenCodeBackend::permission_config`). Kept as its own function so the Docker
/// branch above can emit `-e KEY=VALUE` and the host branch can `.env(key, value)`.
fn permission_env(config: Option<&str>) -> Option<(String, String)> {
    config.map(|c| ("OPENCODE_PERMISSION".to_string(), c.to_string()))
}

/// The `OPENCODE_CONFIG_CONTENT` env `(key, value)` pair wiring Section 10.5's
/// provider-native tools (`open_pull_request`, `update_issue_state`) into an
/// `opencode` session, mirroring `claude.rs::write_mcp_config` -- same
/// `symphony __mcp_tool_server` subprocess, same args, same in-container path
/// substitution (Docker mode runs `opencode` inside the container, so the `command`
/// and `--workflow-dir` value must be in-container paths, not host ones) -- but
/// delivered as inline config content via an env var rather than a file `--mcp-config`
/// points at: `opencode` has no `claude`-equivalent per-invocation config flag, only
/// config-file discovery, and `OPENCODE_CONFIG_CONTENT` is its own env-var escape
/// hatch for exactly this (confirmed against opencode's docs: config layers deep-merge
/// rather than replace each other, so this never disturbs the separately-baked-in
/// Fireworks provider block in the image's own `opencode.json`).
fn mcp_config_env(
    wiring: &McpToolWiring,
    issue_id: &str,
    container: Option<&ContainerHandle>,
) -> (String, String) {
    let (command, workflow_dir_arg) = match container {
        Some(c) => (
            container::CONTAINER_SYMPHONY_BIN.to_string(),
            c.container_root.to_string_lossy().to_string(),
        ),
        None => (
            std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "symphony".to_string()),
            wiring.workflow_dir.to_string_lossy().to_string(),
        ),
    };
    let mut argv = vec![
        command,
        "__mcp_tool_server".to_string(),
        "--tracker-kind".to_string(),
        wiring.tracker_kind.clone(),
        "--tracker-provider".to_string(),
        wiring.tracker_provider_json.clone(),
        "--workflow-dir".to_string(),
        workflow_dir_arg,
        "--issue-id".to_string(),
        issue_id.to_string(),
    ];
    if let Some(repo_pr_json) = &wiring.repo_pr_json {
        argv.push("--repo-pr".to_string());
        argv.push(repo_pr_json.clone());
    }
    let config = json!({
        "mcp": {
            "symphony": {
                "type": "local",
                "command": argv,
                "enabled": true
            }
        }
    });
    ("OPENCODE_CONFIG_CONTENT".to_string(), config.to_string())
}

/// Extract token usage leniently, searching arbitrarily deep (unlike
/// `shallow_find_str`) since usage blocks are commonly nested a variable number of
/// levels deep in practice -- same approach and same reasoning as
/// `codex.rs::extract_usage_leniently`, duplicated rather than shared because the two
/// backends' schemas are independent guesses that may not stay aligned.
///
/// Real shape confirmed via a live Docker-mode capture (Fireworks/Kimi K2.7 Code, a
/// genuine successful turn -- see `real_step_finish_payload_extracts_token_usage`
/// below): a `step_finish` event's `part.tokens` object has plain `input`/`output`
/// keys (no `_token`/`_tokens` suffix at all -- the "token-ness" is carried by the
/// parent key `tokens`, not each child), plus `reasoning` and `cache.{read,write}`.
/// `total_tokens` here is deliberately `input + output` (matching every other
/// backend's definition of the field), NOT the payload's own `tokens.total`, which
/// bundles in reasoning/cache tokens too and would be inconsistent with how Claude's
/// and Codex's `TokenUsage.total_tokens` are computed.
fn extract_usage_leniently(v: &Value) -> Option<TokenUsage> {
    fn find_tokens_object(v: &Value) -> Option<TokenUsage> {
        match v {
            Value::Object(map) => {
                if let Some(Value::Object(tokens)) = map.get("tokens") {
                    let input = tokens.get("input").and_then(|n| n.as_u64());
                    let output = tokens.get("output").and_then(|n| n.as_u64());
                    if input.is_some() || output.is_some() {
                        let i = input.unwrap_or(0);
                        let o = output.unwrap_or(0);
                        return Some(TokenUsage {
                            input_tokens: i,
                            output_tokens: o,
                            total_tokens: i + o,
                        });
                    }
                }
                map.values().find_map(find_tokens_object)
            }
            Value::Array(items) => items.iter().find_map(find_tokens_object),
            _ => None,
        }
    }

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

    if let Some(usage) = find_tokens_object(v) {
        return Some(usage);
    }

    // Fallback for schema variants without a `tokens: {input, output}` object --
    // covers both Anthropic-style (input_tokens/output_tokens) and OpenAI-style
    // (prompt_tokens/completion_tokens) flat naming, unverified against a real
    // payload but kept in case a different opencode version/provider uses it.
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
            permission_config: None,
            mcp_config_env: None,
            workspace: PathBuf::from("."),
            container: None,
            container_workspace_path: None,
            session_id: None,
        }
    }

    #[test]
    fn run_args_orders_flags_before_the_prompt() {
        let mut session = new_session();
        session.model = Some("fireworks/x".to_string());
        session.auto_approve = true;
        session.session_id = Some("ses_1".to_string());
        session.extra_args = vec!["--verbose".to_string()];
        let args = session.run_args("do the thing");
        assert_eq!(
            args,
            vec![
                "run",
                "--format",
                "json",
                "--model",
                "fireworks/x",
                "--auto",
                "--session",
                "ses_1",
                "--verbose",
                "do the thing",
            ]
        );
    }

    #[test]
    fn run_args_without_optional_flags() {
        let mut session = new_session();
        session.auto_approve = false;
        let args = session.run_args("just the prompt");
        assert_eq!(args, vec!["run", "--format", "json", "just the prompt"]);
    }

    /// Regression test for a real bug found running this live: `-e` flags built for
    /// `mcp_config_env`/`permission_env` were being appended *after* the container
    /// name and command, so `docker exec` handed them to `opencode` itself instead
    /// of consuming them as its own options -- `opencode run` has no `-e` flag, so
    /// it rejected the extra argument and dumped its own usage text with exit 1 and
    /// zero NDJSON, wasting a real ticket-dispatch turn. Every `-e` must land before
    /// the container name.
    #[test]
    fn docker_exec_args_puts_env_flags_before_the_container_name() {
        let mut session = new_session();
        session.mcp_config_env = Some(("OPENCODE_CONFIG_CONTENT".to_string(), "{}".to_string()));
        let run_args = session.run_args("do the thing");
        let args = session.docker_exec_args(Path::new("/project"), "my-container", &run_args);
        assert_eq!(
            args,
            vec![
                "exec",
                "-w",
                "/project",
                "-e",
                "OPENCODE_CONFIG_CONTENT={}",
                "my-container",
                "opencode",
                "run",
                "--format",
                "json",
                "--auto",
                "do the thing",
            ]
        );
    }

    #[test]
    fn docker_exec_args_includes_permission_env_before_the_container_name_too() {
        let mut session = new_session();
        session.permission_config = Some(r#"{"edit":"deny"}"#.to_string());
        let run_args = session.run_args("prompt");
        let args = session.docker_exec_args(Path::new("/project"), "my-container", &run_args);
        let container_pos = args.iter().position(|a| a == "my-container").unwrap();
        let env_pos = args.iter().position(|a| a == "-e").unwrap();
        assert!(
            env_pos < container_pos,
            "-e must come before the container name: {args:?}"
        );
    }

    fn test_wiring() -> McpToolWiring {
        McpToolWiring {
            tracker_kind: "local".to_string(),
            tracker_provider_json: r#"{"dir":"issues"}"#.to_string(),
            workflow_dir: PathBuf::from("/wf"),
            repo_pr_json: None,
        }
    }

    #[test]
    fn mcp_config_env_wires_the_symphony_tool_server_on_the_host() {
        let (key, value) = mcp_config_env(&test_wiring(), "42", None);
        assert_eq!(key, "OPENCODE_CONFIG_CONTENT");
        let parsed: Value = serde_json::from_str(&value).unwrap();
        let argv = parsed["mcp"]["symphony"]["command"].as_array().unwrap();
        let argv: Vec<&str> = argv.iter().map(|v| v.as_str().unwrap()).collect();
        // Whatever binary is actually running the test (the test harness executable,
        // not literally `symphony`) -- just confirm it resolved to *some* real path
        // via `std::env::current_exe()`, not the "symphony" fallback used only when
        // that call fails.
        assert_ne!(argv[0], "symphony");
        assert_eq!(
            &argv[1..],
            &[
                "__mcp_tool_server",
                "--tracker-kind",
                "local",
                "--tracker-provider",
                r#"{"dir":"issues"}"#,
                "--workflow-dir",
                "/wf",
                "--issue-id",
                "42"
            ]
        );
        assert_eq!(parsed["mcp"]["symphony"]["type"], "local");
        assert_eq!(parsed["mcp"]["symphony"]["enabled"], true);
    }

    #[test]
    fn mcp_config_env_includes_repo_pr_json_when_pull_request_is_enabled() {
        let mut wiring = test_wiring();
        wiring.repo_pr_json = Some(r#"{"url":"https://github.com/o/r.git"}"#.to_string());
        let (_, value) = mcp_config_env(&wiring, "42", None);
        let parsed: Value = serde_json::from_str(&value).unwrap();
        let argv = parsed["mcp"]["symphony"]["command"].as_array().unwrap();
        let argv: Vec<&str> = argv.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(
            argv.last(),
            Some(&r#"{"url":"https://github.com/o/r.git"}"#)
        );
        assert_eq!(argv[argv.len() - 2], "--repo-pr");
    }

    #[test]
    fn mcp_config_env_uses_in_container_paths_when_containerized() {
        let container = ContainerHandle {
            name: "symphony-abc-42".to_string(),
            container_root: PathBuf::from("/project"),
        };
        let (_, value) = mcp_config_env(&test_wiring(), "42", Some(&container));
        let parsed: Value = serde_json::from_str(&value).unwrap();
        let argv = parsed["mcp"]["symphony"]["command"].as_array().unwrap();
        let argv: Vec<&str> = argv.iter().map(|v| v.as_str().unwrap()).collect();
        // In-container symphony binary and workflow-dir, not host paths.
        assert_eq!(argv[0], container::CONTAINER_SYMPHONY_BIN);
        let workflow_dir_idx = argv.iter().position(|a| *a == "--workflow-dir").unwrap() + 1;
        assert_eq!(argv[workflow_dir_idx], "/project");
    }

    #[test]
    fn permission_env_carries_the_config_as_open_code_permission_variable() {
        assert_eq!(
            permission_env(Some(r#"{"edit":"deny"}"#)).unwrap(),
            (
                "OPENCODE_PERMISSION".to_string(),
                r#"{"edit":"deny"}"#.to_string()
            )
        );
    }

    #[test]
    fn permission_env_is_none_without_a_config() {
        assert_eq!(permission_env(None), None);
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

    /// Real payload captured from a genuinely successful `opencode run --format json`
    /// turn (Fireworks/kimi-k2p7-code, Docker-mode smoke test) -- confirms the actual
    /// completion event is `type: "step_finish"` (matched by the existing
    /// `contains("finish")` check) and that usage lives at `part.tokens.{input,output}`,
    /// not the `input_tokens`/`output_tokens` flat naming this parser originally
    /// guessed at (which is why every real turn recorded zero tokens before this fix).
    /// `tokens.total` (6330 in the real payload) deliberately isn't used for
    /// `total_tokens` -- it bundles in reasoning/cache tokens, whereas `total_tokens`
    /// here means `input + output`, matching every other backend.
    #[test]
    fn real_step_finish_payload_extracts_token_usage() {
        let mut session = new_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        let v = json!({
            "type": "step_finish",
            "timestamp": 1785665654337_u64,
            "sessionID": "ses_03e0878cbffeXblntUlDTnmxOO",
            "part": {
                "id": "prt_fc1f78e370017hF7fLtd9NvRuy",
                "reason": "stop",
                "messageID": "msg_fc1f788650017TGY2e9lQNrkew",
                "sessionID": "ses_03e0878cbffeXblntUlDTnmxOO",
                "type": "step-finish",
                "tokens": {
                    "total": 6330,
                    "input": 212,
                    "output": 22,
                    "reasoning": 0,
                    "cache": {"write": 0, "read": 6096}
                },
                "cost": 0
            }
        });
        let outcome = session.handle_message(&v, &tx);
        match outcome {
            Some(TurnOutcome::Completed { usage: Some(u) }) => {
                assert_eq!(u.input_tokens, 212);
                assert_eq!(u.output_tokens, 22);
                assert_eq!(u.total_tokens, 234); // input + output, not the payload's tokens.total
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

    /// Real payload captured from `opencode run --format json` against Fireworks with
    /// an invalid API key (Docker mode smoke test, symphony-base image) -- not a
    /// synthetic guess like the other fixtures here. Confirms `error.data.message` is
    /// the actual nesting (not `error.message`) and that `sessionID` (not
    /// `session_id`) is the real top-level field name.
    #[test]
    fn real_captured_auth_failure_payload_parses_correctly() {
        let mut session = new_session();
        let (tx, _rx) = mpsc::unbounded_channel();
        let v = json!({
            "type": "error",
            "timestamp": 1785663447156_u64,
            "sessionID": "ses_03e2a20f7ffe24SP8z3Tdr3aav",
            "error": {
                "name": "UnknownError",
                "data": {
                    "message": "Unexpected server error. Check server logs for details.",
                    "ref": "err_a3ebaa40"
                }
            }
        });
        let outcome = session.handle_message(&v, &tx);
        assert_eq!(
            session.session_id.as_deref(),
            Some("ses_03e2a20f7ffe24SP8z3Tdr3aav")
        );
        match outcome {
            Some(TurnOutcome::Failed { reason }) => {
                assert_eq!(
                    reason,
                    "Unexpected server error. Check server logs for details."
                );
            }
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
