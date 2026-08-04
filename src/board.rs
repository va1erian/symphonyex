//! A minimal, self-hosted bulletin board -- an alternative to GitLab's label-filtered
//! Issues (`repo_host::gitlab`'s own module doc comment) as SweBot's Q&A/drafting
//! conversational surface, for a project that would rather not mix that traffic into
//! its real Issues list at all. Two fixed boards, matching SweBot's two capabilities:
//! `"Q&A"` and `"Ideas"` (the same default category/label names GitHub/GitLab
//! configs already use -- `swebot.qa.discussion_category`/`swebot.drafting.label`
//! etc., reused here as plain board names rather than inventing a third naming
//! scheme).
//!
//! Deliberately small: a thread is a title + markdown body + a flat list of
//! markdown-rendered replies, rendered with basic server-rendered HTML/CSS (no JS,
//! no client framework) -- loosely mirroring GitHub Discussions' own board/thread
//! shape, not attempting to reproduce it. No accounts: a poster types a free-text
//! name per post, same trust posture as `status.rs`'s dashboard (README.md "Trust and
//! safety posture" -- not hardened for exposure beyond a trusted network; mounted
//! under the same optional `--port`/`symphony serve` surface as everything else in
//! `status.rs`).
//!
//! Storage: `rusqlite`, connection-opened-per-call, no pool -- same "keep it basic"
//! posture as `registry.rs`/`eventlog.rs`'s own read paths (this is genuinely
//! low-traffic: a handful of threads/replies, not a hot dispatch path).
//!
//! `BulletinBoardHost` (bottom of this file) implements `repo_host::DiscussionHost`
//! against this storage, so `swebot::qa`/`swebot::drafting` can poll it exactly the
//! way they poll a real `RepoHost`'s Discussions/labels -- see
//! `config::SwebotConfig::board_enabled`'s doc comment and `swebot::mod`'s
//! `ConversationHost` for how a project opts into this instead of the underlying
//! `repo.provider`'s own Q&A/drafting surface.

use crate::repo_host::{DiscussionComment, DiscussionHost, DiscussionThread};
use crate::web::{self, escape, urlencode};
use async_trait::async_trait;
use axum::Router;
use axum::extract::{Form, Path as AxumPath, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use rusqlite::Connection;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DB_FILENAME: &str = "board.db";

/// The only two boards a project ever needs -- one per SweBot capability. A human
/// posting a new thread picks one explicitly; SweBot polls each by this exact name
/// (`swebot.qa.discussion_category`/`swebot.drafting.discussion_category`, reused as
/// board names -- see this module's own doc comment).
const BOARDS: &[&str] = &["Q&A", "Ideas"];

#[derive(Debug, Clone)]
pub struct ThreadRow {
    pub id: i64,
    pub category: String,
    pub title: String,
    pub body: String,
    pub author: String,
    pub closed: bool,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct PostRow {
    pub id: i64,
    pub author: String,
    pub body: String,
    pub created_at: String,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS threads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    category TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    author TEXT NOT NULL,
    closed INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS posts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id INTEGER NOT NULL REFERENCES threads(id),
    author TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_posts_thread ON posts(thread_id);
";

pub fn open(data_dir: &Path) -> anyhow::Result<Connection> {
    std::fs::create_dir_all(data_dir)?;
    let conn = Connection::open(data_dir.join(DB_FILENAME))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

pub fn list_threads(
    conn: &Connection,
    category: &str,
    include_closed: bool,
) -> rusqlite::Result<Vec<ThreadRow>> {
    let sql = if include_closed {
        "SELECT id, category, title, body, author, closed, created_at FROM threads \
         WHERE category = ?1 ORDER BY created_at DESC"
    } else {
        "SELECT id, category, title, body, author, closed, created_at FROM threads \
         WHERE category = ?1 AND closed = 0 ORDER BY created_at DESC"
    };
    let mut stmt = conn.prepare(sql)?;
    stmt.query_map([category], row_to_thread)?.collect()
}

pub fn get_thread(conn: &Connection, id: i64) -> rusqlite::Result<Option<ThreadRow>> {
    conn.query_row(
        "SELECT id, category, title, body, author, closed, created_at FROM threads WHERE id = ?1",
        [id],
        row_to_thread,
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        e => Err(e),
    })
}

pub fn list_posts(conn: &Connection, thread_id: i64) -> rusqlite::Result<Vec<PostRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, author, body, created_at FROM posts WHERE thread_id = ?1 ORDER BY id ASC",
    )?;
    stmt.query_map([thread_id], |r| {
        Ok(PostRow {
            id: r.get(0)?,
            author: r.get(1)?,
            body: r.get(2)?,
            created_at: r.get(3)?,
        })
    })?
    .collect()
}

pub fn create_thread(
    conn: &Connection,
    category: &str,
    title: &str,
    body: &str,
    author: &str,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO threads (category, title, body, author, closed, created_at) \
         VALUES (?1, ?2, ?3, ?4, 0, ?5)",
        rusqlite::params![
            category,
            title,
            body,
            author,
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn add_post(
    conn: &Connection,
    thread_id: i64,
    author: &str,
    body: &str,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO posts (thread_id, author, body, created_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![thread_id, author, body, chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn mark_thread_closed(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE threads SET closed = 1 WHERE id = ?1", [id])?;
    Ok(())
}

fn row_to_thread(r: &rusqlite::Row) -> rusqlite::Result<ThreadRow> {
    Ok(ThreadRow {
        id: r.get(0)?,
        category: r.get(1)?,
        title: r.get(2)?,
        body: r.get(3)?,
        author: r.get(4)?,
        closed: r.get::<_, i64>(5)? != 0,
        created_at: r.get(6)?,
    })
}

/// Render `src` (user-submitted markdown) to HTML. Two deliberate restrictions
/// beyond plain CommonMark, since this board's input is untrusted (posted
/// anonymously via a plain web form, no auth):
/// - Raw HTML (block or inline -- part of the CommonMark spec itself) is downgraded
///   to literal text instead of passed through, which `pulldown_cmark::html::push_html`
///   escapes like any other text -- closes a direct stored-XSS hole (`<script>...`)
///   while every standard markdown construct (bold, lists, code, tables, ...) still
///   works.
/// - Link/image destinations are allow-listed to `http(s)://`, `mailto:`, `/`
///   (relative), and `#` (in-page) schemes -- `javascript:`/`data:` URLs in markdown
///   link syntax are a real XSS vector pulldown-cmark doesn't filter on its own.
pub fn render_markdown(src: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, html};

    let parser = Parser::new_ext(src, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES);
    let safe_events = parser.map(|event| match event {
        Event::Html(s) | Event::InlineHtml(s) => Event::Text(s),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) if !is_safe_url(&dest_url) => Event::Start(Tag::Link {
            link_type,
            dest_url: "".into(),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) if !is_safe_url(&dest_url) => Event::Start(Tag::Image {
            link_type,
            dest_url: "".into(),
            title,
            id,
        }),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, safe_events);
    out
}

fn is_safe_url(url: &str) -> bool {
    let lower = url.trim().to_lowercase();
    lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("mailto:")
        || lower.starts_with('#')
        || lower.starts_with('/')
}

// --------------------------------------------------------------------------------
// repo_host::DiscussionHost -- lets swebot::qa/swebot::drafting poll this board
// exactly the way they poll a real RepoHost's Discussions/labels.
// --------------------------------------------------------------------------------

/// `DiscussionHost` implementor backed by this module's own SQLite storage.
/// `base_path` is this router's own mount point (e.g. `/board`, or `""` at the web
/// root) -- used only to build a human-readable `DiscussionThread::url` for logs and
/// drafted-ticket comments, not consulted for any storage operation.
#[derive(Debug, Clone)]
pub struct BulletinBoardHost {
    data_dir: PathBuf,
    base_path: String,
}

impl BulletinBoardHost {
    pub fn new(data_dir: PathBuf, base_path: impl Into<String>) -> Self {
        Self {
            data_dir,
            base_path: base_path.into(),
        }
    }

    fn conn(&self) -> Result<Connection, String> {
        open(&self.data_dir).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl DiscussionHost for BulletinBoardHost {
    /// `selector` is a board name (`"Q&A"`/`"Ideas"`, or whatever
    /// `swebot.qa.discussion_category`/`swebot.drafting.discussion_category` were
    /// overridden to -- this board has no fixed vocabulary of its own beyond what
    /// gets posted into it).
    async fn list_swebot_threads(&self, selector: &str) -> Result<Vec<DiscussionThread>, String> {
        let conn = self.conn()?;
        let threads = list_threads(&conn, selector, false).map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity(threads.len());
        for t in threads {
            let posts = list_posts(&conn, t.id).map_err(|e| e.to_string())?;
            out.push(DiscussionThread {
                id: t.id.to_string(),
                number: t.id as u64,
                title: t.title,
                body: t.body,
                url: format!("{}/threads/{}", self.base_path, t.id),
                comments: posts
                    .into_iter()
                    .map(|p| DiscussionComment {
                        database_id: p.id as u64,
                        body: p.body,
                        author_login: Some(p.author),
                    })
                    .collect(),
            });
        }
        Ok(out)
    }

    async fn post_discussion_comment(&self, thread_id: &str, body: &str) -> Result<String, String> {
        let id: i64 = thread_id
            .parse()
            .map_err(|_| format!("invalid thread id '{thread_id}'"))?;
        let conn = self.conn()?;
        let post_id = add_post(&conn, id, "swebot", body).map_err(|e| e.to_string())?;
        Ok(post_id.to_string())
    }

    // mark_discussion_comment_as_answer: default no-op (this board has no such
    // concept, same as GitLab -- see repo_host::mod's trait default doc comment).

    async fn close_thread(&self, thread_id: &str) -> Result<(), String> {
        let id: i64 = thread_id
            .parse()
            .map_err(|_| format!("invalid thread id '{thread_id}'"))?;
        let conn = self.conn()?;
        mark_thread_closed(&conn, id).map_err(|e| e.to_string())
    }
}

// --------------------------------------------------------------------------------
// HTTP: board (list) view, thread (detail) view, new-thread and reply forms.
// --------------------------------------------------------------------------------

#[derive(Clone)]
struct BoardState {
    data_dir: PathBuf,
    /// Absolute URL path this router is mounted under (e.g. `/board`, or
    /// `/projects/<id>/board` when nested into the multi-project service's own
    /// router) -- every link/form-action/redirect this module emits is prefixed
    /// with this, same convention and same reason as `status.rs`'s own `base_path`:
    /// relative links break the moment this router isn't served at the site root,
    /// and axum's own `nest()` doesn't rewrite emitted HTML for you.
    base_path: String,
}

/// Routes only, no bind/serve -- mounted under `status.rs`'s own router at
/// `base_path` (e.g. `.nest("/board", board::router(data_dir, "/board"))`), same
/// pattern `status::router` itself uses for the pieces it hands to `service.rs`'s
/// per-project nesting.
pub fn router(data_dir: PathBuf, base_path: &str) -> Router {
    let state = BoardState {
        data_dir,
        base_path: base_path.to_string(),
    };
    Router::new()
        .route("/", get(board_list))
        .route("/new", get(new_thread_form))
        .route("/threads", post(create_thread_submit))
        .route("/threads/{id}", get(thread_view))
        .route("/threads/{id}/replies", post(reply_submit))
        .with_state(state)
}

/// The board's own routes (`/`, `/new`, `/threads/:id`) are nested under `status.rs`'s
/// router at `<parent>/board` (see `router()` below); `parent_nav` re-derives that
/// parent base so board pages carry the same Status/Events/Usage/Board nav every other
/// page has, instead of being an island with no way back except the browser's Back
/// button.
fn parent_nav(base_path: &str, active: &str) -> String {
    let parent = base_path.strip_suffix("/board").unwrap_or(base_path);
    web::nav(status_nav_links(), active, parent)
}

fn status_nav_links() -> &'static [web::NavLink<'static>] {
    &[
        web::NavLink {
            href: "/",
            label: "Status",
        },
        web::NavLink {
            href: "/events",
            label: "Events",
        },
        web::NavLink {
            href: "/usage",
            label: "Usage",
        },
        web::NavLink {
            href: "/board",
            label: "Board",
        },
    ]
}

fn page_shell(title: &str, base_path: &str, body: &str) -> Html<String> {
    Html(web::page_shell(
        "Symphony Board",
        title,
        &parent_nav(base_path, "/board"),
        body,
        "",
    ))
}

fn tabs(base: &str, active: &str) -> String {
    let links: String = BOARDS
        .iter()
        .map(|b| {
            let class = if *b == active {
                " class=\"active\""
            } else {
                ""
            };
            format!(
                r#"<a href="{base}?category={}"{class}>{}</a>"#,
                urlencode(b),
                escape(b)
            )
        })
        .collect();
    format!(r#"<div class="tabs">{links}</div>"#)
}

#[derive(Deserialize, Default)]
struct BoardQuery {
    category: Option<String>,
}

async fn board_list(State(state): State<BoardState>, Query(q): Query<BoardQuery>) -> Response {
    let base = state.base_path.as_str();
    let active = q
        .category
        .as_deref()
        .filter(|c| BOARDS.contains(c))
        .unwrap_or(BOARDS[0]);
    let conn = match open(&state.data_dir) {
        Ok(c) => c,
        Err(e) => return error_response(&e.to_string(), base),
    };
    let threads = list_threads(&conn, active, true).unwrap_or_default();

    let rows: String = if threads.is_empty() {
        r#"<div class="empty">No threads yet in this board.</div>"#.to_string()
    } else {
        threads
            .iter()
            .map(|t| {
                let post_count = list_posts(&conn, t.id).map(|p| p.len()).unwrap_or(0);
                let closed_badge = if t.closed {
                    r#"<span class="badge closed">closed</span>"#
                } else {
                    ""
                };
                format!(
                    r#"<div class="thread-row">
  <div>
    <a href="{base}/threads/{id}">{title}</a>{closed_badge}
    <div class="sub">by {author} &middot; {created}</div>
  </div>
  <div class="count">{count}</div>
</div>"#,
                    base = base,
                    id = t.id,
                    title = escape(&t.title),
                    closed_badge = closed_badge,
                    author = escape(&t.author),
                    created = escape(&t.created_at),
                    count = post_count,
                )
            })
            .collect()
    };

    let body = format!(
        r#"{tabs}
<div class="thread-list">{rows}</div>
<p><a href="{base}/new?category={cat}">+ new thread in {cat_esc}</a></p>"#,
        tabs = tabs(base, active),
        rows = rows,
        base = base,
        cat = urlencode(active),
        cat_esc = escape(active),
    );
    page_shell(active, base, &body).into_response()
}

async fn new_thread_form(State(state): State<BoardState>, Query(q): Query<BoardQuery>) -> Response {
    let base = state.base_path.as_str();
    let active = q
        .category
        .as_deref()
        .filter(|c| BOARDS.contains(c))
        .unwrap_or(BOARDS[0]);
    new_thread_form_response(base, active, "", "", "", None)
}

/// Shared by the plain GET form (empty fields, no error) and `create_thread_submit`'s
/// error path (fields pre-filled with what the poster already typed, plus an error
/// banner) -- a failed submission used to just redirect to a blank form, silently
/// discarding a half-written post.
fn new_thread_form_response(
    base: &str,
    category: &str,
    author: &str,
    title: &str,
    body_text: &str,
    error: Option<&str>,
) -> Response {
    let active = if BOARDS.contains(&category) {
        category
    } else {
        BOARDS[0]
    };
    let options: String = BOARDS
        .iter()
        .map(|b| {
            let selected = if *b == active { " selected" } else { "" };
            format!(
                r#"<option value="{}"{selected}>{}</option>"#,
                escape(b),
                escape(b)
            )
        })
        .collect();
    let error_html = error
        .map(|e| web::error_banner(&format!("Couldn't post: {}", escape(e))))
        .unwrap_or_default();
    let body = format!(
        r#"<p><a href="{base}?category={cat}">&larr; back to board</a></p>
<h2>New thread</h2>
{error_html}
<form class="compose" method="post" action="{base}/threads">
  <label for="nt-category">Board</label>
  <select id="nt-category" name="category">{options}</select>
  <label for="nt-author">Your name</label>
  <input type="text" id="nt-author" name="author" placeholder="anonymous" maxlength="80" value="{author}">
  <label for="nt-title">Title</label>
  <input type="text" id="nt-title" name="title" required maxlength="200" value="{title}">
  <label for="nt-body">Body (markdown supported)</label>
  <textarea id="nt-body" name="body" required>{body_text}</textarea>
  <button type="submit">Post</button>
</form>"#,
        base = base,
        cat = urlencode(active),
        options = options,
        error_html = error_html,
        author = escape(author),
        title = escape(title),
        body_text = escape(body_text),
    );
    page_shell("new thread", base, &body).into_response()
}

#[derive(Deserialize)]
struct NewThreadForm {
    category: String,
    #[serde(default)]
    author: String,
    title: String,
    body: String,
}

async fn create_thread_submit(
    State(state): State<BoardState>,
    Form(form): Form<NewThreadForm>,
) -> Response {
    let base = state.base_path.as_str();
    if !BOARDS.contains(&form.category.as_str()) {
        return error_response("unknown board", base);
    }
    let conn = match open(&state.data_dir) {
        Ok(c) => c,
        Err(e) => return error_response(&e.to_string(), base),
    };
    let author = if form.author.trim().is_empty() {
        "anonymous".to_string()
    } else {
        form.author.trim().to_string()
    };
    match create_thread(
        &conn,
        &form.category,
        form.title.trim(),
        &form.body,
        &author,
    ) {
        Ok(id) => Redirect::to(&format!("{}/threads/{id}", state.base_path)).into_response(),
        Err(e) => {
            // Re-render the compose form with the operator's input intact instead of
            // just an error + "try again" link -- losing a half-written post/reply to
            // a transient DB error is exactly the kind of thing that makes people stop
            // trusting a form.
            new_thread_form_response(
                base,
                &form.category,
                &form.author,
                &form.title,
                &form.body,
                Some(&e.to_string()),
            )
        }
    }
}

async fn thread_view(State(state): State<BoardState>, AxumPath(id): AxumPath<i64>) -> Response {
    render_thread_page(&state, id, "", "", None).await
}

/// Shared by the plain GET thread view and `reply_submit`'s error path -- same
/// "don't discard what the poster already typed" reasoning as
/// `new_thread_form_response` above.
async fn render_thread_page(
    state: &BoardState,
    id: i64,
    reply_author: &str,
    reply_body: &str,
    error: Option<&str>,
) -> Response {
    let base = state.base_path.as_str();
    let conn = match open(&state.data_dir) {
        Ok(c) => c,
        Err(e) => return error_response(&e.to_string(), base),
    };
    let Ok(Some(thread)) = get_thread(&conn, id) else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            page_shell("not found", base, "<p>No such thread.</p>"),
        )
            .into_response();
    };
    let posts = list_posts(&conn, id).unwrap_or_default();

    let opening = format!(
        r#"<div class="post">
  <div class="byline"><b>{author}</b> &middot; {created}</div>
  <div class="post-body">{body}</div>
</div>"#,
        author = escape(&thread.author),
        created = escape(&thread.created_at),
        body = render_markdown(&thread.body),
    );
    let replies: String = posts
        .iter()
        .map(|p| {
            format!(
                r#"<div class="post">
  <div class="byline"><b>{author}</b> &middot; {created}</div>
  <div class="post-body">{body}</div>
</div>"#,
                author = escape(&p.author),
                created = escape(&p.created_at),
                body = render_markdown(&p.body),
            )
        })
        .collect();

    let closed_badge = if thread.closed {
        r#"<span class="badge closed">closed</span>"#
    } else {
        ""
    };
    let error_html = error
        .map(|e| web::error_banner(&format!("Couldn't post reply: {}", escape(e))))
        .unwrap_or_default();
    let reply_form = if thread.closed {
        r#"<p class="empty">This thread is closed.</p>"#.to_string()
    } else {
        format!(
            r#"{error_html}
<form class="compose" method="post" action="{base}/threads/{id}/replies">
  <label for="r-author">Your name</label>
  <input type="text" id="r-author" name="author" placeholder="anonymous" maxlength="80" value="{reply_author}">
  <label for="r-body">Reply (markdown supported)</label>
  <textarea id="r-body" name="body" required>{reply_body}</textarea>
  <button type="submit">Reply</button>
</form>"#,
            base = base,
            id = id,
            error_html = error_html,
            reply_author = escape(reply_author),
            reply_body = escape(reply_body),
        )
    };

    let body = format!(
        r#"<p><a href="{base}?category={cat}">&larr; back to {cat_esc}</a></p>
<h2>{title}{closed_badge}</h2>
{opening}
{replies}
{reply_form}"#,
        base = base,
        cat = urlencode(&thread.category),
        cat_esc = escape(&thread.category),
        title = escape(&thread.title),
        closed_badge = closed_badge,
        opening = opening,
        replies = replies,
        reply_form = reply_form,
    );
    page_shell(&thread.title, base, &body).into_response()
}

#[derive(Deserialize)]
struct ReplyForm {
    #[serde(default)]
    author: String,
    body: String,
}

async fn reply_submit(
    State(state): State<BoardState>,
    AxumPath(id): AxumPath<i64>,
    Form(form): Form<ReplyForm>,
) -> Response {
    let conn = match open(&state.data_dir) {
        Ok(c) => c,
        Err(e) => return error_response(&e.to_string(), &state.base_path),
    };
    let author = if form.author.trim().is_empty() {
        "anonymous".to_string()
    } else {
        form.author.trim().to_string()
    };
    match add_post(&conn, id, &author, &form.body) {
        Ok(_) => Redirect::to(&format!("{}/threads/{id}", state.base_path)).into_response(),
        Err(e) => {
            render_thread_page(&state, id, &form.author, &form.body, Some(&e.to_string())).await
        }
    }
}

fn error_response(msg: &str, base: &str) -> Response {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        page_shell("error", base, &web::error_banner(&escape(msg))),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn create_thread_and_list_roundtrips() {
        let dir = tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        let id =
            create_thread(&conn, "Q&A", "How does X work?", "please explain", "alice").unwrap();
        let threads = list_threads(&conn, "Q&A", false).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, id);
        assert_eq!(threads[0].title, "How does X work?");
        assert!(!threads[0].closed);
    }

    #[test]
    fn closed_threads_excluded_unless_include_closed() {
        let dir = tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        let id = create_thread(&conn, "Ideas", "an idea", "body", "bob").unwrap();
        mark_thread_closed(&conn, id).unwrap();
        assert!(list_threads(&conn, "Ideas", false).unwrap().is_empty());
        assert_eq!(list_threads(&conn, "Ideas", true).unwrap().len(), 1);
    }

    #[test]
    fn add_post_and_list_posts_roundtrips() {
        let dir = tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        let thread_id = create_thread(&conn, "Q&A", "t", "b", "alice").unwrap();
        add_post(&conn, thread_id, "bob", "an answer").unwrap();
        add_post(&conn, thread_id, "swebot", "a second answer").unwrap();
        let posts = list_posts(&conn, thread_id).unwrap();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].author, "bob");
        assert_eq!(posts[1].author, "swebot");
    }

    #[test]
    fn render_markdown_supports_basic_formatting() {
        let html = render_markdown("**bold** and a [link](https://example.com)");
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains(r#"href="https://example.com""#));
    }

    /// Regression-shaped test for a real class of bug: raw HTML embedded in markdown
    /// input (a `<script>` block, the classic stored-XSS payload) must never reach
    /// the rendered output verbatim -- this board accepts posts from anyone, no auth.
    #[test]
    fn render_markdown_neutralizes_raw_html() {
        let html = render_markdown("hello <script>alert(1)</script> world");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn render_markdown_strips_javascript_scheme_links() {
        let html = render_markdown("[click me](javascript:alert(1))");
        assert!(!html.contains("javascript:"));
    }

    #[test]
    fn render_markdown_allows_relative_and_https_links() {
        assert!(render_markdown("[a](/foo)").contains(r#"href="/foo""#));
        assert!(render_markdown("[a](https://x.example)").contains(r#"href="https://x.example""#));
    }

    #[tokio::test]
    async fn bulletin_board_host_list_threads_includes_comments() {
        let dir = tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        let thread_id = create_thread(&conn, "Q&A", "How does auth work?", "", "alice").unwrap();
        add_post(&conn, thread_id, "alice", "how does auth work?").unwrap();
        drop(conn);

        let host = BulletinBoardHost::new(dir.path().to_path_buf(), "/board".to_string());
        let threads = host.list_swebot_threads("Q&A").await.unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].comments.len(), 1);
        assert_eq!(
            threads[0].comments[0].author_login.as_deref(),
            Some("alice")
        );
        assert_eq!(threads[0].url, format!("/board/threads/{thread_id}"));
    }

    #[tokio::test]
    async fn bulletin_board_host_post_discussion_comment_and_close_thread() {
        let dir = tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        let thread_id = create_thread(&conn, "Ideas", "an idea", "body", "bob").unwrap();
        drop(conn);

        let host = BulletinBoardHost::new(dir.path().to_path_buf(), "/board".to_string());
        let post_id = host
            .post_discussion_comment(&thread_id.to_string(), "swebot's reply")
            .await
            .unwrap();
        assert!(post_id.parse::<i64>().is_ok());

        host.close_thread(&thread_id.to_string()).await.unwrap();
        let threads = host.list_swebot_threads("Ideas").await.unwrap();
        assert!(
            threads.is_empty(),
            "closed thread should no longer be polled"
        );
    }
}
