//! AIR-5: the human approval gate. A `pipeline.stages[].requires_approval: true` stage
//! parks its cycle here instead of moving straight to the next stage -- Roadmap §3:
//! "Human approval is required for architectural, business-critical and high-risk
//! decisions."
//!
//! SQLite-backed (`symphony.db`, alongside `eventlog.rs`'s `events` table) so a
//! pending decision survives a daemon restart, browsable at `/approvals`
//! (`status.rs`). Same "no pool, no long-lived shared connection, open a short-lived
//! connection per call" posture `eventlog.rs` documents -- approval decisions are rare
//! (at most one per `requires_approval` stage per cycle), nowhere near the write
//! volume that motivated `eventlog.rs`'s dedicated writer task.
//!
//! Two channels write a *decision* onto a pending row (`resolve`): the dashboard's
//! `/approvals/:id/decide` POST handler (`status.rs`, admin-token gated) and
//! `orchestrator::poll_approval_comments` (an issue-comment `/approve` /
//! `/changes <reason>` / `/reject [reason]`, scanned via
//! `TrackerAdapter::fetch_issue_comments`). Neither of those call sites has a
//! `TrackerAdapter` handle it can safely use to mutate tracker state on the spot
//! (`status.rs` is a plain read/write-to-this-db module, and the comment poller must
//! not race the orchestrator's own dispatch loop) -- so `resolve` only *records* the
//! decision. `orchestrator::apply_resolved_approvals`, run from the same tick loop
//! that already owns all tracker-mutating decisions, picks up newly resolved rows
//! (`take_unapplied`) and is the only thing that ever calls `TrackerAdapter::
//! set_issue_state` or writes to the event log on an approval's behalf -- one
//! authority over tracker state, matching `orchestrator.rs`'s own module doc comment.
//!
//! A resolved-and-applied "approve"/"changes" decision leaves a resume point
//! (`resume_stage_id`) for `orchestrator::run_pipeline` to pick up the next time this
//! issue is dispatched (`take_resume`) -- the stage to start at, and (for "changes") the
//! reviewer's comment to inject into that stage's first prompt.

use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS approvals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id TEXT NOT NULL,
    identifier TEXT NOT NULL,
    title TEXT NOT NULL,
    stage_id TEXT NOT NULL,
    next_stage_id TEXT,
    plan_text TEXT,
    plan_json TEXT,
    requested_at TEXT NOT NULL,
    last_seen_comment_id INTEGER NOT NULL DEFAULT 0,
    decision TEXT,
    actor TEXT,
    comment TEXT,
    resolved_at TEXT,
    applied_at TEXT,
    resume_stage_id TEXT,
    consumed_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_approvals_issue ON approvals(issue_id);
CREATE INDEX IF NOT EXISTS idx_approvals_resolved ON approvals(resolved_at);
CREATE INDEX IF NOT EXISTS idx_approvals_applied ON approvals(applied_at);
";

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// A newly finished `requires_approval` stage, ready to park.
#[derive(Debug, Clone)]
pub struct NewApproval {
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub stage_id: String,
    /// The stage that follows in `pipeline.stages`, if any -- where an "approve"
    /// decision resumes. `None` means the approved stage was the last one.
    pub next_stage_id: Option<String>,
    /// The stage's final turn message, shown to a human as-is on `/approvals`.
    pub plan_text: Option<String>,
    /// A fenced ```json block extracted from `plan_text`, if the stage emitted one --
    /// pretty-printed for display and for `auto_approve_when` evaluation.
    pub plan_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRow {
    pub id: i64,
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub stage_id: String,
    pub next_stage_id: Option<String>,
    pub plan_text: Option<String>,
    pub plan_json: Option<String>,
    pub requested_at: String,
    pub decision: Option<String>,
    pub actor: Option<String>,
    pub comment: Option<String>,
    pub resolved_at: Option<String>,
    pub applied_at: Option<String>,
}

impl ApprovalRow {
    /// Test-only: production code always gets pending vs. resolved rows pre-filtered
    /// by SQL (`list_pending`/`list_recent`), never needing to check a row in hand.
    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.resolved_at.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    RequestChanges,
    Reject,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Approve => "approve",
            Decision::RequestChanges => "changes",
            Decision::Reject => "reject",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "approve" => Some(Decision::Approve),
            "changes" => Some(Decision::RequestChanges),
            "reject" => Some(Decision::Reject),
            _ => None,
        }
    }
}

/// Where a resolved-and-applied decision left the cycle for its next dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePoint {
    /// The `pipeline.stages[].id` to start at.
    pub stage_id: String,
    /// Present only for a "request changes" decision -- injected into that stage's
    /// first turn prompt (`orchestrator::run_turn_loop`).
    pub reviewer_comment: Option<String>,
}

const ROW_COLUMNS: &str = "id, issue_id, identifier, title, stage_id, next_stage_id, \
    plan_text, plan_json, requested_at, decision, actor, comment, resolved_at, applied_at";

fn row_from(row: &rusqlite::Row) -> rusqlite::Result<ApprovalRow> {
    Ok(ApprovalRow {
        id: row.get(0)?,
        issue_id: row.get(1)?,
        identifier: row.get(2)?,
        title: row.get(3)?,
        stage_id: row.get(4)?,
        next_stage_id: row.get(5)?,
        plan_text: row.get(6)?,
        plan_json: row.get(7)?,
        requested_at: row.get(8)?,
        decision: row.get(9)?,
        actor: row.get(10)?,
        comment: row.get(11)?,
        resolved_at: row.get(12)?,
        applied_at: row.get(13)?,
    })
}

/// Park a finished `requires_approval` stage. Returns the new row's id.
pub fn create_pending(db_path: &Path, new: &NewApproval) -> rusqlite::Result<i64> {
    let conn = open(db_path)?;
    conn.execute(
        "INSERT INTO approvals (issue_id, identifier, title, stage_id, next_stage_id, \
         plan_text, plan_json, requested_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            new.issue_id,
            new.identifier,
            new.title,
            new.stage_id,
            new.next_stage_id,
            new.plan_text,
            new.plan_json,
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Test-only: no production caller needs a single row by id (`status.rs`/
/// `orchestrator.rs` always work from `list_pending`/`list_recent`/`take_unapplied`),
/// but tests use it constantly to assert on a decision after the fact.
#[cfg(test)]
pub fn get(db_path: &Path, id: i64) -> rusqlite::Result<Option<ApprovalRow>> {
    let conn = open(db_path)?;
    conn.query_row(
        &format!("SELECT {ROW_COLUMNS} FROM approvals WHERE id = ?1"),
        params![id],
        row_from,
    )
    .optional()
}

/// The one still-open approval request for `issue_id`, if any -- a pipeline only ever
/// has one stage awaiting approval at a time. Test-only for now: no production caller
/// needs this yet (`poll_approval_comments` scans all of `list_pending` instead, since
/// it has no single issue in hand to filter by).
#[cfg(test)]
pub fn pending_for_issue(db_path: &Path, issue_id: &str) -> rusqlite::Result<Option<ApprovalRow>> {
    let conn = open(db_path)?;
    conn.query_row(
        &format!(
            "SELECT {ROW_COLUMNS} FROM approvals WHERE issue_id = ?1 AND resolved_at IS NULL \
             ORDER BY id DESC LIMIT 1"
        ),
        params![issue_id],
        row_from,
    )
    .optional()
}

pub fn list_pending(db_path: &Path) -> rusqlite::Result<Vec<ApprovalRow>> {
    let conn = open(db_path)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {ROW_COLUMNS} FROM approvals WHERE resolved_at IS NULL ORDER BY id ASC"
    ))?;
    let rows = stmt.query_map([], row_from)?;
    rows.collect()
}

/// Most-recently-resolved decisions first, for `/approvals`' history view.
pub fn list_recent(db_path: &Path, limit: i64) -> rusqlite::Result<Vec<ApprovalRow>> {
    let conn = open(db_path)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {ROW_COLUMNS} FROM approvals WHERE resolved_at IS NOT NULL \
         ORDER BY id DESC LIMIT ?1"
    ))?;
    let rows = stmt.query_map(params![limit], row_from)?;
    rows.collect()
}

/// Record a decision on a still-pending row. Returns `Ok(false)` (not an error) if
/// `id` doesn't exist or was already resolved -- a double-submit (two browser tabs, a
/// comment arriving right after a dashboard click) is a normal race, not a bug, and
/// the second attempt should just no-op rather than overwrite the first decision.
pub fn resolve(
    db_path: &Path,
    id: i64,
    decision: Decision,
    actor: &str,
    comment: Option<&str>,
) -> rusqlite::Result<bool> {
    let conn = open(db_path)?;
    let updated = conn.execute(
        "UPDATE approvals SET decision = ?1, actor = ?2, comment = ?3, resolved_at = ?4 \
         WHERE id = ?5 AND resolved_at IS NULL",
        params![
            decision.as_str(),
            actor,
            comment,
            chrono::Utc::now().to_rfc3339(),
            id,
        ],
    )?;
    Ok(updated == 1)
}

/// Resolved rows the orchestrator hasn't yet applied to tracker state.
pub fn take_unapplied(db_path: &Path) -> rusqlite::Result<Vec<ApprovalRow>> {
    let conn = open(db_path)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {ROW_COLUMNS} FROM approvals WHERE resolved_at IS NOT NULL \
         AND applied_at IS NULL ORDER BY id ASC"
    ))?;
    let rows = stmt.query_map([], row_from)?;
    rows.collect()
}

/// Mark `id` applied (tracker state already moved, event already logged), leaving
/// `resume_stage_id` for `take_resume` -- `None` for a reject, where there's nothing
/// left to resume.
pub fn mark_applied(
    db_path: &Path,
    id: i64,
    resume_stage_id: Option<&str>,
) -> rusqlite::Result<()> {
    let conn = open(db_path)?;
    conn.execute(
        "UPDATE approvals SET applied_at = ?1, resume_stage_id = ?2 WHERE id = ?3",
        params![chrono::Utc::now().to_rfc3339(), resume_stage_id, id],
    )?;
    Ok(())
}

/// The not-yet-consumed resume point for `issue_id`, if any, marking it consumed in
/// the same call -- `orchestrator::run_pipeline` calls this exactly once at the start
/// of each cycle, so a resume point is used at most once even if the same issue is
/// dispatched again later for an unrelated reason.
pub fn take_resume(db_path: &Path, issue_id: &str) -> rusqlite::Result<Option<ResumePoint>> {
    let conn = open(db_path)?;
    let found: Option<(i64, String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT id, decision, resume_stage_id, comment FROM approvals \
             WHERE issue_id = ?1 AND applied_at IS NOT NULL AND consumed_at IS NULL \
             AND resume_stage_id IS NOT NULL ORDER BY id DESC LIMIT 1",
            params![issue_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((id, decision, resume_stage_id, comment)) = found else {
        return Ok(None);
    };
    conn.execute(
        "UPDATE approvals SET consumed_at = ?1 WHERE id = ?2",
        params![chrono::Utc::now().to_rfc3339(), id],
    )?;
    let Some(stage_id) = resume_stage_id else {
        return Ok(None);
    };
    let reviewer_comment = if decision == Decision::RequestChanges.as_str() {
        comment
    } else {
        None
    };
    Ok(Some(ResumePoint {
        stage_id,
        reviewer_comment,
    }))
}

/// The highest tracker comment id already scanned for this pending approval, so the
/// comment poller (`orchestrator::poll_approval_comments`) doesn't re-evaluate the
/// same comments every tick.
pub fn last_seen_comment_id(db_path: &Path, id: i64) -> rusqlite::Result<u64> {
    let conn = open(db_path)?;
    conn.query_row(
        "SELECT last_seen_comment_id FROM approvals WHERE id = ?1",
        params![id],
        |row| row.get::<_, i64>(0),
    )
    .map(|v| v.max(0) as u64)
}

pub fn set_last_seen_comment_id(db_path: &Path, id: i64, comment_id: u64) -> rusqlite::Result<()> {
    let conn = open(db_path)?;
    conn.execute(
        "UPDATE approvals SET last_seen_comment_id = ?1 WHERE id = ?2",
        params![comment_id as i64, id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(issue_id: &str, stage_id: &str) -> NewApproval {
        NewApproval {
            issue_id: issue_id.to_string(),
            identifier: issue_id.to_string(),
            title: format!("Issue {issue_id}"),
            stage_id: stage_id.to_string(),
            next_stage_id: Some("implement".to_string()),
            plan_text: Some("design summary here".to_string()),
            plan_json: Some(r#"{"risk":"low"}"#.to_string()),
        }
    }

    #[test]
    fn create_and_fetch_pending_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        let id = create_pending(&db, &sample("I-1", "plan")).unwrap();

        let row = get(&db, id).unwrap().unwrap();
        assert_eq!(row.issue_id, "I-1");
        assert_eq!(row.stage_id, "plan");
        assert!(row.is_pending());

        let pending = pending_for_issue(&db, "I-1").unwrap().unwrap();
        assert_eq!(pending.id, id);

        assert_eq!(list_pending(&db).unwrap().len(), 1);
        assert!(list_recent(&db, 10).unwrap().is_empty());
    }

    #[test]
    fn resolve_is_idempotent_against_a_double_submit() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        let id = create_pending(&db, &sample("I-2", "plan")).unwrap();

        assert!(resolve(&db, id, Decision::Approve, "alice", None).unwrap());
        // Second attempt (e.g. a stale comment scanned twice) must not overwrite it.
        assert!(!resolve(&db, id, Decision::Reject, "mallory", Some("nope")).unwrap());

        let row = get(&db, id).unwrap().unwrap();
        assert_eq!(row.decision.as_deref(), Some("approve"));
        assert_eq!(row.actor.as_deref(), Some("alice"));
        assert!(!row.is_pending());
        assert!(list_pending(&db).unwrap().is_empty());
        assert_eq!(list_recent(&db, 10).unwrap().len(), 1);
    }

    #[test]
    fn resolve_unknown_id_returns_false_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        assert!(!resolve(&db, 999, Decision::Approve, "alice", None).unwrap());
    }

    #[test]
    fn take_unapplied_only_returns_resolved_not_yet_applied_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        let pending_id = create_pending(&db, &sample("I-3", "plan")).unwrap();
        let resolved_id = create_pending(&db, &sample("I-4", "plan")).unwrap();
        resolve(&db, resolved_id, Decision::Approve, "alice", None).unwrap();

        let unapplied = take_unapplied(&db).unwrap();
        assert_eq!(unapplied.len(), 1);
        assert_eq!(unapplied[0].id, resolved_id);
        assert_ne!(unapplied[0].id, pending_id);

        mark_applied(&db, resolved_id, Some("implement")).unwrap();
        assert!(take_unapplied(&db).unwrap().is_empty());
    }

    #[test]
    fn take_resume_consumes_the_resume_point_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        let id = create_pending(&db, &sample("I-5", "plan")).unwrap();
        resolve(&db, id, Decision::Approve, "alice", None).unwrap();
        mark_applied(&db, id, Some("implement")).unwrap();

        let resume = take_resume(&db, "I-5").unwrap().unwrap();
        assert_eq!(resume.stage_id, "implement");
        assert!(resume.reviewer_comment.is_none());

        assert!(
            take_resume(&db, "I-5").unwrap().is_none(),
            "a resume point must only be handed out once"
        );
    }

    #[test]
    fn take_resume_injects_the_reviewer_comment_only_for_request_changes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        let id = create_pending(&db, &sample("I-6", "plan")).unwrap();
        resolve(
            &db,
            id,
            Decision::RequestChanges,
            "alice",
            Some("please split the migration into two steps"),
        )
        .unwrap();
        // "changes" resumes at the *same* stage, not `next_stage_id`.
        mark_applied(&db, id, Some("plan")).unwrap();

        let resume = take_resume(&db, "I-6").unwrap().unwrap();
        assert_eq!(resume.stage_id, "plan");
        assert_eq!(
            resume.reviewer_comment.as_deref(),
            Some("please split the migration into two steps")
        );
    }

    #[test]
    fn mark_applied_with_no_resume_stage_leaves_nothing_to_resume() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        let id = create_pending(&db, &sample("I-7", "plan")).unwrap();
        resolve(&db, id, Decision::Reject, "alice", Some("not viable")).unwrap();
        mark_applied(&db, id, None).unwrap();

        assert!(take_resume(&db, "I-7").unwrap().is_none());
    }

    #[test]
    fn last_seen_comment_id_defaults_to_zero_and_updates() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        let id = create_pending(&db, &sample("I-8", "plan")).unwrap();
        assert_eq!(last_seen_comment_id(&db, id).unwrap(), 0);
        set_last_seen_comment_id(&db, id, 42).unwrap();
        assert_eq!(last_seen_comment_id(&db, id).unwrap(), 42);
    }

    #[test]
    fn decision_parse_round_trips_known_values_and_rejects_unknown() {
        assert_eq!(Decision::parse("approve"), Some(Decision::Approve));
        assert_eq!(Decision::parse("Changes"), Some(Decision::RequestChanges));
        assert_eq!(Decision::parse(" reject "), Some(Decision::Reject));
        assert_eq!(Decision::parse("whatever"), None);
    }
}
