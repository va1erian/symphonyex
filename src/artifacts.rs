//! Per-cycle artifact store (AIR-3): the structured output a pipeline stage produces
//! ("validated requirements", "engineer-approved delivery plan", "security evidence",
//! ...), stored so a later stage -- or a human on `/artifacts` (`src/status.rs`) -- can
//! read what an earlier one produced. No downstream consumer could exist before this:
//! `attach_evidence` (`src/repo_host/github.rs`) only pushes an image to a PR, nothing
//! machine-readable ever landed anywhere.
//!
//! Storage mirrors `attach_evidence`'s own conventions: content-hashed so a retry never
//! collides, blob-on-disk plus metadata-in-SQLite. Unlike `attach_evidence`, artifacts
//! never leave Symphony's own storage (`<workflow_dir>/.symphony/cycles/<cycle_id>/...`,
//! table `artifacts` in `symphony.db` -- see `eventlog.rs`'s `SCHEMA`) -- they're a
//! handoff between stages of one cycle, not evidence for a human reviewer. A "cycle" is
//! one issue's run through `pipeline.stages` (`orchestrator.rs::run_pipeline`); this
//! module keys on the same `issue_id` string every other tool already receives, since a
//! new tracker-adapter id is exactly a new cycle and durability across a restart falls
//! out for free (no extra id to persist or reconcile).
//!
//! `record_artifact`, the agent tool this module exposes, is wired into
//! `mcp::run_stdio_server` independently of both `TrackerAdapter` and `RepoHost`: an
//! artifact belongs to the cycle, not to the tracker or the code host, so it gets its
//! own small routing bucket in `mcp::route_call` rather than being folded into either.

use crate::eventlog;
use crate::tracker::{ToolResult, ToolSpec};
use rusqlite::params;
use serde_json::{Value, json};
use std::path::Path;

/// Recognized kinds (ticket's "Known kinds"). An unrecognized kind is still accepted --
/// a project's own pipeline may need a kind this list doesn't anticipate -- but flagged
/// back to the agent so a typo doesn't silently vanish from a later stage's expectations.
pub const KNOWN_KINDS: &[&str] = &[
    "requirements",
    "acceptance_criteria",
    "plan",
    "test_report",
    "coverage",
    "review_findings",
    "security_findings",
    "telemetry_evidence",
    "debt_findings",
    "screenshot",
];

/// Kinds with a versioned JSON schema, validated on record. Deliberately shallow (no
/// JSON-Schema engine, no per-kind field lists): the only thing worth catching here is
/// an agent storing free-form prose, or an old/foreign shape, where a downstream
/// stage's own parsing code expects the versioned object it asked for. A second
/// abstraction (a real schema validator) isn't warranted for that one check.
const STRUCTURED_KINDS: &[&str] = &[
    "requirements",
    "acceptance_criteria",
    "review_findings",
    "security_findings",
    "debt_findings",
];

/// Directory-relative-to-workspace convention every stage's agent can read a cycle's
/// current artifacts from (see this module's doc comment, and `render_turn_prompt`'s
/// `cycle.artifacts` context). One file per kind: a later record of the same kind
/// overwrites the workspace copy (the SQLite table keeps every version).
const WORKSPACE_ARTIFACTS_DIR: &str = ".symphony/artifacts";

/// Marker file `run_pipeline` writes at the start of each stage so this module (running
/// inside the separate `__mcp_tool_server` subprocess, with no other channel back to the
/// orchestrator) can tag a recorded artifact with the stage that produced it.
const CURRENT_STAGE_FILE: &str = ".symphony/current-stage";

#[derive(Debug, Clone, serde::Serialize)]
pub struct ArtifactIndexEntry {
    pub kind: String,
    pub stage: Option<String>,
    pub summary: String,
    /// Workspace-relative path a same-cycle stage can read this artifact's content
    /// from directly (`WORKSPACE_ARTIFACTS_DIR`/`<kind>.<ext>`).
    pub path: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ArtifactRow {
    pub id: String,
    pub cycle_id: String,
    pub issue_identifier: String,
    pub stage_id: Option<String>,
    pub kind: String,
    pub content_type: String,
    /// Path to the on-disk blob, relative to `workflow_dir`.
    pub path: String,
    pub sha256: String,
    pub created_at: String,
    pub summary: String,
}

fn row_from(row: &rusqlite::Row) -> rusqlite::Result<ArtifactRow> {
    Ok(ArtifactRow {
        id: row.get(0)?,
        cycle_id: row.get(1)?,
        issue_identifier: row.get(2)?,
        stage_id: row.get(3)?,
        kind: row.get(4)?,
        content_type: row.get(5)?,
        path: row.get(6)?,
        sha256: row.get(7)?,
        created_at: row.get(8)?,
        summary: row.get(9)?,
    })
}

const SELECT_COLUMNS: &str = "id, cycle_id, issue_identifier, stage_id, kind, content_type, \
    path, sha256, created_at, summary";

fn ext_for(content_type: &str) -> &'static str {
    if content_type.contains("json") {
        "json"
    } else if content_type.contains("png") {
        "png"
    } else if content_type.contains("jpeg") || content_type.contains("jpg") {
        "jpg"
    } else if content_type.contains("markdown") {
        "md"
    } else if content_type.starts_with("text/") {
        "txt"
    } else {
        "bin"
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Ticket's "Schema for structured kinds": a structured kind must be a JSON object
/// carrying an integer `schema_version` field. Anything else returns a message
/// specific enough for the agent to fix and retry, per the acceptance criteria.
fn validate_structured(kind: &str, content_type: &str, bytes: &[u8]) -> Result<(), String> {
    if !content_type.contains("json") {
        return Err(format!(
            "kind '{kind}' requires content_type 'application/json' (got '{content_type}')"
        ));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("kind '{kind}' content is not valid JSON: {e}"))?;
    let Some(obj) = value.as_object() else {
        return Err(format!("kind '{kind}' content must be a JSON object"));
    };
    match obj.get("schema_version") {
        Some(v) if v.is_u64() || v.is_i64() => Ok(()),
        Some(_) => Err(format!(
            "kind '{kind}': 'schema_version' must be an integer, got {}",
            obj["schema_version"]
        )),
        None => Err(format!(
            "kind '{kind}' is missing the required 'schema_version' field \
             (add e.g. \"schema_version\": 1 at the top level and retry)"
        )),
    }
}

struct NewArtifact<'a> {
    cycle_id: &'a str,
    issue_identifier: &'a str,
    stage_id: Option<&'a str>,
    kind: &'a str,
    content_type: &'a str,
    summary: &'a str,
}

struct Recorded {
    id: String,
    unknown_kind: bool,
    workspace_relative_path: String,
}

/// Validate, hash, write the blob under `workflow_dir`, and upsert the SQLite row.
/// Content-hashed like `attach_evidence` (`repo_host::github`) already does: the id is
/// derived from the artifact's own bytes, so re-recording identical content on a retry
/// resolves to the same id/path and the `ON CONFLICT` upsert below just refreshes
/// `summary`/`stage_id`/`created_at` instead of erroring or duplicating a row.
fn record(
    db_path: &Path,
    workflow_dir: &Path,
    new: &NewArtifact,
    bytes: &[u8],
) -> Result<Recorded, String> {
    let unknown_kind = !KNOWN_KINDS.contains(&new.kind);
    if STRUCTURED_KINDS.contains(&new.kind) {
        validate_structured(new.kind, new.content_type, bytes)?;
    }

    let ext = ext_for(new.content_type);
    let hash = sha256_hex(bytes);
    let id = format!("{}-{}", new.kind, &hash[..16]);

    let cycle_dir = workflow_dir
        .join(".symphony")
        .join("cycles")
        .join(new.cycle_id);
    std::fs::create_dir_all(&cycle_dir)
        .map_err(|e| format!("failed to create cycle artifact directory: {e}"))?;
    let disk_path = cycle_dir.join(format!("{id}.{ext}"));
    std::fs::write(&disk_path, bytes).map_err(|e| format!("failed to write artifact file: {e}"))?;

    let relative_path = disk_path
        .strip_prefix(workflow_dir)
        .unwrap_or(&disk_path)
        .to_string_lossy()
        .replace('\\', "/");

    let conn = eventlog::open(db_path).map_err(|e| format!("failed to open symphony.db: {e}"))?;
    let created_at = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO artifacts (id, cycle_id, issue_identifier, stage_id, kind, content_type, \
         path, sha256, created_at, summary) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) \
         ON CONFLICT(id) DO UPDATE SET \
            stage_id = excluded.stage_id, created_at = excluded.created_at, summary = excluded.summary",
        params![
            id,
            new.cycle_id,
            new.issue_identifier,
            new.stage_id,
            new.kind,
            new.content_type,
            relative_path,
            hash,
            created_at,
            new.summary,
        ],
    )
    .map_err(|e| format!("failed to record artifact: {e}"))?;

    Ok(Recorded {
        id,
        unknown_kind,
        workspace_relative_path: format!("{WORKSPACE_ARTIFACTS_DIR}/{}.{ext}", new.kind),
    })
}

/// `ToolSpec` for `record_artifact`, gated on `pipeline.enabled` the same way
/// `open_pull_request`/`attach_evidence` are gated on `repo.pull_request`/
/// `repo.evidence` (see `mcp::run_stdio_server`).
pub fn tool_spec() -> ToolSpec {
    ToolSpec {
        name: "record_artifact".to_string(),
        description: format!(
            "Record this stage's required output as a structured artifact that later \
             pipeline stages (and a human, on the /artifacts dashboard) can read. Known \
             kinds: {}. Unknown kinds are accepted but flagged. For 'requirements', \
             'acceptance_criteria', 'review_findings', 'security_findings' and \
             'debt_findings', content_type must be 'application/json' and the content \
             must be a JSON object with an integer 'schema_version' field -- an invalid \
             schema is rejected with an error explaining exactly what to fix, storing \
             nothing. Provide the content either as a path to a file you already saved \
             in this workspace, or inline as 'content'.",
            KNOWN_KINDS.join(", ")
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": "What this artifact is, e.g. 'requirements', 'plan', 'test_report'"
                },
                "content_type": {
                    "type": "string",
                    "description": "MIME type, e.g. 'application/json', 'text/markdown', 'image/png'"
                },
                "path": {
                    "type": "string",
                    "description": "Path to an existing file in this workspace, relative to its root. Use this or 'content', not both."
                },
                "content": {
                    "type": "string",
                    "description": "Inline content to store. Use this or 'path', not both."
                },
                "summary": {
                    "type": "string",
                    "description": "One or two sentence human-readable summary of what this artifact contains"
                }
            },
            "required": ["kind", "content_type", "summary"]
        }),
    }
}

/// Execute `record_artifact`. `cycle_id`/`issue_identifier` are both the tracker's own
/// issue id -- see this module's doc comment for why a second, separately-minted cycle
/// id isn't needed. `stage_id` is read from `CURRENT_STAGE_FILE`, written into the
/// workspace by `orchestrator::run_pipeline` at the start of each stage -- the only
/// channel available here, since this runs in the one-shot `__mcp_tool_server`
/// subprocess with no other link back to the orchestrator's own state.
pub async fn execute_tool(
    db_path: &Path,
    workflow_dir: &Path,
    workspace_dir: &Path,
    cycle_id: &str,
    issue_identifier: &str,
    arguments: Value,
) -> ToolResult {
    let Some(kind) = arguments.get("kind").and_then(|v| v.as_str()) else {
        return ToolResult::error("missing required argument 'kind'");
    };
    let Some(content_type) = arguments.get("content_type").and_then(|v| v.as_str()) else {
        return ToolResult::error("missing required argument 'content_type'");
    };
    let Some(summary) = arguments.get("summary").and_then(|v| v.as_str()) else {
        return ToolResult::error("missing required argument 'summary'");
    };
    let path_arg = arguments.get("path").and_then(|v| v.as_str());
    let content_arg = arguments.get("content").and_then(|v| v.as_str());

    let bytes: Vec<u8> = match (path_arg, content_arg) {
        (Some(_), Some(_)) => {
            return ToolResult::error("provide either 'path' or 'content', not both");
        }
        (None, None) => {
            return ToolResult::error(
                "missing required argument: provide either 'path' or 'content'",
            );
        }
        (Some(path), None) => {
            let full = workspace_dir.join(path);
            match tokio::fs::read(&full).await {
                Ok(b) => b,
                Err(e) => {
                    return ToolResult::error(format!(
                        "could not read '{path}' (resolved to {}): {e}",
                        full.display()
                    ));
                }
            }
        }
        (None, Some(content)) => content.as_bytes().to_vec(),
    };

    let stage_id = tokio::fs::read_to_string(workspace_dir.join(CURRENT_STAGE_FILE))
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let new = NewArtifact {
        cycle_id,
        issue_identifier,
        stage_id: stage_id.as_deref(),
        kind,
        content_type,
        summary,
    };

    match record(db_path, workflow_dir, &new, &bytes) {
        Ok(recorded) => {
            let dest = workspace_dir.join(&recorded.workspace_relative_path);
            if let Some(parent) = dest.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            if let Err(e) = tokio::fs::write(&dest, &bytes).await {
                tracing::warn!(error = %e, path = %dest.display(), "failed to materialize recorded artifact into workspace (ignored)");
            }
            let note = if recorded.unknown_kind {
                format!(
                    " Note: '{kind}' is not a recognized kind ({}); stored as-is, but a \
                     downstream stage looking for a known kind won't find it under that name.",
                    KNOWN_KINDS.join(", ")
                )
            } else {
                String::new()
            };
            ToolResult::ok(format!(
                "Recorded {kind} artifact (id {}), readable by later stages at \
                 {}.{note}",
                recorded.id, recorded.workspace_relative_path
            ))
        }
        Err(e) => ToolResult::error(e),
    }
}

/// Index of every artifact recorded so far this cycle, oldest first -- feeds the
/// `cycle.artifacts` template context (`orchestrator::render_turn_prompt`) so a later
/// stage's prompt can see what earlier stages produced without reading the workspace.
pub fn list_index(db_path: &Path, cycle_id: &str) -> Vec<ArtifactIndexEntry> {
    list_for_cycle(db_path, cycle_id)
        .into_iter()
        .map(|row| ArtifactIndexEntry {
            kind: row.kind.clone(),
            stage: row.stage_id,
            summary: row.summary,
            path: format!(
                "{WORKSPACE_ARTIFACTS_DIR}/{}.{}",
                row.kind,
                ext_for(&row.content_type)
            ),
        })
        .collect()
}

pub fn list_for_cycle(db_path: &Path, cycle_id: &str) -> Vec<ArtifactRow> {
    let Ok(conn) = eventlog::open(db_path) else {
        return Vec::new();
    };
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM artifacts WHERE cycle_id = ?1 ORDER BY created_at ASC"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![cycle_id], row_from) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

/// Every artifact ever recorded, most-recent-first -- backs `/artifacts`
/// (`src/status.rs`).
pub fn list_all(db_path: &Path) -> Vec<ArtifactRow> {
    let Ok(conn) = eventlog::open(db_path) else {
        return Vec::new();
    };
    let sql = format!("SELECT {SELECT_COLUMNS} FROM artifacts ORDER BY created_at DESC");
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![], row_from) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

pub fn find_by_id(db_path: &Path, id: &str) -> Option<ArtifactRow> {
    let conn = eventlog::open(db_path).ok()?;
    let sql = format!("SELECT {SELECT_COLUMNS} FROM artifacts WHERE id = ?1");
    conn.query_row(&sql, params![id], row_from).ok()
}

/// Raw blob content for the `/artifacts` per-artifact view.
pub fn read_content(workflow_dir: &Path, row: &ArtifactRow) -> std::io::Result<Vec<u8>> {
    std::fs::read(workflow_dir.join(&row.path))
}

/// Called by `orchestrator::run_pipeline` at the start of each stage: tags this
/// workspace with the stage now running (`CURRENT_STAGE_FILE`, read back by
/// `execute_tool` above) and refreshes the workspace's `.symphony/artifacts/` copies
/// with the latest recorded artifact per kind, so a fresh or recreated workspace (e.g.
/// after a restart) still exposes prior stages' output as files, not just through the
/// prompt.
pub fn prepare_workspace_for_stage(
    db_path: &Path,
    workflow_dir: &Path,
    cycle_id: &str,
    workspace_dir: &Path,
    stage_id: &str,
) {
    if let Some(parent) = workspace_dir.join(CURRENT_STAGE_FILE).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(workspace_dir.join(CURRENT_STAGE_FILE), stage_id) {
        tracing::warn!(error = %e, "failed to write current-stage marker (ignored)");
    }

    let dest_dir = workspace_dir.join(WORKSPACE_ARTIFACTS_DIR);
    let _ = std::fs::create_dir_all(&dest_dir);
    // Latest row per kind: a `for` over `list_for_cycle`'s ascending order means a
    // later kind-match simply overwrites the map entry, leaving the newest.
    let mut latest_by_kind: std::collections::HashMap<String, ArtifactRow> =
        std::collections::HashMap::new();
    for row in list_for_cycle(db_path, cycle_id) {
        latest_by_kind.insert(row.kind.clone(), row);
    }
    for row in latest_by_kind.values() {
        let Ok(bytes) = std::fs::read(workflow_dir.join(&row.path)) else {
            continue;
        };
        let ext = ext_for(&row.content_type);
        let _ = std::fs::write(dest_dir.join(format!("{}.{ext}", row.kind)), bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn db_path(dir: &Path) -> PathBuf {
        dir.join("symphony.db")
    }

    #[tokio::test]
    async fn round_trips_a_plain_artifact_through_the_store() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let db = db_path(dir.path());

        let result = execute_tool(
            &db,
            dir.path(),
            &workspace,
            "issue-1",
            "issue-1",
            json!({
                "kind": "plan",
                "content_type": "text/markdown",
                "content": "# Plan\n1. Do the thing.",
                "summary": "Delivery plan"
            }),
        )
        .await;
        assert!(result.success, "{}", result.content);

        let rows = list_for_cycle(&db, "issue-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "plan");
        assert_eq!(rows[0].summary, "Delivery plan");

        let content = read_content(dir.path(), &rows[0]).unwrap();
        assert_eq!(content, b"# Plan\n1. Do the thing.");

        // Materialized into the workspace for a same-cycle stage to read directly.
        let materialized = std::fs::read(workspace.join(".symphony/artifacts/plan.md")).unwrap();
        assert_eq!(materialized, b"# Plan\n1. Do the thing.");
    }

    #[tokio::test]
    async fn retrying_with_identical_content_does_not_duplicate_or_error() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let db = db_path(dir.path());

        let args = json!({
            "kind": "coverage",
            "content_type": "text/plain",
            "content": "87%",
            "summary": "Line coverage"
        });
        let first = execute_tool(
            &db,
            dir.path(),
            &workspace,
            "issue-2",
            "issue-2",
            args.clone(),
        )
        .await;
        assert!(first.success);
        let second = execute_tool(&db, dir.path(), &workspace, "issue-2", "issue-2", args).await;
        assert!(second.success);

        let rows = list_for_cycle(&db, "issue-2");
        assert_eq!(
            rows.len(),
            1,
            "identical retry must not create a second row"
        );
    }

    #[tokio::test]
    async fn rejects_a_structured_kind_missing_schema_version_and_stores_nothing() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let db = db_path(dir.path());

        let result = execute_tool(
            &db,
            dir.path(),
            &workspace,
            "issue-3",
            "issue-3",
            json!({
                "kind": "requirements",
                "content_type": "application/json",
                "content": "{\"stuff\": true}",
                "summary": "Requirements"
            }),
        )
        .await;
        assert!(!result.success);
        assert!(
            result.content.contains("schema_version"),
            "error should explain the missing field: {}",
            result.content
        );
        assert!(list_for_cycle(&db, "issue-3").is_empty());
    }

    #[tokio::test]
    async fn accepts_a_structured_kind_with_schema_version() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let db = db_path(dir.path());

        let result = execute_tool(
            &db,
            dir.path(),
            &workspace,
            "issue-4",
            "issue-4",
            json!({
                "kind": "acceptance_criteria",
                "content_type": "application/json",
                "content": "{\"schema_version\": 1, \"criteria\": []}",
                "summary": "AC"
            }),
        )
        .await;
        assert!(result.success, "{}", result.content);
        assert_eq!(list_for_cycle(&db, "issue-4").len(), 1);
    }

    #[tokio::test]
    async fn flags_unknown_kinds_but_still_stores_them() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let db = db_path(dir.path());

        let result = execute_tool(
            &db,
            dir.path(),
            &workspace,
            "issue-5",
            "issue-5",
            json!({
                "kind": "some_new_kind",
                "content_type": "text/plain",
                "content": "hi",
                "summary": "whatever"
            }),
        )
        .await;
        assert!(result.success);
        assert!(result.content.contains("not a recognized kind"));
        assert_eq!(list_for_cycle(&db, "issue-5").len(), 1);
    }

    #[tokio::test]
    async fn reads_an_existing_workspace_file_by_path() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("report.txt"), "test report body").unwrap();
        let db = db_path(dir.path());

        let result = execute_tool(
            &db,
            dir.path(),
            &workspace,
            "issue-6",
            "issue-6",
            json!({
                "kind": "test_report",
                "content_type": "text/plain",
                "path": "report.txt",
                "summary": "Tests passed"
            }),
        )
        .await;
        assert!(result.success, "{}", result.content);
    }

    #[test]
    fn list_index_reports_workspace_relative_paths_for_a_second_stage_to_read() {
        let dir = tempdir().unwrap();
        let db = db_path(dir.path());
        let new = NewArtifact {
            cycle_id: "issue-7",
            issue_identifier: "issue-7",
            stage_id: Some("requirements-stage"),
            kind: "requirements",
            content_type: "application/json",
            summary: "Reqs",
        };
        record(&db, dir.path(), &new, br#"{"schema_version": 1}"#).unwrap();

        let index = list_index(&db, "issue-7");
        assert_eq!(index.len(), 1);
        assert_eq!(index[0].kind, "requirements");
        assert_eq!(index[0].stage.as_deref(), Some("requirements-stage"));
        assert_eq!(index[0].path, ".symphony/artifacts/requirements.json");
    }

    #[test]
    fn prepare_workspace_for_stage_writes_the_current_stage_marker() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let db = db_path(dir.path());

        prepare_workspace_for_stage(&db, dir.path(), "issue-8", &workspace, "plan-stage");

        let marker = std::fs::read_to_string(workspace.join(CURRENT_STAGE_FILE)).unwrap();
        assert_eq!(marker, "plan-stage");
    }
}
