//! Live status dashboard (subset of the OPTIONAL Section 13.7 HTTP extension), plus
//! two browsable pages backed by the SQLite event log (`src/eventlog.rs`): `/events`
//! (filterable, paginated raw history) and `/usage` (token/turn/tool-call consumption,
//! globally and per-issue). Only enabled when a `--port` is passed on the CLI.
//! Intended for watching dispatch/concurrency behavior during development and test
//! runs, not as a production dashboard.
//!
//! `/` used to full-page-reload every second via `<meta http-equiv="refresh">` --
//! every tick was a real browser navigation (scroll position reset, any open
//! selection lost, a visible flash). It now renders once and subscribes to
//! `/fragment-stream` (Server-Sent Events, `fragment_stream` below) via a small inline
//! `<script>` (`EventSource` + `innerHTML` swap on the one data container), which has
//! none of that: no navigation event, so nothing about the page *outside* that
//! container ever moves -- and unlike the flat-interval poll this replaced, a push
//! only happens when `status_rx` actually changes, landing as soon as it does rather
//! than up to one poll interval late. `/fragment-stream` reuses the exact same
//! `render_fragment`/`running_card`/`retry_row` functions `/` itself uses to build its
//! initial paint -- no HTML templating duplicated into JS, everything server-rendered
//! as before. The plain `/fragment` endpoint (a single non-streaming render) stays
//! mounted alongside it for anything that wants a one-shot fetch instead of a stream.

use crate::artifacts;
use crate::eventlog;
use crate::web::{escape, urlencode};
use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::path::PathBuf;
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::{Stream, StreamExt};

#[derive(Clone, Serialize, Default)]
pub struct StatusSnapshot {
    pub generated_at: String,
    pub running: Vec<RunningRow>,
    pub retrying: Vec<RetryRow>,
}

#[derive(Clone, Serialize)]
pub struct RunningRow {
    /// The tracker's internal issue id -- distinct from `identifier` (the
    /// human-readable label rendered on the card), and the same value
    /// `eventlog::EventRow::issue_id`/the `/events?issue=` filter key on -- so a click
    /// on a running card's title can deep-link straight to that issue's events
    /// (`running_card` below).
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub session_id: String,
    pub started_secs_ago: f64,
    pub turn_count: u32,
    pub tool_call_count: u32,
    pub last_event: Option<String>,
    pub last_message: Option<String>,
    /// Current pipeline stage id (`pipeline.stages[].id`), when the AI Roadmap delivery
    /// pipeline (`pipeline.enabled`) is on for this project. `None` for the legacy
    /// single-stage path, or before the first stage has started.
    pub stage: Option<String>,
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
    /// (`eventlog::DB_FILENAME`) lives, same directory `symphony-report.html`
    /// already defaults to.
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
///
/// `chat`: when set (the SweBot chat UI, `swebot::chat::web::router`), it is nested
/// under `/chat` beside the dashboard routes. Passed in rather than built here so
/// callers keep the router-rendering concern in `swebot::chat::web` and this stays
/// purely "bind what I was handed and serve it."
///
/// The dashboard/fragment/events/usage routes on their own, with no bind/serve
/// attached -- lets a caller that already owns an axum server (`src/service.rs`'s
/// multi-project web UI) `.nest()` one of these per registered project instead of
/// duplicating any of this module's HTML/handler code.
pub fn router(
    status_rx: watch::Receiver<StatusSnapshot>,
    workflow_dir: PathBuf,
    base_path: &str,
) -> Router {
    let state = AppState {
        status_rx,
        workflow_dir,
        base_path: base_path.to_string(),
    };
    Router::new()
        .route("/", get(dashboard))
        .route("/fragment", get(fragment))
        .route("/fragment-stream", get(fragment_stream))
        .route("/events", get(events_page))
        .route("/usage", get(usage_page))
        .route("/artifacts", get(artifacts_page))
        .route("/artifacts/{id}", get(artifact_raw_page))
        .route("/requirements", get(requirements_page))
        .route(
            "/requirements/{issue_id}/unblock",
            post(unblock_clarification),
        )
        .with_state(state)
}

pub async fn serve_composite(
    port: u16,
    bind_all_interfaces: bool,
    status_rx: watch::Receiver<StatusSnapshot>,
    workflow_dir: PathBuf,
    chat: Option<Router>,
) -> anyhow::Result<()> {
    let mut app = router(status_rx, workflow_dir, "");
    if let Some(chat) = chat {
        app = app.nest("/chat", chat);
    }
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

fn page_shell(title: &str, active_nav: &str, body: &str, extra_head: &str, base: &str) -> String {
    crate::web::page_shell(
        "Symphony",
        title,
        &crate::web::nav(crate::web::NAV_LINKS, active_nav, base),
        body,
        extra_head,
    )
}

async fn dashboard(State(state): State<AppState>) -> Html<String> {
    let snapshot = state.status_rx.borrow().clone();
    // `EventSource` pushes a re-render only when `status_rx` actually changes
    // (`fragment_stream` below), instead of the previous flat 2s poll -- fewer
    // requests when nothing's happening, and updates land as soon as they occur
    // rather than up to 2s late. `EventSource` reconnects automatically on its own
    // (with backoff) if the connection drops, so no reconnect logic is needed here.
    //
    // Each push used to be a flat `innerHTML` replace of the whole fragment --
    // simple, but it also silently collapsed any `.msg.expanded` click-to-expand
    // state and would have clobbered a text selection inside the fragment on every
    // update. `diffChildren` instead keys each running card (`#run-<issue_id>`) and
    // retry row (`#retry-<identifier>`) and only touches nodes whose rendered HTML
    // actually changed, reordering/adding/removing by id rather than tearing down
    // the whole subtree -- same approach `swebot::chat::web`'s `patchNode`/
    // `renderThreadList` use for the same reason.
    let script = format!(
        r#"<script>
function diffChildren(container, incoming) {{
  const existing = new Map();
  Array.from(container.children).forEach(function (el) {{ existing.set(el.id, el); }});
  const seen = new Set();
  let prev = null;
  Array.from(incoming.children).forEach(function (el) {{
    seen.add(el.id);
    let node = existing.get(el.id);
    if (node) {{
      if (node.id && node.outerHTML !== el.outerHTML) {{
        // A card being replaced loses its expanded state along with the rest of
        // the node -- restore it on the freshly rendered replacement.
        const wasExpanded = node.querySelector('.msg.expanded') != null;
        node.replaceWith(el);
        if (wasExpanded) {{
          const msg = el.querySelector('.msg[data-expandable]');
          if (msg) msg.classList.add('expanded');
        }}
        node = el;
      }}
    }} else {{
      node = el;
    }}
    const wantNext = prev ? prev.nextSibling : container.firstChild;
    if (wantNext !== node) container.insertBefore(node, wantNext);
    prev = node;
  }});
  existing.forEach(function (el, id) {{
    if (!seen.has(id)) el.remove();
  }});
}}
new EventSource('{base}/fragment-stream').onmessage = function (e) {{
  const incoming = document.createElement('div');
  incoming.innerHTML = e.data;
  const runningCount = incoming.querySelector('#running-count');
  const retryingCount = incoming.querySelector('#retrying-count');
  if (runningCount) document.getElementById('running-count').textContent = runningCount.textContent;
  if (retryingCount) document.getElementById('retrying-count').textContent = retryingCount.textContent;
  diffChildren(document.getElementById('running-grid'), incoming.querySelector('#running-grid'));
  diffChildren(document.getElementById('retry-tbody'), incoming.querySelector('#retry-tbody'));
}};
</script>"#,
        base = state.base_path,
    );
    let body = format!(
        r#"<div class="meta">generated {generated} &middot; live-updates in place, pushed as they happen (no page reload, no polling)</div>
<div id="symphony-fragment" aria-live="polite" role="status">{fragment}</div>"#,
        generated = escape(&snapshot.generated_at),
        fragment = render_fragment(&snapshot, &state.base_path),
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
    Html(render_fragment(&snapshot, &state.base_path))
}

/// Server-Sent Events push of the same fragment `/fragment` renders on demand, but
/// driven directly off `status_rx` (a `watch::Receiver`) instead of a client poll
/// interval: `WatchStream` yields the current value immediately on subscribe, then a
/// new one each time the orchestrator's status-publishing side calls `watch::Sender::
/// send` -- no fixed-interval guesswork, no requests while nothing's changed.
/// `KeepAlive` sends periodic SSE comment pings so idle proxies/load balancers don't
/// time out the connection during a long quiet stretch.
async fn fragment_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let base = state.base_path.clone();
    let stream = WatchStream::new(state.status_rx.clone())
        .map(move |snapshot| Ok(Event::default().data(render_fragment(&snapshot, &base))));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn render_fragment(s: &StatusSnapshot, base: &str) -> String {
    let running_cards: String = if s.running.is_empty() {
        "<p class=\"empty\">No agents running.</p>".to_string()
    } else {
        s.running.iter().map(|r| running_card(r, base)).collect()
    };

    let retry_rows: String = if s.retrying.is_empty() {
        "<tr><td colspan=\"4\" class=\"empty\">Retry queue is empty.</td></tr>".to_string()
    } else {
        s.retrying.iter().map(retry_row).collect()
    };

    format!(
        r#"<section>
<h3>Running <span class="badge" id="running-count">{running}</span></h3>
<div class="grid" id="running-grid">
{running_cards}
</div>
</section>

<section>
<h3>Retry queue <span class="badge" id="retrying-count">{retrying}</span></h3>
<table>
<thead><tr><th>Issue</th><th>Attempt</th><th>Due in</th><th>Last error</th></tr></thead>
<tbody id="retry-tbody">
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

fn running_card(r: &RunningRow, base: &str) -> String {
    let message = r.last_message.as_deref().unwrap_or("");
    // Only hint "click to expand" when the message is actually long enough that the
    // 4.5-line clamp (`.msg`'s `max-height` in web::STYLE) would clip it -- showing the
    // hint on a one-line message would be misleading, since expanding it changes nothing.
    let expandable_attr = if message.len() > 140 {
        " data-expandable"
    } else {
        ""
    };
    let stage_row = r
        .stage
        .as_deref()
        .map(|s| format!(r#"<div class="row"><b>stage</b> {}</div>"#, escape(s)))
        .unwrap_or_default();
    format!(
        r#"<div class="card" id="run-{id_attr}">
  <h2><a href="{base}/events?issue={issue_link}">{identifier}</a></h2>
  <div class="row">{title}</div>
  <div class="row"><b>session</b> {session}</div>
  <div class="row"><b>running for</b> {elapsed:.1}s &middot; <b>turn</b> {turn} &middot; <b>tool calls</b> {tools}</div>
  {stage_row}
  <div class="row"><b>last event</b> {event}</div>
  <div class="msg"{expandable_attr}>{message}</div>
</div>"#,
        id_attr = escape(&r.issue_id),
        base = base,
        issue_link = urlencode(&r.issue_id),
        identifier = escape(&r.identifier),
        title = escape(&r.title),
        session = escape(&r.session_id),
        elapsed = r.started_secs_ago,
        turn = r.turn_count,
        tools = r.tool_call_count,
        stage_row = stage_row,
        event = escape(r.last_event.as_deref().unwrap_or("-")),
        message = escape(message),
    )
}

fn retry_row(r: &RetryRow) -> String {
    format!(
        r#"<tr id="retry-{id_attr}"><td>{identifier}</td><td>{attempt}</td><td>{due:.1}s</td><td>{error}</td></tr>"#,
        id_attr = escape(&r.identifier),
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
    /// `"table"` forces the plain sortable table even when `issue` is set; any other
    /// value (or unset) gets the chat-style transcript instead, but only when an
    /// `issue` filter is actually active -- the unfiltered full history stays a
    /// table regardless, since a wall of every issue's events interleaved doesn't
    /// read as one conversation the way a single issue's does.
    view: Option<String>,
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

    let has_issue_filter = filter.issue_id.is_some();
    let use_transcript_view = has_issue_filter && q.view.as_deref() != Some("table");
    let view_toggle = if has_issue_filter {
        if use_transcript_view {
            format!(
                r#" &middot; <a href="{}">Plain table</a>"#,
                events_link_with_view(&q, offset, None, None, Some("table"), base)
            )
        } else {
            format!(
                r#" &middot; <a href="{}">Conversation view</a>"#,
                events_link_with_view(&q, offset, None, None, Some("chat"), base)
            )
        }
    } else {
        String::new()
    };

    let table_rows: String = if rows.is_empty() {
        "<tr><td colspan=\"6\" class=\"empty\">No events recorded yet.</td></tr>".to_string()
    } else {
        rows.iter().map(|r| event_row(r, base)).collect()
    };

    let importance_toggle = if include_low_importance {
        format!(
            r#"<a href="{}">Hide low-importance (e.g. streaming heartbeats)</a>"#,
            events_link(&q, offset, Some("normal"), None, base)
        )
    } else {
        format!(
            r#"<a href="{}">Show all (incl. low-importance)</a>"#,
            events_link(&q, offset, Some("all"), None, base)
        )
    };

    let prev = if offset > 0 {
        format!(
            r#"<a href="{}">&larr; Newer</a>"#,
            events_link(&q, (offset - limit).max(0), None, None, base)
        )
    } else {
        String::new()
    };
    let next = if rows.len() as i64 == limit {
        format!(
            r#"<a href="{}">Older &rarr;</a>"#,
            events_link(&q, offset + limit, None, None, base)
        )
    } else {
        String::new()
    };
    let event_types = eventlog::distinct_event_types(&state.eventlog_db_path()).unwrap_or_default();
    let type_options: String = event_types
        .iter()
        .map(|t| format!(r#"<option value="{}">"#, escape(t)))
        .collect();

    let listing = if use_transcript_view {
        format!(
            r#"<div class="transcript">{}</div>"#,
            render_transcript(&rows, base)
        )
    } else {
        format!(
            r#"<div class="table-wrap">
<table>
<thead><tr><th data-sort>ID</th><th data-sort>Time</th><th data-sort>Issue</th><th data-sort>Session</th><th data-sort>Type</th><th>Message</th><th data-sort>Tokens</th></tr></thead>
<tbody>
{table_rows}
</tbody>
</table>
</div>"#
        )
    };

    let body = format!(
        r#"<form class="filters" method="get">
  <label for="f-issue">Issue</label>
  <input type="text" id="f-issue" name="issue" placeholder="issue id" value="{issue}">
  <label for="f-type">Type</label>
  <input type="text" id="f-type" name="type" placeholder="event type" value="{event_type}" list="event-types">
  <datalist id="event-types">{type_options}</datalist>
  <button type="submit">Filter</button>
  {importance_toggle}{view_toggle}
</form>
{chips}
{listing}
<div class="pager">{prev} {next}</div>"#,
        issue = escape(q.issue.as_deref().unwrap_or("")),
        event_type = escape(q.event_type.as_deref().unwrap_or("")),
        type_options = type_options,
        importance_toggle = importance_toggle,
        view_toggle = view_toggle,
        chips = chips(&q, base),
        listing = listing,
        prev = prev,
        next = next,
    );
    Html(page_shell("events", "/events", &body, "", base))
}

/// Which active filter a "clear" chip link should drop -- see `chips()` below.
enum ClearFilter {
    Issue,
    Type,
}

fn events_link(
    q: &EventsQuery,
    offset: i64,
    importance_override: Option<&str>,
    clear: Option<ClearFilter>,
    base: &str,
) -> String {
    events_link_with_view(q, offset, importance_override, clear, None, base)
}

/// `events_link` plus an optional `view=` override -- its own function rather than a
/// blanket extra param on every `events_link` call site, since only the transcript/
/// table toggle (`events_page`) ever needs to set it.
fn events_link_with_view(
    q: &EventsQuery,
    offset: i64,
    importance_override: Option<&str>,
    clear: Option<ClearFilter>,
    view_override: Option<&str>,
    base: &str,
) -> String {
    let mut parts = Vec::new();
    if !matches!(clear, Some(ClearFilter::Issue))
        && let Some(issue) = &q.issue
        && !issue.is_empty()
    {
        parts.push(format!("issue={}", urlencode(issue)));
    }
    if !matches!(clear, Some(ClearFilter::Type))
        && let Some(t) = &q.event_type
        && !t.is_empty()
    {
        parts.push(format!("type={}", urlencode(t)));
    }
    let importance = importance_override.or(q.importance.as_deref());
    if let Some(importance) = importance {
        parts.push(format!("importance={}", urlencode(importance)));
    }
    let view = view_override.or(q.view.as_deref());
    if let Some(view) = view {
        parts.push(format!("view={}", urlencode(view)));
    }
    parts.push(format!("offset={offset}"));
    format!("{base}/events?{}", parts.join("&"))
}

/// Active-filter chips row: one chip per non-empty filter with a small "x" that links
/// to the same page minus that one filter, plus a "clear all" link when more than one
/// filter is active. Previously the only feedback that a filter was applied was the
/// URL itself.
fn chips(q: &EventsQuery, base: &str) -> String {
    let mut items = Vec::new();
    if let Some(issue) = q.issue.as_deref().filter(|s| !s.is_empty()) {
        items.push(format!(
            r#"<span class="chip">issue: {}<a href="{}" aria-label="clear issue filter">&times;</a></span>"#,
            escape(issue),
            events_link(q, 0, None, Some(ClearFilter::Issue), base)
        ));
    }
    if let Some(t) = q.event_type.as_deref().filter(|s| !s.is_empty()) {
        items.push(format!(
            r#"<span class="chip">type: {}<a href="{}" aria-label="clear type filter">&times;</a></span>"#,
            escape(t),
            events_link(q, 0, None, Some(ClearFilter::Type), base)
        ));
    }
    if items.is_empty() {
        return String::new();
    }
    if items.len() > 1 {
        items.push(format!(
            r#"<span class="chip"><a href="{base}/events">clear all</a></span>"#
        ));
    }
    format!(r#"<div class="chips">{}</div>"#, items.join(""))
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
        "<tr><td>{id}</td><td>{time}</td><td><a href=\"{base}/events?issue={issue_link}\">{identifier}</a> &mdash; {title}</td><td class=\"empty\">{session}</td><td>{event_type}</td><td class=\"msg-cell\">{message}</td><td>{tokens}</td></tr>",
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

/// Chat-style rendering of a single issue's events -- the "jump to this running
/// job's events" view (`running_card`'s link). Reuses the exact `.msg`/`.bubble`/
/// `.status` markup and classes `swebot::chat::web`'s JS renders for live chat
/// messages (both draw on the shared rules in `web::STYLE`'s "Chat-bubble
/// transcript" block), so an issue's dispatch/turn/tool-call history reads as one
/// conversation instead of a table row per event. `rows` comes in newest-first
/// (`eventlog::recent_events`'s own order, matching the table view); a transcript
/// reads top-to-bottom oldest-first, so this reverses it for display.
fn render_transcript(rows: &[eventlog::EventRow], base: &str) -> String {
    if rows.is_empty() {
        return r#"<p class="empty">No events recorded yet.</p>"#.to_string();
    }
    rows.iter()
        .rev()
        .map(|r| transcript_bubble(r, base))
        .collect()
}

/// Which bubble variant an event type reads as: `other_message` (and anything else
/// with "message" in its name) is Claude's own streamed text -- the thing a human
/// actually wants to read, so it renders full-size like a chat reply. Anything
/// naming a tool call gets a distinct monospace "tool" bubble. Everything else
/// (dispatched, turn started, retry scheduled, worker exited, ...) is dispatch
/// bookkeeping -- present for completeness but not the point of reading this
/// transcript, so it renders small/muted like chat's own system notices.
fn transcript_role(event_type: &str) -> &'static str {
    if event_type.contains("tool") {
        "tool"
    } else if event_type.contains("message") {
        "assistant"
    } else {
        "system"
    }
}

fn transcript_bubble(r: &eventlog::EventRow, base: &str) -> String {
    let role = transcript_role(&r.event_type);
    let message = r.message.as_deref().unwrap_or("");
    let body = if message.is_empty() {
        format!(r#"<em class="empty">{}</em>"#, escape(&r.event_type))
    } else {
        escape(message)
    };
    let tokens = r
        .total_tokens
        .map(|t| format!(" &middot; {t} tokens"))
        .unwrap_or_default();
    format!(
        r#"<div class="msg {role}">
  <div class="bubble">{body}</div>
  <div class="status"><span class="time">{time}</span> &middot; <a href="{base}/events?issue={issue_link}&amp;type={type_link}">{event_type}</a>{tokens}</div>
</div>"#,
        role = role,
        body = body,
        time = escape(&r.created_at),
        base = base,
        issue_link = urlencode(&r.issue_id),
        type_link = urlencode(&r.event_type),
        event_type = escape(&r.event_type),
        tokens = tokens,
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
        r#"<div class="stats">
  <div class="stat"><div class="n">{dispatches}</div><div class="l">Dispatches</div></div>
  <div class="stat"><div class="n">{turns}</div><div class="l">Turns</div></div>
  <div class="stat"><div class="n">{tools}</div><div class="l">Tool calls</div></div>
  <div class="stat"><div class="n">{input_tokens}</div><div class="l">Input tokens</div></div>
  <div class="stat"><div class="n">{output_tokens}</div><div class="l">Output tokens</div></div>
  <div class="stat"><div class="n">{total_tokens}</div><div class="l">Total tokens</div></div>
</div>
<section>
<h3>Per issue</h3>
<div class="table-wrap">
<table>
<thead><tr><th data-sort>Issue</th><th data-sort>Dispatches</th><th data-sort>Turns</th><th data-sort>Tool calls</th><th data-sort>Input</th><th data-sort>Output</th><th data-sort>Total</th><th>Last event</th></tr></thead>
<tbody>
{issue_rows}
</tbody>
</table>
</div>
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

// --------------------------------------------------------------------------------
// /artifacts -- browsable AIR-3 artifact store (`crate::artifacts`), grouped by issue
// and cycle, plus a per-artifact raw view. Read-only: recording only ever happens
// through the `record_artifact` agent tool, never from this dashboard -- there's no
// human action to expose here (unlike a future "approve"/"override" surface), just
// visibility into what each cycle produced.
// --------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct ArtifactsQuery {
    cycle: Option<String>,
}

async fn artifacts_page(
    State(state): State<AppState>,
    Query(q): Query<ArtifactsQuery>,
) -> Html<String> {
    let db_path = state.eventlog_db_path();
    let rows = match q.cycle.as_deref().filter(|s| !s.is_empty()) {
        Some(cycle) => artifacts::list_for_cycle(&db_path, cycle),
        None => artifacts::list_all(&db_path),
    };
    let base = state.base_path.as_str();

    let table_rows: String = if rows.is_empty() {
        "<tr><td colspan=\"6\" class=\"empty\">No artifacts recorded yet.</td></tr>".to_string()
    } else {
        rows.iter().map(|r| artifact_row(r, base)).collect()
    };

    let filter_chip = q
        .cycle
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|cycle| {
            format!(
                r#"<div class="chips"><span class="chip">cycle: {}<a href="{base}/artifacts" aria-label="clear cycle filter">&times;</a></span></div>"#,
                escape(cycle)
            )
        })
        .unwrap_or_default();

    let body = format!(
        r#"<form class="filters" method="get">
  <label for="f-cycle">Cycle</label>
  <input type="text" id="f-cycle" name="cycle" placeholder="issue id" value="{cycle}">
  <button type="submit">Filter</button>
</form>
{filter_chip}
<div class="table-wrap">
<table>
<thead><tr><th data-sort>Recorded</th><th data-sort>Issue</th><th data-sort>Stage</th><th data-sort>Kind</th><th>Summary</th><th data-sort>Content type</th></tr></thead>
<tbody>
{table_rows}
</tbody>
</table>
</div>"#,
        cycle = escape(q.cycle.as_deref().unwrap_or("")),
        filter_chip = filter_chip,
        table_rows = table_rows,
    );
    Html(page_shell("artifacts", "/artifacts", &body, "", base))
}

fn artifact_row(r: &artifacts::ArtifactRow, base: &str) -> String {
    format!(
        "<tr><td>{created}</td><td><a href=\"{base}/artifacts?cycle={cycle_link}\">{issue}</a></td><td>{stage}</td>\
         <td>{kind}</td><td><a href=\"{base}/artifacts/{id_link}\">{summary}</a></td><td class=\"empty\">{content_type}</td></tr>",
        created = escape(&r.created_at),
        cycle_link = urlencode(&r.cycle_id),
        issue = escape(&r.issue_identifier),
        stage = escape(r.stage_id.as_deref().unwrap_or("-")),
        kind = escape(&r.kind),
        id_link = urlencode(&r.id),
        summary = escape(&r.summary),
        content_type = escape(&r.content_type),
    )
}

async fn artifact_raw_page(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Html<String>, StatusCode> {
    let db_path = state.eventlog_db_path();
    let row = artifacts::find_by_id(&db_path, &id).ok_or(StatusCode::NOT_FOUND)?;
    let base = state.base_path.as_str();
    let content = artifacts::read_content(&state.workflow_dir, &row)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_else(|e| format!("(could not read artifact content: {e})"));

    let body = format!(
        r#"<div class="meta">
  <div class="row"><b>id</b> {id}</div>
  <div class="row"><b>issue</b> <a href="{base}/artifacts?cycle={cycle_link}">{issue}</a></div>
  <div class="row"><b>stage</b> {stage}</div>
  <div class="row"><b>kind</b> {kind}</div>
  <div class="row"><b>content type</b> {content_type}</div>
  <div class="row"><b>sha256</b> {sha}</div>
  <div class="row"><b>recorded</b> {created}</div>
  <div class="row"><b>summary</b> {summary}</div>
</div>
<pre class="table-wrap">{content}</pre>"#,
        id = escape(&row.id),
        base = base,
        cycle_link = urlencode(&row.cycle_id),
        issue = escape(&row.issue_identifier),
        stage = escape(row.stage_id.as_deref().unwrap_or("-")),
        kind = escape(&row.kind),
        content_type = escape(&row.content_type),
        sha = escape(&row.sha256),
        created = escape(&row.created_at),
        summary = escape(&row.summary),
        content = escape(&content),
    );
    Ok(Html(page_shell(
        &format!("artifact {}", row.id),
        "/artifacts",
        &body,
        "",
        base,
    )))
}

// --------------------------------------------------------------------------------
// /requirements -- AIR-4's requirements/acceptance-criteria/clarification panel: the
// requirements stage's own state+history+explain surface, read straight from
// `eventlog::artifacts_for_issue` (the `requirements`/`acceptance_criteria` artifacts)
// and `eventlog::recent_events` (`clarification_raised`, already recorded whenever
// `raise_clarification` is called -- see `agent/claude.rs`/`orchestrator.rs`). The one
// action this panel implies -- resuming a cycle a blocking clarification parked -- is
// `unblock_clarification` below: a real POST, not a link.
// --------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct RequirementsQuery {
    issue: Option<String>,
}

async fn requirements_page(
    State(state): State<AppState>,
    Query(q): Query<RequirementsQuery>,
) -> Html<String> {
    let base = state.base_path.as_str();
    let Some(issue_id) = q.issue.filter(|s| !s.is_empty()) else {
        let body = format!(
            r#"<p>Pass an issue id, e.g. <code>{base}/requirements?issue=1</code>. \
               Find one on the <a href="{base}/events">Events</a> page.</p>"#
        );
        return Html(page_shell("requirements", "/requirements", &body, "", base));
    };

    let db = state.eventlog_db_path();
    let requirements_artifact = eventlog::get_artifact(&db, &issue_id, "requirements").unwrap_or_default();
    let requirements: Vec<serde_json::Value> = requirements_artifact
        .as_ref()
        .and_then(|a| serde_json::from_str(&a.content).ok())
        .unwrap_or_default();
    let acceptance_criteria_artifact =
        eventlog::get_artifact(&db, &issue_id, "acceptance_criteria").unwrap_or_default();
    let acceptance_criteria: Vec<serde_json::Value> = acceptance_criteria_artifact
        .as_ref()
        .and_then(|a| serde_json::from_str(&a.content).ok())
        .unwrap_or_default();
    let last_updated = [&requirements_artifact, &acceptance_criteria_artifact]
        .into_iter()
        .flatten()
        .map(|a| a.created_at.clone())
        .max();

    let clarification_filter = eventlog::EventFilter {
        issue_id: Some(issue_id.clone()),
        event_type: Some("clarification_raised".to_string()),
        include_low_importance: true,
    };
    let clarifications = eventlog::recent_events(&db, &clarification_filter, 100, 0).unwrap_or_default();
    let has_blocking = clarifications.iter().any(|c| {
        c.message
            .as_deref()
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .and_then(|v| v.get("blocking").and_then(|b| b.as_bool()))
            == Some(true)
    });

    let req_rows: String = if requirements.is_empty() {
        r#"<tr><td colspan="6" class="empty">No requirements recorded yet.</td></tr>"#.to_string()
    } else {
        requirements.iter().map(requirement_row).collect()
    };
    let ac_rows: String = if acceptance_criteria.is_empty() {
        r#"<tr><td colspan="5" class="empty">No acceptance criteria recorded yet.</td></tr>"#
            .to_string()
    } else {
        acceptance_criteria.iter().map(acceptance_criterion_row).collect()
    };
    let clarification_items: String = if clarifications.is_empty() {
        r#"<p class="empty">No clarifications raised.</p>"#.to_string()
    } else {
        clarifications
            .iter()
            .map(clarification_item)
            .collect()
    };

    let unblock_form = if has_blocking {
        format!(
            r#"<form method="post" action="{base}/requirements/{issue_link}/unblock">
  <button type="submit">Mark answered &amp; resume cycle</button>
  <span class="empty">Parks are lifted by moving the issue back to an active tracker state; make sure a human has actually answered the question above first.</span>
</form>"#,
            issue_link = urlencode(&issue_id),
        )
    } else {
        String::new()
    };

    let body = format!(
        r#"<form class="filters" method="get">
  <label for="f-issue">Issue</label>
  <input type="text" id="f-issue" name="issue" placeholder="issue id" value="{issue}">
  <button type="submit">Load</button>
  <a href="{base}/events?issue={issue_link}">View raw events for this issue</a>
</form>
<div class="meta">{last_updated}</div>

<section>
<h3>Requirements <span class="badge">{req_count}</span></h3>
<div class="table-wrap">
<table>
<thead><tr><th>ID</th><th>Type</th><th>Statement</th><th>Constraint</th><th>Dependency</th><th>Assumption</th></tr></thead>
<tbody>{req_rows}</tbody>
</table>
</div>
</section>

<section>
<h3>Acceptance criteria <span class="badge">{ac_count}</span></h3>
<div class="table-wrap">
<table>
<thead><tr><th>ID</th><th>Requirements</th><th>Given</th><th>When</th><th>Then</th></tr></thead>
<tbody>{ac_rows}</tbody>
</table>
</div>
</section>

<section>
<h3>Clarifications <span class="badge">{cl_count}</span></h3>
{clarification_items}
{unblock_form}
</section>"#,
        issue = escape(&issue_id),
        issue_link = urlencode(&issue_id),
        last_updated = last_updated
            .map(|t| format!("last recorded {}", escape(&t)))
            .unwrap_or_else(|| "nothing recorded yet for this issue".to_string()),
        req_count = requirements.len(),
        ac_count = acceptance_criteria.len(),
        cl_count = clarifications.len(),
        req_rows = req_rows,
        ac_rows = ac_rows,
        clarification_items = clarification_items,
        unblock_form = unblock_form,
    );
    Html(page_shell("requirements", "/requirements", &body, "", base))
}

fn requirement_row(r: &serde_json::Value) -> String {
    let get = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let assumption = r
        .get("assumption")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    format!(
        "<tr><td>{id}</td><td>{ty}</td><td>{statement}</td><td>{constraint}</td><td>{dependency}</td><td>{assumption}</td></tr>",
        id = escape(get("id")),
        ty = escape(get("type")),
        statement = escape(get("statement")),
        constraint = escape(get("constraint")),
        dependency = escape(get("dependency")),
        assumption = if assumption { "yes" } else { "" },
    )
}

fn acceptance_criterion_row(a: &serde_json::Value) -> String {
    let get = |k: &str| a.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let requirement_ids: String = a
        .get("requirement_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    format!(
        "<tr><td>{id}</td><td>{reqs}</td><td>{given}</td><td>{when}</td><td>{then}</td></tr>",
        id = escape(get("id")),
        reqs = escape(&requirement_ids),
        given = escape(get("given")),
        when = escape(get("when")),
        then = escape(get("then")),
    )
}

/// One `clarification_raised` event, parsed back out of its JSON `message` (the raw
/// `raise_clarification` tool arguments -- see `agent/claude.rs`) -- the explanation
/// surface this panel owes a human: not just "the cycle stopped" but the exact
/// question, whether it was blocking, and which requirement (if any) it concerns.
fn clarification_item(e: &eventlog::EventRow) -> String {
    let parsed = e
        .message
        .as_deref()
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .unwrap_or_default();
    let question = parsed
        .get("question")
        .and_then(|v| v.as_str())
        .unwrap_or("(no question given)");
    let blocking = parsed.get("blocking").and_then(|v| v.as_bool()).unwrap_or(false);
    let requirement_id = parsed.get("requirement_id").and_then(|v| v.as_str());
    let kind = if blocking {
        r#"<span class="badge" style="background:#c0392b">blocking</span>"#
    } else {
        r#"<span class="badge">non-blocking (assumption)</span>"#
    };
    let req_note = requirement_id
        .map(|r| format!(" &middot; concerns {}", escape(r)))
        .unwrap_or_default();
    format!(
        r#"<div class="card"><div class="row">{kind}{req_note} &middot; {at}</div><div class="row">{question}</div></div>"#,
        kind = kind,
        req_note = req_note,
        at = escape(&e.created_at),
        question = escape(question),
    )
}

fn admin_token_allows(headers: &HeaderMap) -> bool {
    let expected = std::env::var("SYMPHONY_ADMIN_TOKEN").unwrap_or_default();
    if expected.is_empty() {
        // No token configured: same open-by-default posture the rest of this
        // dashboard already has (Section 13.7 -- loopback-only, not hardened for
        // exposure beyond the local machine). Setting SYMPHONY_ADMIN_TOKEN opts a
        // deployment into requiring it here too.
        return true;
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if bearer == Some(expected.as_str()) {
        return true;
    }
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .into_iter()
        .flat_map(|s| s.split(';'))
        .filter_map(|part| part.trim().strip_prefix("symphony_admin="))
        .any(|token| token == expected)
}

/// Resumes a cycle a blocking `raise_clarification` parked: moves the issue back to
/// the project's first configured `tracker.active_states` entry, the same
/// `TrackerAdapter::set_issue_state` host-side path `orchestrator::block_issue` used
/// to park it (AIR-1's dispatch loop then simply picks it back up on its own, same as
/// any other active-state issue -- no separate "resume" mechanism needed). Rebuilds a
/// short-lived tracker adapter from `WORKFLOW.md` per request, same as `mcp.rs`'s
/// pipeline-tool gating does in the MCP subprocess -- this dashboard has no long-lived
/// tracker handle of its own.
async fn unblock_clarification(
    State(state): State<AppState>,
    AxumPath(issue_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if !admin_token_allows(&headers) {
        return (StatusCode::UNAUTHORIZED, "invalid or missing admin token").into_response();
    }
    let def = match crate::workflow::load(&state.workflow_dir.join("WORKFLOW.md")) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load WORKFLOW.md: {e}"),
            )
                .into_response();
        }
    };
    let cfg = match crate::config::resolve(&def.config, &state.workflow_dir) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to resolve config: {e}"),
            )
                .into_response();
        }
    };
    let Some(target_state) = cfg.active_states.first() else {
        return (
            StatusCode::BAD_REQUEST,
            "project has no tracker.active_states configured",
        )
            .into_response();
    };
    let adapter = match crate::tracker::build(&cfg.tracker_kind, &cfg.tracker_provider, &state.workflow_dir) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to build tracker adapter: {e}"),
            )
                .into_response();
        }
    };
    if let Err(e) = adapter.set_issue_state(&issue_id, target_state).await {
        return (
            StatusCode::BAD_GATEWAY,
            format!("failed to move issue back to an active state: {e}"),
        )
            .into_response();
    }
    Redirect::to(&format!(
        "{}/requirements?issue={}",
        state.base_path,
        urlencode(&issue_id)
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fragment_stream_pushes_initial_snapshot_then_updates_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = watch::channel(StatusSnapshot::default());
        let app = router(rx, dir.path().to_path_buf(), "");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let mut resp = client
            .get(format!("http://{addr}/fragment-stream"))
            .send()
            .await
            .unwrap();

        let first = tokio::time::timeout(std::time::Duration::from_secs(5), resp.chunk())
            .await
            .expect("timed out waiting for initial SSE event")
            .unwrap()
            .expect("stream ended before first event");
        let first = String::from_utf8_lossy(&first);
        assert!(first.contains("No agents running"), "{first}");

        tx.send(StatusSnapshot {
            generated_at: "now".to_string(),
            running: vec![RunningRow {
                issue_id: "42".to_string(),
                identifier: "AR-1".to_string(),
                title: "Scaffold".to_string(),
                session_id: "sess".to_string(),
                started_secs_ago: 1.0,
                turn_count: 1,
                tool_call_count: 0,
                last_event: None,
                last_message: None,
                stage: None,
            }],
            retrying: vec![],
        })
        .unwrap();

        let second = tokio::time::timeout(std::time::Duration::from_secs(5), resp.chunk())
            .await
            .expect("timed out waiting for pushed update")
            .unwrap()
            .expect("stream ended before update event");
        let second = String::from_utf8_lossy(&second);
        assert!(second.contains("AR-1"), "{second}");
    }

    #[test]
    fn render_fragment_shows_empty_states() {
        let snapshot = StatusSnapshot::default();
        let html = render_fragment(&snapshot, "");
        assert!(html.contains("No agents running"));
        assert!(html.contains("Retry queue is empty"));
    }

    #[test]
    fn running_card_escapes_untrusted_content() {
        let row = RunningRow {
            issue_id: "1".to_string(),
            identifier: "1".to_string(),
            title: "<script>alert(1)</script>".to_string(),
            session_id: "sess".to_string(),
            started_secs_ago: 1.0,
            turn_count: 1,
            tool_call_count: 0,
            last_event: None,
            last_message: None,
            stage: None,
        };
        let html = running_card(&row, "");
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn running_card_links_to_events_filtered_by_issue_id() {
        let row = RunningRow {
            issue_id: "issue-42".to_string(),
            identifier: "AR-1".to_string(),
            title: "Scaffold".to_string(),
            session_id: "sess".to_string(),
            started_secs_ago: 1.0,
            turn_count: 1,
            tool_call_count: 0,
            last_event: None,
            last_message: None,
            stage: None,
        };
        let html = running_card(&row, "/projects/p1");
        assert!(html.contains(r#"<a href="/projects/p1/events?issue=issue-42">AR-1</a>"#));
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
    fn transcript_role_prioritizes_messages_and_tool_calls_over_bookkeeping() {
        assert_eq!(transcript_role("other_message"), "assistant");
        assert_eq!(transcript_role("tool_call"), "tool");
        assert_eq!(transcript_role("dispatched"), "system");
    }

    fn event_row_fixture(event_type: &str, message: Option<&str>) -> eventlog::EventRow {
        eventlog::EventRow {
            id: 1,
            issue_id: "42".to_string(),
            identifier: "AR-1".to_string(),
            title: "Some issue".to_string(),
            session_id: None,
            event_type: event_type.to_string(),
            importance: "normal".to_string(),
            message: message.map(str::to_string),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn render_transcript_renders_oldest_first_and_escapes_message_bodies() {
        let rows = vec![
            event_row_fixture("other_message", Some("newest reply")),
            event_row_fixture("dispatched", Some("<script>alert(1)</script>")),
        ];
        let html = render_transcript(&rows, "");
        let dispatched_pos = html.find("dispatched").unwrap();
        let reply_pos = html.find("newest reply").unwrap();
        assert!(
            dispatched_pos < reply_pos,
            "oldest (dispatched) event should render before the newer reply"
        );
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains(r#"class="msg assistant""#));
        assert!(html.contains(r#"class="msg system""#));
    }

    #[test]
    fn render_transcript_shows_empty_state() {
        assert!(render_transcript(&[], "").contains("No events recorded yet"));
    }

    #[test]
    fn chips_empty_when_no_filters_active() {
        let q = EventsQuery::default();
        assert_eq!(chips(&q, ""), "");
    }

    #[test]
    fn chips_shows_one_per_active_filter_and_clear_all_when_multiple() {
        let q = EventsQuery {
            issue: Some("42".to_string()),
            event_type: Some("tool_call".to_string()),
            ..Default::default()
        };
        let html = chips(&q, "/base");
        assert!(html.contains("issue: 42"));
        assert!(html.contains("type: tool_call"));
        assert!(html.contains("clear all"));
        assert!(html.contains("/base/events"));
    }

    #[test]
    fn chips_omits_clear_all_when_only_one_filter_active() {
        let q = EventsQuery {
            issue: Some("42".to_string()),
            ..Default::default()
        };
        let html = chips(&q, "");
        assert!(html.contains("issue: 42"));
        assert!(!html.contains("clear all"));
    }
}
