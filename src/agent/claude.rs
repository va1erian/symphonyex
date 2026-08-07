//! Claude Code backend: launches the `claude` CLI in headless (`-p`) mode with
//! `--output-format stream-json`, one subprocess per turn, continuity across turns via
//! `--resume <session_id>`.
//!
//! High-trust default posture (Section 10.5 example): `--permission-mode
//! bypassPermissions` auto-approves file edits and command execution for the session.
//! (`acceptEdits` looks like the obvious choice but only covers file-edit tools —
//! Write/Edit/NotebookEdit — not Bash/PowerShell; in a headless run with no human to
//! approve anything, that leaves every shell command auto-denied, which silently
//! prevents agents from ever running `cargo build`/tests to verify their own work.)
//! There is no user-input-required signal in this mode; a turn either produces a
//! `result` event or is treated as failed.
//!
//! Provider-native tracker tools (Section 10.5, OPTIONAL): if the active tracker
//! adapter exposes any (`TrackerAdapter::agent_tool_specs`), `start_session` writes a
//! per-session `--mcp-config` file pointing back at this same `symphony` binary
//! (`symphony __mcp_tool_server ...`, see `crate::mcp`). `claude` spawns that as an MCP
//! server subprocess and calls into it; the subprocess re-builds the tracker adapter
//! and executes the tool host-side, so the coding-agent process never touches tracker
//! storage directly. The MCP tool names are always auto-approved
//! (`--allowedTools mcp__symphony__*`) independent of `permission_mode`, since they are
//! host-mediated and adapter-scoped, not raw command/file access.

use super::{
    AgentBackend, AgentError, AgentEvent, AgentSession, TokenUsage, ToolPolicy, TurnOutcome,
};
use crate::container::{self, ContainerHandle, ContainerKillGuard};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Config needed to point `claude` at Symphony's own MCP tool-server subcommand for
/// the active tracker adapter (Section 10.5).
#[derive(Clone)]
pub struct McpToolWiring {
    pub tracker_kind: String,
    /// `tracker.provider`, pre-serialized to a JSON string (parseable as YAML too).
    pub tracker_provider_json: String,
    pub workflow_dir: PathBuf,
    /// `repo:` config, pre-serialized to a JSON string, present only when
    /// `repo.pull_request` is enabled -- lets `__mcp_tool_server` build a
    /// `repo_host::GithubRepoHost` alongside the tracker adapter (Section "PR-based
    /// branch workflow"). Independent of `tracker_kind`/`tracker_provider_json`: a
    /// pull request is a property of the code host, not the issue tracker.
    pub repo_pr_json: Option<String>,
    /// `pipeline.enabled` (AIR-3) -- gates whether `record_artifact` is wired in
    /// alongside whatever tracker/repo-host tools are already exposed. See
    /// `crate::artifacts`'s own doc comment.
    pub pipeline_enabled: bool,
}

pub struct ClaudeBackend {
    pub command: String,
    pub extra_args: Vec<String>,
    pub model: Option<String>,
    pub permission_mode: String,
    pub turn_timeout_ms: u64,
    pub mcp_wiring: Option<McpToolWiring>,
    /// Needed independent of `mcp_wiring` to map a host workspace path to its
    /// in-container equivalent in Docker mode (see README.md "Docker mode").
    pub workflow_dir: PathBuf,
}

#[async_trait]
impl AgentBackend for ClaudeBackend {
    async fn start_session(
        &self,
        workspace: &Path,
        issue_id: &str,
        _title: &str,
        container: Option<&ContainerHandle>,
        tool_policy: &ToolPolicy,
    ) -> Result<Box<dyn AgentSession>, AgentError> {
        let mcp_config_path = match &self.mcp_wiring {
            Some(wiring) => match write_mcp_config(workspace, wiring, issue_id, container) {
                Ok(path) => Some(path),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to write MCP tool config; continuing without provider-native tools");
                    None
                }
            },
            None => None,
        };

        let container_workspace_path =
            container.map(|c| c.to_container_path(&self.workflow_dir, workspace));

        Ok(Box::new(ClaudeSession {
            command: self.command.clone(),
            extra_args: tool_policy_extra_args(&self.extra_args, tool_policy),
            model: self.model.clone(),
            permission_mode: self.permission_mode.clone(),
            turn_timeout_ms: self.turn_timeout_ms,
            workspace: workspace.to_path_buf(),
            container: container.cloned(),
            container_workspace_path,
            mcp_config_path,
            session_id: None,
        }))
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

/// Translate a `ToolPolicy` (AIR-2) into `claude`'s own `--disallowedTools` flag,
/// appended after `base` -- the backend-native mechanism this used to be SweBot's own
/// hardcoded `--disallowedTools Edit,Write,NotebookEdit` (see `swebot::mod`'s history),
/// now shared by any restricted session (SweBot, or a pipeline role with
/// `tools.allow_edits: false`).
fn tool_policy_extra_args(base: &[String], policy: &ToolPolicy) -> Vec<String> {
    let mut args = base.to_vec();
    let mut disallowed: Vec<&str> = Vec::new();
    if !policy.allow_edits {
        disallowed.extend(["Edit", "Write", "NotebookEdit"]);
    }
    if !policy.allow_commands {
        disallowed.push("Bash");
    }
    if !disallowed.is_empty() {
        args.push("--disallowedTools".to_string());
        args.push(disallowed.join(","));
    }
    args
}

/// Write `<workspace>/.symphony-mcp.json` pointing back at `symphony __mcp_tool_server`
/// for this issue, bound once per session per Section 10.5's "one session snapshot"
/// requirement (not re-derived per turn). Written via the host filesystem either way --
/// in Docker mode the file lands in the bind-mounted workspace directory, so it's
/// visible inside the container at the corresponding mapped path without any extra
/// step -- but its *contents* (the `command` to exec and the `--workflow-dir` value)
/// must point at in-container paths, not host ones, since `claude` itself runs inside
/// the container in that mode and would otherwise try to exec a Windows `.exe`.
fn write_mcp_config(
    workspace: &Path,
    wiring: &McpToolWiring,
    issue_id: &str,
    container: Option<&ContainerHandle>,
) -> std::io::Result<PathBuf> {
    let (command, workflow_dir_arg) = match container {
        Some(c) => (
            container::CONTAINER_SYMPHONY_BIN.to_string(),
            c.container_root.to_string_lossy().to_string(),
        ),
        None => (
            std::env::current_exe()?.to_string_lossy().to_string(),
            wiring.workflow_dir.to_string_lossy().to_string(),
        ),
    };
    // Same container-aware mapping as `workflow_dir_arg` above: the subprocess must see
    // this path exactly as its own filesystem does, which is the in-container path when
    // `claude` itself runs inside a container, and the host path otherwise.
    let workspace_dir_arg = match container {
        Some(c) => c
            .to_container_path(&wiring.workflow_dir, workspace)
            .to_string_lossy()
            .to_string(),
        None => workspace.to_string_lossy().to_string(),
    };
    let mut args = vec![
        "__mcp_tool_server".to_string(),
        "--tracker-kind".to_string(),
        wiring.tracker_kind.clone(),
        "--tracker-provider".to_string(),
        wiring.tracker_provider_json.clone(),
        "--workflow-dir".to_string(),
        workflow_dir_arg,
        "--issue-id".to_string(),
        issue_id.to_string(),
        "--workspace-dir".to_string(),
        workspace_dir_arg,
    ];
    if let Some(repo_pr_json) = &wiring.repo_pr_json {
        args.push("--repo-pr".to_string());
        args.push(repo_pr_json.clone());
    }
    if wiring.pipeline_enabled {
        args.push("--pipeline-enabled".to_string());
    }
    let config = json!({
        "mcpServers": {
            "symphony": {
                "command": command,
                "args": args
            }
        }
    });
    let path = workspace.join(".symphony-mcp.json");
    std::fs::write(&path, serde_json::to_string_pretty(&config)?)?;
    Ok(path)
}

struct ClaudeSession {
    command: String,
    extra_args: Vec<String>,
    model: Option<String>,
    permission_mode: String,
    turn_timeout_ms: u64,
    workspace: PathBuf,
    container: Option<ContainerHandle>,
    container_workspace_path: Option<PathBuf>,
    mcp_config_path: Option<PathBuf>,
    session_id: Option<String>,
}

#[async_trait]
impl AgentSession for ClaudeSession {
    fn session_id(&self) -> &str {
        self.session_id.as_deref().unwrap_or("")
    }

    async fn run_turn(
        &mut self,
        prompt: &str,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<TurnOutcome, AgentError> {
        let mut claude_args: Vec<String> = self.extra_args.clone();
        claude_args.extend([
            "-p".to_string(),
            prompt.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--permission-mode".to_string(),
            self.permission_mode.clone(),
        ]);
        if let Some(model) = &self.model {
            claude_args.push("--model".to_string());
            claude_args.push(model.clone());
        }
        if let Some(sid) = &self.session_id {
            claude_args.push("--resume".to_string());
            claude_args.push(sid.clone());
        }
        if let Some(mcp_config_path) = &self.mcp_config_path {
            // `claude`'s cwd is already the workspace (host or in-container, set
            // below), and it appears to mis-resolve an absolute --mcp-config path
            // against that cwd (doubling it) rather than using it as-is. Passing just
            // the filename sidesteps that.
            let file_name = mcp_config_path
                .file_name()
                .unwrap_or(mcp_config_path.as_os_str());
            claude_args.push("--mcp-config".to_string());
            claude_args.push(file_name.to_string_lossy().to_string());
            claude_args.push("--strict-mcp-config".to_string());
            claude_args.push("--allowedTools".to_string());
            claude_args.push("mcp__symphony__*".to_string());
        }

        let mut kill_guard: Option<ContainerKillGuard> = None;
        let mut cmd = match (&self.container, &self.container_workspace_path) {
            (Some(container), Some(container_workspace)) => {
                let mut c = Command::new("docker");
                c.arg("exec")
                    .arg("-w")
                    .arg(container_workspace.to_string_lossy().to_string())
                    .arg(&container.name)
                    .arg(&self.command)
                    .args(&claude_args);
                kill_guard = Some(ContainerKillGuard::armed(
                    container.name.clone(),
                    self.command.clone(),
                ));
                c
            }
            _ => {
                let mut c = Command::new(&self.command);
                c.args(&claude_args).current_dir(&self.workspace);
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

        // Kept around (last few lines only) so the "no result event" fallback below can
        // surface *something* diagnostic on `/events` instead of a bare "no result
        // event" with no clue why -- see BUG-1.
        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        if let Some(stderr) = child.stderr.take() {
            let stderr_tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!(target: "claude_stderr", "{line}");
                    let mut tail = stderr_tail.lock().unwrap();
                    tail.push(line);
                    let len = tail.len();
                    if len > 5 {
                        tail.drain(0..len - 5);
                    }
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

        // The subprocess (host `claude`, or the `docker exec` client) exited on its
        // own by this point -- whatever it was attached to inside a container is
        // already gone too, so the guard's cleanup-on-drop would be a no-op anyway.
        // Disarming just skips the pointless `pkill` call.
        if let Some(guard) = &mut kill_guard {
            guard.disarm();
        }

        match outcome {
            Some(o) => Ok(o),
            None if status.success() => {
                // The subprocess exited cleanly but stdout hit EOF before a `"type":
                // "result"` line ever arrived -- BUG-1's "narrow race" (reconciliation
                // preempting a turn) or an unrecognized terminal message shape. Either
                // way, surface it on `/events` rather than let this look like an
                // ordinary free turn.
                let tail = stderr_tail.lock().unwrap().join(" | ");
                let msg = if tail.is_empty() {
                    "no result event seen before stdout EOF; token usage unavailable for this turn"
                        .to_string()
                } else {
                    format!(
                        "no result event seen before stdout EOF; token usage unavailable for this turn (stderr tail: {tail})"
                    )
                };
                tracing::warn!(issue = "no result event before EOF, usage unavailable", stderr_tail = %tail);
                let _ = events.send(AgentEvent::new("turn_completed").with_message(msg));
                Ok(TurnOutcome::Completed { usage: None })
            }
            None => Ok(TurnOutcome::Failed {
                reason: format!("subprocess exited with {status} and no result event"),
            }),
        }
    }

    async fn stop(self: Box<Self>) {
        // Each turn is a self-contained subprocess; nothing persistent to tear down.
    }
}

impl ClaudeSession {
    /// Handle one parsed stream-json line. Returns `Some(outcome)` only for the
    /// terminal `result` event.
    fn handle_message(
        &mut self,
        v: &Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Option<TurnOutcome> {
        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match msg_type {
            "system" if v.get("subtype").and_then(|s| s.as_str()) == Some("init") => {
                if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                    self.session_id = Some(sid.to_string());
                }
                let _ = events.send(AgentEvent::new("session_started"));
                None
            }
            "assistant" | "user" => {
                if let Some(text) = extract_text(v) {
                    let _ = events.send(AgentEvent::new("notification").with_message(text));
                }
                for (tool_name, input) in extract_tool_uses(v) {
                    // `raise_clarification` (AIR-4) is the one provider-native tool whose
                    // arguments the orchestrator itself must see, not just the coding
                    // agent: a `blocking: true` call has to stop the cycle promptly
                    // rather than run out the stage's turn budget first. Surfaced as its
                    // own event (alongside the generic `tool_call`) so `run_turn_loop`
                    // can detect it without every other tool call needing this.
                    if tool_name.ends_with("raise_clarification") {
                        let _ = events.send(
                            AgentEvent::new("clarification_raised").with_message(input.to_string()),
                        );
                    }
                    let _ = events.send(AgentEvent::new("tool_call").with_message(tool_name));
                }
                None
            }
            "result" => {
                if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                    self.session_id = Some(sid.to_string());
                }
                let usage = extract_usage(v);
                if usage.is_none() {
                    // Deliberately visible on `/events`, not just tracing -- a `result`
                    // event with no readable `usage` field looks identical to a free
                    // turn otherwise, and BUG-1 was exactly that kind of silent gap.
                    tracing::warn!(raw = %v, "claude result event carried no usage field");
                }
                let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
                if is_error {
                    let reason = v
                        .get("result")
                        .and_then(|r| r.as_str())
                        .unwrap_or("claude reported is_error=true")
                        .to_string();
                    let _ =
                        events.send(AgentEvent::new("turn_failed").with_message(reason.clone()));
                    Some(TurnOutcome::Failed { reason })
                } else {
                    let _ = events.send({
                        let mut e = AgentEvent::new("turn_completed");
                        match usage.clone() {
                            Some(u) => e = e.with_usage(u),
                            // Visible on `/events` and in `symphony-report.html`'s
                            // per-issue table (rather than reading as a normal,
                            // cheap-but-real turn) -- see BUG-1.
                            None => e = e.with_message(
                                "usage unavailable: claude's result event carried no usage field",
                            ),
                        }
                        e
                    });
                    Some(TurnOutcome::Completed { usage })
                }
            }
            "" => {
                let _ = events.send(AgentEvent::new("malformed"));
                None
            }
            other => {
                let _ =
                    events.send(AgentEvent::new("other_message").with_message(other.to_string()));
                None
            }
        }
    }
}

fn extract_text(v: &Value) -> Option<String> {
    let content = v.get("message")?.get("content")?.as_array()?;
    let mut out = String::new();
    for block in content {
        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// `(name, input)` of every `tool_use` content block in an assistant message (Section
/// 10.4's event vocabulary doesn't name tool calls explicitly; we surface them as our
/// own `tool_call` event for usage metrics, plus `input` so a handful of tools whose
/// arguments matter to the orchestrator itself -- currently just `raise_clarification`,
/// AIR-4 -- can be inspected without a second parse pass).
fn extract_tool_uses(v: &Value) -> Vec<(String, Value)> {
    let Some(content) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        .filter_map(|block| {
            let name = block.get("name").and_then(|n| n.as_str())?.to_string();
            let input = block.get("input").cloned().unwrap_or(json!({}));
            Some((name, input))
        })
        .collect()
}

/// Extract token usage leniently from common field names (Section 13.5).
fn extract_usage(v: &Value) -> Option<TokenUsage> {
    let usage = v.get("usage")?;
    let input = usage
        .get("input_tokens")
        .or_else(|| usage.get("inputTokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .or_else(|| usage.get("outputTokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    Some(TokenUsage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
    })
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

    #[test]
    fn extracts_text_blocks() {
        let v = json!({"message": {"content": [{"type": "text", "text": "hello"}]}});
        assert_eq!(extract_text(&v).as_deref(), Some("hello"));
    }

    #[test]
    fn text_only_message_has_no_tool_uses() {
        let v = json!({"message": {"content": [{"type": "text", "text": "hello"}]}});
        assert!(extract_tool_uses(&v).is_empty());
    }

    #[test]
    fn extracts_single_tool_use() {
        let v = json!({"message": {"content": [
            {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}
        ]}});
        assert_eq!(
            extract_tool_uses(&v),
            vec![("Bash".to_string(), json!({"command": "ls"}))]
        );
        assert_eq!(extract_text(&v), None);
    }

    #[test]
    fn extracts_multiple_tool_uses_and_mixed_text() {
        let v = json!({"message": {"content": [
            {"type": "text", "text": "doing two things"},
            {"type": "tool_use", "name": "Read", "input": {}},
            {"type": "tool_use", "name": "mcp__symphony__update_issue_state", "input": {"state": "done"}}
        ]}});
        assert_eq!(extract_text(&v).as_deref(), Some("doing two things"));
        assert_eq!(
            extract_tool_uses(&v),
            vec![
                ("Read".to_string(), json!({})),
                (
                    "mcp__symphony__update_issue_state".to_string(),
                    json!({"state": "done"})
                )
            ]
        );
    }

    #[test]
    fn raise_clarification_tool_call_also_emits_clarification_raised_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let v = json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "mcp__symphony__raise_clarification",
             "input": {"question": "which auth scheme?", "blocking": true}}
        ]}});
        let mut session = ClaudeSession {
            command: "claude".to_string(),
            extra_args: Vec::new(),
            model: None,
            permission_mode: "default".to_string(),
            turn_timeout_ms: 1000,
            workspace: PathBuf::from("."),
            container: None,
            container_workspace_path: None,
            mcp_config_path: None,
            session_id: None,
        };
        session.handle_message(&v, &tx);
        drop(tx);
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(events.iter().any(|e| e.event == "clarification_raised"
            && e.message.as_deref()
                == Some(r#"{"blocking":true,"question":"which auth scheme?"}"#)));
        assert!(events.iter().any(|e| e.event == "tool_call"
            && e.message.as_deref() == Some("mcp__symphony__raise_clarification")));
    }

    #[test]
    fn missing_content_is_handled_gracefully() {
        let v = json!({"type": "system"});
        assert_eq!(extract_text(&v), None);
        assert!(extract_tool_uses(&v).is_empty());
    }

    #[test]
    fn unrestricted_tool_policy_adds_no_flags() {
        let args = tool_policy_extra_args(&["--verbose".to_string()], &ToolPolicy::default());
        assert_eq!(args, vec!["--verbose".to_string()]);
    }

    #[test]
    fn edits_denied_produces_disallowed_tools_flag() {
        let policy = ToolPolicy {
            allow_edits: false,
            allow_commands: true,
        };
        let args = tool_policy_extra_args(&[], &policy);
        assert_eq!(
            args,
            vec![
                "--disallowedTools".to_string(),
                "Edit,Write,NotebookEdit".to_string()
            ]
        );
    }

    #[test]
    fn commands_denied_adds_bash_to_disallowed_tools() {
        let policy = ToolPolicy {
            allow_edits: false,
            allow_commands: false,
        };
        let args = tool_policy_extra_args(&[], &policy);
        assert_eq!(args[1], "Edit,Write,NotebookEdit,Bash");
    }

    /// Regression test for BUG-1 ("token usage is always 0"): built from a real
    /// `"type":"result"` line captured from an actual `claude` CLI 2.1.223 run against
    /// this repo's own MCP wiring (`--resume`, `--mcp-config --strict-mcp-config`,
    /// `--allowedTools mcp__symphony__*`, ending on an auto-approved MCP tool call) --
    /// not a hand-written guess at the JSON shape. Extensive live reproduction attempts
    /// (single turns, `--resume`ed turns, 12-30 tool-call turns, two concurrent
    /// sessions, and two full end-to-end `symphony` dispatches) all correctly captured
    /// non-zero usage; the original incident's exact trigger stayed unreproduced. This
    /// pins the known-good parse path so a future CLI update that silently renames or
    /// moves the `usage` field is caught here instead of showing up as 0 tokens again.
    #[test]
    fn extracts_real_usage_from_a_captured_result_transcript() {
        let raw = include_str!("testdata/claude_real_result_event.json");
        let v: Value = serde_json::from_str(raw).expect("captured fixture must be valid JSON");
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("result"));

        let usage = extract_usage(&v).expect("real captured result event must yield usage");
        assert_eq!(usage.input_tokens, 8);
        assert_eq!(usage.output_tokens, 353);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = ClaudeSession {
            command: "claude".to_string(),
            extra_args: Vec::new(),
            model: None,
            permission_mode: "default".to_string(),
            turn_timeout_ms: 1000,
            workspace: PathBuf::from("."),
            container: None,
            container_workspace_path: None,
            mcp_config_path: None,
            session_id: None,
        };
        let outcome = session.handle_message(&v, &tx);
        drop(tx);
        match outcome {
            Some(TurnOutcome::Completed { usage: Some(u) }) => {
                assert_eq!(u.input_tokens, 8);
                assert_eq!(u.output_tokens, 353);
            }
            other => panic!("expected Completed with usage, got {other:?}"),
        }
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let completed = events
            .iter()
            .find(|e| e.event == "turn_completed")
            .expect("turn_completed event must be emitted");
        assert!(completed.usage.is_some());
    }

    /// A `"result"` event with `is_error: true` and no `usage` field at all (a real
    /// shape the CLI does emit, e.g. an auth failure) must not be mistaken for a
    /// successful-but-unmetered turn -- BUG-1's fallback path is for the *EOF-with-no-
    /// result* case, not this one.
    #[test]
    fn errored_result_with_no_usage_is_reported_as_failed_not_completed() {
        let v = json!({"type": "result", "is_error": true, "result": "boom"});
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = ClaudeSession {
            command: "claude".to_string(),
            extra_args: Vec::new(),
            model: None,
            permission_mode: "default".to_string(),
            turn_timeout_ms: 1000,
            workspace: PathBuf::from("."),
            container: None,
            container_workspace_path: None,
            mcp_config_path: None,
            session_id: None,
        };
        let outcome = session.handle_message(&v, &tx);
        drop(tx);
        assert!(matches!(outcome, Some(TurnOutcome::Failed { reason }) if reason == "boom"));
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(events.iter().any(|e| e.event == "turn_failed"));
    }
}
