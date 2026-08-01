//! Minimal live status dashboard (subset of the OPTIONAL Section 13.7 HTTP
//! extension): one auto-refreshing HTML page at `/`, no JS, no JSON API, no client
//! polling — just plain server-rendered HTML with `<meta http-equiv="refresh">`. Only
//! enabled when a `--port` is passed on the CLI. Intended for watching dispatch/
//! concurrency behavior during development and test runs, not as a production
//! dashboard.

use axum::Router;
use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use serde::Serialize;
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

/// Bind and serve the dashboard until the process exits. Loopback-only
/// (`127.0.0.1`) unless `bind_all_interfaces` is set, in which case it binds
/// `0.0.0.0` instead.
///
/// The loopback-only default is a deliberate security choice on a bare host (Section
/// 13.7: "no JS, no JSON API" -- not hardened for exposure beyond the local machine).
/// That reasoning flips inside a container, though: a daemonized Symphony (see
/// `crate::daemon`, README.md "Daemonizing Symphony") runs the dashboard inside its
/// *own* container's network namespace, where `127.0.0.1` refers to the container's
/// own loopback interface -- unreachable from the host even with the port published
/// via `docker run -p`, since port publishing forwards to a container's external
/// interface, not its loopback. There's no "other users on the same host" to guard
/// against inside that namespace; the container boundary itself is the isolation
/// mechanism, and reachability is already gated by whether `-p` was passed at all.
pub async fn serve(
    port: u16,
    bind_all_interfaces: bool,
    rx: watch::Receiver<StatusSnapshot>,
) -> anyhow::Result<()> {
    let app = Router::new().route("/", get(dashboard)).with_state(rx);
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

async fn dashboard(State(rx): State<watch::Receiver<StatusSnapshot>>) -> Html<String> {
    let snapshot = rx.borrow().clone();
    Html(render(&snapshot))
}

fn render(s: &StatusSnapshot) -> String {
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
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta http-equiv="refresh" content="1">
<title>Symphony &mdash; {running} running</title>
<style>
  body {{ font-family: system-ui, sans-serif; background: #111; color: #eee; margin: 0; padding: 24px; }}
  h1 {{ font-size: 1.1rem; font-weight: 600; color: #9cf; margin: 0 0 4px; }}
  .meta {{ color: #888; font-size: 0.8rem; margin-bottom: 20px; }}
  .grid {{ display: flex; flex-wrap: wrap; gap: 12px; margin-bottom: 32px; }}
  .card {{ background: #1c1c1c; border: 1px solid #333; border-left: 4px solid #4caf50; border-radius: 6px; padding: 12px 14px; width: 300px; }}
  .card h2 {{ font-size: 0.95rem; margin: 0 0 6px; color: #fff; }}
  .card .row {{ font-size: 0.8rem; color: #aaa; margin: 2px 0; }}
  .card .row b {{ color: #ccc; }}
  .card .msg {{ margin-top: 8px; font-size: 0.78rem; color: #ddd; background: #151515; border-radius: 4px; padding: 6px 8px; max-height: 4.5em; overflow: hidden; }}
  .badge {{ display: inline-block; background: #2d4d2d; color: #9f9; border-radius: 10px; padding: 1px 8px; font-size: 0.72rem; }}
  table {{ border-collapse: collapse; width: 100%; max-width: 900px; }}
  th, td {{ text-align: left; padding: 6px 10px; font-size: 0.82rem; border-bottom: 1px solid #2a2a2a; }}
  th {{ color: #888; font-weight: 500; }}
  .empty {{ color: #666; font-style: italic; }}
  section h3 {{ font-size: 0.85rem; text-transform: uppercase; letter-spacing: 0.04em; color: #888; }}
</style>
</head>
<body>
<h1>Symphony live status</h1>
<div class="meta">generated {generated} &middot; refreshes every 1s</div>

<section>
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
</section>
</body>
</html>
"#,
        running = s.running.len(),
        retrying = s.retrying.len(),
        generated = escape(&s.generated_at),
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

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
