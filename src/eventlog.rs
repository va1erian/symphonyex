//! SQLite-backed event log: persistent history of every dispatch/turn/tool-call event,
//! browsable via the status server's `/events` and `/usage` pages (`src/status.rs`).
//!
//! Deliberately separate from `metrics.rs`'s in-memory `Metrics` struct, which keeps
//! driving the existing `--report` HTML file unchanged -- this is purely additive so
//! that already-working code path isn't put at risk. Unlike `Metrics`, this survives a
//! restart: the whole point is to let an operator browse what happened before now, not
//! just watch current state (`status.rs`'s `StatusSnapshot`, which *does* still reset
//! on restart, correctly -- that's live state, not history).
//!
//! Write path: one dedicated task owns the single write connection (WAL journal mode)
//! and receives rows over an `mpsc::UnboundedSender<NewEvent>` -- mirrors the
//! `OrchMsg`/`status_tx` channel pattern already used throughout `orchestrator.rs`, so
//! a slow or momentarily locked DB write never blocks ticket dispatch. Read path: each
//! HTTP handler in `status.rs` opens its own short-lived connection per request (WAL
//! mode handles concurrent readers fine at this traffic volume) -- no connection pool,
//! no `Arc<Mutex<Connection>>`, matching "keep it basic."

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

/// Default filename, joined onto `workflow_dir` -- same directory convention
/// `symphony-report.html` already defaults to. Exposed as a constant (not just a
/// literal at the one call site that opens the writer) since `status.rs`'s read-side
/// handlers need to resolve the exact same path independently.
pub const DB_FILENAME: &str = "symphony.db";

/// A row about to be written. Mirrors the fields already available at every call site
/// in `orchestrator.rs::handle_msg` -- no new data collection needed, just persisting
/// what's already flowing through `OrchMsg`/`AgentEvent`.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub session_id: Option<String>,
    pub event_type: String,
    pub message: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Importance {
    Normal,
    Low,
}

impl Importance {
    fn as_str(self) -> &'static str {
        match self {
            Importance::Normal => "normal",
            Importance::Low => "low",
        }
    }
}

/// `other_message` events are Claude's own intermediate streaming chunks -- observed
/// live firing every 1-2s during a turn (`message: Some("system")`). Real data worth
/// keeping, not something anyone wants to page through by default: classified `Low` so
/// `/events`/`/usage` can filter it out via an indexed column instead of a hardcoded
/// type-list scattered across query code.
pub fn classify_importance(ev: &NewEvent) -> Importance {
    match ev.event_type.as_str() {
        "other_message" => Importance::Low,
        _ => Importance::Normal,
    }
}

#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub issue_id: Option<String>,
    pub event_type: Option<String>,
    /// `None` means "normal only" (the default, hides `Low`-importance noise);
    /// `Some(Importance::Low)` or explicitly requesting "all" is how a caller opts
    /// into seeing everything -- see `status.rs`'s `?importance=all` handling.
    pub include_low_importance: bool,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub id: i64,
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub session_id: Option<String>,
    pub event_type: String,
    pub importance: String,
    pub message: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct UsageSummary {
    pub dispatch_count: i64,
    pub turn_count: i64,
    pub tool_call_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Clone, Default)]
pub struct IssueUsageRow {
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub dispatch_count: i64,
    pub turn_count: i64,
    pub tool_call_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub last_event_type: String,
    pub last_event_at: String,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id TEXT NOT NULL,
    identifier TEXT NOT NULL,
    title TEXT NOT NULL,
    session_id TEXT,
    event_type TEXT NOT NULL,
    importance TEXT NOT NULL,
    message TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    total_tokens INTEGER,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_issue ON events(issue_id);
CREATE INDEX IF NOT EXISTS idx_events_created_at ON events(created_at);
CREATE INDEX IF NOT EXISTS idx_events_importance ON events(importance);
";

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

fn insert(conn: &Connection, ev: &NewEvent) -> rusqlite::Result<()> {
    let importance = classify_importance(ev);
    conn.execute(
        "INSERT INTO events (issue_id, identifier, title, session_id, event_type, \
         importance, message, input_tokens, output_tokens, total_tokens, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            ev.issue_id,
            ev.identifier,
            ev.title,
            ev.session_id,
            ev.event_type,
            importance.as_str(),
            ev.message,
            ev.input_tokens.map(|v| v as i64),
            ev.output_tokens.map(|v| v as i64),
            ev.total_tokens.map(|v| v as i64),
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// Spawn the dedicated writer task and return the channel to feed it. Best-effort: a
/// write failure is logged and dropped, never propagated back to the orchestrator's
/// hot dispatch path -- losing one history row is not worth destabilizing dispatch
/// over (mirrors `metrics::write_report`'s own "best-effort, log don't panic" stance).
pub fn spawn_writer(db_path: PathBuf) -> mpsc::UnboundedSender<NewEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel::<NewEvent>();
    tokio::spawn(async move {
        let conn = match tokio::task::spawn_blocking({
            let path = db_path.clone();
            move || open(&path)
        })
        .await
        {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, path = %db_path.display(), "failed to open event log database; event history will not be recorded");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "event log opener task panicked");
                return;
            }
        };
        while let Some(ev) = rx.recv().await {
            if let Err(e) = insert(&conn, &ev) {
                tracing::warn!(error = %e, issue_id = %ev.issue_id, event_type = %ev.event_type, "failed to persist event (ignored)");
            }
        }
    });
    tx
}

fn row_from(row: &rusqlite::Row) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        id: row.get(0)?,
        issue_id: row.get(1)?,
        identifier: row.get(2)?,
        title: row.get(3)?,
        session_id: row.get(4)?,
        event_type: row.get(5)?,
        importance: row.get(6)?,
        message: row.get(7)?,
        input_tokens: row.get(8)?,
        output_tokens: row.get(9)?,
        total_tokens: row.get(10)?,
        created_at: row.get(11)?,
    })
}

pub fn recent_events(
    db_path: &Path,
    filter: &EventFilter,
    limit: i64,
    offset: i64,
) -> rusqlite::Result<Vec<EventRow>> {
    let conn = open(db_path)?;
    let mut sql = "SELECT id, issue_id, identifier, title, session_id, event_type, \
        importance, message, input_tokens, output_tokens, total_tokens, created_at \
        FROM events WHERE 1=1"
        .to_string();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if !filter.include_low_importance {
        sql.push_str(" AND importance = 'normal'");
    }
    if let Some(issue_id) = &filter.issue_id {
        sql.push_str(" AND issue_id = ?");
        params.push(Box::new(issue_id.clone()));
    }
    if let Some(event_type) = &filter.event_type {
        sql.push_str(" AND event_type = ?");
        params.push(Box::new(event_type.clone()));
    }
    sql.push_str(" ORDER BY id DESC LIMIT ? OFFSET ?");
    params.push(Box::new(limit));
    params.push(Box::new(offset));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), row_from)?;
    rows.collect()
}

/// Distinct `event_type` values seen so far, most-recent-first -- feeds `/events`'s
/// type filter `<datalist>` (`status.rs`) so an operator can pick from what's actually
/// been recorded instead of guessing/misspelling a type name into a plain text box.
pub fn distinct_event_types(db_path: &Path) -> rusqlite::Result<Vec<String>> {
    let conn = open(db_path)?;
    let mut stmt =
        conn.prepare("SELECT event_type FROM events GROUP BY event_type ORDER BY MAX(id) DESC")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}

pub fn usage_summary(db_path: &Path) -> rusqlite::Result<UsageSummary> {
    let conn = open(db_path)?;
    conn.query_row(
        "SELECT \
            (SELECT COUNT(*) FROM events WHERE event_type = 'dispatched'), \
            (SELECT COUNT(*) FROM events WHERE event_type = 'turn_started'), \
            (SELECT COUNT(*) FROM events WHERE event_type = 'tool_call'), \
            (SELECT COALESCE(SUM(input_tokens), 0) FROM events), \
            (SELECT COALESCE(SUM(output_tokens), 0) FROM events), \
            (SELECT COALESCE(SUM(total_tokens), 0) FROM events)",
        [],
        |row| {
            Ok(UsageSummary {
                dispatch_count: row.get(0)?,
                turn_count: row.get(1)?,
                tool_call_count: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                total_tokens: row.get(5)?,
            })
        },
    )
}

pub fn usage_by_issue(db_path: &Path) -> rusqlite::Result<Vec<IssueUsageRow>> {
    let conn = open(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT issue_id, \
            MAX(identifier), \
            MAX(title), \
            SUM(CASE WHEN event_type = 'dispatched' THEN 1 ELSE 0 END), \
            SUM(CASE WHEN event_type = 'turn_started' THEN 1 ELSE 0 END), \
            SUM(CASE WHEN event_type = 'tool_call' THEN 1 ELSE 0 END), \
            COALESCE(SUM(input_tokens), 0), \
            COALESCE(SUM(output_tokens), 0), \
            COALESCE(SUM(total_tokens), 0), \
            (SELECT event_type FROM events e2 WHERE e2.issue_id = e1.issue_id ORDER BY id DESC LIMIT 1), \
            (SELECT created_at FROM events e3 WHERE e3.issue_id = e1.issue_id ORDER BY id DESC LIMIT 1) \
         FROM events e1 \
         GROUP BY issue_id \
         ORDER BY MAX(id) DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(IssueUsageRow {
            issue_id: row.get(0)?,
            identifier: row.get(1)?,
            title: row.get(2)?,
            dispatch_count: row.get(3)?,
            turn_count: row.get(4)?,
            tool_call_count: row.get(5)?,
            input_tokens: row.get(6)?,
            output_tokens: row.get(7)?,
            total_tokens: row.get(8)?,
            last_event_type: row.get(9)?,
            last_event_at: row.get(10)?,
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_event(issue_id: &str, event_type: &str) -> NewEvent {
        NewEvent {
            issue_id: issue_id.to_string(),
            identifier: issue_id.to_string(),
            title: format!("Issue {issue_id}"),
            session_id: Some("sess-1".to_string()),
            event_type: event_type.to_string(),
            message: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
        }
    }

    #[test]
    fn classify_importance_marks_other_message_low_and_everything_else_normal() {
        assert_eq!(
            classify_importance(&new_event("1", "other_message")),
            Importance::Low
        );
        for t in [
            "dispatched",
            "session_started",
            "turn_started",
            "tool_call",
            "notification",
            "turn_completed",
            "turn_failed",
            "worker_exit",
            "retry_fired",
        ] {
            assert_eq!(classify_importance(&new_event("1", t)), Importance::Normal);
        }
    }

    #[tokio::test]
    async fn recent_events_defaults_to_hiding_low_importance_noise() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let conn = open(&db).unwrap();
        insert(&conn, &new_event("1", "dispatched")).unwrap();
        insert(&conn, &new_event("1", "other_message")).unwrap();
        drop(conn);

        let default_filter = EventFilter::default();
        let visible = recent_events(&db, &default_filter, 50, 0).unwrap();
        assert_eq!(visible.len(), 1, "{visible:?}");
        assert_eq!(visible[0].event_type, "dispatched");

        let all_filter = EventFilter {
            include_low_importance: true,
            ..Default::default()
        };
        let all = recent_events(&db, &all_filter, 50, 0).unwrap();
        assert_eq!(all.len(), 2, "{all:?}");
    }

    #[tokio::test]
    async fn distinct_event_types_deduplicates_and_orders_most_recent_first() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let conn = open(&db).unwrap();
        insert(&conn, &new_event("1", "dispatched")).unwrap();
        insert(&conn, &new_event("1", "turn_started")).unwrap();
        insert(&conn, &new_event("2", "dispatched")).unwrap();
        insert(&conn, &new_event("1", "tool_call")).unwrap();
        drop(conn);

        let types = distinct_event_types(&db).unwrap();
        assert_eq!(types, vec!["tool_call", "dispatched", "turn_started"]);
    }

    #[tokio::test]
    async fn recent_events_filters_by_issue_and_type() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let conn = open(&db).unwrap();
        insert(&conn, &new_event("1", "dispatched")).unwrap();
        insert(&conn, &new_event("2", "dispatched")).unwrap();
        insert(&conn, &new_event("1", "turn_started")).unwrap();
        drop(conn);

        let by_issue = recent_events(
            &db,
            &EventFilter {
                issue_id: Some("1".to_string()),
                ..Default::default()
            },
            50,
            0,
        )
        .unwrap();
        assert_eq!(by_issue.len(), 2, "{by_issue:?}");
        assert!(by_issue.iter().all(|r| r.issue_id == "1"));

        let by_type = recent_events(
            &db,
            &EventFilter {
                event_type: Some("dispatched".to_string()),
                ..Default::default()
            },
            50,
            0,
        )
        .unwrap();
        assert_eq!(by_type.len(), 2, "{by_type:?}");
        assert!(by_type.iter().all(|r| r.event_type == "dispatched"));
    }

    #[tokio::test]
    async fn usage_summary_aggregates_tokens_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let conn = open(&db).unwrap();
        insert(&conn, &new_event("1", "dispatched")).unwrap();
        insert(&conn, &new_event("1", "turn_started")).unwrap();
        insert(
            &conn,
            &NewEvent {
                input_tokens: Some(100),
                output_tokens: Some(50),
                total_tokens: Some(150),
                ..new_event("1", "tool_call")
            },
        )
        .unwrap();
        drop(conn);

        let summary = usage_summary(&db).unwrap();
        assert_eq!(summary.dispatch_count, 1);
        assert_eq!(summary.turn_count, 1);
        assert_eq!(summary.tool_call_count, 1);
        assert_eq!(summary.input_tokens, 100);
        assert_eq!(summary.output_tokens, 50);
        assert_eq!(summary.total_tokens, 150);
    }

    #[tokio::test]
    async fn usage_by_issue_groups_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let conn = open(&db).unwrap();
        insert(&conn, &new_event("1", "dispatched")).unwrap();
        insert(&conn, &new_event("1", "turn_started")).unwrap();
        insert(&conn, &new_event("2", "dispatched")).unwrap();
        drop(conn);

        let rows = usage_by_issue(&db).unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        let issue1 = rows.iter().find(|r| r.issue_id == "1").unwrap();
        assert_eq!(issue1.dispatch_count, 1);
        assert_eq!(issue1.turn_count, 1);
    }
}
