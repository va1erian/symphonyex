//! Codex app-server backend.
//!
//! **Verification caveat**: Section 10 of the spec is explicit that "the Codex app-server
//! protocol for the targeted Codex version is the source of truth" and that
//! implementations must consult `codex app-server generate-json-schema` for the
//! installed version rather than treating the spec text as a protocol schema. This
//! module implements the transport-level contract Symphony controls (launch via
//! `bash -lc <codex.command>` in the workspace, newline-delimited JSON-RPC 2.0 framing,
//! stdout/stderr kept separate, read/turn timeouts) and a best-effort guess at method
//! names (`initialize`, `thread/start`, `turn/start`) and completion-notification
//! matching (substring match on `turn` + `completed`/`failed`/`cancelled` in the
//! notification method name, with token usage extracted leniently from any numeric
//! field named like `*token*`). Treat the method names as a starting point to adjust
//! against the schema for your installed Codex build, not as a verified contract.

use super::{
    AgentBackend, AgentError, AgentEvent, AgentSession, TokenUsage, ToolPolicy, TurnOutcome,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

pub struct CodexBackend {
    pub command: String,
    pub approval_policy: Option<String>,
    pub thread_sandbox: Option<String>,
    pub turn_sandbox_policy: Option<String>,
    pub turn_timeout_ms: u64,
    pub read_timeout_ms: u64,
}

#[async_trait]
impl AgentBackend for CodexBackend {
    async fn start_session(
        &self,
        workspace: &Path,
        _issue_id: &str,
        title: &str,
        _container: Option<&crate::container::ContainerHandle>,
        tool_policy: &ToolPolicy,
    ) -> Result<Box<dyn AgentSession>, AgentError> {
        if !workspace.is_dir() {
            return Err(AgentError::InvalidCwd(format!(
                "{workspace:?} is not a directory"
            )));
        }

        // AIR-2: `codex` has no known native tool-denial mechanism (no
        // `--disallowedTools`-equivalent flag in the app-server protocol as
        // documented). Refuse to start rather than silently running an unrestricted
        // session under a role/SweBot config that asked for one -- the same posture
        // `swebot::build_restricted_backend` already takes for this backend.
        if tool_policy.is_restricted() {
            return Err(AgentError::UnsupportedToolPolicy(
                "codex backend has no native tool-restriction mechanism yet -- \
                 a restricted role or SweBot session cannot run on codex"
                    .to_string(),
            ));
        }

        // Unlike hooks (see src/hooks.rs's module doc for the argv-corruption pitfalls
        // this avoids), this process needs its stdin free for the ongoing JSON-RPC
        // protocol after launch, so the script-over-stdin trick hooks use doesn't
        // apply here. `codex.command` is documented as a simple shell command string
        // (default `codex app-server`), not an arbitrary multi-line script, so plain
        // `bash -lc <command>` argv passing is expected to be safe in practice.
        let mut child = Command::new("bash")
            .arg("-lc")
            .arg(&self.command)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                AgentError::NotFound(format!("failed to launch '{}': {e}", self.command))
            })?;

        // Diagnostic stderr is drained separately from the protocol stream (Section 10.3).
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "codex_stderr", "{line}");
                }
            });
        }

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let reader = BufReader::new(stdout).lines();

        let mut session = CodexSession {
            child,
            stdin,
            reader,
            next_id: AtomicU64::new(1),
            thread_id: String::new(),
            session_id: String::new(),
            turn_timeout_ms: self.turn_timeout_ms,
            read_timeout_ms: self.read_timeout_ms,
        };

        session
            .call(
                "initialize",
                json!({"clientInfo": {"name": "symphony", "version": env!("CARGO_PKG_VERSION")}}),
            )
            .await?;

        let thread_result = session
            .call(
                "thread/start",
                json!({
                    "cwd": workspace.to_string_lossy(),
                    "approvalPolicy": self.approval_policy,
                    "sandbox": self.thread_sandbox,
                    "sandboxPolicy": self.turn_sandbox_policy,
                    "title": title,
                }),
            )
            .await?;

        session.thread_id = thread_result
            .get("thread_id")
            .or_else(|| thread_result.get("threadId"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        Ok(Box::new(session))
    }
}

struct CodexSession {
    child: Child,
    stdin: ChildStdin,
    reader: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: AtomicU64,
    thread_id: String,
    session_id: String,
    turn_timeout_ms: u64,
    read_timeout_ms: u64,
}

impl CodexSession {
    async fn send(&mut self, payload: &Value) -> Result<(), AgentError> {
        let mut line =
            serde_json::to_string(payload).map_err(|e| AgentError::ResponseError(e.to_string()))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| AgentError::ProcessExit(e.to_string()))
    }

    /// Send a JSON-RPC request and wait (bounded by `read_timeout_ms`) for the matching
    /// `result`/`error` response, skipping unrelated notifications in between.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, AgentError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;

        let deadline = Duration::from_millis(self.read_timeout_ms);
        loop {
            let line = tokio::time::timeout(deadline, self.reader.next_line())
                .await
                .map_err(|_| AgentError::ResponseTimeout(format!("no response to '{method}'")))?
                .map_err(|e| AgentError::ProcessExit(e.to_string()))?
                .ok_or_else(|| {
                    AgentError::ProcessExit("codex app-server closed stdout".to_string())
                })?;

            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if let Some(err) = msg.get("error") {
                    return Err(AgentError::ResponseError(err.to_string()));
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            // Not our response; a real implementation would forward this notification
            // to the orchestrator event stream here.
        }
    }
}

#[async_trait]
impl AgentSession for CodexSession {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    async fn run_turn(
        &mut self,
        prompt: &str,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<TurnOutcome, AgentError> {
        let turn_id_val = self
            .call(
                "turn/start",
                json!({"thread_id": self.thread_id, "prompt": prompt}),
            )
            .await?;
        let turn_id = turn_id_val
            .get("turn_id")
            .or_else(|| turn_id_val.get("turnId"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        self.session_id = format!("{}-{}", self.thread_id, turn_id);
        let _ = events.send(AgentEvent::new("session_started"));

        let deadline = Duration::from_millis(self.turn_timeout_ms);
        loop {
            let line = match tokio::time::timeout(deadline, self.reader.next_line()).await {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => {
                    return Err(AgentError::ProcessExit(
                        "codex app-server closed stdout".to_string(),
                    ));
                }
                Ok(Err(e)) => return Err(AgentError::ProcessExit(e.to_string())),
                Err(_) => {
                    return Err(AgentError::TurnTimeout(format!(
                        "no turn activity for {}ms",
                        self.turn_timeout_ms
                    )));
                }
            };

            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                let _ = events.send(AgentEvent::new("malformed"));
                continue;
            };

            let usage = extract_usage_leniently(&msg);
            let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let lower = method.to_lowercase();

            if lower.contains("turn") && lower.contains("completed") {
                let mut e = AgentEvent::new("turn_completed");
                if let Some(u) = usage.clone() {
                    e = e.with_usage(u);
                }
                let _ = events.send(e);
                return Ok(TurnOutcome::Completed { usage });
            }
            if lower.contains("turn") && (lower.contains("failed") || lower.contains("error")) {
                let _ = events.send(AgentEvent::new("turn_failed"));
                return Ok(TurnOutcome::Failed {
                    reason: format!("codex reported '{method}'"),
                });
            }
            if lower.contains("turn") && lower.contains("cancel") {
                let _ = events.send(AgentEvent::new("turn_cancelled"));
                return Ok(TurnOutcome::Failed {
                    reason: "turn cancelled".to_string(),
                });
            }
            if lower.contains("input") && lower.contains("required") {
                // High-trust example policy (Section 10.5): fail rather than stall.
                let _ = events.send(AgentEvent::new("turn_input_required"));
                return Ok(TurnOutcome::Failed {
                    reason: "turn_input_required".to_string(),
                });
            }
            let _ = events.send(AgentEvent::new("notification"));
        }
    }

    async fn stop(mut self: Box<Self>) {
        let _ = self
            .send(&json!({"jsonrpc": "2.0", "method": "shutdown", "params": {}}))
            .await;
        let _ = self.child.start_kill();
    }
}

fn extract_usage_leniently(msg: &Value) -> Option<TokenUsage> {
    fn find_u64(v: &Value, needle: &str) -> Option<u64> {
        match v {
            Value::Object(map) => {
                for (k, val) in map {
                    if k.to_lowercase().contains(needle)
                        && let Some(n) = val.as_u64()
                    {
                        return Some(n);
                    }
                    if let Some(n) = find_u64(val, needle) {
                        return Some(n);
                    }
                }
                None
            }
            Value::Array(items) => items.iter().find_map(|v| find_u64(v, needle)),
            _ => None,
        }
    }
    let input = find_u64(msg, "input_token").or_else(|| find_u64(msg, "inputtoken"));
    let output = find_u64(msg, "output_token").or_else(|| find_u64(msg, "outputtoken"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// AIR-2 acceptance criterion: `allow_edits: false` must produce "a clean startup
    /// error for `codex`" -- codex has no native tool-denial mechanism, so a restricted
    /// session must be refused outright rather than silently running unrestricted.
    #[tokio::test]
    async fn restricted_tool_policy_is_refused_before_spawning_anything() {
        let backend = CodexBackend {
            command: "definitely-not-a-real-binary-xyz".to_string(),
            approval_policy: None,
            thread_sandbox: None,
            turn_sandbox_policy: None,
            turn_timeout_ms: 1_000,
            read_timeout_ms: 1_000,
        };
        let workspace = tempdir().unwrap();
        let policy = ToolPolicy {
            allow_edits: false,
            allow_commands: true,
        };
        let result = backend
            .start_session(workspace.path(), "issue-1", "t", None, &policy)
            .await;
        assert!(matches!(result, Err(AgentError::UnsupportedToolPolicy(_))));
    }

    #[tokio::test]
    async fn unrestricted_tool_policy_proceeds_past_the_refusal_check() {
        let backend = CodexBackend {
            command: "definitely-not-a-real-binary-xyz".to_string(),
            approval_policy: None,
            thread_sandbox: None,
            turn_sandbox_policy: None,
            turn_timeout_ms: 1_000,
            read_timeout_ms: 1_000,
        };
        let workspace = tempdir().unwrap();
        let result = backend
            .start_session(workspace.path(), "issue-1", "t", None, &ToolPolicy::default())
            .await;
        // Fails later for an unrelated reason (no such binary/RPC handshake), not on
        // the tool-policy check.
        assert!(!matches!(result, Err(AgentError::UnsupportedToolPolicy(_))));
    }
}
