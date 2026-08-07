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

use crate::artifacts;
use crate::repo_host::RepoHost;
use crate::tracker::{ToolResult, ToolSpec, TrackerAdapter};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// `record_artifact`'s own wiring (`crate::artifacts`), present only when
/// `pipeline.enabled` -- see this module's own doc comment and `artifacts.rs`'s. Kept
/// as its own struct (rather than two loose `PathBuf` params threaded everywhere)
/// since both fields always travel together.
pub struct ArtifactsWiring {
    pub db_path: PathBuf,
    pub workflow_dir: PathBuf,
}

/// The delivery pipeline's own second tool source (AIR-4), independent of tracker kind,
/// repo host, and of `ArtifactsWiring`/`record_artifact` above -- `record_requirements`,
/// `record_acceptance_criteria` and `raise_clarification` are properties of the
/// pipeline stage running, not of any particular tracker/host integration, so (like the
/// "PR-based branch workflow" note on `route_call` explains for `open_pull_request`)
/// they get their own tool source rather than being bolted onto `TrackerAdapter` or
/// `RepoHost`. Only constructed when `pipeline.enabled` (main.rs), so a project not
/// using the pipeline never sees these tools at all. Uses its own storage
/// (`eventlog::put_artifact`/`get_artifact`, the `artifact_snapshots` table) rather
/// than `crate::artifacts`' file-based store -- a known duplication worth reconciling
/// later, not unified here.
pub struct PipelineToolCtx {
    /// `workflow_dir/symphony.db` -- the same event log/artifact store the running
    /// orchestrator process writes to (Section: `eventlog.rs`).
    pub db_path: PathBuf,
}

fn pipeline_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "record_requirements".to_string(),
            description: "Record the full, current set of validated requirements for \
                this issue (replaces any previously recorded set). Call this once you \
                have extracted every requirement you're confident about -- do not \
                invent a requirement you're not sure of; call raise_clarification \
                instead."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "requirements": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string", "description": "Stable id, e.g. 'R1'"},
                                "statement": {"type": "string"},
                                "source": {"type": "string", "description": "Where this came from, e.g. 'issue description' or 'DEMO-1 depends_on'"},
                                "type": {"type": "string", "enum": ["functional", "non_functional"]},
                                "constraint": {"type": "string", "description": "Set for a constraint this requirement imposes (e.g. a performance/security/operability bound)"},
                                "dependency": {"type": "string", "description": "Set when this requirement is blocked on another issue (from depends_on)"},
                                "assumption": {"type": "boolean", "description": "true if this requirement was decided via a non-blocking clarification rather than stated explicitly"}
                            },
                            "required": ["id", "statement", "source", "type"]
                        }
                    }
                },
                "required": ["requirements"]
            }),
        },
        ToolSpec {
            name: "record_acceptance_criteria".to_string(),
            description: "Record the full, current set of acceptance criteria for this \
                issue (replaces any previously recorded set). Each criterion must \
                reference the requirement id(s) it verifies."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "acceptance_criteria": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string", "description": "Stable id, e.g. 'AC1'"},
                                "requirement_ids": {"type": "array", "items": {"type": "string"}},
                                "given": {"type": "string"},
                                "when": {"type": "string"},
                                "then": {"type": "string"}
                            },
                            "required": ["id", "requirement_ids", "given", "when", "then"]
                        }
                    }
                },
                "required": ["acceptance_criteria"]
            }),
        },
        ToolSpec {
            name: "raise_clarification".to_string(),
            description: "Ask about a requirement you cannot resolve instead of \
                guessing. `blocking: true` stops this cycle and parks the issue until \
                a human answers (use this when guessing wrong would be costly or \
                irreversible). `blocking: false` means you'll proceed on a documented \
                assumption -- follow up by calling record_requirements with that \
                requirement's `assumption: true`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "blocking": {"type": "boolean"},
                    "requirement_id": {"type": "string", "description": "The requirement id this clarification concerns, if any"}
                },
                "required": ["question", "blocking"]
            }),
        },
    ]
}

fn json_array_or_error(value: &Value, field: &str) -> Result<Vec<Value>, ToolResult> {
    value
        .get(field)
        .and_then(|v| v.as_array())
        .cloned()
        .ok_or_else(|| ToolResult::error(format!("missing required array argument '{field}'")))
}

/// `record_requirements`/`record_acceptance_criteria` each store their argument
/// verbatim as the artifact's JSON content after a light shape check (every item has
/// its required string/array fields) -- schema conformance for the artifact itself is
/// enforced here so a malformed entry is rejected before it's ever persisted, rather
/// than trusting the agent's JSON blindly.
fn execute_pipeline_tool(ctx: &PipelineToolCtx, name: &str, arguments: Value, issue_id: &str) -> ToolResult {
    match name {
        "record_requirements" => {
            let items = match json_array_or_error(&arguments, "requirements") {
                Ok(v) => v,
                Err(e) => return e,
            };
            for (i, item) in items.iter().enumerate() {
                for field in ["id", "statement", "source", "type"] {
                    if item.get(field).and_then(|v| v.as_str()).is_none() {
                        return ToolResult::error(format!(
                            "requirements[{i}] is missing required string field '{field}'"
                        ));
                    }
                }
                let ty = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if ty != "functional" && ty != "non_functional" {
                    return ToolResult::error(format!(
                        "requirements[{i}].type must be 'functional' or 'non_functional', got '{ty}'"
                    ));
                }
            }
            let count = items.len();
            let content = Value::Array(items).to_string();
            match crate::eventlog::put_artifact(&ctx.db_path, issue_id, "requirements", &content) {
                Ok(()) => ToolResult::ok(format!("Recorded {count} requirement(s).")),
                Err(e) => ToolResult::error(format!("failed to record requirements: {e}")),
            }
        }
        "record_acceptance_criteria" => {
            let items = match json_array_or_error(&arguments, "acceptance_criteria") {
                Ok(v) => v,
                Err(e) => return e,
            };
            for (i, item) in items.iter().enumerate() {
                for field in ["id", "given", "when", "then"] {
                    if item.get(field).and_then(|v| v.as_str()).is_none() {
                        return ToolResult::error(format!(
                            "acceptance_criteria[{i}] is missing required string field '{field}'"
                        ));
                    }
                }
                if item.get("requirement_ids").and_then(|v| v.as_array()).is_none() {
                    return ToolResult::error(format!(
                        "acceptance_criteria[{i}] is missing required array field 'requirement_ids'"
                    ));
                }
            }
            let content = Value::Array(items).to_string();
            match crate::eventlog::put_artifact(&ctx.db_path, issue_id, "acceptance_criteria", &content) {
                Ok(()) => ToolResult::ok("Recorded acceptance criteria.".to_string()),
                Err(e) => ToolResult::error(format!("failed to record acceptance criteria: {e}")),
            }
        }
        "raise_clarification" => {
            let Some(question) = arguments.get("question").and_then(|v| v.as_str()) else {
                return ToolResult::error("missing required argument 'question'");
            };
            let Some(blocking) = arguments.get("blocking").and_then(|v| v.as_bool()) else {
                return ToolResult::error("missing required argument 'blocking'");
            };
            if blocking {
                ToolResult::ok(format!(
                    "Recorded blocking clarification: {question}. The cycle will stop \
                     once this turn ends and the issue will be parked until a human answers."
                ))
            } else {
                ToolResult::ok(format!(
                    "Recorded non-blocking clarification: {question}. Proceed on your \
                     best judgement, then call record_requirements with this \
                     requirement's assumption: true so a reviewer sees what you decided."
                ))
            }
        }
        other => ToolResult::error(format!("unsupported tool '{other}'")),
    }
}

/// Route a `tools/call` to whichever of the (up to three) independent tool sources
/// actually owns `name` (Section "PR-based branch workflow"): the tracker's own tools
/// (e.g. `update_issue_state`), `open_pull_request` from `repo_host` when
/// `repo.pull_request` is enabled, or (AIR-4) the delivery pipeline's own
/// `record_requirements`/`record_acceptance_criteria`/`raise_clarification` when
/// `pipeline.enabled`. Each is a property of a different thing (the tracker, the code
/// host, the pipeline stage running) so none of them is folded into another.
/// `tracker_names`/`pipeline_names` decide routing explicitly (not by matching on an
/// "unsupported tool" error text, which would be fragile).
#[allow(clippy::too_many_arguments)]
async fn route_call(
    adapter: &dyn TrackerAdapter,
    repo_host: Option<&dyn RepoHost>,
    artifacts_wiring: Option<&ArtifactsWiring>,
    pipeline: Option<&PipelineToolCtx>,
    tracker_names: &HashSet<&str>,
    pipeline_names: &HashSet<&str>,
    name: &str,
    arguments: Value,
    issue_id: &str,
    workspace_dir: &Path,
) -> ToolResult {
    if name == "record_artifact" {
        return match artifacts_wiring {
            Some(w) => {
                artifacts::execute_tool(
                    &w.db_path,
                    &w.workflow_dir,
                    workspace_dir,
                    issue_id,
                    issue_id,
                    arguments,
                )
                .await
            }
            None => ToolResult::error("record_artifact is unavailable: pipeline is not enabled"),
        };
    }
    if tracker_names.contains(name) {
        adapter.execute_agent_tool(name, arguments, issue_id).await
    } else if pipeline_names.contains(name) {
        match pipeline {
            Some(ctx) => execute_pipeline_tool(ctx, name, arguments, issue_id),
            None => ToolResult::error(format!("unsupported tool '{name}'")),
        }
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
    artifacts_wiring: Option<ArtifactsWiring>,
    pipeline: Option<PipelineToolCtx>,
    issue_id: &str,
    workspace_dir: &Path,
) -> anyhow::Result<()> {
    let tracker_specs = adapter.agent_tool_specs();
    let repo_specs = repo_host
        .as_ref()
        .map(|r| r.agent_tool_specs())
        .unwrap_or_default();
    let artifact_specs = if artifacts_wiring.is_some() {
        vec![artifacts::tool_spec()]
    } else {
        Vec::new()
    };
    let pipeline_specs = if pipeline.is_some() {
        pipeline_tool_specs()
    } else {
        Vec::new()
    };
    let tracker_names: HashSet<&str> = tracker_specs.iter().map(|s| s.name.as_str()).collect();
    let pipeline_names: HashSet<&str> = pipeline_specs.iter().map(|s| s.name.as_str()).collect();
    let specs: Vec<_> = tracker_specs
        .iter()
        .chain(repo_specs.iter())
        .chain(artifact_specs.iter())
        .chain(pipeline_specs.iter())
        .collect();

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
                    artifacts_wiring.as_ref(),
                    pipeline.as_ref(),
                    &tracker_names,
                    &pipeline_names,
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
            None,
            None,
            &names,
            &HashSet::new(),
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
            None,
            None,
            &names,
            &HashSet::new(),
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
            None,
            None,
            &names,
            &HashSet::new(),
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

    #[tokio::test]
    async fn record_artifact_is_unavailable_when_pipeline_is_not_enabled() {
        let tracker = FakeTracker;
        let specs = tracker.agent_tool_specs();
        let names: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        let empty_pipeline_names: HashSet<&str> = HashSet::new();

        let result = route_call(
            &tracker,
            None,
            None,
            None,
            &names,
            &empty_pipeline_names,
            "record_artifact",
            json!({}),
            "1",
            Path::new("."),
        )
        .await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn pipeline_tool_is_unsupported_without_pipeline_enabled() {
        let tracker = FakeTracker;
        let specs = tracker.agent_tool_specs();
        let tracker_names: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        let pipeline_names: HashSet<&str> = ["raise_clarification"].into_iter().collect();

        let result = route_call(
            &tracker,
            None,
            None,
            None,
            &tracker_names,
            &pipeline_names,
            "raise_clarification",
            json!({"question": "which auth scheme?", "blocking": true}),
            "1",
            Path::new("."),
        )
        .await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn record_artifact_routes_to_the_artifact_store_when_pipeline_is_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let wiring = ArtifactsWiring {
            db_path: dir.path().join("symphony.db"),
            workflow_dir: dir.path().to_path_buf(),
        };
        let tracker = FakeTracker;
        let specs = tracker.agent_tool_specs();
        let names: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        let empty_pipeline_names: HashSet<&str> = HashSet::new();

        let result = route_call(
            &tracker,
            None,
            Some(&wiring),
            None,
            &names,
            &empty_pipeline_names,
            "record_artifact",
            json!({
                "kind": "plan",
                "content_type": "text/plain",
                "content": "do the thing",
                "summary": "plan"
            }),
            "1",
            &workspace,
        )
        .await;
        assert!(result.success, "{}", result.content);
    }

    #[tokio::test]
    async fn record_requirements_rejects_missing_fields_and_bad_type() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = PipelineToolCtx {
            db_path: dir.path().join("symphony.db"),
        };
        let missing_source = execute_pipeline_tool(
            &ctx,
            "record_requirements",
            json!({"requirements": [{"id": "R1", "statement": "s", "type": "functional"}]}),
            "1",
        );
        assert!(!missing_source.success);

        let bad_type = execute_pipeline_tool(
            &ctx,
            "record_requirements",
            json!({"requirements": [{"id": "R1", "statement": "s", "source": "x", "type": "nope"}]}),
            "1",
        );
        assert!(!bad_type.success);
    }

    #[tokio::test]
    async fn record_requirements_then_record_acceptance_criteria_round_trip_via_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = PipelineToolCtx {
            db_path: dir.path().join("symphony.db"),
        };
        let req_result = execute_pipeline_tool(
            &ctx,
            "record_requirements",
            json!({"requirements": [
                {"id": "R1", "statement": "Users can log in", "source": "issue body", "type": "functional"},
                {"id": "R2", "statement": "p99 < 200ms", "source": "issue body", "type": "non_functional", "constraint": "performance"},
                {"id": "R3", "statement": "Export respects the new schema", "source": "issue depends_on", "type": "functional", "dependency": "DEMO-2"}
            ]}),
            "42",
        );
        assert!(req_result.success, "{}", req_result.content);

        let ac_result = execute_pipeline_tool(
            &ctx,
            "record_acceptance_criteria",
            json!({"acceptance_criteria": [
                {"id": "AC1", "requirement_ids": ["R1"], "given": "a user", "when": "they submit valid credentials", "then": "they are logged in"}
            ]}),
            "42",
        );
        assert!(ac_result.success, "{}", ac_result.content);

        let reqs = crate::eventlog::get_artifact(&ctx.db_path, "42", "requirements")
            .unwrap()
            .unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&reqs.content).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0]["id"], "R1");
        assert_eq!(parsed[1]["constraint"], "performance");
        assert_eq!(parsed[2]["dependency"], "DEMO-2");

        let acs = crate::eventlog::get_artifact(&ctx.db_path, "42", "acceptance_criteria")
            .unwrap()
            .unwrap();
        let parsed_ac: Vec<Value> = serde_json::from_str(&acs.content).unwrap();
        assert_eq!(parsed_ac[0]["id"], "AC1");
        assert_eq!(parsed_ac[0]["requirement_ids"], json!(["R1"]));
    }

    #[tokio::test]
    async fn raise_clarification_requires_question_and_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = PipelineToolCtx {
            db_path: dir.path().join("symphony.db"),
        };
        let missing_blocking =
            execute_pipeline_tool(&ctx, "raise_clarification", json!({"question": "q?"}), "1");
        assert!(!missing_blocking.success);

        let blocking_ok = execute_pipeline_tool(
            &ctx,
            "raise_clarification",
            json!({"question": "which auth scheme?", "blocking": true}),
            "1",
        );
        assert!(blocking_ok.success);
        assert!(blocking_ok.content.contains("stop"));

        let non_blocking_ok = execute_pipeline_tool(
            &ctx,
            "raise_clarification",
            json!({"question": "default page size?", "blocking": false}),
            "1",
        );
        assert!(non_blocking_ok.success);
        assert!(non_blocking_ok.content.contains("assumption"));
    }
}
