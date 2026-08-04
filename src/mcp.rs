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

use crate::repo_host::RepoHost;
use crate::tracker::{ToolResult, TrackerAdapter};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Route a `tools/call` to whichever of the two independent tool sources actually
/// owns `name` (Section "PR-based branch workflow"): the tracker's own tools (e.g.
/// `update_issue_state`) or, when `repo.pull_request` is enabled, `open_pull_request`
/// from `repo_host` -- a pull request is a property of the code host, not the issue
/// tracker, so it's kept as its own capability rather than folded into
/// `TrackerAdapter`. `tracker_names` decides routing explicitly (not by matching on
/// the tracker's own "unsupported tool" error text, which would be fragile).
async fn route_call(
    adapter: &dyn TrackerAdapter,
    repo_host: Option<&dyn RepoHost>,
    tracker_names: &HashSet<&str>,
    name: &str,
    arguments: Value,
    issue_id: &str,
    workspace_dir: &Path,
) -> ToolResult {
    if tracker_names.contains(name) {
        adapter.execute_agent_tool(name, arguments, issue_id).await
    } else if let Some(host) = repo_host {
        host.execute_agent_tool(name, arguments, issue_id, workspace_dir)
            .await
    } else {
        ToolResult::error(format!("unsupported tool '{name}'"))
    }
}

pub async fn run_stdio_server(
    adapter: Box<dyn TrackerAdapter>,
    repo_host: Option<Box<dyn RepoHost>>,
    issue_id: &str,
    workspace_dir: &Path,
) -> anyhow::Result<()> {
    let tracker_specs = adapter.agent_tool_specs();
    let repo_specs = repo_host
        .as_ref()
        .map(|r| r.agent_tool_specs())
        .unwrap_or_default();
    let tracker_names: HashSet<&str> = tracker_specs.iter().map(|s| s.name.as_str()).collect();
    let specs: Vec<_> = tracker_specs.iter().chain(repo_specs.iter()).collect();

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
                let result = route_call(
                    adapter.as_ref(),
                    repo_host.as_deref(),
                    &tracker_names,
                    name,
                    arguments,
                    issue_id,
                    workspace_dir,
                )
                .await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoConfig;
    use crate::domain::Issue;
    use crate::repo_host::github::GithubRepoHost;
    use crate::tracker::{ToolResult, ToolSpec, TrackerError};
    use async_trait::async_trait;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Exposes exactly one tool (`update_issue_state`), mirroring the shape of a
    /// real adapter closely enough to prove routing picks the tracker over the repo
    /// host for names it owns, and vice versa.
    struct FakeTracker;

    #[async_trait]
    impl TrackerAdapter for FakeTracker {
        async fn fetch_issues_by_states(
            &self,
            _states: &[String],
        ) -> Result<Vec<Issue>, TrackerError> {
            Ok(Vec::new())
        }
        async fn fetch_issues_by_ids(&self, _ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
            Ok(Vec::new())
        }
        fn agent_tool_specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "update_issue_state".to_string(),
                description: "fake".to_string(),
                input_schema: json!({}),
            }]
        }
        async fn execute_agent_tool(
            &self,
            name: &str,
            _arguments: Value,
            _issue_id: &str,
        ) -> ToolResult {
            ToolResult::ok(format!("tracker handled {name}"))
        }
    }

    async fn repo_host_against_mock(server: &MockServer) -> GithubRepoHost {
        unsafe {
            std::env::set_var("SYMPHONY_TEST_MCP_TOKEN", "t");
        }
        GithubRepoHost::new(&RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            token_env: Some("SYMPHONY_TEST_MCP_TOKEN".to_string()),
            pull_request: true,
            ..Default::default()
        })
        .unwrap()
        .with_base_url_for_test(&server.uri())
    }

    #[tokio::test]
    async fn routes_tracker_owned_tool_to_the_tracker() {
        let server = MockServer::start().await;
        let host = repo_host_against_mock(&server).await;
        let tracker = FakeTracker;
        let specs = tracker.agent_tool_specs();
        let names: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();

        let result = route_call(
            &tracker,
            Some(&host),
            &names,
            "update_issue_state",
            json!({}),
            "1",
            Path::new("."),
        )
        .await;
        assert!(result.success);
        assert_eq!(result.content, "tracker handled update_issue_state");
    }

    #[tokio::test]
    async fn routes_repo_owned_tool_to_the_repo_host() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<Value>::new()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "number": 1,
                "html_url": "https://github.com/owner/name/pull/1"
            })))
            .mount(&server)
            .await;
        let host = repo_host_against_mock(&server).await;
        let tracker = FakeTracker;
        let specs = tracker.agent_tool_specs();
        let names: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();

        let result = route_call(
            &tracker,
            Some(&host),
            &names,
            "open_pull_request",
            json!({"title": "t", "body": "b"}),
            "1",
            Path::new("."),
        )
        .await;
        assert!(result.success, "{}", result.content);
    }

    #[tokio::test]
    async fn unknown_tool_name_errors_without_a_repo_host() {
        let tracker = FakeTracker;
        let specs = tracker.agent_tool_specs();
        let names: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();

        let result = route_call(
            &tracker,
            None,
            &names,
            "nonexistent",
            json!({}),
            "1",
            Path::new("."),
        )
        .await;
        assert!(!result.success);
    }

    #[test]
    fn tool_list_unions_both_sources() {
        let tracker_specs = [ToolSpec {
            name: "update_issue_state".to_string(),
            description: String::new(),
            input_schema: json!({}),
        }];
        let repo_specs = [ToolSpec {
            name: "open_pull_request".to_string(),
            description: String::new(),
            input_schema: json!({}),
        }];
        let merged: Vec<_> = tracker_specs.iter().chain(repo_specs.iter()).collect();
        let names: Vec<&str> = merged.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["update_issue_state", "open_pull_request"]);
    }
}
