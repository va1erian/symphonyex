//! Live status dashboard (subset of the OPTIONAL Section 13.7 HTTP extension), plus
//! two browsable pages backed by the SQLite event log (`src/eventlog.rs`): `/events`
//! (filterable, paginated raw history) and `/usage` (token/turn/tool-call consumption,
//! globally and per-issue). Only enabled when a `--port` is passed on the CLI.
//! Intended for watching dispatch/concurrency behavior during development and test
//! runs, not as a production dashboard.
//!
//! `/` used to full-page-reload every second via `<meta http-equiv="refresh">` --
//! every tick was a real browser navigation (scroll position reset, any open
//! selection lost, a visible flash). It now renders once and polls `/fragment` via a
//! small inline `<script>` (`fetch` + `innerHTML` swap on the two data containers
//! only), which has none of that: no navigation event, so nothing about the page
//! *outside* those two containers ever moves. `/fragment` reuses the exact same
//! `running_card`/`retry_row` render functions `/` itself uses -- no HTML templating
//! duplicated into JS, everything server-rendered as before.
//!
//! Also nests `crate::board`'s bulletin-board UI at `/board`, unconditionally --
//! harmless when a project isn't using `swebot.board.enabled` (an empty board, same
//! posture as `/events`/`/usage` showing "nothing recorded yet" until something
//! populates them), and avoids threading a project's `swebot.board.enabled` value
//! through this constructor just to decide whether to mount one more route.

use crate::eventlog;
use axum::Router;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::watch;

#[derive(Clone, Serialize, Default)]
pub struct StatusSnapshot {
    pub generated_at: String,
    pub running: Vec<RunningRow>,
    pub retrying: Vec<RetryRow>,
}

#[derive(Clone, Serialize)]
pub struct RunningRow {
    pub identifier: String,
    pub title: String,
    pub session_id: String,
    pub started_secs_ago: f64,
    pub turn_count: u32,
    pub tool_call_count: u32,
    pub last_event: Option<String>,
    pub last_message: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct RetryRow {
    pub identifier: String,
    pub attempt: u32,
    pub due_in_secs: f64,
    pub error: Option<String>,
}

#[derive(Clone)]
struct AppState {
    status_rx: watch::Receiver<StatusSnapshot>,
    /// A project's `EffectiveConfig::workflow_dir` -- where `symphony.db`
    /// (`eventlog::DB_FILENAME`) and `board.db` (`board::DB_FILENAME`) both live,
    /// same directory `symphony-report.html` already defaults to. Stored as the
    /// directory (not a pre-joined file path) so this one field can feed both
    /// `eventlog`'s functions (which want the full `.../symphony.db` path) and
    /// `board::router` (which wants the directory and joins its own filename).
    workflow_dir: PathBuf,
    /// URL path this router is mounted under -- `""` when served at the root (the
    /// single-project CLI path, `serve` below), or `/projects/<id>` when nested into
    /// the multi-project service's own router (`src/service.rs`'s `project_proxy`).
    /// Every absolute link/asset URL this module renders (`nav`, `/fragment`'s
    /// `fetch()`, the `/events` links) is prefixed with this so in-page navigation
    /// still lands within the same mount point -- without it, a nested dashboard's
    /// own links would silently escape to the service's top-level routes instead.
    base_path: String,
}

impl AppState {
    fn eventlog_db_path(&self) -> PathBuf {
        self.workflow_dir.join(eventlog::DB_FILENAME)
    }
}

/// Bind and serve the dashboard until the process exits. Loopback-only
/// (`127.0.0.1`) unless `bind_all_interfaces` is set, in which case it binds
/// `0.0.0.0` instead.
///
/// The loopback-only default is a deliberate security choice on a bare host (Section
/// 13.7: not hardened for exposure beyond the local machine). That reasoning flips
/// inside a container, though: a daemonized Symphony (see `crate::daemon`, README.md
/// "Daemonizing Symphony") runs the dashboard inside its *own* container's network
/// namespace, where `127.0.0.1` refers to the container's own loopback interface --
/// unreachable from the host even with the port published via `docker run -p`, since
/// port publishing forwards to a container's external interface, not its loopback.
/// There's no "other users on the same host" to guard against inside that namespace;
/// the container boundary itself is the isolation mechanism, and reachability is
/// already gated by whether `-p` was passed at all.
/// The dashboard/fragment/events/usage routes on their own, with no bind/serve
/// attached -- lets a caller that already owns an axum server (`src/service.rs`'s
/// multi-project web UI) `.nest()` one of these per registered project instead of
/// duplicating any of this module's HTML/handler code.
pub fn router(
    status_rx: watch::Receiver<StatusSnapshot>,
    workflow_dir: PathBuf,
    base_path: &str,
) -> Router {
    let board_router = crate::board::router(workflow_dir.clone(), &format!("{base_path}/board"));
    let state = AppState {
        status_rx,
        workflow_dir,
        base_path: base_path.to_string(),
    };
    Router::new()
        .route("/", get(dashboard))
        .route("/fragment", get(fragment))
        .route("/events", get(events_page))
        .route("/usage", get(usage_page))
        .with_state(state)
        .nest("/board", board_router)
}

pub async fn serve(
    port: u16,
    bind_all_interfaces: bool,
    status_rx: watch::Receiver<StatusSnapshot>,
    workflow_dir: PathBuf,
) -> anyhow::Result<()> {
    let app = router(status_rx, workflow_dir, "");
    let bind_addr = if bind_all_interfaces {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    };
    let listener = tokio::net::TcpListener::bind((bind_addr, port)).await?;
    tracing::info!("status dashboard listening on http://{bind_addr}:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

const STYLE: &str = r#"
  body { font-family: system-ui, sans-serif; background: #111; color: #eee; margin: 0; padding: 24px; }
  h1 { font-size: 1.1rem; font-weight: 600; color: #9cf; margin: 0 0 4px; }
  .meta { color: #888; font-size: 0.8rem; margin-bottom: 12px; }
  nav { margin-bottom: 20px; }
  nav a { color: #9cf; text-decoration: none; margin-right: 16px; font-size: 0.85rem; }
  nav a:hover { text-decoration: underline; }
  .grid { display: flex; flex-wrap: wrap; gap: 12px; margin-bottom: 32px; }
  .card { background: #1c1c1c; border: 1px solid #333; border-left: 4px solid #4caf50; border-radius: 6px; padding: 12px 14px; width: 300px; }
  .card h2 { font-size: 0.95rem; margin: 0 0 6px; color: #fff; }
  .card .row { font-size: 0.8rem; color: #aaa; margin: 2px 0; }
  .card .row b { color: #ccc; }
  .card .msg { margin-top: 8px; font-size: 0.78rem; color: #ddd; background: #151515; border-radius: 4px; padding: 6px 8px; max-height: 4.5em; overflow: hidden; }
  .badge { display: inline-block; background: #2d4d2d; color: #9f9; border-radius: 10px; padding: 1px 8px; font-size: 0.72rem; }
  table { border-collapse: collapse; width: 100%; max-width: 1100px; }
  th, td { text-align: left; padding: 6px 10px; font-size: 0.82rem; border-bottom: 1px solid #2a2a2a; }
  th { color: #888; font-weight: 500; }
  .empty { color: #666; font-style: italic; }
  section h3 { font-size: 0.85rem; text-transform: uppercase; letter-spacing: 0.04em; color: #888; }
  form.filters { margin: 12px 0; display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
  form.filters input, form.filters select { background: #1c1c1c; border: 1px solid #333; color: #eee; padding: 4px 8px; border-radius: 4px; font-size: 0.8rem; }
  form.filters button { background: #2d4d2d; border: 1px solid #3a6a3a; color: #9f9; padding: 4px 12px; border-radius: 4px; font-size: 0.8rem; cursor: pointer; }
  .pager { margin-top: 12px; font-size: 0.82rem; }
  .pager a { color: #9cf; margin-right: 12px; text-decoration: none; }
  .totals { display: flex; gap: 24px; margin-bottom: 24px; flex-wrap: wrap; }
  .totals .stat { background: #1c1c1c; border: 1px solid #333; border-radius: 6px; padding: 10px 16px; min-width: 100px; }
  .totals .stat .n { font-size: 1.4rem; font-weight: 600; color: #9cf; }
  .totals .stat .l { font-size: 0.72rem; color: #888; text-transform: uppercase; letter-spacing: 0.04em; }
"#;

/// `active` and each route below are mount-relative (`"/"`, `"/events"`, `"/usage"`);
/// `base` (`AppState::base_path`) is prepended only to the emitted `href`, so callers
/// keep comparing/passing the same unprefixed identifiers regardless of where this
/// router ends up mounted.
fn nav(active: &str, base: &str) -> String {
    let link = |href: &str, label: &str| {
        if href == active {
            format!(r#"<a href="{base}{href}" style="color:#fff;font-weight:600">{label}</a>"#)
        } else {
            format!(r#"<a href="{base}{href}">{label}</a>"#)
        }
    };
    format!(
        "<nav>{}{}{}{}</nav>",
        link("/", "Status"),
        link("/events", "Events"),
        link("/usage", "Usage"),
        link("/board", "Board"),
    )
}

fn page_shell(title: &str, active_nav: &str, body: &str, extra_head: &str, base: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>Symphony &mdash; {title}</title>
<style>{STYLE}</style>
{extra_head}
</head>
<body>
<h1>Symphony</h1>
{nav}
{body}
</body>
</html>
"#,
        nav = nav(active_nav, base),
    )
}

async fn dashboard(State(state): State<AppState>) -> Html<String> {
    let snapshot = state.status_rx.borrow().clone();
    let script = format!(
        r#"<script>
function refreshFragment() {{
  fetch('{base}/fragment').then(r => r.text()).then(html => {{
    document.getElementById('symphony-fragment').innerHTML = html;
  }}).catch(() => {{}});
}}
setInterval(refreshFragment, 2000);
</script>"#,
        base = state.base_path,
    );
    let body = format!(
        r#"<div class="meta">generated {generated} &middot; live-updates every 2s, in place (no page reload)</div>
<div id="symphony-fragment">{fragment}</div>"#,
        generated = escape(&snapshot.generated_at),
        fragment = render_fragment(&snapshot),
    );
    Html(page_shell(
        "live status",
        "/",
        &body,
        &script,
        &state.base_path,
    ))
}

async fn fragment(State(state): State<AppState>) -> Html<String> {
    let snapshot = state.status_rx.borrow().clone();
    Html(render_fragment(&snapshot))
}

fn render_fragment(s: &StatusSnapshot) -> String {
    let running_cards: String = if s.running.is_empty() {
        "<p class=\"empty\">No agents running.</p>".to_string()
    } else {
        s.running.iter().map(running_card).collect()
    };

    let retry_rows: String = if s.retrying.is_empty() {
        "<tr><td colspan=\"4\" class=\"empty\">Retry queue is empty.</td></tr>".to_string()
    } else {
        s.retrying.iter().map(retry_row).collect()
    };

    format!(
        r#"<section>
<h3>Running <span class="badge">{running}</span></h3>
<div class="grid">
{running_cards}
</div>
</section>

<section>
<h3>Retry queue <span class="badge">{retrying}</span></h3>
<table>
<thead><tr><th>Issue</th><th>Attempt</th><th>Due in</th><th>Last error</th></tr></thead>
<tbody>
{retry_rows}
</tbody>
</table>
</section>"#,
        running = s.running.len(),
        retrying = s.retrying.len(),
        running_cards = running_cards,
        retry_rows = retry_rows,
    )
}

fn running_card(r: &RunningRow) -> String {
    format!(
        r#"<div class="card">
  <h2>{identifier}</h2>
  <div class="row">{title}</div>
  <div class="row"><b>session</b> {session}</div>
  <div class="row"><b>running for</b> {elapsed:.1}s &middot; <b>turn</b> {turn} &middot; <b>tool calls</b> {tools}</div>
  <div class="row"><b>last event</b> {event}</div>
  <div class="msg">{message}</div>
</div>"#,
        identifier = escape(&r.identifier),
        title = escape(&r.title),
        session = escape(&r.session_id),
        elapsed = r.started_secs_ago,
        turn = r.turn_count,
        tools = r.tool_call_count,
        event = escape(r.last_event.as_deref().unwrap_or("-")),
        message = escape(r.last_message.as_deref().unwrap_or("")),
    )
}

fn retry_row(r: &RetryRow) -> String {
    format!(
        "<tr><td>{identifier}</td><td>{attempt}</td><td>{due:.1}s</td><td>{error}</td></tr>",
        identifier = escape(&r.identifier),
        attempt = r.attempt,
        due = r.due_in_secs,
        error = escape(r.error.as_deref().unwrap_or("-")),
    )
}

// --------------------------------------------------------------------------------
// /events -- filterable, paginated browse of eventlog::recent_events
// --------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct EventsQuery {
    issue: Option<String>,
    #[serde(rename = "type")]
    event_type: Option<String>,
    importance: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn events_page(State(state): State<AppState>, Query(q): Query<EventsQuery>) -> Html<String> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let include_low_importance = q.importance.as_deref() == Some("all");
    let filter = eventlog::EventFilter {
        issue_id: q.issue.clone().filter(|s| !s.is_empty()),
        event_type: q.event_type.clone().filter(|s| !s.is_empty()),
        include_low_importance,
    };

    let rows = eventlog::recent_events(&state.eventlog_db_path(), &filter, limit, offset)
        .unwrap_or_default();
    let base = state.base_path.as_str();

    let table_rows: String = if rows.is_empty() {
        "<tr><td colspan=\"6\" class=\"empty\">No events recorded yet.</td></tr>".to_string()
    } else {
        rows.iter().map(|r| event_row(r, base)).collect()
    };

    let importance_toggle = if include_low_importance {
        format!(
            r#"<a href="{}">Hide low-importance (e.g. streaming heartbeats)</a>"#,
            events_link(&q, offset, Some("normal"), base)
        )
    } else {
        format!(
            r#"<a href="{}">Show all (incl. low-importance)</a>"#,
            events_link(&q, offset, Some("all"), base)
        )
    };

    let prev = if offset > 0 {
        format!(
            r#"<a href="{}">&larr; Newer</a>"#,
            events_link(&q, (offset - limit).max(0), None, base)
        )
    } else {
        String::new()
    };
    let next = if rows.len() as i64 == limit {
        format!(
            r#"<a href="{}">Older &rarr;</a>"#,
            events_link(&q, offset + limit, None, base)
        )
    } else {
        String::new()
    };

    let body = format!(
        r#"<form class="filters" method="get">
  <input type="text" name="issue" placeholder="issue id" value="{issue}">
  <input type="text" name="type" placeholder="event type" value="{event_type}">
  <button type="submit">Filter</button>
  {importance_toggle}
</form>
<table>
<thead><tr><th>ID</th><th>Time</th><th>Issue</th><th>Session</th><th>Type</th><th>Message</th><th>Tokens</th></tr></thead>
<tbody>
{table_rows}
</tbody>
</table>
<div class="pager">{prev} {next}</div>"#,
        issue = escape(q.issue.as_deref().unwrap_or("")),
        event_type = escape(q.event_type.as_deref().unwrap_or("")),
        importance_toggle = importance_toggle,
        table_rows = table_rows,
        prev = prev,
        next = next,
    );
    Html(page_shell("events", "/events", &body, "", base))
}

fn events_link(
    q: &EventsQuery,
    offset: i64,
    importance_override: Option<&str>,
    base: &str,
) -> String {
    let mut parts = Vec::new();
    if let Some(issue) = &q.issue
        && !issue.is_empty()
    {
        parts.push(format!("issue={}", urlencode(issue)));
    }
    if let Some(t) = &q.event_type
        && !t.is_empty()
    {
        parts.push(format!("type={}", urlencode(t)));
    }
    let importance = importance_override.or(q.importance.as_deref());
    if let Some(importance) = importance {
        parts.push(format!("importance={}", urlencode(importance)));
    }
    parts.push(format!("offset={offset}"));
    format!("{base}/events?{}", parts.join("&"))
}

fn event_row(r: &eventlog::EventRow, base: &str) -> String {
    let tokens = if r.total_tokens.is_some() {
        format!(
            "{}/{}",
            r.input_tokens.unwrap_or(0),
            r.output_tokens.unwrap_or(0)
        )
    } else {
        "-".to_string()
    };
    // Importance is only worth flagging when it's *not* the default-filtered case --
    // a "normal" row viewed in the default (already-filtered-to-normal) list would
    // just be visual noise repeated on every row.
    let event_type = if r.importance == "low" {
        format!(
            "{} <span class=\"empty\">(low)</span>",
            escape(&r.event_type)
        )
    } else {
        escape(&r.event_type)
    };
    format!(
        "<tr><td>{id}</td><td>{time}</td><td><a href=\"{base}/events?issue={issue_link}\">{identifier}</a> &mdash; {title}</td><td class=\"empty\">{session}</td><td>{event_type}</td><td>{message}</td><td>{tokens}</td></tr>",
        id = r.id,
        time = escape(&r.created_at),
        issue_link = urlencode(&r.issue_id),
        identifier = escape(&r.identifier),
        title = escape(&r.title),
        session = escape(r.session_id.as_deref().unwrap_or("-")),
        event_type = event_type,
        message = escape(r.message.as_deref().unwrap_or("-")),
        tokens = escape(&tokens),
    )
}

// --------------------------------------------------------------------------------
// /usage -- global + per-issue consumption, from eventlog::usage_summary/usage_by_issue
// --------------------------------------------------------------------------------

async fn usage_page(State(state): State<AppState>) -> Html<String> {
    let summary = eventlog::usage_summary(&state.eventlog_db_path()).unwrap_or_default();
    let by_issue = eventlog::usage_by_issue(&state.eventlog_db_path()).unwrap_or_default();

    let base = state.base_path.as_str();
    let issue_rows: String = if by_issue.is_empty() {
        "<tr><td colspan=\"8\" class=\"empty\">No usage recorded yet.</td></tr>".to_string()
    } else {
        by_issue.iter().map(|r| issue_usage_row(r, base)).collect()
    };

    let body = format!(
        r#"<div class="totals">
  <div class="stat"><div class="n">{dispatches}</div><div class="l">Dispatches</div></div>
  <div class="stat"><div class="n">{turns}</div><div class="l">Turns</div></div>
  <div class="stat"><div class="n">{tools}</div><div class="l">Tool calls</div></div>
  <div class="stat"><div class="n">{input_tokens}</div><div class="l">Input tokens</div></div>
  <div class="stat"><div class="n">{output_tokens}</div><div class="l">Output tokens</div></div>
  <div class="stat"><div class="n">{total_tokens}</div><div class="l">Total tokens</div></div>
</div>
<section>
<h3>Per issue</h3>
<table>
<thead><tr><th>Issue</th><th>Dispatches</th><th>Turns</th><th>Tool calls</th><th>Input</th><th>Output</th><th>Total</th><th>Last event</th></tr></thead>
<tbody>
{issue_rows}
</tbody>
</table>
</section>"#,
        dispatches = summary.dispatch_count,
        turns = summary.turn_count,
        tools = summary.tool_call_count,
        input_tokens = summary.input_tokens,
        output_tokens = summary.output_tokens,
        total_tokens = summary.total_tokens,
        issue_rows = issue_rows,
    );
    Html(page_shell("usage", "/usage", &body, "", base))
}

fn issue_usage_row(r: &eventlog::IssueUsageRow, base: &str) -> String {
    format!(
        "<tr><td><a href=\"{base}/events?issue={issue_link}\">{identifier}</a> &mdash; {title}</td><td>{dispatches}</td><td>{turns}</td><td>{tools}</td><td>{input}</td><td>{output}</td><td>{total}</td><td>{last_event} <span class=\"empty\">{last_at}</span></td></tr>",
        issue_link = urlencode(&r.issue_id),
        identifier = escape(&r.identifier),
        title = escape(&r.title),
        dispatches = r.dispatch_count,
        turns = r.turn_count,
        tools = r.tool_call_count,
        input = r.input_tokens,
        output = r.output_tokens,
        total = r.total_tokens,
        last_event = escape(&r.last_event_type),
        last_at = escape(&r.last_event_at),
    )
}

fn urlencode(s: &str) -> String {
    // Minimal, dependency-free percent-encoding: this codebase's identifiers/event
    // types are plain ASCII (issue numbers, snake_case event names), so covering the
    // handful of characters that are actually meaningful in a query string (space,
    // &, =, #, %, +) is enough -- not a general-purpose encoder.
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '&' => "%26".to_string(),
            '=' => "%3D".to_string(),
            '#' => "%23".to_string(),
            '%' => "%25".to_string(),
            '+' => "%2B".to_string(),
            c => c.to_string(),
        })
        .collect()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_fragment_shows_empty_states() {
        let snapshot = StatusSnapshot::default();
        let html = render_fragment(&snapshot);
        assert!(html.contains("No agents running"));
        assert!(html.contains("Retry queue is empty"));
    }

    #[test]
    fn running_card_escapes_untrusted_content() {
        let row = RunningRow {
            identifier: "1".to_string(),
            title: "<script>alert(1)</script>".to_string(),
            session_id: "sess".to_string(),
            started_secs_ago: 1.0,
            turn_count: 1,
            tool_call_count: 0,
            last_event: None,
            last_message: None,
        };
        let html = running_card(&row);
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn event_row_renders_tokens_when_present() {
        let row = eventlog::EventRow {
            id: 1,
            issue_id: "1".to_string(),
            identifier: "1".to_string(),
            title: "Some issue".to_string(),
            session_id: None,
            event_type: "tool_call".to_string(),
            importance: "normal".to_string(),
            message: Some("Edit".to_string()),
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: Some(150),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let html = event_row(&row, "");
        assert!(html.contains("100/50"));
    }

    #[test]
    fn event_row_renders_dash_when_no_tokens() {
        let row = eventlog::EventRow {
            id: 1,
            issue_id: "1".to_string(),
            identifier: "1".to_string(),
            title: "Some issue".to_string(),
            session_id: None,
            event_type: "dispatched".to_string(),
            importance: "normal".to_string(),
            message: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let html = event_row(&row, "");
        assert!(html.contains(">-<"));
    }

    #[test]
    fn urlencode_handles_query_meaningful_characters() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a&b"), "a%26b");
        assert_eq!(urlencode("plain"), "plain");
    }
}
