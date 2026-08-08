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

use crate::approvals;
use crate::artifacts;
use crate::config;
use crate::eventlog;
use crate::release::{self, EvidenceBundle, Verdict};
use crate::security::SecurityFindings;
use crate::tracker::TrackerAdapter;
use crate::web;
use crate::web::{error_banner, escape, urlencode};
use axum::Router;
use axum::extract::{Form, Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// Last genuine assistant notification, never clobbered by housekeeping noise.
    pub last_notification: Option<String>,
    /// Last tool call name, independent of `last_notification`.
    pub last_tool_call: Option<String>,
    /// Current pipeline stage id (`pipeline.stages[].id`), when the AI Roadmap delivery
    /// pipeline (`pipeline.enabled`) is on for this project. `None` for the legacy
    /// single-stage path, or before the first stage has started.
    pub stage: Option<String>,
    /// Most recent budget/stop-condition note (AIR-11: `budget::check_after_turn`,
    /// `stop_conditions.*`), e.g. "cycle budget exceeded: ...". `None` until one fires.
    pub budget_note: Option<String>,
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
    /// Whether `/chat` is actually mounted alongside this router -- i.e. whether the
    /// caller is about to (or already did) `.nest("/chat", ...)` a chat UI next to it.
    /// Purely a nav-rendering signal (BUG-2): this module never mounts `/chat` itself,
    /// so without this the nav would advertise a "Chat" tab pointing at a route that
    /// doesn't exist whenever chat is off (the common case).
    chat_enabled: bool,
    /// Present iff `pipeline.security` has somewhere to send a human override
    /// (`AppState`'s owner resolved a tracker for this project) -- `None` renders
    /// `/security` read-only with an explanatory banner instead of a broken form.
    security: Option<SecurityContext>,
    /// Present only when the caller has a `repo:`/event-log writer to hand over --
    /// backs the `/observability` page's "rescan now" action (AIR-10). `None` (e.g.
    /// SweBot chat's own test router, or a project with no `repo:` configured) simply
    /// hides that control rather than erroring.
    observability: Option<ObservabilityHandle>,
    /// Backs `/budget/extend` (AIR-11): moves an issue an `escalate`/`stop` budget
    /// decision parked in `pipeline.blocked_state` back to an active tracker state, the
    /// same `TrackerAdapter` write `orchestrator::block_issue` used to park it.
    tracker: Arc<dyn TrackerAdapter>,
}

/// What `/observability`'s "rescan now" button needs to run one pre-merge scan
/// on-demand (`observability::pre_merge::poll_once`), outside that scan's own
/// background poll loop.
#[derive(Clone)]
pub struct ObservabilityHandle {
    pub repo: Option<crate::config::RepoConfig>,
    pub event_tx: tokio::sync::mpsc::UnboundedSender<eventlog::NewEvent>,
}

impl AppState {
    fn eventlog_db_path(&self) -> PathBuf {
        self.workflow_dir.join(eventlog::DB_FILENAME)
    }
}

/// What `/security`'s override action (AIR-8) needs beyond read-only eventlog access:
/// somewhere to write the resumed tracker state. General enough that AIR-5's
/// `/approvals` page (the next dashboard action that mutates tracker state from a
/// human click) can reuse the same shape rather than inventing its own.
#[derive(Clone)]
pub struct SecurityContext {
    pub tracker: Arc<dyn TrackerAdapter>,
    /// `pipeline.blocked_state` -- shown on the page so a human can see what state a
    /// blocked issue is actually parked in.
    pub blocked_state: String,
    /// Tracker state an override moves the issue back to so the dispatcher picks it
    /// up again -- `cfg.active_states.first()`, when the project has any configured.
    /// `None` disables the override action (nothing to resume into) but still shows
    /// findings.
    pub resume_state: Option<String>,
}

/// Re-reads `WORKFLOW.md` and resolves it the same way `orchestrator::build_shared`
/// does, just for the handful of read-only fields the dashboard needs
/// (`budgets`/`pricing`/`active_states`) -- deliberately not threaded through
/// `Shared`/`AppState` at startup, since that would mean plumbing a config-reload path
/// into the dashboard just to read a few numbers it can cheaply re-derive itself on
/// each render.
struct BudgetView {
    budgets: crate::config::BudgetsConfig,
    pricing: crate::budget::PricingTable,
    model_key: String,
    active_states: Vec<String>,
}

/// `None` on any error (missing file, bad YAML): the budgets panel simply doesn't
/// render rather than breaking the rest of the dashboard.
fn load_budget_view(workflow_dir: &Path) -> Option<BudgetView> {
    let wf = crate::workflow::load(&workflow_dir.join("WORKFLOW.md")).ok()?;
    let cfg = crate::config::resolve(&wf.config, workflow_dir).ok()?;
    Some(BudgetView {
        budgets: cfg.budgets.clone(),
        pricing: cfg.pricing.clone(),
        model_key: cfg.effective_model_key(),
        active_states: cfg.active_states.clone(),
    })
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
/// `security`: `Some` wires `/security`'s override action to a real tracker
/// (`SecurityContext`); `None` renders `/security` read-only (no `pipeline.security`
/// configured for this project, or the caller has no tracker handle to give it).
/// `observability: None` still mounts `/observability` (read-only history, no
/// "rescan now" button) rather than omitting the route.
pub fn router(
    status_rx: watch::Receiver<StatusSnapshot>,
    workflow_dir: PathBuf,
    tracker: Arc<dyn TrackerAdapter>,
    base_path: &str,
    chat_enabled: bool,
    security: Option<SecurityContext>,
    observability: Option<ObservabilityHandle>,
) -> Router {
    let state = AppState {
        status_rx,
        workflow_dir,
        base_path: base_path.to_string(),
        chat_enabled,
        security,
        observability,
        tracker,
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
        .route("/approvals", get(approvals_page))
        .route("/approvals/{id}/decide", post(approvals_decide))
        .route("/reviews", get(reviews_page))
        .route("/reviews/{issue_id}/unblock", post(unblock_review))
        .route("/security", get(security_page))
        .route("/security/override", post(security_override))
        .route("/evidence", get(evidence_index))
        .route("/evidence/{key}", get(evidence_page))
        .route("/evidence/{key}/fragment", get(evidence_fragment))
        .route("/evidence/{key}/override", post(evidence_override))
        .route("/observability", get(observability_page))
        .route("/observability/rescan", post(observability_rescan))
        .route("/budget/extend", post(extend_budget))
        .route("/insights", get(insights_page))
        .route("/metrics", get(metrics_endpoint))
        .with_state(state)
}

/// `security`/`observability`: see `router`'s doc comment.
#[allow(clippy::too_many_arguments)]
pub async fn serve_composite(
    port: u16,
    bind_all_interfaces: bool,
    status_rx: watch::Receiver<StatusSnapshot>,
    workflow_dir: PathBuf,
    tracker: Arc<dyn TrackerAdapter>,
    chat: Option<Router>,
    security: Option<SecurityContext>,
    observability: Option<ObservabilityHandle>,
) -> anyhow::Result<()> {
    let chat_enabled = chat.is_some();
    let mut app = router(
        status_rx,
        workflow_dir,
        tracker,
        "",
        chat_enabled,
        security,
        observability,
    );
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

fn page_shell(
    title: &str,
    active_nav: &str,
    body: &str,
    extra_head: &str,
    base: &str,
    chat_enabled: bool,
) -> String {
    crate::web::page_shell(
        "Symphony",
        title,
        &crate::web::nav(&crate::web::nav_links(chat_enabled), active_nav, base),
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
  const approvalsBanner = incoming.querySelector('#approvals-banner');
  if (runningCount) document.getElementById('running-count').textContent = runningCount.textContent;
  if (retryingCount) document.getElementById('retrying-count').textContent = retryingCount.textContent;
  if (approvalsBanner) document.getElementById('approvals-banner').innerHTML = approvalsBanner.innerHTML;
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
        fragment = render_fragment(&snapshot, &state.base_path, &state.eventlog_db_path()),
    );
    Html(page_shell(
        "live status",
        "/",
        &body,
        &script,
        &state.base_path,
        state.chat_enabled,
    ))
}

async fn fragment(State(state): State<AppState>) -> Html<String> {
    let snapshot = state.status_rx.borrow().clone();
    Html(render_fragment(
        &snapshot,
        &state.base_path,
        &state.eventlog_db_path(),
    ))
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
    let db_path = state.eventlog_db_path();
    let stream = WatchStream::new(state.status_rx.clone()).map(move |snapshot| {
        Ok(Event::default().data(render_fragment(&snapshot, &base, &db_path)))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn render_fragment(s: &StatusSnapshot, base: &str, eventlog_db_path: &Path) -> String {
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

    // AIR-5: a fresh read every render (this function is called both by the plain
    // `/fragment` GET and by every `/fragment-stream` push), same "no cache, just
    // re-query" posture `/events`/`/usage` already take -- pending approvals live in
    // SQLite, not `StatusSnapshot`, so this is how a human sees "N awaiting approval"
    // update live without a page reload rather than only on the next full `/approvals`
    // visit.
    let pending_approvals = crate::approvals::list_pending(eventlog_db_path).unwrap_or_default();
    let approvals_banner_inner = if pending_approvals.is_empty() {
        String::new()
    } else {
        format!(
            r#"<section>
<p class="meta"><a href="{base}/approvals"><span class="badge">{count}</span> awaiting approval &rarr;</a></p>
</section>"#,
            count = pending_approvals.len(),
        )
    };
    let approvals_banner = format!(r#"<div id="approvals-banner">{approvals_banner_inner}</div>"#);

    format!(
        r#"{approvals_banner}<section>
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
</section>

<section>
<h3>Budget-blocked cycle</h3>
<p class="empty">A cycle an <code>escalate</code>/<code>stop</code> budget decision parked in <code>pipeline.blocked_state</code> (see its issue id on the card above, or on <a href="{base}/events">/events</a>) can be resumed here.</p>
<form class="admin" method="post" action="{base}/budget/extend">
  <label for="extend-issue-id">Issue id</label>
  <input type="text" id="extend-issue-id" name="issue_id" required>
  <br><button type="submit" class="btn">Extend &amp; resume</button>
</form>
</section>"#,
        approvals_banner = approvals_banner,
        base = base,
        running = s.running.len(),
        retrying = s.retrying.len(),
        running_cards = running_cards,
        retry_rows = retry_rows,
    )
}

fn running_card(r: &RunningRow, base: &str) -> String {
    let notification = r.last_notification.as_deref().unwrap_or("");
    let expandable_attr = if notification.len() > 140 {
        " data-expandable"
    } else {
        ""
    };
    let stage_row = r
        .stage
        .as_deref()
        .map(|s| format!(r#"<div class="row"><b>stage</b> {}</div>"#, escape(s)))
        .unwrap_or_default();
    let budget_row = r
        .budget_note
        .as_deref()
        .map(|note| {
            format!(
                r#"<div class="row budget-note"><b>budget</b> {}</div>"#,
                escape(note)
            )
        })
        .unwrap_or_default();
    let tool_call_row = r
        .last_tool_call
        .as_deref()
        .map(|t| format!(r#"<div class="row"><b>last tool</b> {}</div>"#, escape(t)))
        .unwrap_or_default();
    format!(
        r#"<div class="card" id="run-{id_attr}">
  <h2><a href="{base}/events?issue={issue_link}">{identifier}</a></h2>
  <div class="row">{title}</div>
  <div class="row"><b>session</b> {session}</div>
  <div class="row"><b>running for</b> {elapsed:.1}s &middot; <b>turn</b> {turn} &middot; <b>tool calls</b> {tools}</div>
  {stage_row}
  {budget_row}
  <div class="row"><b>last event</b> {event}</div>
  {tool_call_row}
  <div class="msg"{expandable_attr}>{notification}</div>
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
        budget_row = budget_row,
        event = escape(r.last_event.as_deref().unwrap_or("-")),
        tool_call_row = tool_call_row,
        notification = escape(notification),
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
    Html(page_shell(
        "events",
        "/events",
        &body,
        "",
        base,
        state.chat_enabled,
    ))
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
///
/// Two rendering features compose here:
///   - FEAT-3: events are bucketed by pipeline stage (walking `stage_started`/
///     `stage_finished` markers). When no stage boundaries exist, rendering falls
///     back to the same flat list as before -- purely additive for pipeline-enabled
///     projects.
///   - FEAT-2: within each stage's bucket, consecutive non-assistant events that
///     share the same whole-second timestamp are collapsed into one compact line
///     (e.g. `tool_call: Grep, Read · turn_started, session_started`) instead of
///     one full bubble each. Assistant narrative bubbles are never collapsed.
fn render_transcript(rows: &[eventlog::EventRow], base: &str) -> String {
    if rows.is_empty() {
        return r#"<p class="empty">No events recorded yet.</p>"#.to_string();
    }
    let stages = bucket_stages(rows);

    if stages.len() == 1 && stages[0].stage_id.is_none() {
        let mut events = stages[0].events.clone();
        events.reverse();
        return collapse_same_second(&events, base);
    }

    let mut html = String::new();
    for stage in stages.iter().rev() {
        let mut events = stage.events.clone();
        events.reverse();
        if let Some(id) = &stage.stage_id {
            let heading = match &stage.outcome {
                Some(outcome) => {
                    format!("{} &mdash; {}", escape(id), escape(outcome))
                }
                None => escape(id).to_string(),
            };
            html.push_str(&format!(
                r#"<section><h3>{heading}</h3>{body}</section>"#,
                body = collapse_same_second(&events, base),
            ));
        } else {
            html.push_str(&collapse_same_second(&events, base));
        }
    }
    html
}

struct StageBucket<'a> {
    stage_id: Option<String>,
    outcome: Option<String>,
    events: Vec<&'a eventlog::EventRow>,
}

fn bucket_stages(rows: &[eventlog::EventRow]) -> Vec<StageBucket<'_>> {
    let mut buckets: Vec<StageBucket<'_>> = Vec::new();
    let mut current_stage_id: Option<String> = None;
    let mut current_outcome: Option<String> = None;
    let mut current_events: Vec<&eventlog::EventRow> = Vec::new();

    for row in rows.iter().rev() {
        if row.event_type == "stage_started" {
            let prev_events = std::mem::take(&mut current_events);
            if !prev_events.is_empty() || current_stage_id.is_some() {
                buckets.push(StageBucket {
                    stage_id: current_stage_id.take(),
                    outcome: current_outcome.take(),
                    events: prev_events,
                });
            }
            current_stage_id = row.message.clone();
            current_outcome = None;
            continue;
        }
        if row.event_type == "stage_finished" {
            if let Some(msg) = &row.message
                && let Some((_id, outcome)) = msg.split_once(": ")
            {
                current_outcome = Some(outcome.to_string());
            }
            continue;
        }
        current_events.push(row);
    }
    let final_events = std::mem::take(&mut current_events);
    buckets.push(StageBucket {
        stage_id: current_stage_id.take(),
        outcome: current_outcome.take(),
        events: final_events,
    });
    buckets
}

fn timestamp_second(ts: &str) -> &str {
    if ts.len() >= 19 { &ts[..19] } else { ts }
}

fn collapse_same_second(events: &[&eventlog::EventRow], base: &str) -> String {
    if events.is_empty() {
        return String::new();
    }
    let mut output = String::new();
    let mut i = 0;
    while i < events.len() {
        let role = transcript_role(&events[i].event_type);
        if role == "assistant" {
            output.push_str(&transcript_bubble(events[i], base));
            i += 1;
        } else {
            let second = timestamp_second(&events[i].created_at);
            let mut group = vec![events[i]];
            i += 1;
            while i < events.len() {
                let next_role = transcript_role(&events[i].event_type);
                if next_role == "assistant" {
                    break;
                }
                if timestamp_second(&events[i].created_at) == second {
                    group.push(events[i]);
                    i += 1;
                } else {
                    break;
                }
            }
            if group.len() == 1 {
                output.push_str(&transcript_bubble(group[0], base));
            } else {
                output.push_str(&collapsed_bubble(&group, base));
            }
        }
    }
    output
}

fn collapsed_bubble(events: &[&eventlog::EventRow], base: &str) -> String {
    let time = escape(timestamp_second(&events[0].created_at));
    let parts: Vec<String> = events
        .iter()
        .map(|r| {
            let issue_link = urlencode(&r.issue_id);
            let type_link = urlencode(&r.event_type);
            if transcript_role(&r.event_type) == "tool" {
                let tool_name = r.message.as_deref().unwrap_or(&r.event_type);
                format!(
                    r#"<a href="{base}/events?issue={issue_link}&amp;type={type_link}">{type}: {name}</a>"#,
                    type = escape(&r.event_type),
                    name = escape(tool_name),
                )
            } else {
                format!(
                    r#"<a href="{base}/events?issue={issue_link}&amp;type={type_link}">{name}</a>"#,
                    name = escape(&r.event_type),
                )
            }
        })
        .collect();
    format!(
        r#"<div class="msg compact">
  <div class="bubble">{}</div>
  <div class="status"><span class="time">{}</span></div>
</div>"#,
        parts.join(" &middot; "),
        time,
    )
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
    let db_path = state.eventlog_db_path();
    let summary = eventlog::usage_summary(&db_path).unwrap_or_default();
    let by_issue = eventlog::usage_by_issue(&db_path).unwrap_or_default();
    // Per-issue test/coverage evidence (AIR-6): the latest `test_summary`/
    // `coverage_summary` event per issue, not just "last event overall" -- a test
    // stage's summary shouldn't disappear from this column just because a later stage
    // produced more recent events.
    let test_summaries =
        eventlog::latest_message_by_type(&db_path, "test_summary").unwrap_or_default();
    let coverage_summaries =
        eventlog::latest_message_by_type(&db_path, "coverage_summary").unwrap_or_default();
    // FEAT-1: an open dispatch span (no `worker_exit` yet) reads very differently
    // depending on whether the issue is actually running right now -- a live worker vs.
    // a process that got killed mid-attempt and never recorded one. `StatusSnapshot` is
    // the one place that distinction already lives.
    let running_issue_ids: std::collections::HashSet<String> = state
        .status_rx
        .borrow()
        .running
        .iter()
        .map(|r| r.issue_id.clone())
        .collect();

    let base = state.base_path.as_str();
    let issue_rows: String = if by_issue.is_empty() {
        "<tr><td colspan=\"11\" class=\"empty\">No usage recorded yet.</td></tr>".to_string()
    } else {
        by_issue
            .iter()
            .map(|r| {
                issue_usage_row(
                    r,
                    base,
                    test_summaries.get(&r.issue_id).map(String::as_str),
                    coverage_summaries.get(&r.issue_id).map(String::as_str),
                    running_issue_ids.contains(r.issue_id.as_str()),
                )
            })
            .collect()
    };

    let budget_panel = render_budget_panel(&state.workflow_dir, &state.eventlog_db_path());

    let body = format!(
        r#"{budget_panel}<div class="stats">
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
<thead><tr><th data-sort>Issue</th><th data-sort>Dispatches</th><th data-sort>Turns</th><th data-sort>Tool calls</th><th data-sort>Input</th><th data-sort>Output</th><th data-sort>Total</th><th data-sort>Duration</th><th>Last event</th><th>Tests</th><th>Coverage</th></tr></thead>
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
    Html(page_shell(
        "usage",
        "/usage",
        &body,
        "",
        base,
        state.chat_enabled,
    ))
}

fn issue_picker(base: &str, active_path: &str, recent: &[eventlog::IssueUsageRow]) -> String {
    let mut body = r#"<form class="filters" method="get">
  <label for="f-issue">Issue</label>
  <input type="text" id="f-issue" name="issue" placeholder="issue id">
  <button type="submit">Load</button>
</form>

<h3>Recent issues</h3>
"#
    .to_string();
    if recent.is_empty() {
        body.push_str(r#"<p class="empty">No issues with recorded events yet.</p>"#);
    } else {
        body.push_str(
            r#"<div class="table-wrap"><table>
<thead><tr><th>Identifier</th><th>Title</th></tr></thead>
<tbody>"#,
        );
        for r in recent.iter().take(10) {
            body.push_str(&format!(
                "<tr><td><a href=\"{base}{active_path}?issue={issue_link}\">{identifier}</a></td><td>{title}</td></tr>",
                issue_link = urlencode(&r.issue_id),
                identifier = escape(&r.identifier),
                title = escape(&r.title),
            ));
        }
        body.push_str("</tbody></table></div>");
    }
    body
}

fn issue_usage_row(
    r: &eventlog::IssueUsageRow,
    base: &str,
    test_summary: Option<&str>,
    coverage_summary: Option<&str>,
    currently_running: bool,
) -> String {
    let issue_link = urlencode(&r.issue_id);
    // Each cell links through to `/events` filtered to the full `test_report`/
    // `coverage` JSON for that issue -- the explanation surface: a human can see not
    // just the pass/fail count but the per-suite/per-AC evidence behind it.
    let tests_cell = match test_summary {
        Some(s) => format!(
            "<a href=\"{base}/events?issue={issue_link}&amp;type=test_report\">{}</a>",
            escape(s)
        ),
        None => "<span class=\"empty\">not run</span>".to_string(),
    };
    let coverage_cell = match coverage_summary {
        Some(s) => format!(
            "<a href=\"{base}/events?issue={issue_link}&amp;type=coverage\">{}</a>",
            escape(s)
        ),
        None => "<span class=\"empty\">not run</span>".to_string(),
    };
    let duration_cell = format_issue_duration(r.duration_secs, r.duration_open, currently_running);
    format!(
        "<tr><td><a href=\"{base}/events?issue={issue_link}\">{identifier}</a> &mdash; {title}</td><td>{dispatches}</td><td>{turns}</td><td>{tools}</td><td>{input}</td><td>{output}</td><td>{total}</td><td>{duration_cell}</td><td>{last_event} <span class=\"empty\">{last_at}</span></td><td>{tests_cell}</td><td>{coverage_cell}</td></tr>",
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

/// FEAT-1: render a per-issue Duration cell. A closed total (every dispatch attempt
/// accounted for by a matching `worker_exit`) renders plain. An open span -- the last
/// `dispatched` has no `worker_exit` yet -- renders plain too *while the issue is
/// actually running right now* (it's expected to keep advancing); otherwise it's a
/// stale/uncertain span from a process that was killed rather than stopped cleanly, so
/// it's prefixed with `~` and gets a `title` tooltip explaining why, rather than
/// silently reading as an exact number.
fn format_issue_duration(
    duration_secs: f64,
    duration_open: bool,
    currently_running: bool,
) -> String {
    let text = crate::metrics::format_duration(duration_secs.round() as u64);
    if !duration_open || currently_running {
        escape(&text)
    } else {
        format!(
            "<span title=\"open dispatch span with no worker_exit event; the process likely stopped without exiting cleanly, so this may be an undercount or stale\">~{}</span>",
            escape(&text)
        )
    }
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
            return Html(page_shell(
                "insights",
                "/insights",
                &body,
                "",
                base,
                state.chat_enabled,
            ));
        }
    };

    let periods = [
        ("all", "All time"),
        ("7d", "7d"),
        ("30d", "30d"),
        ("90d", "90d"),
    ];
    let selector: String = periods
        .iter()
        .map(|(value, label)| {
            let class = if *value == period {
                " class=\"active\""
            } else {
                ""
            };
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
    Html(page_shell(
        "insights",
        "/insights",
        &body,
        "",
        base,
        state.chat_enabled,
    ))
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

/// One consumption bar per configured scope (`budgets.application`/`budgets.platform`)
/// -- the "known token consumption and AI cost" the roadmap's acceptance criteria ask
/// for, made visible without reading logs or SQLite. Empty string when no `budgets:`
/// block is configured at all, so a project that never opted in sees nothing extra on
/// `/usage`.
fn render_budget_panel(workflow_dir: &Path, db_path: &Path) -> String {
    let Some(view) = load_budget_view(workflow_dir) else {
        return String::new();
    };
    if view.budgets.application.is_none() && view.budgets.platform.is_none() {
        return String::new();
    }
    let since = crate::budget::window_start(view.budgets.window, chrono::Utc::now()).to_rfc3339();
    let usage = eventlog::usage_summary_since(db_path, &since).unwrap_or_default();
    let token_usage = crate::agent::TokenUsage {
        input_tokens: usage.input_tokens.max(0) as u64,
        output_tokens: usage.output_tokens.max(0) as u64,
        total_tokens: usage.total_tokens.max(0) as u64,
    };
    let cost = crate::budget::compute_cost(&token_usage, &view.model_key, &view.pricing);

    let mut bars = String::new();
    if let Some(limits) = &view.budgets.application {
        bars.push_str(&budget_bar(
            "Application",
            &token_usage,
            cost,
            limits,
            &view.budgets.currency,
        ));
    }
    if let Some(limits) = &view.budgets.platform {
        bars.push_str(&budget_bar(
            "Platform (this project's own share)",
            &token_usage,
            cost,
            limits,
            &view.budgets.currency,
        ));
    }
    format!(
        r#"<section>
<h3>Budgets <span class="empty">rolling {window}, since {since}</span></h3>
{bars}
</section>"#,
        window = match view.budgets.window {
            config::BudgetWindow::Daily => "daily",
            config::BudgetWindow::Weekly => "weekly",
            config::BudgetWindow::Monthly => "monthly",
        },
        since = escape(&since),
        bars = bars,
    )
}

fn budget_bar(
    label: &str,
    usage: &crate::agent::TokenUsage,
    cost: crate::budget::Cost,
    limits: &config::BudgetLimits,
    currency: &str,
) -> String {
    let token_pct = limits
        .tokens
        .filter(|l| *l > 0)
        .map(|l| ((usage.total_tokens as f64 / l as f64) * 100.0).min(100.0))
        .unwrap_or(0.0);
    let tokens_line = match limits.tokens {
        Some(l) => format!("{} / {} tokens", usage.total_tokens, l),
        None => format!("{} tokens (no token limit set)", usage.total_tokens),
    };
    let cost_line = match (limits.cost, cost) {
        (Some(l), crate::budget::Cost::Known(spent)) => {
            format!("{spent:.2} / {l:.2} {currency}")
        }
        (Some(l), crate::budget::Cost::Unknown) => {
            format!("cost unknown (unpriced model) / {l:.2} {currency} budget")
        }
        (None, crate::budget::Cost::Known(spent)) => {
            format!("{spent:.2} {currency} (no cost limit set)")
        }
        (None, crate::budget::Cost::Unknown) => "cost unknown (unpriced model)".to_string(),
    };
    format!(
        r#"<div class="row"><b>{label}</b></div>
<div class="meter"><div class="meter-fill" style="width:{pct:.1}%"></div></div>
<div class="row">{tokens_line} &middot; {cost_line}</div>"#,
        label = escape(label),
        pct = token_pct,
        tokens_line = escape(&tokens_line),
        cost_line = escape(&cost_line),
    )
}

#[derive(Deserialize)]
struct ExtendBudgetForm {
    issue_id: String,
}

/// Human action for a cycle an `escalate`/`stop` budget decision parked in
/// `pipeline.blocked_state` (AIR-11): moves it back to the project's first configured
/// `tracker.active_states` entry so the normal dispatch loop picks it up again. Gated
/// by `SYMPHONY_ADMIN_TOKEN` when one is set (always true under `symphony serve`, see
/// `service.rs`); the single-project CLI dashboard has no admin-token concept at all
/// today (same "not hardened for exposure" posture the rest of this module already
/// documents), so it stays open there rather than inventing a second auth story.
async fn extend_budget(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ExtendBudgetForm>,
) -> Response {
    if !crate::web::admin_token_ok(&headers) {
        return Redirect::to(&format!("{}/", state.base_path)).into_response();
    }
    let Some(view) = load_budget_view(&state.workflow_dir) else {
        return Redirect::to(&format!("{}/", state.base_path)).into_response();
    };
    let Some(active_state) = view.active_states.first().cloned() else {
        return Redirect::to(&format!("{}/", state.base_path)).into_response();
    };
    match state
        .tracker
        .set_issue_state(&form.issue_id, &active_state)
        .await
    {
        Ok(()) => {
            let db_path = state.eventlog_db_path();
            let issue_id = form.issue_id.clone();
            // Best-effort (`eventlog::record_now`'s own doc comment) -- a lost history
            // row here isn't worth failing the actual unblock over.
            let _ = tokio::task::spawn_blocking(move || {
                let _ = eventlog::record_now(
                    &db_path,
                    &eventlog::NewEvent {
                        issue_id: issue_id.clone(),
                        identifier: issue_id,
                        title: String::new(),
                        session_id: None,
                        event_type: "budget_extended".to_string(),
                        message: Some(format!("resumed into '{active_state}' by an operator")),
                        input_tokens: None,
                        output_tokens: None,
                        total_tokens: None,
                    },
                );
            })
            .await;
        }
        Err(e) => {
            tracing::warn!(issue_id = %form.issue_id, error = %e, "/budget/extend: failed to resume issue");
        }
    }
    Redirect::to(&format!("{}/", state.base_path)).into_response()
}

// --------------------------------------------------------------------------------
// /observability -- pre-merge telemetry evidence + post-deploy production
// validation (AIR-10), from eventlog::recent_events_of_types
// --------------------------------------------------------------------------------

async fn observability_page(State(state): State<AppState>) -> Html<String> {
    let rows = eventlog::recent_events_of_types(
        &state.eventlog_db_path(),
        &["telemetry_evidence", "production_validation"],
        100,
    )
    .unwrap_or_default();
    let base = state.base_path.as_str();

    let can_rescan = state
        .observability
        .as_ref()
        .is_some_and(|o| o.repo.is_some());
    let rescan_form = if can_rescan {
        format!(
            r#"<form method="post" action="{base}/observability/rescan">
<button type="submit" class="btn">Rescan open PRs now</button></form>"#
        )
    } else {
        String::new()
    };

    let listing: String = if rows.is_empty() {
        "<p class=\"empty\">No observability evidence recorded yet.</p>".to_string()
    } else {
        rows.iter().map(observability_row).collect()
    };

    let body = format!(
        r#"<div class="meta">Pre-merge telemetry evidence and post-deploy production
validation verdicts (AIR-10). "unknown" means the backend was unreachable or
returned no data -- never treated as healthy.</div>
{rescan_form}
<div class="grid">
{listing}
</div>"#
    );
    Html(page_shell(
        "observability",
        "/observability",
        &body,
        "",
        base,
        state.chat_enabled,
    ))
}

/// One card per event: `production_validation` shows the verdict badge, each check's
/// pass/fail against its threshold, and the recorded reason (the "why" constraint 2
/// asks for); `telemetry_evidence` shows every detected signal and any secret/PII
/// findings.
fn observability_row(r: &eventlog::EventRow) -> String {
    let value: serde_json::Value = r
        .message
        .as_deref()
        .and_then(|m| serde_json::from_str(m).ok())
        .unwrap_or(serde_json::Value::Null);

    match r.event_type.as_str() {
        "production_validation" => {
            let verdict = value
                .get("verdict")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let reason = value.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            let checks_html: String = value
                .get("checks")
                .and_then(|v| v.as_array())
                .map(|checks| {
                    checks
                        .iter()
                        .map(|c| {
                            let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            let val = c
                                .get("value")
                                .and_then(|v| v.as_f64())
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "no data".to_string());
                            let max = c.get("max").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            let passed = c.get("passed").and_then(|v| v.as_bool()).unwrap_or(false);
                            format!(
                                r#"<li class="{cls}">{name}: {val} (max {max}) &mdash; {status}</li>"#,
                                cls = if passed { "ok" } else { "bad" },
                                name = escape(name),
                                val = escape(&val),
                                max = max,
                                status = if passed { "pass" } else { "fail" },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            format!(
                r#"<div class="card">
  <h2><span class="badge verdict-{verdict_class}">{verdict}</span> {identifier}</h2>
  <div class="row">{reason}</div>
  <ul>{checks_html}</ul>
  <div class="row"><b>when</b> {created}</div>
</div>"#,
                verdict_class = escape(verdict),
                verdict = escape(verdict),
                identifier = escape(&r.identifier),
                reason = escape(reason),
                checks_html = checks_html,
                created = escape(&r.created_at),
            )
        }
        "telemetry_evidence" => {
            let signals_html: String = value
                .get("signals")
                .and_then(|v| v.as_array())
                .map(|signals| {
                    signals
                        .iter()
                        .map(|s| {
                            let kind = s.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
                            let location =
                                s.get("location").and_then(|v| v.as_str()).unwrap_or("?");
                            let requirement = s
                                .get("requirement_id")
                                .and_then(|v| v.as_str())
                                .map(|r| format!(" (satisfies {})", escape(r)))
                                .unwrap_or_default();
                            format!(
                                "<li>{kind} at {location}{requirement}</li>",
                                kind = escape(kind),
                                location = escape(location),
                                requirement = requirement,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let findings_html: String = value
                .get("findings")
                .and_then(|v| v.as_array())
                .filter(|f| !f.is_empty())
                .map(|findings| {
                    let items: String = findings
                        .iter()
                        .map(|f| {
                            let location =
                                f.get("location").and_then(|v| v.as_str()).unwrap_or("?");
                            let description =
                                f.get("description").and_then(|v| v.as_str()).unwrap_or("?");
                            format!(
                                r#"<li class="bad">{location}: {description}</li>"#,
                                location = escape(location),
                                description = escape(description),
                            )
                        })
                        .collect();
                    format!(r#"<div class="row"><b>Findings</b><ul>{items}</ul></div>"#)
                })
                .unwrap_or_default();
            format!(
                r#"<div class="card">
  <h2>{identifier}</h2>
  <div class="row"><b>Signals</b><ul>{signals_html}</ul></div>
  {findings_html}
  <div class="row"><b>when</b> {created}</div>
</div>"#,
                identifier = escape(&r.identifier),
                signals_html = signals_html,
                findings_html = findings_html,
                created = escape(&r.created_at),
            )
        }
        _ => String::new(),
    }
}

/// The dashboard's only observability mutation: run one pre-merge scan immediately
/// instead of waiting for the background poller's next tick. Fire-and-forget
/// (spawned, not awaited) since a real scan clones a repo and can take longer than a
/// request should block on -- the redirect lands before it finishes, and the new
/// evidence shows up on the page's next load (or its own next `/observability` visit)
/// once the spawned scan completes.
async fn observability_rescan(State(state): State<AppState>) -> Response {
    let base = state.base_path.clone();
    if let Some(handle) = state.observability.clone()
        && let Some(repo) = handle.repo.clone()
    {
        tokio::spawn(async move {
            if let Ok(host) = crate::repo_host::build(&repo) {
                let mut seen = std::collections::HashSet::new();
                if let Err(e) = crate::observability::pre_merge::poll_once(
                    &repo,
                    host.as_ref(),
                    &handle.event_tx,
                    &mut seen,
                )
                .await
                {
                    tracing::warn!(error = %e, "observability: on-demand rescan failed");
                }
            }
        });
    }
    Redirect::to(&format!("{base}/observability")).into_response()
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
    Html(page_shell(
        "artifacts",
        "/artifacts",
        &body,
        "",
        base,
        state.chat_enabled,
    ))
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

#[derive(Deserialize, Default)]
struct ArtifactRawQuery {
    raw: Option<String>,
}

async fn artifact_raw_page(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<ArtifactRawQuery>,
) -> Result<Html<String>, StatusCode> {
    let db_path = state.eventlog_db_path();
    let row = artifacts::find_by_id(&db_path, &id).ok_or(StatusCode::NOT_FOUND)?;
    let base = state.base_path.as_str();
    let raw_bytes =
        artifacts::read_content(&state.workflow_dir, &row).map_err(|_| StatusCode::NOT_FOUND)?;
    let content = String::from_utf8_lossy(&raw_bytes).into_owned();
    let raw = q.raw.as_deref() == Some("1") || q.raw.as_deref() == Some("true");

    let id_link = urlencode(&row.id);
    let toggle_link = if raw {
        format!("<a href=\"{base}/artifacts/{id_link}\">View rendered</a>")
    } else {
        format!("<a href=\"{base}/artifacts/{id_link}?raw=1\">View raw</a>")
    };

    let rendered = if raw {
        format!("<pre class=\"table-wrap\">{}</pre>", escape(&content))
    } else if row.content_type.contains("json") {
        match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(v) => {
                let pretty = serde_json::to_string_pretty(&v).unwrap_or_else(|_| content.clone());
                format!("<pre class=\"table-wrap\">{}</pre>", escape(&pretty))
            }
            Err(_) => {
                format!("<pre class=\"table-wrap\">{}</pre>", escape(&content))
            }
        }
    } else if row.content_type.contains("markdown") {
        let html = render_markdown_html(&content);
        format!("<div class=\"post-body\">{html}</div>")
    } else {
        format!("<pre class=\"table-wrap\">{}</pre>", escape(&content))
    };

    let body = format!(
        r#"<div class="meta">
  <div class="row"><b>id</b> {id}</div>
  <div class="row"><b>issue</b> <a href="{base}/artifacts?cycle={cycle_link}">{issue}</a></div>
  <div class="row"><b>stage</b> {stage}</div>
  <div class="row"><b>kind</b> {kind}</div>
  <div class="row"><b>content type</b> {content_type} &middot; {toggle_link}</div>
  <div class="row"><b>sha256</b> {sha}</div>
  <div class="row"><b>recorded</b> {created}</div>
  <div class="row"><b>summary</b> {summary}</div>
</div>
{rendered}"#,
        id = escape(&row.id),
        base = base,
        cycle_link = urlencode(&row.cycle_id),
        issue = escape(&row.issue_identifier),
        stage = escape(row.stage_id.as_deref().unwrap_or("-")),
        kind = escape(&row.kind),
        content_type = escape(&row.content_type),
        toggle_link = toggle_link,
        sha = escape(&row.sha256),
        created = escape(&row.created_at),
        summary = escape(&row.summary),
        rendered = rendered,
    );
    Ok(Html(page_shell(
        &format!("artifact {}", row.id),
        "/artifacts",
        &body,
        "",
        base,
        state.chat_enabled,
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
        let db = state.eventlog_db_path();
        let recent = eventlog::usage_by_issue(&db).unwrap_or_default();
        let body = issue_picker(base, "/requirements", &recent);
        return Html(page_shell(
            "requirements",
            "/requirements",
            &body,
            "",
            base,
            state.chat_enabled,
        ));
    };

    let db = state.eventlog_db_path();
    let requirements_artifact =
        eventlog::get_artifact(&db, &issue_id, "requirements").unwrap_or_default();
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
    let clarifications =
        eventlog::recent_events(&db, &clarification_filter, 100, 0).unwrap_or_default();
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
        acceptance_criteria
            .iter()
            .map(acceptance_criterion_row)
            .collect()
    };
    let clarification_items: String = if clarifications.is_empty() {
        r#"<p class="empty">No clarifications raised.</p>"#.to_string()
    } else {
        clarifications.iter().map(clarification_item).collect()
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
    Html(page_shell(
        "requirements",
        "/requirements",
        &body,
        "",
        base,
        state.chat_enabled,
    ))
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
    let blocking = parsed
        .get("blocking")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
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

// --------------------------------------------------------------------------------
// /reviews -- AIR-7's Reviewer-stage state+history+explain surface: the
// `review_findings` artifacts a cycle's Reviewer stage has recorded (read from the
// same `crate::artifacts` store `/artifacts` browses, per-cycle history, not just the
// latest -- a human reviewing a rework loop needs to see how the verdict changed round
// over round) plus the `rework_round` events `eventlog::rework_rounds_for_issue`
// returns. The one action this panel implies -- resuming a cycle the rework loop
// escalated past `pipeline.review.max_rework_rounds` -- is `unblock_review` above: a
// real POST, not a link, same pattern `/requirements`'s clarification panel already
// uses for the same kind of park.
// --------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct ReviewsQuery {
    issue: Option<String>,
}

async fn reviews_page(
    State(state): State<AppState>,
    Query(q): Query<ReviewsQuery>,
) -> Html<String> {
    let base = state.base_path.as_str();
    let Some(issue_id) = q.issue.filter(|s| !s.is_empty()) else {
        let db = state.eventlog_db_path();
        let recent = eventlog::usage_by_issue(&db).unwrap_or_default();
        let body = issue_picker(base, "/reviews", &recent);
        return Html(page_shell(
            "reviews",
            "/reviews",
            &body,
            "",
            base,
            state.chat_enabled,
        ));
    };

    let db = state.eventlog_db_path();
    let findings_rows: Vec<artifacts::ArtifactRow> = artifacts::list_for_cycle(&db, &issue_id)
        .into_iter()
        .filter(|r| r.kind == "review_findings")
        .collect();
    // Most-recent-first from the store; oldest-first here so "round N" numbering
    // (the rework loop's own counter, `eventlog::record_rework_round`) reads
    // top-to-bottom in the order it actually happened.
    let mut rounds = eventlog::rework_rounds_for_issue(&db, &issue_id).unwrap_or_default();
    rounds.reverse();
    let escalated = rounds.iter().any(|r| {
        r.message
            .as_deref()
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .and_then(|v| v.get("escalated").and_then(|b| b.as_bool()))
            == Some(true)
    });

    let review_cards: String = if findings_rows.is_empty() {
        r#"<p class="empty">No review findings recorded yet.</p>"#.to_string()
    } else {
        findings_rows
            .iter()
            .rev()
            .map(|r| review_card(&state.workflow_dir, r))
            .collect()
    };

    let round_rows: String = if rounds.is_empty() {
        r#"<tr><td colspan="4" class="empty">No rework rounds recorded.</td></tr>"#.to_string()
    } else {
        rounds
            .iter()
            .enumerate()
            .map(|(i, r)| rework_round_row(i + 1, r))
            .collect()
    };

    let unblock_form = if escalated {
        format!(
            r#"<form method="post" action="{base}/reviews/{issue_link}/unblock">
  <button type="submit">Resume cycle</button>
  <span class="empty">This cycle was escalated after exceeding pipeline.review.max_rework_rounds. Make sure a human has actually addressed the findings above before resuming.</span>
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

<section>
<h3>Review findings <span class="badge">{count}</span></h3>
{review_cards}
</section>

<section>
<h3>Rework rounds <span class="badge">{round_count}</span></h3>
<div class="table-wrap">
<table>
<thead><tr><th>Round</th><th>Stage</th><th>Recommendation</th><th>Escalated</th></tr></thead>
<tbody>{round_rows}</tbody>
</table>
</div>
{unblock_form}
</section>"#,
        issue = escape(&issue_id),
        issue_link = urlencode(&issue_id),
        count = findings_rows.len(),
        review_cards = review_cards,
        round_count = rounds.len(),
        round_rows = round_rows,
        unblock_form = unblock_form,
    );
    Html(page_shell(
        "reviews",
        "/reviews",
        &body,
        "",
        base,
        state.chat_enabled,
    ))
}

/// One recorded `review_findings` artifact, rendered as the explanation surface a
/// human needs: the verdict, the unmet acceptance criteria and over-implementation
/// call-outs, and the finding-by-finding table -- not just "the cycle stopped."
fn review_card(workflow_dir: &std::path::Path, row: &artifacts::ArtifactRow) -> String {
    let value: serde_json::Value = artifacts::read_content(workflow_dir, row)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let recommendation = value
        .get("recommendation")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");
    let rec_color = match recommendation {
        "approve" => "#2e7d32",
        "request_changes" => "#c0392b",
        _ => "#7f8c8d",
    };
    let findings = value
        .get("findings")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let finding_rows: String = if findings.is_empty() {
        r#"<tr><td colspan="6" class="empty">No findings.</td></tr>"#.to_string()
    } else {
        findings.iter().map(finding_row).collect()
    };
    let unmet = value
        .get("unmet_acceptance_criteria")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(escape)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".to_string());
    let over = value
        .get("over_implementation")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(escape)
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".to_string());
    format!(
        r#"<div class="card">
  <div class="row"><span class="badge" style="background:{rec_color}">{recommendation}</span> &middot; stage {stage} &middot; {at}</div>
  <div class="row"><b>unmet acceptance criteria</b> {unmet}</div>
  <div class="row"><b>over-implementation</b> {over}</div>
  <div class="table-wrap">
  <table>
  <thead><tr><th>ID</th><th>Severity</th><th>Category</th><th>File</th><th>Requirement</th><th>Summary</th></tr></thead>
  <tbody>{finding_rows}</tbody>
  </table>
  </div>
</div>"#,
        rec_color = rec_color,
        recommendation = escape(recommendation),
        stage = escape(row.stage_id.as_deref().unwrap_or("-")),
        at = escape(&row.created_at),
        unmet = unmet,
        over = over,
        finding_rows = finding_rows,
    )
}

fn finding_row(f: &serde_json::Value) -> String {
    let get = |k: &str| f.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let line = f.get("line").and_then(|v| v.as_i64());
    let file = match line {
        Some(n) => format!("{}:{n}", get("file")),
        None => get("file").to_string(),
    };
    format!(
        "<tr><td>{id}</td><td>{severity}</td><td>{category}</td><td>{file}</td><td>{requirement}</td><td>{summary}</td></tr>",
        id = escape(get("id")),
        severity = escape(get("severity")),
        category = escape(get("category")),
        file = escape(&file),
        requirement = escape(get("requirement_id")),
        summary = escape(get("summary")),
    )
}

/// One `rework_round` event (`eventlog::record_rework_round`'s JSON `message`),
/// `round` numbered in the order it happened (see `reviews_page`'s `rounds.reverse()`).
fn rework_round_row(round: usize, r: &eventlog::EventRow) -> String {
    let parsed = r
        .message
        .as_deref()
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .unwrap_or_default();
    let stage = parsed.get("stage").and_then(|v| v.as_str()).unwrap_or("-");
    let recommendation = parsed
        .get("recommendation")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let escalated = parsed
        .get("escalated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    format!(
        "<tr><td>{round}</td><td>{stage}</td><td>{recommendation}</td><td>{escalated}</td></tr>",
        round = round,
        stage = escape(stage),
        recommendation = escape(recommendation),
        escalated = if escalated {
            r#"<span class="badge" style="background:#c0392b">yes</span>"#
        } else {
            "no"
        },
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
    match resume_cycle(&state, &issue_id, &headers).await {
        Ok(()) => Redirect::to(&format!(
            "{}/requirements?issue={}",
            state.base_path,
            urlencode(&issue_id)
        ))
        .into_response(),
        Err(resp) => resp,
    }
}

/// AIR-7: resumes a cycle the Reviewer stage's rework loop escalated (exceeded
/// `pipeline.review.max_rework_rounds`) -- same mechanism `unblock_clarification`
/// (AIR-4) uses to lift a blocking-clarification park, since both are just "move the
/// issue back to an active tracker state and let AIR-1's dispatch loop pick it back
/// up" (see `resume_cycle` below). A human should have actually acted on the findings
/// on `/reviews` first; this control doesn't verify that, same as
/// `unblock_clarification` doesn't verify the clarification was actually answered.
async fn unblock_review(
    State(state): State<AppState>,
    AxumPath(issue_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    match resume_cycle(&state, &issue_id, &headers).await {
        Ok(()) => Redirect::to(&format!(
            "{}/reviews?issue={}",
            state.base_path,
            urlencode(&issue_id)
        ))
        .into_response(),
        Err(resp) => resp,
    }
}

/// Moves `issue_id` back to the project's first configured `tracker.active_states`
/// entry -- the one park-lifting action both `/requirements` (blocking clarification)
/// and `/reviews` (rework-round escalation) need, factored out rather than
/// copy-pasted twice. Rebuilds a short-lived tracker adapter from `WORKFLOW.md` per
/// request, same as `mcp.rs`'s pipeline-tool gating does in the MCP subprocess -- this
/// dashboard has no long-lived tracker handle of its own.
async fn resume_cycle(
    state: &AppState,
    issue_id: &str,
    headers: &HeaderMap,
) -> Result<(), Response> {
    if !admin_token_allows(headers) {
        return Err((StatusCode::UNAUTHORIZED, "invalid or missing admin token").into_response());
    }
    let def = crate::workflow::load(&state.workflow_dir.join("WORKFLOW.md")).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to load WORKFLOW.md: {e}"),
        )
            .into_response()
    })?;
    let cfg = crate::config::resolve(&def.config, &state.workflow_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to resolve config: {e}"),
        )
            .into_response()
    })?;
    let Some(target_state) = cfg.active_states.first() else {
        return Err((
            StatusCode::BAD_REQUEST,
            "project has no tracker.active_states configured",
        )
            .into_response());
    };
    let adapter = crate::tracker::build(
        &cfg.tracker_kind,
        &cfg.tracker_provider,
        &state.workflow_dir,
    )
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to build tracker adapter: {e}"),
        )
            .into_response()
    })?;
    adapter
        .set_issue_state(issue_id, target_state)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("failed to move issue back to an active state: {e}"),
            )
                .into_response()
        })
}

// --------------------------------------------------------------------------------
// /approvals -- AIR-5's human approval gate: pending requests + decision history,
// Approve/Request changes/Reject as admin-token-gated POST forms.
// --------------------------------------------------------------------------------

/// Whether `SYMPHONY_ADMIN_TOKEN` (unset means "no gate" -- matches this dashboard's
/// existing open-by-default posture for everything else) is present in either the
/// `Authorization: Bearer` header or the `symphony_admin` cookie `service.rs`'s own
/// `/login` sets. Deliberately its own small check rather than importing `service.rs`'s
/// private `require_admin` -- this router is mounted both standalone (`serve_composite`,
/// no service.rs involved at all) and nested read-only under `symphony serve` (where
/// `service.rs` leaves nested project routes unguarded, see its `require_admin` doc
/// comment): the browser still carries the same `symphony_admin` cookie either way
/// (`Path=/`), so reading it here is what actually gates this one mutating route.
fn admin_token_ok(headers: &HeaderMap) -> bool {
    let expected = std::env::var("SYMPHONY_ADMIN_TOKEN").unwrap_or_default();
    if expected.trim().is_empty() {
        return true;
    }
    extract_admin_token(headers).as_deref() == Some(expected.as_str())
}

fn extract_admin_token(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers.get(header::AUTHORIZATION)
        && let Ok(s) = auth.to_str()
        && let Some(token) = s.strip_prefix("Bearer ")
    {
        return Some(token.to_string());
    }
    if let Some(cookie_header) = headers.get(header::COOKIE)
        && let Ok(s) = cookie_header.to_str()
    {
        for part in s.split(';') {
            if let Some(v) = part.trim().strip_prefix("symphony_admin=") {
                return Some(v.to_string());
            }
        }
    }
    None
}

async fn approvals_page(State(state): State<AppState>) -> Html<String> {
    let db_path = state.eventlog_db_path();
    let pending = approvals::list_pending(&db_path).unwrap_or_default();
    let recent = approvals::list_recent(&db_path, 50).unwrap_or_default();
    let base = state.base_path.as_str();

    let pending_html: String = if pending.is_empty() {
        "<p class=\"empty\">No approvals pending.</p>".to_string()
    } else {
        pending
            .iter()
            .map(|r| pending_approval_card(r, base))
            .collect()
    };
    let history_rows: String = if recent.is_empty() {
        "<tr><td colspan=\"6\" class=\"empty\">No decisions recorded yet.</td></tr>".to_string()
    } else {
        recent
            .iter()
            .map(|r| approval_history_row(r, base))
            .collect()
    };

    let body = format!(
        r#"<section>
<h3>Pending <span class="badge">{pending_count}</span></h3>
{pending_html}
</section>

<section>
<h3>Decision history</h3>
<div class="table-wrap">
<table>
<thead><tr><th data-sort>Issue</th><th data-sort>Stage</th><th data-sort>Decision</th><th data-sort>Actor</th><th>Comment</th><th data-sort>Decided at</th></tr></thead>
<tbody>
{history_rows}
</tbody>
</table>
</div>
</section>"#,
        pending_count = pending.len(),
        pending_html = pending_html,
        history_rows = history_rows,
    );
    Html(page_shell(
        "approvals",
        "/approvals",
        &body,
        "",
        base,
        state.chat_enabled,
    ))
}

/// One pending request: the requesting stage's own output (the "why" -- roadmap §4's
/// decision-traceability bar applies to what a human is asked to approve, not just to
/// the eventual decision) plus the three real actions as admin-token-gated POST forms,
/// never a state-changing GET link.
fn pending_approval_card(r: &approvals::ApprovalRow, base: &str) -> String {
    let plan_html = match (&r.plan_json, &r.plan_text) {
        (Some(json), _) => format!(r#"<pre class="msg">{}</pre>"#, escape(json)),
        (None, Some(text)) => format!(r#"<pre class="msg">{}</pre>"#, escape(text)),
        (None, None) => "<p class=\"empty\">No output was captured for this stage.</p>".to_string(),
    };
    format!(
        r#"<div class="card" id="approval-{id}">
  <h2><a href="{base}/events?issue={issue_link}">{identifier}</a> &mdash; stage &quot;{stage}&quot;</h2>
  <div class="row">{title}</div>
  <div class="row"><b>requested</b> {requested_at}</div>
  {plan_html}
  <form method="post" action="{base}/approvals/{id}/decide" class="filters">
    <label for="comment-{id}">Comment <span class="empty">(required for "request changes"/"reject")</span></label>
    <input type="text" id="comment-{id}" name="comment" placeholder="reason">
    <button type="submit" name="decision" value="approve">Approve</button>
    <button type="submit" name="decision" value="changes">Request changes</button>
    <button type="submit" name="decision" value="reject">Reject</button>
  </form>
</div>"#,
        id = r.id,
        base = base,
        issue_link = urlencode(&r.issue_id),
        identifier = escape(&r.identifier),
        stage = escape(&r.stage_id),
        title = escape(&r.title),
        requested_at = escape(&r.requested_at),
        plan_html = plan_html,
    )
}

fn approval_history_row(r: &approvals::ApprovalRow, base: &str) -> String {
    format!(
        "<tr><td><a href=\"{base}/events?issue={issue_link}\">{identifier}</a></td><td>{stage}</td><td>{decision}</td><td>{actor}</td><td>{comment}</td><td>{resolved_at}</td></tr>",
        base = base,
        issue_link = urlencode(&r.issue_id),
        identifier = escape(&r.identifier),
        stage = escape(&r.stage_id),
        decision = escape(r.decision.as_deref().unwrap_or("-")),
        actor = escape(r.actor.as_deref().unwrap_or("-")),
        comment = escape(r.comment.as_deref().unwrap_or("-")),
        resolved_at = escape(r.resolved_at.as_deref().unwrap_or("-")),
    )
}

// /security -- AIR-8: security_findings artifacts, blocking state, human override.
//
// `orchestrator::run_pipeline`'s `evaluate_security_stage` records three event types
// per evaluation, always in this order: `security_findings` (the full redacted
// artifact, first line `stage=<id> risk=<risk>` then the JSON), and then at most one
// of `security_blocked` or `security_override_consumed`. `security_status_rows` below
// reconstructs "is this issue currently blocked" from nothing but relative event ids
// (the latest of the three per issue wins) -- no extra table, same one `events` log
// every other page here already reads.
// --------------------------------------------------------------------------------

struct SecurityRow {
    issue_id: String,
    identifier: String,
    title: String,
    created_at: String,
    findings: Option<SecurityFindings>,
    blocked: bool,
    /// A previous block on this same evaluation round was overridden (rather than
    /// this round having no blocking findings at all) -- shown as a distinct badge
    /// from plain "clear" so the override stays visible after it's been applied.
    overridden: bool,
    override_pending: bool,
}

fn security_status_rows(db_path: &std::path::Path) -> Vec<SecurityRow> {
    let findings_events =
        eventlog::latest_events_by_type(db_path, "security_findings").unwrap_or_default();
    let blocked_ids: std::collections::HashMap<String, i64> =
        eventlog::latest_events_by_type(db_path, "security_blocked")
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.issue_id, e.id))
            .collect();
    let consumed_ids: std::collections::HashMap<String, i64> =
        eventlog::latest_events_by_type(db_path, "security_override_consumed")
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.issue_id, e.id))
            .collect();

    findings_events
        .into_iter()
        .map(|e| {
            let findings = e
                .message
                .as_deref()
                .and_then(|m| m.split_once('\n'))
                .and_then(|(_, json)| serde_json::from_str::<SecurityFindings>(json).ok());
            // `security_findings` is always recorded before the `security_blocked` /
            // `security_override_consumed` that (may) follow it for the same
            // evaluation, so "id newer than this findings row" is exactly "belongs to
            // this evaluation, not a stale earlier one."
            let blocked = blocked_ids.get(&e.issue_id).is_some_and(|&b| b > e.id);
            let overridden = consumed_ids.get(&e.issue_id).is_some_and(|&c| c > e.id);
            let override_pending = eventlog::pending_override(db_path, &e.issue_id)
                .ok()
                .flatten()
                .is_some();
            SecurityRow {
                issue_id: e.issue_id,
                identifier: e.identifier,
                title: e.title,
                created_at: e.created_at,
                findings,
                blocked,
                overridden,
                override_pending,
            }
        })
        .collect()
}

async fn security_page(State(state): State<AppState>) -> Html<String> {
    let base = state.base_path.as_str();
    let rows = security_status_rows(&state.eventlog_db_path());

    let banner = if state.security.is_none() {
        web::error_banner(
            "This project has no security stage configured, or its dashboard was mounted \
             without tracker access -- findings below are read-only and overrides are disabled.",
        )
    } else {
        String::new()
    };

    let cards: String = if rows.is_empty() {
        "<p class=\"empty\">No security stage has run yet.</p>".to_string()
    } else {
        rows.iter()
            .map(|r| security_card(r, &state, base))
            .collect()
    };

    let body = format!("{banner}<div class=\"grid\">{cards}</div>");
    Html(page_shell(
        "security",
        "/security",
        &body,
        "",
        base,
        state.chat_enabled,
    ))
}

fn owasp_checklist_html(findings: &SecurityFindings) -> String {
    if findings.owasp_checklist.is_empty() {
        return "<p class=\"empty\">No OWASP checklist recorded.</p>".to_string();
    }
    let rows: String = findings
        .owasp_checklist
        .iter()
        .map(|item| {
            let status = match item.status {
                crate::security::OwaspStatus::Pass => "pass",
                crate::security::OwaspStatus::Fail => "fail",
                crate::security::OwaspStatus::NotApplicable => "not_applicable",
            };
            format!(
                "<tr><td>{id}</td><td>{name}</td><td>{status}</td><td>{evidence}</td></tr>",
                id = escape(&item.id),
                name = escape(&item.name),
                status = escape(status),
                evidence = escape(&item.evidence),
            )
        })
        .collect();
    format!(
        "<div class=\"table-wrap\"><table><thead><tr><th>ID</th><th>Name</th><th>Status</th><th>Evidence</th></tr></thead><tbody>{rows}</tbody></table></div>"
    )
}

fn findings_list_html(findings: &SecurityFindings) -> String {
    if findings.findings.is_empty() {
        return "<p class=\"empty\">No findings.</p>".to_string();
    }
    findings
        .findings
        .iter()
        .map(|f| {
            format!(
                r#"<div class="card" id="finding-{id}">
  <h2>{id} &middot; <span class="badge">{severity}</span></h2>
  <div class="row">{summary}</div>
  <div class="row"><b>location</b> {file}{line}</div>
  <div class="row"><b>owasp</b> {owasp} &middot; <b>cwe</b> {cwe}</div>
  <div class="msg"><b>exploit scenario:</b> {exploit}<br><b>remediation:</b> {remediation}</div>
</div>"#,
                id = escape(&f.id),
                severity = escape(f.severity.as_str()),
                summary = escape(&f.summary),
                file = escape(f.file.as_deref().unwrap_or("-")),
                line = f.line.map(|l| format!(":{l}")).unwrap_or_default(),
                owasp = escape(f.owasp_id.as_deref().unwrap_or("-")),
                cwe = escape(f.cwe.as_deref().unwrap_or("-")),
                exploit = escape(f.exploit_scenario.as_deref().unwrap_or("-")),
                remediation = escape(f.remediation.as_deref().unwrap_or("-")),
            )
        })
        .collect()
}

fn secrets_and_deps_html(findings: &SecurityFindings) -> String {
    let secret_rows: String = if findings.secrets_scan.matches.is_empty() {
        "<tr><td colspan=\"2\" class=\"empty\">none</td></tr>".to_string()
    } else {
        findings
            .secrets_scan
            .matches
            .iter()
            .map(|m| {
                format!(
                    "<tr><td>{file}</td><td>{line}</td></tr>",
                    file = escape(&m.file),
                    line = m.line,
                )
            })
            .collect()
    };
    let advisories: String = if findings.dependency_scan.advisories.is_empty() {
        "<span class=\"empty\">none</span>".to_string()
    } else {
        findings
            .dependency_scan
            .advisories
            .iter()
            .map(|a| escape(a))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let secrets_status = match findings.secrets_scan.status {
        crate::security::ScanStatus::Clean => "clean",
        crate::security::ScanStatus::Findings => "findings",
        crate::security::ScanStatus::NotRun => "not_run",
    };
    let deps_status = match findings.dependency_scan.status {
        crate::security::ScanStatus::Clean => "clean",
        crate::security::ScanStatus::Findings => "findings",
        crate::security::ScanStatus::NotRun => "not_run",
    };
    format!(
        r#"<div class="row"><b>secrets scan</b> {secrets_status} -- file/line only, matched text is never stored</div>
<div class="table-wrap"><table><thead><tr><th>File</th><th>Line</th></tr></thead><tbody>{secret_rows}</tbody></table></div>
<div class="row"><b>dependency scan</b> ({tool}) {deps_status} -- {advisories}</div>"#,
        tool = escape(&findings.dependency_scan.tool),
    )
}

fn security_card(r: &SecurityRow, state: &AppState, base: &str) -> String {
    let risk = r
        .findings
        .as_ref()
        .map(|f| f.risk_classification.as_str())
        .unwrap_or("unknown");
    let status_badge = if r.blocked {
        r#"<span class="badge closed">blocked</span>"#
    } else if r.overridden {
        r#"<span class="badge">overridden</span>"#
    } else {
        r#"<span class="badge">clear</span>"#
    };
    let pending_note = if r.override_pending {
        r#"<div class="row">an override has been recorded and will be applied on the next run</div>"#
    } else {
        ""
    };

    let (checklist, findings_list, scans) = match &r.findings {
        Some(f) => (
            owasp_checklist_html(f),
            findings_list_html(f),
            secrets_and_deps_html(f),
        ),
        None => (String::new(), String::new(), String::new()),
    };

    let override_form = if r.blocked && state.security.is_some() {
        format!(
            r#"<form class="compose" method="post" action="{base}/security/override" data-confirm="Override this blocking security finding and resume the cycle?">
  <input type="hidden" name="issue_id" value="{issue_id}">
  <label for="reason-{id_attr}">Override reason (required)</label>
  <textarea id="reason-{id_attr}" name="reason" maxlength="2000" required></textarea>
  <label for="token-{id_attr}">Admin token</label>
  <input type="password" id="token-{id_attr}" name="admin_token" required>
  <button type="submit" class="btn">Override and resume</button>
</form>"#,
            issue_id = escape(&r.issue_id),
            id_attr = escape(&r.issue_id),
        )
    } else {
        String::new()
    };

    format!(
        r#"<div class="card" id="sec-{id_attr}" style="flex-basis: 100%;">
  <h2><a href="{base}/events?issue={issue_link}">{identifier}</a> &mdash; {title} {status_badge}</h2>
  <div class="row"><b>risk</b> {risk} &middot; <b>evaluated</b> {created_at} &middot; <b>blocked_state</b> {blocked_state}</div>
  {pending_note}
  <section><h3>OWASP checklist</h3>{checklist}</section>
  <section><h3>Findings</h3>{findings_list}</section>
  <section><h3>Scanners</h3>{scans}</section>
  {override_form}
</div>"#,
        id_attr = escape(&r.issue_id),
        base = base,
        issue_link = urlencode(&r.issue_id),
        identifier = escape(&r.identifier),
        title = escape(&r.title),
        risk = escape(risk),
        created_at = escape(&r.created_at),
        blocked_state = state
            .security
            .as_ref()
            .map(|s| escape(&s.blocked_state))
            .unwrap_or_else(|| "-".to_string()),
    )
}

#[derive(Deserialize)]
struct DecideForm {
    decision: String,
    comment: Option<String>,
}

/// Only *records* the decision (`approvals::resolve`) -- applying it to tracker state
/// and the event log happens on the orchestrator's own tick
/// (`orchestrator::apply_resolved_approvals`), the one place with standing authority
/// to mutate tracker state. See `approvals.rs`'s module doc comment for why this
/// handler doesn't do that directly.
async fn approvals_decide(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<i64>,
    headers: HeaderMap,
    Form(form): Form<DecideForm>,
) -> Response {
    if !admin_token_ok(&headers) {
        return (StatusCode::UNAUTHORIZED, "invalid or missing admin token").into_response();
    }
    let Some(decision) = approvals::Decision::parse(&form.decision) else {
        return (StatusCode::BAD_REQUEST, "unknown decision").into_response();
    };
    let comment = form.comment.filter(|c| !c.trim().is_empty());
    if matches!(
        decision,
        approvals::Decision::RequestChanges | approvals::Decision::Reject
    ) && comment.is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            "a comment is required for \"request changes\" and \"reject\"",
        )
            .into_response();
    }
    let db_path = state.eventlog_db_path();
    if let Err(e) = approvals::resolve(&db_path, id, decision, "dashboard", comment.as_deref()) {
        tracing::warn!(approval_id = id, error = %e, "failed to record dashboard approval decision");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to record decision",
        )
            .into_response();
    }
    Redirect::to(&format!("{}/approvals", state.base_path)).into_response()
}

#[derive(Deserialize)]
struct OverrideForm {
    issue_id: String,
    reason: String,
    admin_token: String,
}

/// POST-only, admin-token gated (per-request token field rather than the cookie-based
/// login `service.rs` uses for its own admin routes -- this dashboard has no login
/// page of its own, single-project or nested). Records a `security_override` event
/// (consumed one-shot by the next `evaluate_security_stage` run, see
/// `eventlog::pending_override`) and, when a resume state is configured, moves the
/// issue out of `pipeline.blocked_state` immediately so the dispatcher picks it back
/// up without waiting for the next poll to notice a state nothing changed.
async fn security_override(
    State(state): State<AppState>,
    Form(form): Form<OverrideForm>,
) -> Response {
    let base = state.base_path.clone();
    let Some(security) = &state.security else {
        return Html(page_shell(
            "security",
            "/security",
            &web::error_banner("This project's dashboard has no tracker access configured; overrides are disabled."),
            "",
            &base,
            state.chat_enabled,
        ))
        .into_response();
    };

    let expected = std::env::var("SYMPHONY_ADMIN_TOKEN").unwrap_or_default();
    if expected.is_empty() || form.admin_token != expected {
        return Html(page_shell(
            "security",
            "/security",
            &web::error_banner("Invalid admin token."),
            "",
            &base,
            state.chat_enabled,
        ))
        .into_response();
    }
    if form.reason.trim().is_empty() {
        return Html(page_shell(
            "security",
            "/security",
            &web::error_banner("A reason is required to override a blocking security finding."),
            "",
            &base,
            state.chat_enabled,
        ))
        .into_response();
    }

    let db_path = state.eventlog_db_path();
    if let Err(e) = eventlog::insert_event(
        &db_path,
        &eventlog::NewEvent {
            issue_id: form.issue_id.clone(),
            identifier: form.issue_id.clone(),
            title: String::new(),
            session_id: None,
            event_type: "security_override".to_string(),
            message: Some(form.reason.clone()),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
        },
    ) {
        tracing::warn!(error = %e, issue_id = %form.issue_id, "failed to record security override");
    }

    if let Some(resume_state) = &security.resume_state
        && let Err(e) = security
            .tracker
            .set_issue_state(&form.issue_id, resume_state)
            .await
    {
        tracing::warn!(error = %e, issue_id = %form.issue_id, "failed to resume issue after security override");
    }

    Redirect::to(&format!("{base}/security")).into_response()
}

// --------------------------------------------------------------------------------
// /evidence -- AIR-9 release evidence bundles, persisted by
// `orchestrator::finalize_release_evidence` under `<workflow_dir>/.symphony/release/
// <key>.json` (`key` is `workspace::derive_workspace_key(issue_id)`, the same
// sanitized key `issue-<key>` branch names use -- see that function's own doc
// comment). Live-updates by re-fetching `/evidence/<key>/fragment` off the same
// `/fragment-stream` connection the dashboard already holds open (`dashboard`'s own
// doc comment), rather than opening a second stream just for this page.
// --------------------------------------------------------------------------------

fn release_dir(workflow_dir: &std::path::Path) -> PathBuf {
    workflow_dir.join(".symphony").join("release")
}

fn load_bundle(workflow_dir: &std::path::Path, key: &str) -> Option<EvidenceBundle> {
    let bytes = std::fs::read(release_dir(workflow_dir).join(format!("{key}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_bundle(
    workflow_dir: &std::path::Path,
    key: &str,
    bundle: &EvidenceBundle,
) -> std::io::Result<()> {
    let dir = release_dir(workflow_dir);
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_vec_pretty(bundle).unwrap_or_default();
    std::fs::write(dir.join(format!("{key}.json")), json)
}

/// Every persisted bundle's key, newest first by file modification time -- a plain
/// directory scan, not an index: there are at most a handful of open cycles at once,
/// so scanning on every request is cheap enough (same posture `eventlog`'s
/// per-request connection open takes -- see that module's own doc comment).
fn list_bundle_keys(workflow_dir: &std::path::Path) -> Vec<String> {
    let mut entries: Vec<(std::time::SystemTime, String)> =
        std::fs::read_dir(release_dir(workflow_dir))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    return None;
                }
                let key = path.file_stem()?.to_str()?.to_string();
                let modified = e.metadata().ok()?.modified().ok()?;
                Some((modified, key))
            })
            .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.0));
    entries.into_iter().map(|(_, k)| k).collect()
}

fn verdict_badge(v: Verdict) -> String {
    let (class, label) = match v {
        Verdict::Ready => ("badge", "READY"),
        Verdict::ReadyWithRisk => ("badge warn", "READY WITH RISK"),
        Verdict::Blocked => ("badge closed", "BLOCKED"),
    };
    format!(r#"<span class="{class}">{label}</span>"#)
}

/// Render a bundle's evidence sections as HTML for the dashboard view (the "why"
/// surface the global constraints require: verdict badge, the rule(s) that produced
/// it, then every section `release::render_markdown` builds). Reuses
/// `release::render_markdown`'s own Markdown -- already redacted -- and converts it
/// with `pulldown-cmark` rather than hand-rolling a second HTML renderer (see
/// `release.rs`'s own doc comment: "reuse pulldown-cmark only for HTML views").
fn render_markdown_html(markdown: &str) -> String {
    let mut options = pulldown_cmark::Options::empty();
    options.insert(pulldown_cmark::Options::ENABLE_TABLES);
    let parser = pulldown_cmark::Parser::new_ext(markdown, options);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}

fn render_evidence_content(bundle: &EvidenceBundle) -> String {
    let verdict = release::compute_verdict(bundle);
    let matrix = release::build_traceability_matrix(bundle);
    let markdown =
        release::render_markdown(bundle, verdict, &matrix, &std::collections::BTreeMap::new());
    let html = render_markdown_html(&markdown);
    format!(
        r#"<div class="row">{badge}</div>
<div class="post-body">{html}</div>"#,
        badge = verdict_badge(verdict),
    )
}

async fn evidence_index(State(state): State<AppState>) -> Html<String> {
    let base = state.base_path.as_str();
    let keys = list_bundle_keys(&state.workflow_dir);
    let rows: String = if keys.is_empty() {
        "<p class=\"empty\">No release evidence bundles recorded yet.</p>".to_string()
    } else {
        keys.iter()
            .filter_map(|k| load_bundle(&state.workflow_dir, k).map(|b| (k, b)))
            .map(|(key, bundle)| {
                let verdict = release::compute_verdict(&bundle);
                format!(
                    r#"<div class="thread-row"><a href="{base}/evidence/{key_link}">{title}</a>{badge}</div>"#,
                    key_link = urlencode(key),
                    title = escape(&bundle.title),
                    badge = verdict_badge(verdict),
                )
            })
            .collect()
    };
    let body = format!(
        r#"<section><h3>Release evidence</h3><div class="thread-list">{rows}</div></section>"#
    );
    Html(page_shell(
        "release evidence",
        "/evidence",
        &body,
        "",
        base,
        state.chat_enabled,
    ))
}

async fn evidence_page(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
) -> Html<String> {
    let base = state.base_path.as_str();
    let Some(bundle) = load_bundle(&state.workflow_dir, &key) else {
        return Html(page_shell(
            "release evidence",
            "/evidence",
            &error_banner(&format!(
                "No evidence bundle found for &lsquo;{}&rsquo;.",
                escape(&key)
            )),
            "",
            base,
            state.chat_enabled,
        ));
    };
    let script = format!(
        r#"<script>
new EventSource('{base}/fragment-stream').onmessage = function () {{
  fetch('{base}/evidence/{key_link}/fragment').then(function (r) {{ return r.text(); }}).then(function (html) {{
    document.getElementById('evidence-content').innerHTML = html;
  }});
}};
</script>"#,
        base = base,
        key_link = urlencode(&key),
    );
    let body = format!(
        r#"<div class="meta">generated {generated} &middot; live-updates when a new bundle lands (no page reload)</div>
<div id="evidence-content">{content}</div>
<form class="admin" method="post" action="{base}/evidence/{key_link}/override" data-confirm="Override every unresolved blocking security finding with this justification?">
  <label for="reason">Override unresolved blocking security findings (admin token required)</label>
  <input id="reason" name="reason" required maxlength="500">
  <button type="submit">Override &amp; re-verdict</button>
</form>"#,
        generated = escape(&bundle.generated_at),
        content = render_evidence_content(&bundle),
        base = base,
        key_link = urlencode(&key),
    );
    Html(page_shell(
        &format!("evidence: {}", bundle.title),
        "/evidence",
        &body,
        &script,
        base,
        state.chat_enabled,
    ))
}

async fn evidence_fragment(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
) -> Html<String> {
    match load_bundle(&state.workflow_dir, &key) {
        Some(bundle) => Html(render_evidence_content(&bundle)),
        None => Html(error_banner("No evidence bundle found for this cycle.")),
    }
}

#[derive(Deserialize)]
struct EvidenceOverrideForm {
    reason: String,
}

/// `SYMPHONY_ADMIN_TOKEN` must be set and match `Authorization: Bearer <token>` --
/// mirrors `service.rs::extract_admin_token`'s bearer-token check (that module's own
/// cookie/login-form half doesn't apply here: the single-project dashboard has no
/// login flow, only this one state-changing action). Unlike `service.rs`, an unset
/// token means "reject", not "allow": `service.rs` refuses to even start without one
/// because registering/removing whole repos is destructive; this dashboard stays
/// read-only by default (its own module doc comment) and this is the one exception,
/// so the safer default when nobody configured a token is "this action is off."
fn evidence_override_authorized(headers: &HeaderMap) -> bool {
    let expected = std::env::var("SYMPHONY_ADMIN_TOKEN").unwrap_or_default();
    if expected.trim().is_empty() {
        return false;
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .is_some_and(|token| token == expected)
}

/// Human override with justification (Roadmap §4's "security checklist, risk
/// classification and any human override with justification"): marks every currently
/// unresolved blocking security finding as overridden with `reason`, moving a
/// `Blocked` verdict to `ReadyWithRisk` (`release::compute_verdict`'s own rule --
/// nothing here computes a verdict directly, so this can never silently disagree with
/// what the bundle itself would report). A no-op, but not an error, when there's
/// nothing blocking to override (e.g. before AIR-8 ever records a security finding).
async fn evidence_override(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
    headers: HeaderMap,
    Form(form): Form<EvidenceOverrideForm>,
) -> Response {
    if !evidence_override_authorized(&headers) {
        return (
            StatusCode::FORBIDDEN,
            "admin token required (SYMPHONY_ADMIN_TOKEN)",
        )
            .into_response();
    }
    let Some(mut bundle) = load_bundle(&state.workflow_dir, &key) else {
        return (StatusCode::NOT_FOUND, "no evidence bundle for this cycle").into_response();
    };
    let reason = form.reason.trim();
    if reason.is_empty() {
        return (StatusCode::BAD_REQUEST, "reason is required").into_response();
    }
    for finding in bundle
        .security_findings
        .iter_mut()
        .filter(|f| f.blocking && f.override_reason.is_none())
    {
        finding.override_reason = Some(reason.to_string());
    }
    if let Err(e) = save_bundle(&state.workflow_dir, &key, &bundle) {
        tracing::warn!(key = %key, error = %e, "failed to persist evidence override");
        return (StatusCode::INTERNAL_SERVER_ERROR, "failed to save override").into_response();
    }
    Redirect::to(&format!("{}/evidence/{}", state.base_path, urlencode(&key))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tracker(dir: &Path) -> Arc<dyn TrackerAdapter> {
        let mut map = serde_yaml::Mapping::new();
        map.insert(
            serde_yaml::Value::from("dir"),
            serde_yaml::Value::from(dir.to_string_lossy().to_string()),
        );
        Arc::new(
            crate::tracker::local::LocalTrackerAdapter::new(&serde_yaml::Value::Mapping(map), dir)
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn fragment_stream_pushes_initial_snapshot_then_updates_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = watch::channel(StatusSnapshot::default());
        let app = router(
            rx,
            dir.path().to_path_buf(),
            test_tracker(dir.path()),
            "",
            false,
            None,
            None,
        );
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
                last_notification: None,
                last_tool_call: None,
                stage: None,
                budget_note: None,
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
        let dir = tempfile::tempdir().unwrap();
        let html = render_fragment(&snapshot, "", &dir.path().join("symphony.db"));
        assert!(html.contains("No agents running"));
        assert!(html.contains("Retry queue is empty"));
    }

    #[test]
    fn render_fragment_surfaces_a_live_pending_approvals_count() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(eventlog::DB_FILENAME);
        approvals::create_pending(
            &db_path,
            &approvals::NewApproval {
                issue_id: "I-1".to_string(),
                identifier: "I-1".to_string(),
                title: "T".to_string(),
                stage_id: "plan".to_string(),
                next_stage_id: None,
                plan_text: None,
                plan_json: None,
            },
        )
        .unwrap();

        let html = render_fragment(&StatusSnapshot::default(), "", &db_path);
        assert!(html.contains("awaiting approval"), "{html}");
        assert!(html.contains("/approvals"), "{html}");
    }

    async fn spawn_router(dir: &std::path::Path) -> String {
        spawn_router_with_chat(dir, false).await
    }

    async fn spawn_router_with_chat(dir: &std::path::Path, chat_enabled: bool) -> String {
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let app = router(
            rx,
            dir.to_path_buf(),
            test_tracker(dir),
            "",
            chat_enabled,
            None,
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// Regression test for BUG-2: without chat enabled, the dashboard nav must not
    /// advertise a `/chat` link that 404s.
    #[tokio::test]
    async fn dashboard_nav_omits_chat_link_when_chat_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let base = spawn_router_with_chat(dir.path(), false).await;
        let body = reqwest::get(format!("{base}/events"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!body.contains("href=\"/chat\""), "{body}");
        assert!(body.contains("href=\"/events\""), "{body}");
    }

    #[tokio::test]
    async fn dashboard_nav_includes_chat_link_when_chat_is_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let base = spawn_router_with_chat(dir.path(), true).await;
        let body = reqwest::get(format!("{base}/events"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("href=\"/chat\""), "{body}");
    }

    fn approval_fixture(issue_id: &str, next_stage_id: Option<&str>) -> approvals::NewApproval {
        approvals::NewApproval {
            issue_id: issue_id.to_string(),
            identifier: issue_id.to_string(),
            title: format!("Issue {issue_id}"),
            stage_id: "plan".to_string(),
            next_stage_id: next_stage_id.map(str::to_string),
            plan_text: Some("design summary here".to_string()),
            plan_json: None,
        }
    }

    #[tokio::test]
    async fn approvals_page_lists_pending_requests_and_decision_history() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(eventlog::DB_FILENAME);
        approvals::create_pending(&db_path, &approval_fixture("I-1", Some("implement"))).unwrap();
        let resolved_id =
            approvals::create_pending(&db_path, &approval_fixture("I-2", None)).unwrap();
        approvals::resolve(
            &db_path,
            resolved_id,
            approvals::Decision::Approve,
            "alice",
            None,
        )
        .unwrap();

        let base = spawn_router(dir.path()).await;
        let body = reqwest::get(format!("{base}/approvals"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("I-1"), "{body}");
        assert!(body.contains("design summary here"), "{body}");
        assert!(body.contains("I-2"), "{body}");
        assert!(body.contains("alice"), "{body}");
    }

    #[tokio::test]
    async fn approvals_decide_approve_records_the_decision_and_redirects() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(eventlog::DB_FILENAME);
        let id = approvals::create_pending(&db_path, &approval_fixture("I-3", Some("implement")))
            .unwrap();

        let base = spawn_router(dir.path()).await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let resp = client
            .post(format!("{base}/approvals/{id}/decide"))
            .form(&[("decision", "approve")])
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_redirection(), "{}", resp.status());

        let row = approvals::get(&db_path, id).unwrap().unwrap();
        assert_eq!(row.decision.as_deref(), Some("approve"));
        assert_eq!(row.actor.as_deref(), Some("dashboard"));
        assert!(!row.is_pending());
    }

    #[tokio::test]
    async fn approvals_decide_requires_a_comment_for_changes_and_reject() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(eventlog::DB_FILENAME);
        let id = approvals::create_pending(&db_path, &approval_fixture("I-4", Some("implement")))
            .unwrap();

        let base = spawn_router(dir.path()).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/approvals/{id}/decide"))
            .form(&[("decision", "changes")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

        let row = approvals::get(&db_path, id).unwrap().unwrap();
        assert!(
            row.is_pending(),
            "a rejected-for-missing-comment submission must not resolve the row"
        );
    }

    // `admin_token_ok` reads the process-global `SYMPHONY_ADMIN_TOKEN` env var, so its
    // gating behavior is covered as a pure function here rather than via a live
    // `SYMPHONY_ADMIN_TOKEN`-mutating HTTP test -- `cargo test` runs this binary's
    // tests concurrently in one process, and every other `/approvals` HTTP test above
    // depends on the token being unset (the default, open-dashboard posture).
    #[test]
    fn admin_token_ok_allows_open_by_default_and_gates_once_a_token_is_supplied() {
        let expected = "test-admin-token-air5-unit";
        assert!(
            extract_admin_token(&HeaderMap::new()).is_none(),
            "sanity: no headers means no token"
        );
        // No test in this module ever sets `SYMPHONY_ADMIN_TOKEN` (see the comment
        // above), so the dashboard's default open-by-default posture is safe to assert
        // here too.
        assert!(admin_token_ok(&HeaderMap::new()));

        // "no SYMPHONY_ADMIN_TOKEN configured" is exercised by every other test in
        // this module already (none of them set it); here we exercise the gated path
        // directly against `extract_admin_token`'s own matching, without touching the
        // shared env var.
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert_ne!(extract_admin_token(&headers).as_deref(), Some(expected));

        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {expected}").parse().unwrap(),
        );
        assert_eq!(extract_admin_token(&headers).as_deref(), Some(expected));

        let mut cookie_headers = HeaderMap::new();
        cookie_headers.insert(
            header::COOKIE,
            format!("other=x; symphony_admin={expected}; more=y")
                .parse()
                .unwrap(),
        );
        assert_eq!(
            extract_admin_token(&cookie_headers).as_deref(),
            Some(expected)
        );
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
            last_notification: None,
            last_tool_call: None,
            stage: None,
            budget_note: None,
        };
        let html = running_card(&row, "");
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn issue_usage_row_shows_test_and_coverage_evidence_with_links() {
        let r = eventlog::IssueUsageRow {
            issue_id: "issue-1".to_string(),
            identifier: "AR-1".to_string(),
            title: "Scaffold".to_string(),
            ..Default::default()
        };
        let html = issue_usage_row(&r, "", Some("3 passed, 1 failed"), Some("82.3%"), false);
        assert!(html.contains("3 passed, 1 failed"));
        assert!(html.contains("82.3%"));
        assert!(html.contains("type=test_report"));
        assert!(html.contains("type=coverage\""));
    }

    #[test]
    fn issue_usage_row_shows_not_run_when_no_test_stage_has_run_yet() {
        let r = eventlog::IssueUsageRow {
            issue_id: "issue-1".to_string(),
            ..Default::default()
        };
        let html = issue_usage_row(&r, "", None, None, false);
        assert!(html.contains("not run"));
    }

    /// Regression test for FEAT-1: the Duration cell's three states.
    #[test]
    fn issue_usage_row_shows_duration_states() {
        let closed = eventlog::IssueUsageRow {
            issue_id: "issue-1".to_string(),
            duration_secs: 125.0,
            duration_open: false,
            ..Default::default()
        };
        let html = issue_usage_row(&closed, "", None, None, false);
        assert!(html.contains("2m 5s"), "{html}");
        assert!(!html.contains('~'), "{html}");

        let open_running = eventlog::IssueUsageRow {
            issue_id: "issue-2".to_string(),
            duration_secs: 30.0,
            duration_open: true,
            ..Default::default()
        };
        let html = issue_usage_row(&open_running, "", None, None, true);
        assert!(html.contains("30s"), "{html}");
        assert!(!html.contains('~'), "{html}");

        let open_stale = eventlog::IssueUsageRow {
            issue_id: "issue-3".to_string(),
            duration_secs: 30.0,
            duration_open: true,
            ..Default::default()
        };
        let html = issue_usage_row(&open_stale, "", None, None, false);
        assert!(html.contains("~30s"), "{html}");
        assert!(html.contains("title="), "{html}");
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
            last_notification: None,
            last_tool_call: None,
            stage: None,
            budget_note: None,
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
        event_row_at(event_type, message, "2026-01-01T00:00:00Z")
    }

    fn event_row_at(
        event_type: &str,
        message: Option<&str>,
        created_at: &str,
    ) -> eventlog::EventRow {
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
            created_at: created_at.to_string(),
        }
    }

    // ── FEAT-2: same-second collapsing ──

    #[test]
    fn feats_different_seconds_never_merged() {
        let rows = vec![
            event_row_at("tool_call", Some("Grep"), "2026-01-01T00:00:02Z"),
            event_row_at("turn_started", None, "2026-01-01T00:00:01Z"),
        ];
        let html = render_transcript(&rows, "");
        assert!(html.contains("00:00:02"), "newer second: {html}");
        assert!(html.contains("00:00:01"), "older second: {html}");
        assert_eq!(
            html.matches("class=\"msg").count(),
            2,
            "two separate bubbles: {html}"
        );
    }

    #[test]
    fn feats_same_second_tool_and_system_events_collapsed_into_one_line() {
        let rows = vec![
            event_row_at("turn_started", None, "2026-01-01T00:00:01Z"),
            event_row_at("session_started", None, "2026-01-01T00:00:01.021Z"),
        ];
        let html = render_transcript(&rows, "");
        assert!(html.contains("turn_started"), "{html}");
        assert!(html.contains("session_started"), "{html}");
        assert!(html.contains("&middot;"), "collapsed separator: {html}");
        let bubble_count = html.matches("class=\"msg").count();
        assert_eq!(
            bubble_count, 1,
            "two same-second events → one collapsed bubble, got {bubble_count}: {html}"
        );
        assert!(html.contains("00:00:01"), "{html}");
    }

    #[test]
    fn feats_assistant_never_collapsed_even_when_same_second() {
        let rows = vec![
            event_row_at(
                "other_message",
                Some("Here's the fix"),
                "2026-01-01T00:00:01.100Z",
            ),
            event_row_at("turn_started", None, "2026-01-01T00:00:01Z"),
        ];
        let html = render_transcript(&rows, "");
        let assistant_pos = html.find("Here&#39;s the fix").unwrap();
        let system_pos = html.find("turn_started").unwrap();
        assert!(
            assistant_pos < system_pos,
            "assistant (newest) before system (older): assistant at {assistant_pos}, system at {system_pos}"
        );
        assert!(
            html.contains(r#"class="msg assistant""#),
            "assistant bubble preserved: {html}"
        );
        assert_eq!(
            html.matches("class=\"msg").count(),
            2,
            "assistant never merged: {html}"
        );
    }

    #[test]
    fn feats_same_second_tool_calls_show_tool_names() {
        let rows = vec![
            event_row_at("tool_call", Some("Grep"), "2026-01-01T00:00:01Z"),
            event_row_at("tool_call", Some("Read"), "2026-01-01T00:00:01.050Z"),
            event_row_at("tool_call", Some("Edit"), "2026-01-01T00:00:01.100Z"),
        ];
        let html = render_transcript(&rows, "");
        assert!(html.contains("tool_call: Grep"), "{html}");
        assert!(html.contains("tool_call: Read"), "{html}");
        assert!(html.contains("tool_call: Edit"), "{html}");
        assert_eq!(
            html.matches("class=\"msg").count(),
            1,
            "three same-second tools → one collapsed bubble: {html}"
        );
    }

    #[test]
    fn feats_mixed_tool_and_system_in_same_second_collapses() {
        let rows = vec![
            event_row_at("turn_started", None, "2026-01-01T00:00:01Z"),
            event_row_at("tool_call", Some("Grep"), "2026-01-01T00:00:01.010Z"),
            event_row_at("session_started", None, "2026-01-01T00:00:01.020Z"),
        ];
        let html = render_transcript(&rows, "");
        assert!(html.contains("turn_started"), "{html}");
        assert!(html.contains("tool_call: Grep"), "{html}");
        assert!(html.contains("session_started"), "{html}");
        assert_eq!(
            html.matches("class=\"msg").count(),
            1,
            "three same-second events → one collapsed bubble: {html}"
        );
    }

    // ── FEAT-3: stage grouping ──

    #[test]
    fn feats_no_stage_events_falls_back_to_flat_rendering() {
        let rows = vec![
            event_row_at("dispatched", None, "2026-01-01T00:00:02Z"),
            event_row_at("turn_started", None, "2026-01-01T00:00:01Z"),
        ];
        let html = render_transcript(&rows, "");
        assert!(!html.contains("<section>"), "no stage sections: {html}");
        assert!(html.contains("turn_started"), "{html}");
        assert!(html.contains("dispatched"), "{html}");
    }

    #[test]
    fn feats_multi_stage_events_grouped_into_sections_with_heading_and_outcome() {
        let rows = vec![
            // Stage "review" finished
            event_row_at(
                "stage_finished",
                Some("review: blocked: 1 critical"),
                "2026-01-01T00:00:06Z",
            ),
            event_row_at("other_message", Some("here's why"), "2026-01-01T00:00:05Z"),
            event_row_at("tool_call", Some("Read"), "2026-01-01T00:00:04Z"),
            // Stage "implement" started
            event_row_at("stage_started", Some("review"), "2026-01-01T00:00:03Z"),
            event_row_at("tool_call", Some("Edit"), "2026-01-01T00:00:02Z"),
            event_row_at("tool_call", Some("Write"), "2026-01-01T00:00:02Z"),
            // Stage "implement" finished
            event_row_at(
                "stage_finished",
                Some("implement: completed"),
                "2026-01-01T00:00:01Z",
            ),
            // Stage "implement" started
            event_row_at("stage_started", Some("implement"), "2026-01-01T00:00:00Z"),
        ];
        let html = render_transcript(&rows, "");
        assert!(html.contains("<section>"), "stage sections: {html}");
        assert!(html.contains("<h3>review"), "{html}");
        assert!(html.contains("blocked: 1 critical"), "{html}");
        assert!(html.contains("<h3>implement"), "{html}");
        assert!(html.contains("completed"), "{html}");
        // stage_started/stage_finished are not rendered as individual bubbles
        assert!(
            !html.contains("stage_started"),
            "boundary markers excluded: {html}"
        );
        assert!(
            !html.contains("stage_finished"),
            "boundary markers excluded: {html}"
        );
        // Tool events inside stages are still present
        assert!(html.contains("tool_call"), "{html}");
        assert!(html.contains("Read"), "{html}");
        assert!(html.contains("Edit"), "{html}");
        assert!(html.contains("Write"), "{html}");
        // Assistant content is present
        assert!(html.contains("here&#39;s why"), "{html}");
        // implement's two same-second tools are collapsed
        let implement_start = html.find("<h3>implement").unwrap();
        let implement_section = &html[implement_start..];
        let bubbles_in_implement = implement_section.matches("class=\"msg").count();
        assert_eq!(
            bubbles_in_implement, 1,
            "implement's two same-second tools collapsed into one: {implement_section}"
        );
    }

    #[test]
    fn feats_pre_first_stage_events_flat_no_section_header() {
        let rows = vec![
            event_row_at("turn_started", None, "2026-01-01T00:00:04Z"),
            event_row_at("stage_started", Some("plan"), "2026-01-01T00:00:03Z"),
            event_row_at("dispatched", None, "2026-01-01T00:00:02Z"),
            event_row_at("other_message", Some("hello"), "2026-01-01T00:00:01Z"),
        ];
        let html = render_transcript(&rows, "");
        assert!(html.contains("<h3>plan"), "stage section: {html}");
        assert!(
            html.contains("turn_started"),
            "pre-stage event inside plan: {html}"
        );
        assert!(html.contains("dispatched"), "{html}");
        assert!(html.contains("hello"), "{html}");
        // stage_started boundary not rendered as a bubble
        assert!(!html.contains(">stage_started<"), "{html}");
    }

    #[test]
    fn feats_same_second_collapsing_does_not_cross_stage_boundaries() {
        let rows = vec![
            event_row_at("turn_started", None, "2026-01-01T00:00:05Z"),
            event_row_at("tool_call", Some("Read"), "2026-01-01T00:00:05.010Z"),
            event_row_at("stage_started", Some("plan"), "2026-01-01T00:00:04Z"),
            event_row_at("tool_call", Some("Write"), "2026-01-01T00:00:04Z"),
            event_row_at(
                "stage_started",
                Some("requirements"),
                "2026-01-01T00:00:03Z",
            ),
            event_row_at("tool_call", Some("Grep"), "2026-01-01T00:00:04Z"),
        ];
        let html = render_transcript(&rows, "");
        let bubbles = html.matches("class=\"msg").count();
        assert_eq!(
            bubbles, 3,
            "bucketed by stage: requirements→1 Grep, plan→1 Write, plan_stage→1 turn_started+Read collapsed: {html}"
        );
    }

    #[test]
    fn render_transcript_renders_newest_first_and_escapes_message_bodies() {
        let rows = vec![
            event_row_fixture("other_message", Some("newest reply")),
            event_row_fixture("dispatched", Some("<script>alert(1)</script>")),
        ];
        let html = render_transcript(&rows, "");
        let dispatched_pos = html.find("dispatched").unwrap();
        let reply_pos = html.find("newest reply").unwrap();
        assert!(
            reply_pos < dispatched_pos,
            "newest (reply) event should render before the older dispatched event"
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

    #[test]
    fn finding_row_renders_fields_and_escapes_untrusted_content() {
        let finding = serde_json::json!({
            "id": "F1",
            "severity": "blocker",
            "category": "over-implementation",
            "file": "<script>alert(1)</script>",
            "line": 42,
            "requirement_id": "R1",
            "summary": "unnecessary abstraction"
        });
        let html = finding_row(&finding);
        assert!(html.contains("F1"));
        assert!(html.contains("blocker"));
        assert!(html.contains("over-implementation"));
        assert!(html.contains("42"));
        assert!(html.contains("R1"));
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
    }

    // -----------------------------------------------------------------------------
    // AIR-8: /security
    // -----------------------------------------------------------------------------

    fn sample_findings_json(risk: &str) -> String {
        format!(
            r#"{{"schema_version":1,"risk_classification":"{risk}","owasp_checklist":[],
            "findings":[{{"id":"S1","severity":"{risk}","owasp_id":"A03:2021","cwe":"CWE-89",
                "file":"src/x.rs","line":10,"summary":"sql injection","exploit_scenario":"...",
                "remediation":"..."}}],
            "secrets_scan":{{"status":"clean","matches":[]}},
            "dependency_scan":{{"tool":"","status":"not_run","advisories":[]}}}}"#
        )
    }

    #[tokio::test]
    async fn security_status_rows_derives_blocked_overridden_and_clear_from_event_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(eventlog::DB_FILENAME);

        let findings_event = |issue: &str, risk: &str| eventlog::NewEvent {
            issue_id: issue.to_string(),
            identifier: issue.to_string(),
            title: "t".to_string(),
            session_id: None,
            event_type: "security_findings".to_string(),
            message: Some(format!(
                "stage=security risk={risk}\n{}",
                sample_findings_json(risk)
            )),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
        };
        let marker_event = |issue: &str, event_type: &str| eventlog::NewEvent {
            issue_id: issue.to_string(),
            identifier: issue.to_string(),
            title: "t".to_string(),
            session_id: None,
            event_type: event_type.to_string(),
            message: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
        };

        // A: findings then blocked -> currently blocked.
        eventlog::insert_event(&db, &findings_event("A", "critical")).unwrap();
        eventlog::insert_event(&db, &marker_event("A", "security_blocked")).unwrap();
        // B: findings then override-consumed -> overridden, not blocked.
        eventlog::insert_event(&db, &findings_event("B", "critical")).unwrap();
        eventlog::insert_event(&db, &marker_event("B", "security_override_consumed")).unwrap();
        // C: findings only, nothing blocking -> clear.
        eventlog::insert_event(&db, &findings_event("C", "low")).unwrap();

        let rows = security_status_rows(&db);
        assert_eq!(
            rows.len(),
            3,
            "{:?}",
            rows.iter().map(|r| &r.issue_id).collect::<Vec<_>>()
        );

        let a = rows.iter().find(|r| r.issue_id == "A").unwrap();
        assert!(a.blocked);
        assert!(!a.overridden);

        let b = rows.iter().find(|r| r.issue_id == "B").unwrap();
        assert!(!b.blocked);
        assert!(b.overridden);

        let c = rows.iter().find(|r| r.issue_id == "C").unwrap();
        assert!(!c.blocked);
        assert!(!c.overridden);
        assert_eq!(
            c.findings.as_ref().unwrap().risk_classification,
            crate::security::Severity::Low
        );
    }

    #[test]
    fn owasp_checklist_html_escapes_evidence_text() {
        let findings: SecurityFindings = serde_json::from_str(&sample_findings_json("low"))
            .map(|mut f: SecurityFindings| {
                f.owasp_checklist.push(crate::security::OwaspItem {
                    id: "A01:2021".to_string(),
                    name: "Broken Access Control".to_string(),
                    applicable: false,
                    status: crate::security::OwaspStatus::NotApplicable,
                    evidence: "<script>alert(1)</script>".to_string(),
                });
                f
            })
            .unwrap();
        let html = owasp_checklist_html(&findings);
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn rework_round_row_shows_the_round_number_and_escalated_badge() {
        let event = eventlog::EventRow {
            id: 1,
            issue_id: "1".to_string(),
            identifier: "AIR-7".to_string(),
            title: "t".to_string(),
            session_id: None,
            event_type: "rework_round".to_string(),
            importance: "normal".to_string(),
            message: Some(
                serde_json::json!({
                    "stage": "review",
                    "recommendation": "request_changes",
                    "summary": "still missing a test",
                    "escalated": true
                })
                .to_string(),
            ),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            created_at: "now".to_string(),
        };
        let html = rework_round_row(3, &event);
        assert!(html.contains("<td>3</td>"));
        assert!(html.contains("review"));
        assert!(html.contains("request_changes"));
        assert!(html.contains("yes"));
    }

    /// AIR-7 acceptance criterion: the `/reviews` panel is the explanation surface for
    /// a recorded `review_findings` artifact -- recommendation, unmet acceptance
    /// criteria, over-implementation and the finding table all readable from it.
    #[tokio::test]
    async fn review_card_renders_recommendation_unmet_criteria_and_findings() {
        let dir = tempfile::tempdir().unwrap();
        let workflow_dir = dir.path().to_path_buf();
        let db = workflow_dir.join(eventlog::DB_FILENAME);
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(workspace.join(".symphony")).unwrap();
        std::fs::write(workspace.join(".symphony/current-stage"), "review").unwrap();

        let content = serde_json::json!({
            "schema_version": 1,
            "recommendation": "request_changes",
            "findings": [{
                "id": "F1", "severity": "major", "category": "requirement-coverage",
                "file": "src/x.rs", "line": 10, "requirement_id": "R2",
                "summary": "does not handle the empty case"
            }],
            "unmet_acceptance_criteria": ["AC3"],
            "over_implementation": ["speculative caching layer"]
        })
        .to_string();
        let result = artifacts::execute_tool(
            &db,
            &workflow_dir,
            &workspace,
            "issue-1",
            "issue-1",
            serde_json::json!({
                "kind": "review_findings",
                "content_type": "application/json",
                "content": content,
                "summary": "requests changes"
            }),
        )
        .await;
        assert!(result.success, "{}", result.content);

        let row = artifacts::list_for_cycle(&db, "issue-1")
            .into_iter()
            .find(|r| r.kind == "review_findings")
            .unwrap();
        let html = review_card(&workflow_dir, &row);
        assert!(html.contains("request_changes"));
        assert!(html.contains("AC3"));
        assert!(html.contains("speculative caching layer"));
        assert!(html.contains("does not handle the empty case"));
        assert!(html.contains("R2"));
    }

    #[tokio::test]
    async fn reviews_page_without_an_issue_shows_picker_and_form() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let state = AppState {
            status_rx: rx,
            workflow_dir: dir.path().to_path_buf(),
            base_path: String::new(),
            chat_enabled: false,
            security: None,
            observability: None,
            tracker: test_tracker(dir.path()),
        };
        let Html(body) = reviews_page(State(state), Query(ReviewsQuery { issue: None })).await;
        assert!(body.contains(r#"<form class="filters" method="get">"#));
        assert!(body.contains("<h3>Recent issues</h3>"));
        assert!(body.contains("No issues with recorded events yet."));
    }

    #[tokio::test]
    async fn requirements_page_without_an_issue_shows_picker_and_form() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let state = AppState {
            status_rx: rx,
            workflow_dir: dir.path().to_path_buf(),
            base_path: String::new(),
            chat_enabled: false,
            security: None,
            observability: None,
            tracker: test_tracker(dir.path()),
        };
        let Html(body) =
            requirements_page(State(state), Query(RequirementsQuery { issue: None })).await;
        assert!(body.contains(r#"<form class="filters" method="get">"#));
        assert!(body.contains("<h3>Recent issues</h3>"));
        assert!(body.contains("No issues with recorded events yet."));
    }

    #[tokio::test]
    async fn reviews_page_shows_an_unblock_form_only_when_a_round_was_escalated() {
        let dir = tempfile::tempdir().unwrap();
        let workflow_dir = dir.path().to_path_buf();
        let db = workflow_dir.join(eventlog::DB_FILENAME);
        eventlog::record_rework_round(
            &db,
            &eventlog::NewReworkRound {
                issue_id: "1",
                identifier: "AIR-7",
                title: "t",
                stage_id: "review",
                recommendation: "request_changes",
                summary: "round 1",
                escalated: false,
            },
        )
        .unwrap();

        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let state = AppState {
            status_rx: rx.clone(),
            workflow_dir: workflow_dir.clone(),
            base_path: String::new(),
            chat_enabled: false,
            security: None,
            observability: None,
            tracker: test_tracker(&workflow_dir),
        };
        let Html(body) = reviews_page(
            State(state),
            Query(ReviewsQuery {
                issue: Some("1".to_string()),
            }),
        )
        .await;
        assert!(!body.contains("Resume cycle"), "not escalated yet: {body}");

        eventlog::record_rework_round(
            &db,
            &eventlog::NewReworkRound {
                issue_id: "1",
                identifier: "AIR-7",
                title: "t",
                stage_id: "review",
                recommendation: "request_changes",
                summary: "round 2",
                escalated: true,
            },
        )
        .unwrap();
        let state = AppState {
            status_rx: rx,
            workflow_dir: workflow_dir.clone(),
            base_path: String::new(),
            chat_enabled: false,
            security: None,
            observability: None,
            tracker: test_tracker(&workflow_dir),
        };
        let Html(body) = reviews_page(
            State(state),
            Query(ReviewsQuery {
                issue: Some("1".to_string()),
            }),
        )
        .await;
        assert!(body.contains("Resume cycle"));
        assert!(body.contains("/reviews/1/unblock"));
        assert!(body.contains("<td>1</td>"));
        assert!(body.contains("<td>2</td>"));
    }

    #[test]
    fn findings_list_html_never_contains_the_word_secret_value_placeholder() {
        // Regression guard: findings/secrets rendering must never echo raw secret
        // text -- `SecretMatch` structurally can't hold it (see `security` module's
        // own tests), so this just checks the rendered HTML sticks to file/line.
        let findings: SecurityFindings =
            serde_json::from_str(&sample_findings_json("high")).unwrap();
        let html = secrets_and_deps_html(&findings);
        assert!(html.contains("clean"));
        assert!(!html.contains("sk-"));
    }

    #[test]
    fn security_override_form_requires_reason_and_admin_token_fields() {
        let row = SecurityRow {
            issue_id: "A".to_string(),
            identifier: "A".to_string(),
            title: "t".to_string(),
            created_at: "now".to_string(),
            findings: None,
            blocked: true,
            overridden: false,
            override_pending: false,
        };
        let state = AppState {
            status_rx: watch::channel(StatusSnapshot::default()).1,
            workflow_dir: PathBuf::from("."),
            base_path: String::new(),
            chat_enabled: false,
            security: Some(SecurityContext {
                tracker: Arc::new(
                    crate::tracker::local::LocalTrackerAdapter::new(
                        &serde_yaml::from_str("dir: .").unwrap(),
                        std::path::Path::new("."),
                    )
                    .unwrap(),
                ),
                blocked_state: "blocked".to_string(),
                resume_state: Some("todo".to_string()),
            }),
            observability: None,
            tracker: test_tracker(Path::new(".")),
        };
        let html = security_card(&row, &state, "");
        assert!(html.contains(r#"name="reason""#));
        assert!(html.contains(r#"name="admin_token""#));
        assert!(html.contains(r#"action="/security/override""#));
    }

    #[test]
    fn security_override_form_is_absent_without_security_context() {
        let row = SecurityRow {
            issue_id: "A".to_string(),
            identifier: "A".to_string(),
            title: "t".to_string(),
            created_at: "now".to_string(),
            findings: None,
            blocked: true,
            overridden: false,
            override_pending: false,
        };
        let state = AppState {
            status_rx: watch::channel(StatusSnapshot::default()).1,
            workflow_dir: PathBuf::from("."),
            base_path: String::new(),
            chat_enabled: false,
            security: None,
            observability: None,
            tracker: test_tracker(Path::new(".")),
        };
        let html = security_card(&row, &state, "");
        assert!(!html.contains("action=\"/security/override\""));
    }

    // -----------------------------------------------------------------------------
    // AIR-9: /evidence
    // -----------------------------------------------------------------------------

    fn test_bundle(cycle_id: &str) -> EvidenceBundle {
        release::assemble(
            &crate::domain::Issue {
                id: cycle_id.to_string(),
                native_ref: None,
                identifier: "AIR-9".to_string(),
                title: "Release agent".to_string(),
                description: Some("- [x] ships evidence bundle".to_string()),
                priority: None,
                state: "in_progress".to_string(),
                branch_name: None,
                url: None,
                assignee_id: None,
                labels: vec![],
                blocked_by: vec![],
                dispatchable: true,
                created_at: None,
                updated_at: None,
            },
            &[],
            None,
        )
    }

    #[test]
    fn evidence_index_lists_no_bundles_when_none_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let keys = list_bundle_keys(dir.path());
        assert!(keys.is_empty());
    }

    #[test]
    fn save_and_load_bundle_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = test_bundle("issue-1");
        save_bundle(dir.path(), "issue-1", &bundle).unwrap();
        let loaded = load_bundle(dir.path(), "issue-1").unwrap();
        assert_eq!(loaded.cycle_id, "issue-1");
        assert_eq!(list_bundle_keys(dir.path()), vec!["issue-1".to_string()]);
    }

    #[test]
    fn evidence_override_flips_blocked_to_ready_with_risk() {
        let mut bundle = test_bundle("issue-1");
        bundle.security_findings.push(release::SecurityFinding {
            id: "S1".to_string(),
            description: "sql injection".to_string(),
            severity: release::SecuritySeverity::Critical,
            blocking: true,
            override_reason: None,
        });
        assert_eq!(release::compute_verdict(&bundle), Verdict::Blocked);

        for finding in bundle
            .security_findings
            .iter_mut()
            .filter(|f| f.blocking && f.override_reason.is_none())
        {
            finding.override_reason = Some("accepted, ticket SEC-1".to_string());
        }
        assert_eq!(release::compute_verdict(&bundle), Verdict::ReadyWithRisk);
    }

    #[tokio::test]
    async fn evidence_page_shows_not_found_for_an_unknown_key() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let app = router(
            rx,
            dir.path().to_path_buf(),
            test_tracker(dir.path()),
            "",
            false,
            None,
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp = reqwest::get(format!("http://{addr}/evidence/does-not-exist"))
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let body = resp.text().await.unwrap();
        assert!(body.contains("No evidence bundle found"));
    }

    #[tokio::test]
    async fn insights_page_renders_all_five_dimensions_and_a_period_selector() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let app = router(
            rx,
            dir.path().to_path_buf(),
            test_tracker(dir.path()),
            "/base",
            false,
            None,
            None,
        );
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
        let app = router(
            rx,
            dir.path().to_path_buf(),
            test_tracker(dir.path()),
            "",
            false,
            None,
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp = reqwest::get(format!("http://{addr}/metrics"))
            .await
            .unwrap();
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

    #[tokio::test]
    async fn evidence_page_renders_a_persisted_bundle() {
        let dir = tempfile::tempdir().unwrap();
        save_bundle(dir.path(), "issue-1", &test_bundle("issue-1")).unwrap();
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let app = router(
            rx,
            dir.path().to_path_buf(),
            test_tracker(dir.path()),
            "",
            false,
            None,
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp = reqwest::get(format!("http://{addr}/evidence/issue-1"))
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let body = resp.text().await.unwrap();
        assert!(body.contains("READY"));
        assert!(body.contains("ships evidence bundle"));
    }

    /// One test, not two: `SYMPHONY_ADMIN_TOKEN` is a process-global env var, and
    /// cargo runs tests concurrently by default -- two tests each setting it to a
    /// different expected value would race. Exercising "wrong token rejected" then
    /// "correct token accepted" sequentially, inside a single test, sidesteps that
    /// entirely rather than trying to serialize two separate test functions.
    #[tokio::test]
    async fn evidence_override_requires_a_matching_admin_token() {
        // SAFETY: test-only env var, scoped to this test's own token value.
        unsafe {
            std::env::set_var("SYMPHONY_ADMIN_TOKEN", "test-token-air9-override");
        }
        let dir = tempfile::tempdir().unwrap();
        let mut bundle = test_bundle("issue-1");
        bundle.security_findings.push(release::SecurityFinding {
            id: "S1".to_string(),
            description: "sql injection".to_string(),
            severity: release::SecuritySeverity::Critical,
            blocking: true,
            override_reason: None,
        });
        save_bundle(dir.path(), "issue-1", &bundle).unwrap();

        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let app = router(
            rx,
            dir.path().to_path_buf(),
            test_tracker(dir.path()),
            "",
            false,
            None,
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let rejected = client
            .post(format!("http://{addr}/evidence/issue-1/override"))
            .bearer_auth("wrong-token")
            .form(&[("reason", "accepted")])
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), reqwest::StatusCode::FORBIDDEN);
        assert!(
            load_bundle(dir.path(), "issue-1")
                .unwrap()
                .security_findings[0]
                .override_reason
                .is_none(),
            "a rejected request must not modify the bundle"
        );

        let accepted = client
            .post(format!("http://{addr}/evidence/issue-1/override"))
            .bearer_auth("test-token-air9-override")
            .form(&[("reason", "accepted, ticket SEC-1")])
            .send()
            .await
            .unwrap();
        assert!(accepted.status().is_redirection(), "{}", accepted.status());

        let updated = load_bundle(dir.path(), "issue-1").unwrap();
        assert_eq!(
            updated.security_findings[0].override_reason.as_deref(),
            Some("accepted, ticket SEC-1")
        );
        assert_eq!(release::compute_verdict(&updated), Verdict::ReadyWithRisk);

        unsafe {
            std::env::remove_var("SYMPHONY_ADMIN_TOKEN");
        }
    }

    #[test]
    fn observability_row_renders_a_degraded_verdict_with_its_failing_check() {
        let message = serde_json::json!({
            "verdict": "degraded",
            "window_minutes": 30,
            "checks": [{"name": "error_rate", "value": 0.5, "max": 0.01, "passed": false}],
            "reason": "check(s) exceeded threshold: error_rate",
        })
        .to_string();
        let row = event_row_fixture("production_validation", Some(&message));
        let html = observability_row(&row);
        assert!(html.contains("verdict-degraded"));
        assert!(html.contains("error_rate"));
        assert!(html.contains("fail"));
    }

    #[test]
    fn observability_row_renders_unknown_distinctly_from_healthy() {
        let message = serde_json::json!({
            "verdict": "unknown",
            "window_minutes": 30,
            "checks": [],
            "reason": "no data returned for check 'error_rate'",
        })
        .to_string();
        let row = event_row_fixture("production_validation", Some(&message));
        let html = observability_row(&row);
        assert!(html.contains("verdict-unknown"));
        assert!(!html.contains("verdict-healthy"));
    }

    #[test]
    fn observability_row_renders_telemetry_evidence_signals_and_findings() {
        let message = serde_json::json!({
            "signals": [{"kind": "log", "location": "src/lib.rs:1", "snippet": "x", "requirement_id": "R1"}],
            "findings": [{"location": "src/lib.rs:2", "description": "possible secret"}],
        })
        .to_string();
        let row = event_row_fixture("telemetry_evidence", Some(&message));
        let html = observability_row(&row);
        assert!(html.contains("src/lib.rs:1"));
        assert!(html.contains("satisfies R1"));
        assert!(html.contains("possible secret"));
    }

    #[tokio::test]
    async fn observability_page_lists_recorded_evidence_and_verdicts() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(eventlog::DB_FILENAME);
        let tx = eventlog::spawn_writer(db.clone());
        tx.send(eventlog::NewEvent {
            issue_id: "1".to_string(),
            identifier: "AIR-10".to_string(),
            title: "deploy abc123".to_string(),
            session_id: None,
            event_type: "production_validation".to_string(),
            message: Some(r#"{"verdict":"healthy","window_minutes":30,"checks":[],"reason":"all checks within threshold"}"#.to_string()),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
        })
        .unwrap();
        // Give the writer task a moment to persist before the page reads it back.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let (_status_tx, status_rx) = watch::channel(StatusSnapshot::default());
        let app = router(
            status_rx,
            dir.path().to_path_buf(),
            test_tracker(dir.path()),
            "",
            false,
            None,
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp = reqwest::get(format!("http://{addr}/observability"))
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let body = resp.text().await.unwrap();
        assert!(body.contains("verdict-healthy"));
        assert!(!body.contains("Rescan open PRs now"));
    }

    #[tokio::test]
    async fn observability_page_shows_the_rescan_button_only_when_a_repo_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let (_status_tx, status_rx) = watch::channel(StatusSnapshot::default());
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = ObservabilityHandle {
            repo: Some(crate::config::RepoConfig {
                url: "https://github.com/owner/name.git".to_string(),
                default_branch: "main".to_string(),
                ..Default::default()
            }),
            event_tx,
        };
        let app = router(
            status_rx,
            dir.path().to_path_buf(),
            test_tracker(dir.path()),
            "",
            false,
            None,
            Some(handle),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp = reqwest::get(format!("http://{addr}/observability"))
            .await
            .unwrap();
        let body = resp.text().await.unwrap();
        assert!(body.contains("Rescan open PRs now"));
    }

    // ── FEAT-6: artifact_raw_page content-type rendering ──

    fn insert_test_artifact(workflow_dir: &Path, id: &str, content_type: &str, content: &[u8]) {
        let db = workflow_dir.join(eventlog::DB_FILENAME);
        let conn = eventlog::open(&db).unwrap();
        let relative = format!(".symphony/cycles/test-cycle/{id}");
        let full = workflow_dir.join(&relative);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
        conn.execute(
            "INSERT INTO artifacts (id, cycle_id, issue_identifier, stage_id, kind, \
             content_type, path, sha256, created_at, summary) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            rusqlite::params![
                id,
                "test-cycle",
                "test",
                Some("plan"),
                "plan",
                content_type,
                relative,
                "abc123",
                "2026-01-01T00:00:00Z",
                "test summary",
            ],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn artifact_raw_page_renders_json_pretty_printed() {
        let dir = tempfile::tempdir().unwrap();
        insert_test_artifact(
            dir.path(),
            "plan-abc",
            "application/json",
            br#"{"name":"test","nested":{"key":"val"}}"#,
        );
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let state = AppState {
            status_rx: rx,
            workflow_dir: dir.path().to_path_buf(),
            base_path: String::new(),
            chat_enabled: false,
            security: None,
            observability: None,
            tracker: test_tracker(dir.path()),
        };
        let Html(body) = artifact_raw_page(
            State(state),
            AxumPath("plan-abc".to_string()),
            Query(ArtifactRawQuery { raw: None }),
        )
        .await
        .unwrap();
        assert!(
            body.contains("&quot;name&quot;: &quot;test&quot;"),
            "pretty-printed: {body}"
        );
        assert!(
            body.contains("&quot;nested&quot;"),
            "nested present: {body}"
        );
        assert!(body.contains("View raw"), "raw toggle link: {body}");
    }

    #[tokio::test]
    async fn artifact_raw_page_renders_markdown_as_html() {
        let dir = tempfile::tempdir().unwrap();
        insert_test_artifact(
            dir.path(),
            "plan-md",
            "text/markdown",
            b"# Hello\n\nA **bold** paragraph.",
        );
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let state = AppState {
            status_rx: rx,
            workflow_dir: dir.path().to_path_buf(),
            base_path: String::new(),
            chat_enabled: false,
            security: None,
            observability: None,
            tracker: test_tracker(dir.path()),
        };
        let Html(body) = artifact_raw_page(
            State(state),
            AxumPath("plan-md".to_string()),
            Query(ArtifactRawQuery { raw: None }),
        )
        .await
        .unwrap();
        assert!(body.contains("<h1>Hello</h1>"), "md heading: {body}");
        assert!(body.contains("<strong>bold</strong>"), "md bold: {body}");
        assert!(body.contains("post-body"), "post-body class: {body}");
    }

    #[tokio::test]
    async fn artifact_raw_page_falls_back_to_raw_for_unknown_content_type() {
        let dir = tempfile::tempdir().unwrap();
        insert_test_artifact(dir.path(), "plan-txt", "text/plain", b"just plain text");
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let state = AppState {
            status_rx: rx,
            workflow_dir: dir.path().to_path_buf(),
            base_path: String::new(),
            chat_enabled: false,
            security: None,
            observability: None,
            tracker: test_tracker(dir.path()),
        };
        let Html(body) = artifact_raw_page(
            State(state),
            AxumPath("plan-txt".to_string()),
            Query(ArtifactRawQuery { raw: None }),
        )
        .await
        .unwrap();
        assert!(body.contains("<pre class=\"table-wrap\">"), "{body}");
        assert!(body.contains("just plain text"), "{body}");
    }

    #[tokio::test]
    async fn artifact_raw_page_falls_back_to_raw_when_json_is_malformed() {
        let dir = tempfile::tempdir().unwrap();
        insert_test_artifact(
            dir.path(),
            "plan-bad",
            "application/json",
            b"not valid json {{{",
        );
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let state = AppState {
            status_rx: rx,
            workflow_dir: dir.path().to_path_buf(),
            base_path: String::new(),
            chat_enabled: false,
            security: None,
            observability: None,
            tracker: test_tracker(dir.path()),
        };
        let Html(body) = artifact_raw_page(
            State(state),
            AxumPath("plan-bad".to_string()),
            Query(ArtifactRawQuery { raw: None }),
        )
        .await
        .unwrap();
        // Should still render without error, just as raw dump
        assert!(
            body.contains("not valid json"),
            "raw content present: {body}"
        );
        assert!(
            !body.contains("  \""),
            "should not be pretty-printed: {body}"
        );
    }

    #[tokio::test]
    async fn artifact_raw_page_raw_param_shows_original_bytes() {
        let dir = tempfile::tempdir().unwrap();
        insert_test_artifact(
            dir.path(),
            "plan-raw",
            "application/json",
            br#"{"a":1,"b":2}"#,
        );
        let (_tx, rx) = watch::channel(StatusSnapshot::default());
        let state = AppState {
            status_rx: rx,
            workflow_dir: dir.path().to_path_buf(),
            base_path: String::new(),
            chat_enabled: false,
            security: None,
            observability: None,
            tracker: test_tracker(dir.path()),
        };
        let Html(body) = artifact_raw_page(
            State(state),
            AxumPath("plan-raw".to_string()),
            Query(ArtifactRawQuery {
                raw: Some("1".to_string()),
            }),
        )
        .await
        .unwrap();
        assert!(
            body.contains("{&quot;a&quot;:1,&quot;b&quot;:2}"),
            "raw unformatted JSON: {body}"
        );
        assert!(body.contains("View rendered"), "toggle back: {body}");
    }
}
