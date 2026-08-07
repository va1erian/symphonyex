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

use crate::eventlog;
use crate::web::{escape, urlencode};
use axum::Router;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
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
        .route("/insights", get(insights_page))
        .route("/metrics", get(metrics_endpoint))
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

// --------------------------------------------------------------------------------
// /insights -- roadmap SS11 success-measure dashboard (src/insights.rs), grouped by
// dimension with a period selector. /metrics -- the same computation, rendered as
// Prometheus text exposition format for the Open Observability Platform to scrape.
//
// Both are plain per-request renders (like /events and /usage above), not pushed
// through /fragment-stream: that mechanism carries live *in-process* orchestrator
// state (`StatusSnapshot`), while insights are a point-in-time read of `symphony.db`
// -- there is nothing to push between requests that a fresh read wouldn't already
// pick up, exactly the same reasoning that already keeps /events and /usage on plain
// per-request renders instead of SSE.
// --------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct InsightsQuery {
    /// `"7d"`, `"30d"`, `"90d"`, or `"all"` (default).
    since: Option<String>,
}

fn since_from_period(period: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let days = match period {
        "7d" => 7,
        "30d" => 30,
        "90d" => 90,
        _ => return None,
    };
    Some(chrono::Utc::now() - chrono::Duration::days(days))
}

async fn insights_page(
    State(state): State<AppState>,
    Query(q): Query<InsightsQuery>,
) -> Html<String> {
    let period = q.since.as_deref().unwrap_or("all");
    let since = since_from_period(period);
    let base = state.base_path.as_str();

    let report = match crate::insights::compute(&state.eventlog_db_path(), since) {
        Ok(r) => r,
        Err(e) => {
            let body = format!(
                "<p class=\"empty\">Failed to compute insights: {}</p>",
                escape(&e.to_string())
            );
            return Html(page_shell("insights", "/insights", &body, "", base));
        }
    };

    let periods = [("all", "All time"), ("7d", "7d"), ("30d", "30d"), ("90d", "90d")];
    let selector: String = periods
        .iter()
        .map(|(value, label)| {
            let class = if *value == period { " class=\"active\"" } else { "" };
            format!(r#"<a href="{base}/insights?since={value}"{class}>{label}</a>"#)
        })
        .collect();

    let dimensions: String = report
        .dimensions
        .iter()
        .map(|dim| {
            let rows: String = dim
                .metrics
                .iter()
                .map(|m| {
                    let value = match &m.value {
                        crate::insights::MetricValue::Number(n) => format!("{n}"),
                        crate::insights::MetricValue::Unknown(_) => {
                            "<span class=\"empty\">unknown</span>".to_string()
                        }
                    };
                    let reason = match &m.value {
                        crate::insights::MetricValue::Unknown(reason) => {
                            format!(" &mdash; <span class=\"empty\">{}</span>", escape(reason))
                        }
                        crate::insights::MetricValue::Number(_) => String::new(),
                    };
                    format!(
                        "<tr><td title=\"{formula}\">{label}</td><td>{value}{reason}</td></tr>",
                        formula = escape(m.formula),
                        label = escape(m.label),
                    )
                })
                .collect();
            format!(
                "<section>\n<h3>{label}</h3>\n<div class=\"table-wrap\">\n<table>\n\
                 <thead><tr><th>Measure</th><th>Value</th></tr></thead>\n<tbody>{rows}</tbody>\n\
                 </table>\n</div>\n</section>",
                label = escape(dim.label),
            )
        })
        .collect();

    let body = format!(
        r#"<div class="filters">{selector}</div>
<p class="empty">Since {since} &middot; as of {until} &middot; hover a measure for its exact formula.</p>
{dimensions}"#,
        selector = selector,
        since = escape(
            &report
                .since
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| "the beginning of history".to_string())
        ),
        until = escape(&report.until.to_rfc3339()),
    );
    Html(page_shell("insights", "/insights", &body, "", base))
}

async fn metrics_endpoint(State(state): State<AppState>) -> impl axum::response::IntoResponse {
    let report = crate::insights::compute(&state.eventlog_db_path(), None).unwrap_or_else(|_| {
        crate::insights::InsightsReport {
            since: None,
            until: chrono::Utc::now(),
            dimensions: Vec::new(),
        }
    });
    (
        [("content-type", "text/plain; version=0.0.4")],
        crate::insights::render_prometheus(&report),
    )
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

    #[tokio::test]
    async fn insights_page_renders_all_five_dimensions_and_a_period_selector() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let app = router(rx, dir.path().to_path_buf(), "/base");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let body = reqwest::get(format!("http://{addr}/insights"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        for label in ["Flow", "Quality", "Autonomy", "Economics", "Reliability"] {
            assert!(body.contains(label), "missing dimension {label}: {body}");
        }
        // Period selector links, nested correctly under this router's base_path.
        assert!(body.contains("/base/insights?since=7d"), "{body}");
        assert!(body.contains("/base/insights?since=30d"), "{body}");
    }

    #[tokio::test]
    async fn metrics_endpoint_serves_valid_prometheus_text() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let app = router(rx, dir.path().to_path_buf(), "");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp = reqwest::get(format!("http://{addr}/metrics")).await.unwrap();
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/plain; version=0.0.4"
        );
        let body = resp.text().await.unwrap();
        assert!(body.contains("# HELP symphony_autonomy_successful_parallel_cycles"));
        assert!(body.contains("# TYPE symphony_autonomy_successful_parallel_cycles gauge"));
        assert!(body.contains("symphony_reliability_repeated_failures 0"));
        assert!(body.contains("symphony_flow_deployment_frequency NaN"));
    }
}
