//! Minimal MCP (Model Context Protocol) stdio tool server.
//!
//! Runs as `symphony __mcp_tool_server ...`, spawned by the `claude` CLI itself (per an
//! `--mcp-config` file we generate — see `agent::claude`), not by the running
//! orchestrator process. This is what lets a tracker adapter's provider-native tools
//! (Section 10.5) reach the coding agent: this subprocess re-builds the same tracker
//! adapter from the same config and executes tool calls host-side, so the coding-agent
//! process itself never touches tracker storage directly (Section 11.5).
//!
//! Protocol: JSON-RPC 2.0, one message per line on stdin/stdout (MCP's stdio
//! transport). Only the methods a tools-only server needs are handled: `initialize`,
//! `notifications/initialized`, `tools/list`, `tools/call`, `ping`.
//!
//! stdout is reserved for protocol messages only — all logging in this process must go
//! to stderr (enforced in `main.rs`).

use crate::tracker::TrackerAdapter;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub async fn run_stdio_server(
    adapter: Box<dyn TrackerAdapter>,
    issue_id: &str,
) -> anyhow::Result<()> {
    let specs = adapter.agent_tool_specs();

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            tracing::warn!("mcp: ignoring non-JSON line on stdin");
            continue;
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

        match method {
            "initialize" => {
                write_result(
                    &mut stdout,
                    id,
                    json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "symphony", "version": env!("CARGO_PKG_VERSION")}
                    }),
                )
                .await?;
            }
            "notifications/initialized" | "notifications/cancelled" => {
                // No response required for notifications (no `id`).
            }
            "ping" => {
                write_result(&mut stdout, id, json!({})).await?;
            }
            "tools/list" => {
                let tools: Vec<Value> = specs
                    .iter()
                    .map(|s| json!({"name": s.name, "description": s.description, "inputSchema": s.input_schema}))
                    .collect();
                write_result(&mut stdout, id, json!({"tools": tools})).await?;
            }
            "tools/call" => {
                let name = msg
                    .pointer("/params/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let arguments = msg
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or(json!({}));
                let result = adapter.execute_agent_tool(name, arguments, issue_id).await;
                write_result(
                    &mut stdout,
                    id,
                    json!({
                        "content": [{"type": "text", "text": result.content}],
                        "isError": !result.success
                    }),
                )
                .await?;
            }
            other => {
                if id.is_some() {
                    write_error(
                        &mut stdout,
                        id,
                        -32601,
                        &format!("method not found: {other}"),
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}

async fn write_result(
    stdout: &mut tokio::io::Stdout,
    id: Option<Value>,
    result: Value,
) -> anyhow::Result<()> {
    write_line(
        stdout,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
    .await
}

async fn write_error(
    stdout: &mut tokio::io::Stdout,
    id: Option<Value>,
    code: i64,
    message: &str,
) -> anyhow::Result<()> {
    write_line(
        stdout,
        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    )
    .await
}

async fn write_line(stdout: &mut tokio::io::Stdout, value: &Value) -> anyhow::Result<()> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    stdout.write_all(s.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}
