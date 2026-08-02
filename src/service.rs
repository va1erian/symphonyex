//! `symphony serve`: the long-running, multi-repo web service (see AGENTS.md
//! "Long-running multi-repo service"). One process manages N repos registered
//! through a browser -- each gets its own headless `orchestrator::run_managed` task
//! (tracker polling, SweBot Q&A/drafting/review, all unchanged from the single-repo
//! path) plus a nested dashboard reusing `status::router` unmodified.
//!
//! State model mirrors `orchestrator.rs`'s own stated philosophy (`orchestrator.rs`'s
//! module doc comment): one task (whichever handler currently holds `running`'s lock)
//! owns the map of what's running; everything else is either a fresh, short-lived
//! SQLite connection (`registry.rs`, same "no pool, no long-lived shared connection"
//! posture as `eventlog.rs`) or a message (a `oneshot::Sender<()>` fired to stop a
//! project's task).
//!
//! Never accepts or stores a literal GitHub token -- only an env var *name*
//! (`token_env`), resolved to a value solely at the point of an outbound GitHub call
//! (`repo_host::fetch_file`). This matches `config::RepoConfig::token_env`'s
//! convention exactly; see that type's doc comment for why.

use crate::orchestrator;
use crate::registry;
use crate::repo_host;
use crate::status;
use axum::Router;
use axum::extract::{Form, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot, watch};
use tower::ServiceExt;

struct RunningProject {
    shutdown_tx: Option<oneshot::Sender<()>>,
    status_rx: watch::Receiver<status::StatusSnapshot>,
    db_path: PathBuf,
}

#[derive(Clone)]
struct ServiceState {
    data_dir: PathBuf,
    running: Arc<Mutex<HashMap<String, RunningProject>>>,
}

/// Entry point for `symphony serve --port <port> [--data-dir <dir>]` (`main.rs`).
/// Requires `SYMPHONY_ADMIN_TOKEN` -- unlike the single-project dashboard
/// (`status.rs`), this UI can register/remove repos, so it refuses to start
/// unprotected rather than defaulting to open.
pub async fn run(port: u16, data_dir: PathBuf) -> anyhow::Result<()> {
    if std::env::var("SYMPHONY_ADMIN_TOKEN")
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        anyhow::bail!(
            "symphony serve requires SYMPHONY_ADMIN_TOKEN to be set -- this web UI can \
             register and remove repos, so it needs an access gate before it's safe to \
             run as a long-lived, network-reachable service (see AGENTS.md \"Long-running \
             multi-repo service\")"
        );
    }

    std::fs::create_dir_all(&data_dir)?;
    let state = ServiceState {
        data_dir,
        running: Arc::new(Mutex::new(HashMap::new())),
    };

    let existing = {
        let conn = registry::open(&state.data_dir)?;
        registry::list_active(&conn)?
    };
    for row in existing {
        if let Err(e) = start_from_github(&state, &row).await {
            tracing::error!(error = %e, project = %row.id, "failed to start project on startup (left registered; fix and restart to retry)");
        }
    }

    spawn_refresh_task(state.clone());

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("symphony serve listening on http://0.0.0.0:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Every ~5 minutes, re-fetch each active project's `WORKFLOW.md` from GitHub and
/// overwrite the local materialized copy if it changed. Deliberately does *not*
/// duplicate `orchestrator.rs`'s own hot-reload logic: `maybe_reload`
/// (`orchestrator.rs`) already mtime-watches this same local file every poll tick --
/// this task's only job is to be the thing that changes it, from the GitHub side.
fn spawn_refresh_task(state: ServiceState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        interval.tick().await; // first tick fires immediately; startup already fetched fresh
        loop {
            interval.tick().await;
            let rows = match registry::open(&state.data_dir) {
                Ok(conn) => registry::list_active(&conn).unwrap_or_default(),
                Err(e) => {
                    tracing::warn!(error = %e, "periodic refresh: failed to open registry (ignored)");
                    continue;
                }
            };
            for row in rows {
                if let Err(e) = refresh_one(&state, &row).await {
                    tracing::warn!(error = %e, project = %row.id, "periodic WORKFLOW.md refresh failed (ignored)");
                }
            }
        }
    });
}

async fn refresh_one(state: &ServiceState, row: &registry::ProjectRow) -> anyhow::Result<()> {
    let (owner, repo) = repo_host::parse_github_owner_repo(&row.repo_url)
        .ok_or_else(|| anyhow::anyhow!("'{}' is not a github.com URL", row.repo_url))?;
    let content = repo_host::fetch_file(
        &owner,
        &repo,
        &row.default_branch,
        &row.workflow_path,
        row.token_env.as_deref(),
    )
    .await?;
    let workflow_path = registry::project_dir(&state.data_dir, &row.id).join("WORKFLOW.md");
    let existing = tokio::fs::read_to_string(&workflow_path)
        .await
        .unwrap_or_default();
    if existing != content {
        tokio::fs::write(&workflow_path, &content).await?;
        tracing::info!(project = %row.id, "WORKFLOW.md changed on GitHub; refreshed local copy");
    }
    Ok(())
}

/// Fetch `row`'s `WORKFLOW.md` fresh from GitHub, materialize it locally, and spawn
/// its orchestrator task. Shared by startup (every active row) and `do_register`
/// (one new row).
async fn start_from_github(state: &ServiceState, row: &registry::ProjectRow) -> anyhow::Result<()> {
    let (owner, repo) = repo_host::parse_github_owner_repo(&row.repo_url)
        .ok_or_else(|| anyhow::anyhow!("'{}' is not a github.com URL", row.repo_url))?;
    let content = repo_host::fetch_file(
        &owner,
        &repo,
        &row.default_branch,
        &row.workflow_path,
        row.token_env.as_deref(),
    )
    .await?;
    let dir = registry::project_dir(&state.data_dir, &row.id);
    tokio::fs::create_dir_all(&dir).await?;
    let workflow_path = dir.join("WORKFLOW.md");
    tokio::fs::write(&workflow_path, &content).await?;
    spawn_project(state, row.id.clone(), workflow_path).await
}

async fn spawn_project(
    state: &ServiceState,
    id: String,
    workflow_path: PathBuf,
) -> anyhow::Result<()> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (handles_tx, handles_rx) = oneshot::channel();
    let task_id = id.clone();
    tokio::spawn(async move {
        if let Err(e) =
            orchestrator::run_managed(workflow_path, None, shutdown_rx, handles_tx).await
        {
            tracing::error!(error = %e, project = %task_id, "project orchestrator exited with error");
        }
    });
    let handles = handles_rx.await.map_err(|_| {
        anyhow::anyhow!("project failed to start -- see logs for the underlying error")
    })?;
    let mut running = state.running.lock().await;
    running.insert(
        id,
        RunningProject {
            shutdown_tx: Some(shutdown_tx),
            status_rx: handles.status_rx,
            db_path: handles.db_path,
        },
    );
    Ok(())
}

async fn do_register(
    state: &ServiceState,
    repo_url: &str,
    branch: &str,
    workflow_path: &str,
    token_env: Option<String>,
) -> anyhow::Result<String> {
    let (owner, repo) = repo_host::parse_github_owner_repo(repo_url)
        .ok_or_else(|| anyhow::anyhow!("'{repo_url}' is not a github.com URL"))?;
    let id = registry::slug(&owner, &repo);
    let row = registry::ProjectRow {
        id: id.clone(),
        repo_url: repo_url.to_string(),
        default_branch: branch.to_string(),
        workflow_path: workflow_path.to_string(),
        token_env,
        status: "active".to_string(),
        added_at: chrono::Utc::now().to_rfc3339(),
    };
    // Fetch before persisting: a bad URL/branch/path/token should fail the
    // registration outright rather than leave a row nothing is running for.
    start_from_github(state, &row).await?;
    let conn = registry::open(&state.data_dir)?;
    registry::insert(&conn, &row)?;
    Ok(id)
}

async fn do_remove(state: &ServiceState, id: &str) -> anyhow::Result<()> {
    {
        let mut running = state.running.lock().await;
        if let Some(mut project) = running.remove(id)
            && let Some(tx) = project.shutdown_tx.take()
        {
            let _ = tx.send(());
        }
    }
    let conn = registry::open(&state.data_dir)?;
    registry::mark_removed(&conn, id)?;
    Ok(())
}

// --------------------------------------------------------------------------------
// Web UI
// --------------------------------------------------------------------------------

fn build_router(state: ServiceState) -> Router {
    let admin_routes = Router::new()
        .route("/register", get(register_form).post(register_submit))
        .route("/projects/{id}/remove", post(remove_project))
        .route_layer(middleware::from_fn(require_admin));

    Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login_form).post(login_submit))
        .route("/projects/{id}", any(project_proxy))
        .route("/projects/{id}/", any(project_proxy))
        .route("/projects/{id}/{*rest}", any(project_proxy))
        .merge(admin_routes)
        .with_state(state)
}

const HEAD: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Symphony</title><style>
body { font-family: system-ui, sans-serif; background: #111; color: #eee; margin: 0; padding: 24px; max-width: 900px; }
h1 { font-size: 1.1rem; font-weight: 600; color: #9cf; margin: 0 0 4px; }
.meta { color: #888; font-size: 0.85rem; margin-bottom: 16px; }
table { border-collapse: collapse; width: 100%; }
th, td { text-align: left; padding: 6px 10px; border-bottom: 1px solid #333; font-size: 0.9rem; }
a { color: #9cf; }
label { display: block; font-size: 0.85rem; color: #ccc; margin-top: 10px; }
input { background: #1a1a1a; color: #eee; border: 1px solid #333; padding: 5px 7px; margin-top: 4px; width: 320px; max-width: 100%; }
button { background: #2a2a2a; color: #eee; border: 1px solid #444; padding: 5px 12px; cursor: pointer; margin-top: 12px; }
button:hover { background: #333; }
.empty { color: #666; }
.status-running { color: #8c8; }
.status-starting { color: #cc8; }
</style></head><body>"#;
const TAIL: &str = "</body></html>";

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn dashboard(State(state): State<ServiceState>) -> Html<String> {
    let rows = match registry::open(&state.data_dir).and_then(|c| Ok(registry::list_active(&c)?)) {
        Ok(rows) => rows,
        Err(e) => {
            return Html(format!(
                "{HEAD}<h1>Symphony</h1><p>registry error: {}</p>{TAIL}",
                escape(&e.to_string())
            ));
        }
    };
    let running = state.running.lock().await;
    let project_rows: String = if rows.is_empty() {
        "<tr><td colspan=\"4\" class=\"empty\">No repos registered yet.</td></tr>".to_string()
    } else {
        rows.iter()
            .map(|r| project_row(r, running.contains_key(&r.id)))
            .collect()
    };
    drop(running);

    Html(format!(
        "{HEAD}<h1>Symphony</h1>\
         <p class=\"meta\">Multi-repo service &mdash; <a href=\"/register\">register a repo</a></p>\
         <table><tr><th>Repo</th><th>Branch</th><th>Status</th><th></th></tr>{project_rows}</table>\
         {TAIL}"
    ))
}

fn project_row(p: &registry::ProjectRow, is_running: bool) -> String {
    let (status_class, status_text) = if is_running {
        ("status-running", "running")
    } else {
        ("status-starting", "starting / failed to start")
    };
    format!(
        "<tr><td><a href=\"/projects/{id}/\">{repo}</a></td><td>{branch}</td>\
         <td class=\"{status_class}\">{status_text}</td>\
         <td><form method=\"post\" action=\"/projects/{id}/remove\">\
         <button type=\"submit\">Remove</button></form></td></tr>",
        id = escape(&p.id),
        repo = escape(&p.repo_url),
        branch = escape(&p.default_branch),
    )
}

async fn login_form() -> Html<String> {
    Html(format!(
        "{HEAD}<h1>Symphony</h1>\
         <form method=\"post\" action=\"/login\">\
         <label>Admin token <input type=\"password\" name=\"token\" autofocus></label>\
         <br><button type=\"submit\">Log in</button>\
         </form>{TAIL}"
    ))
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
}

async fn login_submit(Form(form): Form<LoginForm>) -> Response {
    let expected = std::env::var("SYMPHONY_ADMIN_TOKEN").unwrap_or_default();
    if !expected.is_empty() && form.token == expected {
        let mut resp = Redirect::to("/").into_response();
        let cookie = format!(
            "symphony_admin={}; HttpOnly; Path=/; SameSite=Strict",
            form.token
        );
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            resp.headers_mut().insert(header::SET_COOKIE, v);
        }
        resp
    } else {
        Html(format!(
            "{HEAD}<p>Invalid token.</p><p><a href=\"/login\">Try again</a></p>{TAIL}"
        ))
        .into_response()
    }
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

/// Gates `/register` and `/projects/:id/remove` only -- the read-only dashboard and
/// nested per-project status pages stay open, same posture `status.rs` already has
/// for its own (single-project) dashboard.
async fn require_admin(headers: HeaderMap, req: Request, next: Next) -> Response {
    let expected = std::env::var("SYMPHONY_ADMIN_TOKEN").unwrap_or_default();
    let provided = extract_admin_token(&headers).unwrap_or_default();
    if !expected.is_empty() && provided == expected {
        next.run(req).await
    } else {
        Redirect::to("/login").into_response()
    }
}

async fn register_form() -> Html<String> {
    Html(format!(
        "{HEAD}<h1>Register a repo</h1>\
         <form method=\"post\" action=\"/register\">\
         <label>Repo URL <input name=\"repo_url\" placeholder=\"https://github.com/owner/repo\" required></label>\
         <label>Branch <input name=\"branch\" value=\"main\"></label>\
         <label>WORKFLOW.md path in repo <input name=\"workflow_path\" value=\"WORKFLOW.md\"></label>\
         <label>Token env var (optional -- for private repos; must already be set in this \
         service's own environment, never a literal token) <input name=\"token_env\" placeholder=\"GITHUB_TOKEN\"></label>\
         <br><button type=\"submit\">Register</button>\
         </form><p class=\"meta\"><a href=\"/\">&larr; back</a></p>{TAIL}"
    ))
}

#[derive(Deserialize)]
struct RegisterForm {
    repo_url: String,
    branch: String,
    workflow_path: String,
    token_env: Option<String>,
}

async fn register_submit(
    State(state): State<ServiceState>,
    Form(form): Form<RegisterForm>,
) -> Response {
    let branch = if form.branch.trim().is_empty() {
        "main".to_string()
    } else {
        form.branch.trim().to_string()
    };
    let workflow_path = if form.workflow_path.trim().is_empty() {
        "WORKFLOW.md".to_string()
    } else {
        form.workflow_path.trim().to_string()
    };
    let token_env = form.token_env.filter(|s| !s.trim().is_empty());
    match do_register(
        &state,
        form.repo_url.trim(),
        &branch,
        &workflow_path,
        token_env,
    )
    .await
    {
        Ok(_id) => Redirect::to("/").into_response(),
        Err(e) => Html(format!(
            "{HEAD}<p>Failed to register: {}</p><p><a href=\"/register\">Try again</a></p>{TAIL}",
            escape(&e.to_string())
        ))
        .into_response(),
    }
}

async fn remove_project(State(state): State<ServiceState>, Path(id): Path<String>) -> Response {
    match do_remove(&state, &id).await {
        Ok(()) => Redirect::to("/").into_response(),
        Err(e) => Html(format!(
            "{HEAD}<p>Failed to remove '{}': {}</p>{TAIL}",
            escape(&id),
            escape(&e.to_string())
        ))
        .into_response(),
    }
}

/// Reuses `status::router` (dashboard/fragment/events/usage) for one project's own
/// pages, unmodified -- the router has no idea it's being served under
/// `/projects/<id>/...` here rather than at the root the way the single-project CLI
/// path (`status::serve`) mounts it, so the `/projects/<id>` prefix is stripped from
/// the request's URI before handing it off, and `status::router`'s own `base_path`
/// makes it emit `/projects/<id>/...`-prefixed links so in-page navigation stays
/// nested. `Router` implements `tower::Service` with `Infallible` as its error type,
/// so `.oneshot(..).await` can only ever `Ok(..)` here.
///
/// Mounted at three route patterns with differing numbers of captured path params
/// (`/projects/{id}`, `/projects/{id}/`, `/projects/{id}/{*rest}`) -- `Path<String>`
/// requires exactly one captured param and errors on the `{*rest}` route's two, so
/// this extracts into a `HashMap` instead, which tolerates however many are present.
async fn project_proxy(
    State(state): State<ServiceState>,
    Path(params): Path<HashMap<String, String>>,
    mut req: Request,
) -> Response {
    let id = match params.get("id") {
        Some(id) => id.clone(),
        None => return (StatusCode::NOT_FOUND, "missing project id").into_response(),
    };
    let (status_rx, db_path) = {
        let running = state.running.lock().await;
        match running.get(&id) {
            Some(p) => (p.status_rx.clone(), p.db_path.clone()),
            None => return (StatusCode::NOT_FOUND, "unknown or removed project").into_response(),
        }
    };

    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let prefix = format!("/projects/{id}");
    let rest = path_and_query.strip_prefix(&prefix).unwrap_or("/");
    let rest = if rest.is_empty() { "/" } else { rest };
    if let Ok(new_uri) = rest.parse() {
        *req.uri_mut() = new_uri;
    }

    let sub_router = status::router(status_rx, db_path, &prefix);
    sub_router
        .oneshot(req)
        .await
        .expect("Router's Service::Error is Infallible")
}
