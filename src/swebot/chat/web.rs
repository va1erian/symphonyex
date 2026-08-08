//! The bundled `web` chat connector: a browser chat UI plus the tiny JSON API it
//! talks to, both backed directly by the shared `ChatStore` (no remote platform to
//! poll, so `ingest`/`deliver` are no-ops -- the store *is* the platform here).
//!
//! Server-rendered like `status.rs`'s dashboard, on the same shared `web::page_shell`/
//! `web::STYLE`/`web::nav` shell (so this page matches the dashboard's theme, font
//! sizes, centered layout, and light/dark support instead of carrying its own
//! hand-rolled copy) plus a small `CHAT_STYLE` addendum for the bubble/sidebar/
//! composer look. Same minimal-JS posture as before: one static page, a narrow JSON
//! API (`/threads`, `/send`, `/messages`, `/read`), small inline JS that polls the
//! *active* thread's `/messages` roughly every 1.5s and patches the page in place
//! (see `patchNode`/`renderThreadList` below -- both update existing DOM nodes rather
//! than replacing whole subtrees, so an in-progress text selection elsewhere on the
//! page survives a poll tick). Multiple threads (`conversations` rows with
//! `connector='web'`) can coexist -- the sidebar lists them, a click switches the
//! active one, "+ New chat" creates another; each is independent (its own
//! `has_active_work`/typing state), and the worker answers pending messages across
//! *all* of them concurrently (see `worker::process_cycle`), so one busy thread
//! doesn't stall another's reply. The status vocabulary the UI renders:
//!
//! - user messages: `pending` ("sending…") → `processing` (the typing banner) →
//!   `processed` (✓) | `failed`;
//! - assistant messages: `streaming` (… / in-place text updates as chunks arrive) →
//!   `sent` → `read` (✓✓ read receipt);
//! - system rows: `notice-active` ("still working" banner while a slow turn is
//!   mid-thought) → `notice-done`.
//!
//! Mounted either at `/chat` under the single-project dashboard, or nested under a
//! multi-project service at `/projects/<id>/chat` -- `base_path` makes every URL this
//! page emits (the API `fetch`s and the nav link) come out correctly prefixed either
//! way, mirroring `status::router`.

use super::store::{ChatError, ChatStore, ROLE_USER, STATUS_PENDING};
use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Every web thread's fixed user handle -- a single-operator local surface, so there
/// is no per-thread identity, only per-thread conversation state.
const WEB_USER: &str = "web";

/// A brand new thread's title, before its first message auto-renames it (see `send`).
const NEW_THREAD_TITLE: &str = "New chat";

/// How much of a thread's first message becomes its auto-title (chars, not bytes --
/// truncating mid multi-byte UTF-8 char would panic on the byte-index slice).
const TITLE_MAX_CHARS: usize = 60;

/// Stateless connector registration: the HTTP routes below are the ingest/deliver;
/// there is no remote platform for a poll loop to talk to.
pub struct WebChatConnector;

#[async_trait::async_trait]
impl super::connector::ChatConnector for WebChatConnector {
    fn name(&self) -> &str {
        "web"
    }

    async fn ingest(&self, _store: &ChatStore) -> Result<(), String> {
        Ok(())
    }

    async fn deliver(&self, _store: &ChatStore) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone)]
struct WebState {
    store: ChatStore,
    base_path: Arc<str>,
    /// URL path of the dashboard this chat UI is nested under (base_path minus its
    /// `/chat` suffix): the "Status" nav link points there. `""` at the single-project
    /// root, `/projects/<id>` in the service.
    nav_path: Arc<str>,
}

/// The chat UI + API routes for one chat surface. `base_path` is the URL path this
/// router is served under (`""` at the root, `/chat` from the single-project status
/// server, `/projects/<id>/chat` in the multi-project service); every `fetch` URL and
/// link the page emits is prefixed with it.
pub fn router(store: ChatStore, base_path: impl Into<Arc<str>>) -> Router {
    let base_path = base_path.into();
    let nav_path: Arc<str> = base_path
        .strip_suffix("/chat")
        .unwrap_or("")
        .to_string()
        .into();
    Router::new()
        .route("/", get(index))
        .route("/threads", get(list_threads).post(new_thread))
        .route("/send", post(send))
        .route("/messages", get(messages))
        .route("/read", post(mark_read))
        .with_state(WebState {
            store,
            base_path,
            nav_path,
        })
}

#[derive(Deserialize)]
struct SendForm {
    conv: i64,
    text: String,
}

#[derive(Deserialize)]
struct ReadForm {
    conv: i64,
    ids: Vec<i64>,
}

#[derive(Deserialize)]
struct MessagesQuery {
    conv: i64,
    /// Append-only cursor: return messages newer than this id.
    since: Option<i64>,
    /// When set, also return the newest `recent` messages (for in-place status/read
    /// receipt updates -- ids the client already rendered but whose status moved on).
    recent: Option<usize>,
}

fn internal(e: ChatError) -> (axum::http::StatusCode, String) {
    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// A request-supplied conversation id is only ever valid if it's a real, non-archived
/// `web` conversation -- guards `/send`/`/messages` against a stale id (a thread
/// deleted from under the client) or a copy-pasted id belonging to some other
/// connector (e.g. a GitHub Discussion's conversation row) silently cross-wiring the
/// web UI into a different platform's thread.
async fn require_web_conversation(
    state: &WebState,
    conv: i64,
) -> Result<(), (axum::http::StatusCode, String)> {
    match state.store.conversation(conv).map_err(internal)? {
        Some(c) if c.connector == "web" => Ok(()),
        _ => Err((
            axum::http::StatusCode::NOT_FOUND,
            "unknown chat thread".to_string(),
        )),
    }
}

async fn index(State(state): State<WebState>) -> Html<String> {
    let body = BODY_TEMPLATE;
    let extra_head = format!(
        "<style>{CHAT_STYLE}</style>\n<script>{}</script>",
        CHAT_SCRIPT
            .replace("{base}", &state.base_path)
            .replace("{navbase}", &state.nav_path)
    );
    Html(crate::web::page_shell(
        "Symphony",
        "chat",
        &crate::web::nav(&crate::web::nav_links(true), "/chat", &state.nav_path),
        body,
        &extra_head,
    ))
}

fn thread_json(c: &super::store::ConversationRow) -> Value {
    json!({ "id": c.id, "title": c.title, "updated_at": c.updated_at })
}

async fn list_threads(
    State(state): State<WebState>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let threads = state
        .store
        .list_conversations_by_connector("web")
        .map_err(internal)?;
    Ok(Json(json!({
        "threads": threads.iter().map(thread_json).collect::<Vec<_>>(),
    })))
}

async fn new_thread(
    State(state): State<WebState>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let id = state
        .store
        .create_conversation("web", None, WEB_USER, NEW_THREAD_TITLE)
        .map_err(internal)?;
    Ok(Json(json!({ "id": id, "title": NEW_THREAD_TITLE })))
}

async fn send(
    State(state): State<WebState>,
    Json(form): Json<SendForm>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let text = form.text.trim();
    if text.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "empty message".to_string(),
        ));
    }
    require_web_conversation(&state, form.conv).await?;
    // Auto-title a fresh thread from its first message, so the sidebar shows
    // something more useful than "New chat" for every thread ever created.
    let is_first_message = state
        .store
        .messages_of_conversation(form.conv, 0)
        .map_err(internal)?
        .is_empty();
    if is_first_message {
        let title: String = text.chars().take(TITLE_MAX_CHARS).collect();
        state
            .store
            .set_conversation_title(form.conv, &title)
            .map_err(internal)?;
    }
    let id = state
        .store
        .insert_message(form.conv, ROLE_USER, text, STATUS_PENDING, &json!({}), None)
        .map_err(internal)?;
    Ok(Json(
        json!({ "ok": true, "conversation_id": form.conv, "message_id": id }),
    ))
}

async fn messages(
    State(state): State<WebState>,
    Query(q): Query<MessagesQuery>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    require_web_conversation(&state, q.conv).await?;
    let since = q.since.unwrap_or(0).max(0);
    let appended: Vec<Value> = state
        .store
        .messages_of_conversation(q.conv, since)
        .map_err(internal)?
        .iter()
        .map(message_json)
        .collect();
    let recent: Vec<Value> = match q.recent {
        Some(n) if n > 0 => state
            .store
            .recent_messages(q.conv, n)
            .map_err(internal)?
            .iter()
            .map(message_json)
            .collect(),
        _ => Vec::new(),
    };
    let active = state.store.has_active_work(q.conv).map_err(internal)?;
    Ok(Json(json!({
        "conversation_id": q.conv,
        "active": active,
        "messages": appended,
        "recent": recent,
    })))
}

fn message_json(m: &super::store::MessageRow) -> Value {
    json!({
        "id": m.id,
        "role": m.role,
        "body": m.body,
        "status": m.status,
        "reply_to": m.reply_to,
        "created_at": m.created_at,
        "meta": m.meta,
    })
}

async fn mark_read(
    State(state): State<WebState>,
    Json(form): Json<ReadForm>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    require_web_conversation(&state, form.conv).await?;
    state
        .store
        .mark_read(form.conv, &form.ids)
        .map_err(internal)?;
    Ok(Json(json!({})))
}

/// Layered on top of `web::STYLE`'s shared theme/vars and shared `.msg`/`.bubble`/
/// `.status`/`.transcript` bubble rules (see that module's own doc comment on the
/// "Chat-bubble transcript" block -- the same markup this page's JS produces is also
/// what `status.rs::render_transcript` server-renders for the issue-filtered
/// `/events` view). This addendum is only the sidebar/composer/typing-indicator
/// chrome specific to the interactive chat page. `typing-pulse` is the only
/// animation added here (the message entrance animation lives in the shared
/// `.msg`/`msg-in` rule); both respect `prefers-reduced-motion: reduce`.
const CHAT_STYLE: &str = r#"
  #layout { display: flex; gap: 20px; margin-top: 16px; align-items: flex-start; }
  #sidebar { width: 220px; flex-shrink: 0; }
  #newThreadBtn { width: 100%; background: var(--bg-alt); border: 1px solid var(--border); color: var(--fg); padding: 8px 10px; border-radius: 6px; font-size: 0.85rem; cursor: pointer; margin-bottom: 10px; }
  #newThreadBtn:hover { filter: brightness(1.2); }
  #threadList { display: flex; flex-direction: column; gap: 4px; }
  .thread-item { padding: 8px 10px; border-radius: 6px; font-size: 0.82rem; color: var(--fg-dim); cursor: pointer; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; border: 1px solid transparent; transition: background-color 0.15s ease, border-color 0.15s ease; }
  .thread-item:hover { background: var(--bg-alt); }
  .thread-item.active { background: var(--bg-alt); border-color: var(--border); color: var(--fg-strong); }
  #chatMain { flex: 1; min-width: 0; }
  #typing { color: var(--accent); font-size: 0.8rem; font-style: italic; min-height: 1.2em; margin-top: 14px; }
  #typing.visible { animation: typing-pulse 1.4s ease-in-out infinite; }
  form#chat-form { display: flex; gap: 8px; margin-top: 8px; align-items: flex-end; }
  form#chat-form textarea { flex: 1; background: var(--bg-alt); border: 1px solid var(--border); color: var(--fg); padding: 10px 12px; border-radius: 6px; font-size: 0.9rem; font-family: inherit; resize: none; min-height: 40px; max-height: 160px; line-height: 1.4; }
  form#chat-form textarea:focus-visible { outline: 2px solid var(--focus-ring); outline-offset: 1px; }
  form#chat-form button { background: var(--good-bg); border: 1px solid var(--good-fg); color: var(--good-fg); padding: 10px 18px; border-radius: 6px; font-size: 0.9rem; cursor: pointer; }
  form#chat-form button:hover:not(:disabled) { filter: brightness(1.15); }
  form#chat-form button:disabled { opacity: 0.5; cursor: default; }
  .composer-hint { font-size: 0.72rem; color: var(--fg-dimmer); margin-top: 4px; }
  .empty { color: var(--fg-dimmer); font-style: italic; }
  @keyframes typing-pulse {
    0%, 100% { opacity: 0.55; }
    50% { opacity: 1; }
  }
  @media (prefers-reduced-motion: reduce) {
    #typing.visible { animation: none; }
  }
  /* Same breakpoint web::STYLE's own responsive block uses, so this page tightens up
     at the same viewport width every other page does. The two-column layout (fixed
     220px sidebar + main pane) has no room to breathe under that width -- collapse
     to a single column instead, with the thread list turned into a horizontally
     scrollable row of pills (rather than a tall stack) so it doesn't push the
     composer off the bottom of a phone-height screen. */
  @media (max-width: 640px) {
    #layout { flex-direction: column; gap: 12px; }
    #sidebar { width: 100%; display: flex; align-items: center; gap: 8px; }
    #newThreadBtn { width: auto; margin-bottom: 0; flex-shrink: 0; }
    #threadList { flex-direction: row; overflow-x: auto; gap: 8px; padding-bottom: 2px; }
    .thread-item { flex-shrink: 0; max-width: 60vw; }
    /* Send next to the textarea eats too much of an already-narrow width (the
       placeholder wraps and gets clipped) -- stack it below instead so the textarea
       gets the full row. 16px keeps iOS Safari from auto-zooming the page on focus. */
    form#chat-form { flex-wrap: wrap; }
    form#chat-form textarea { font-size: 16px; flex-basis: 100%; }
    form#chat-form button { flex: 1; }
  }
"#;

/// The page body -- everything below `web::page_shell`'s own `<h1>`/nav. `{base}`/
/// `{navbase}` are not needed here (only the script references them); `index` embeds
/// this verbatim as `page_shell`'s `body` argument.
const BODY_TEMPLATE: &str = r#"<div class="meta">unified Q&amp;A and ticket drafting &middot; live updates every 1.5s, in place</div>
<div id="layout">
  <aside id="sidebar">
    <button id="newThreadBtn" type="button">+ New chat</button>
    <div id="threadList"></div>
  </aside>
  <main id="chatMain">
    <div id="messages" class="transcript"><p class="empty">No messages yet &mdash; ask a question about the repo, or ask SweBot to draft a ticket.</p></div>
    <div id="typing" style="display:none"></div>
    <form id="chat-form">
      <textarea id="text" autocomplete="off" rows="1" placeholder="Ask something, or: draft a ticket for &hellip;"></textarea>
      <button id="sendBtn" type="submit">Send</button>
    </form>
    <div class="composer-hint">Shift+Enter for a newline</div>
  </main>
</div>"#;

/// Embedded into `web::page_shell`'s `extra_head` (inside `<head>`, same spot
/// `PAGE_TEMPLATE`'s inline `<script>` used to live) -- everything below still runs
/// after `DOMContentLoaded`, so its position in `<head>` vs. end-of-`<body>` doesn't
/// matter. `index` substitutes `{base}`/`{navbase}` before embedding.
const CHAT_SCRIPT: &str = r#"
    const BASE = "{base}";
let NAVBASE = "{navbase}";
let lastId = 0;
let currentConv = null;

function esc(s) {
  return (s == null ? "" : String(s))
    .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

// Small, self-contained markdown-to-HTML subset (fenced/inline code, headers, bold,
// italic, links, "- " lists) -- no CDN dependency, matching this page's minimal-JS,
// self-contained posture (see module doc comment). Escapes first, so markdown syntax
// is only ever recognized in already-HTML-safe text; no raw HTML from a message body
// is ever interpreted, even inside a code block.
function md(raw) {
  let s = esc(raw);
  const blocks = [];
  s = s.replace(/```[a-zA-Z0-9_+-]*\n?([\s\S]*?)```/g, (_, code) => {
    blocks.push("<pre><code>" + code + "</code></pre>");
    return "\x00" + (blocks.length - 1) + "\x00";
  });
  s = s.replace(/`([^`\n]+)`/g, "<code>$1</code>");
  s = s.replace(/^### (.*)$/gm, "<h3>$1</h3>");
  s = s.replace(/^## (.*)$/gm, "<h2>$1</h2>");
  s = s.replace(/^# (.*)$/gm, "<h1>$1</h1>");
  s = s.replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
  s = s.replace(/(^|[^*\w])\*([^*\n]+)\*(?!\*)/g, "$1<em>$2</em>");
  s = s.replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
  // Shield the anchors the line above just created before auto-linking bare URLs
  // below, or a `[text](url)` link's own URL (appearing again inside its now-set
  // href="...") would get wrapped a second time, nesting an <a> inside its own href.
  s = s.replace(/<a [^>]*>[^<]*<\/a>/g, (tag) => {
    blocks.push(tag);
    return "\x00" + (blocks.length - 1) + "\x00";
  });
  // Auto-link anything else that looks like a bare URL (no markdown [text](url)
  // wrapper) -- e.g. a link pasted straight into the chat.
  s = s.replace(/https?:\/\/[^\s<>"]+/g, (url) => {
    // Trim common trailing punctuation a sentence would leave stuck to the URL
    // (".", ",", ")", etc.) so "see https://x.com." doesn't link-wrap the period.
    const trailing = url.match(/[.,;:!?)]+$/);
    const clean = trailing ? url.slice(0, -trailing[0].length) : url;
    const rest = trailing ? trailing[0] : "";
    return '<a href="' + clean + '" target="_blank" rel="noopener">' + clean + "</a>" + rest;
  });
  s = s.replace(/(?:^|\n)((?:[-*] .*(?:\n|$))+)/g, (m) => {
    const items = m.trim().split("\n").map((l) => "<li>" + l.replace(/^[-*] /, "") + "</li>").join("");
    return "\n<ul>" + items + "</ul>";
  });
  s = s.replace(/\x00(\d+)\x00/g, (_, i) => blocks[Number(i)]);
  return s;
}

function statusLine(m) {
  if (m.role === "user") {
    if (m.status === "pending") return "sending&hellip;";
    if (m.status === "processing") return "processing&hellip;";
    if (m.status === "processed") return "&check;";
    if (m.status === "failed") return "failed";
    return "";
  }
  if (m.role === "assistant") {
    if (m.status === "streaming") return "&hellip;";
    if (m.status === "read") return '<span class="read">read &check;&check;</span>';
    return "";
  }
  return "";
}

// A resolved "still working"/live-status notice has nothing left to say once the
// real reply has landed -- once its status flips to notice-done, its bubble should
// disappear rather than sit in the transcript forever repeating stale progress text.
function isDoneNotice(m) {
  return m.role === "system" && m.status === "notice-done";
}

// `created_at` is stored/served as RFC3339 (UTC) -- render it in the browser's own
// local time/format rather than shipping a fixed format from the server, so it reads
// naturally regardless of where the browser is. Empty/unparseable falls back to "".
function formatTime(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  if (isNaN(d.getTime())) return "";
  const datePart = d.toLocaleDateString(undefined, {month: "short", day: "numeric"});
  const timePart = d.toLocaleTimeString(undefined, {hour: "numeric", minute: "2-digit"});
  return datePart + ", " + timePart;
}

function render(m) {
  const body = md(m.body) || (m.role === "assistant" && m.status === "streaming" ? "…" : "");
  const status = statusLine(m);
  const time = esc(formatTime(m.created_at));
  return '<div class="msg ' + m.role + '" id="msg-' + m.id + '">' +
    '<div class="bubble">' + body + '</div>' +
    '<div class="status"><span class="time">' + time + '</span>' +
    (status ? ' &middot; ' + status : '') + '</div>' +
    '</div>';
}

// Patches an existing message node's bubble/status text in place instead of
// replacing the whole node (the old `node.outerHTML = render(m)` did that, tearing
// down and recreating the element on every poll tick -- which also collapses any
// text selection the user had started inside it). Only touches the DOM when the
// rendered content actually changed, so a poll tick with nothing new to say for this
// message is a no-op.
function patchNode(m) {
  const node = document.getElementById("msg-" + m.id);
  if (!node) return;
  if (isDoneNotice(m)) {
    node.remove();
    return;
  }
  const bubble = node.querySelector(".bubble");
  const newBody = md(m.body) || (m.role === "assistant" && m.status === "streaming" ? "…" : "");
  if (bubble.innerHTML !== newBody) bubble.innerHTML = newBody;
  const statusEl = node.querySelector(".status");
  const status = statusLine(m);
  const time = esc(formatTime(m.created_at));
  const newStatusHtml = '<span class="time">' + time + '</span>' + (status ? ' &middot; ' + status : '');
  if (statusEl.innerHTML !== newStatusHtml) statusEl.innerHTML = newStatusHtml;
}

async function poll() {
  if (currentConv == null) return;
  let res;
  try {
    res = await fetch(BASE + "/messages?conv=" + currentConv + "&since=" + lastId + "&recent=6");
  } catch (e) {
    return;
  }
  if (!res.ok) return; // e.g. this thread was removed from under us; next loadThreads() recovers
  const data = await res.json();
  const out = document.getElementById("messages");
  const typing = document.getElementById("typing");

  const readIds = [];
  // A message already appended while still `streaming` (the common case for any
  // turn slower than one poll tick) has scrolled past the `since` cursor by the
  // time it actually finishes -- it only ever shows up in `recent` again, not
  // `messages`, so read-marking has to happen here too, not just in the append
  // loop below, or a finished reply never gets marked read at all.
  for (const m of (data.recent || [])) {
    patchNode(m);
    if (m.role === "assistant" && m.status === "sent") {
      readIds.push(m.id);
    }
  }

  for (const m of (data.messages || [])) {
    if (m.id > lastId) {
      lastId = m.id;
      // Already resolved by the time we first see it (a fast turn, or a slow
      // client poll) -- never render it at all, same end state as removing it.
      if (isDoneNotice(m)) continue;
      out.insertAdjacentHTML("beforeend", render(m));
      // Only a *finished* reply is meaningfully "read" -- marking a still-streaming
      // (in-progress, possibly still-empty) message as read flips its status away
      // from "streaming" before any text has arrived, which breaks the "…" typing
      // placeholder render() shows only while status stays "streaming": the bubble
      // then renders as genuinely empty instead of showing that it's still working.
      if (m.role === "assistant" && m.status === "sent") {
        readIds.push(m.id);
      }
    }
  }
  if (data.messages && data.messages.length) {
    const empty = out.querySelector(".empty");
    if (empty) empty.remove();
    const last = out.lastElementChild;
    if (last) last.scrollIntoView({behavior: "smooth", block: "nearest"});
  }
  typing.style.display = data.active ? "block" : "none";
  typing.classList.toggle("visible", !!data.active);
  typing.innerHTML = data.active ? "SweBot is working&hellip;" : "";
  if (readIds.length) {
    fetch(BASE + "/read", {
      method: "POST",
      headers: {"Content-Type": "application/json"},
      body: JSON.stringify({conv: currentConv, ids: readIds})
    });
  }
}

// Diffs against the sidebar's existing DOM nodes by thread id instead of rebuilding
// `#threadList.innerHTML` from scratch on every call (this runs after every send and
// every new-thread action) -- reusing existing nodes means any state anchored to them
// (e.g. an in-progress selection spanning into the sidebar) survives a refresh.
function renderThreadList(threads) {
  const list = document.getElementById("threadList");
  const existing = new Map();
  list.querySelectorAll(".thread-item").forEach((el) => existing.set(Number(el.dataset.id), el));
  const seen = new Set();
  let prevNode = null;
  threads.forEach((t) => {
    seen.add(t.id);
    const isActive = t.id === currentConv;
    let el = existing.get(t.id);
    if (el) {
      if (el.textContent !== t.title) el.textContent = t.title;
    } else {
      el = document.createElement("div");
      el.className = "thread-item";
      el.dataset.id = t.id;
      el.textContent = t.title;
    }
    el.classList.toggle("active", isActive);
    const wantNext = prevNode ? prevNode.nextSibling : list.firstChild;
    if (wantNext !== el) list.insertBefore(el, wantNext);
    prevNode = el;
  });
  existing.forEach((el, id) => {
    if (!seen.has(id)) el.remove();
  });
}

async function loadThreads(selectId) {
  const res = await fetch(BASE + "/threads");
  const data = await res.json();
  let threads = data.threads || [];
  if (threads.length === 0) {
    const created = await fetch(BASE + "/threads", {method: "POST"});
    const t = await created.json();
    threads = [{id: t.id, title: t.title, updated_at: ""}];
  }
  if (selectId != null && threads.some((t) => t.id === selectId)) {
    currentConv = selectId;
  } else if (currentConv == null || !threads.some((t) => t.id === currentConv)) {
    currentConv = threads[0].id;
  }
  renderThreadList(threads);
}

async function selectThread(id) {
  if (id === currentConv) return;
  currentConv = id;
  lastId = 0;
  document.getElementById("messages").innerHTML = "";
  document.querySelectorAll(".thread-item").forEach((el) => {
    el.classList.toggle("active", Number(el.dataset.id) === id);
  });
  await poll();
  document.getElementById("text").focus();
}

async function newThread() {
  const res = await fetch(BASE + "/threads", {method: "POST"});
  const t = await res.json();
  // selectThread() *before* loadThreads(): its "already on this thread" guard
  // compares against currentConv, so switching (and resetting lastId/#messages)
  // must happen while currentConv still points at the *old* thread -- calling
  // loadThreads(t.id) first would set currentConv = t.id early and make
  // selectThread(t.id) a same-thread no-op, silently keeping the old thread's
  // messages and lastId on screen under the new thread's name.
  await selectThread(t.id);
  await loadThreads(t.id);
}

// Grows the composer textarea to fit its content (up to CHAT_STYLE's `max-height`,
// past which it scrolls) instead of a fixed single-line height -- needed now that
// Shift+Enter can put multiple lines into it.
function autoGrow(el) {
  el.style.height = "auto";
  el.style.height = Math.min(el.scrollHeight, 160) + "px";
}

async function sendMessage(ev) {
  ev.preventDefault();
  if (currentConv == null) return;
  const input = document.getElementById("text");
  const text = input.value.trim();
  if (!text) return;
  input.disabled = true;
  const btn = document.getElementById("sendBtn");
  btn.disabled = true;
  try {
    const res = await fetch(BASE + "/send", {
      method: "POST",
      headers: {"Content-Type": "application/json"},
      body: JSON.stringify({conv: currentConv, text: text})
    });
    if (res.ok) {
      input.value = "";
      autoGrow(input);
      await poll();
      // The first message on a thread renames it (server-side) -- refresh the
      // sidebar so the new title shows up without a manual reload.
      await loadThreads(currentConv);
    }
  } finally {
    input.disabled = false;
    btn.disabled = false;
    input.focus();
  }
}

window.addEventListener("DOMContentLoaded", async () => {
  const textEl = document.getElementById("text");
  document.getElementById("chat-form").addEventListener("submit", sendMessage);
  document.getElementById("newThreadBtn").addEventListener("click", newThread);
  document.getElementById("threadList").addEventListener("click", (ev) => {
    const item = ev.target.closest(".thread-item");
    if (item) selectThread(Number(item.dataset.id));
  });
  // Enter sends; Shift+Enter inserts a newline (the textarea's own default for
  // Enter, so only the plain-Enter case needs intercepting).
  textEl.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter" && !ev.shiftKey) {
      ev.preventDefault();
      document.getElementById("chat-form").requestSubmit();
    }
  });
  textEl.addEventListener("input", () => autoGrow(textEl));
  await loadThreads();
  await poll();
  setInterval(poll, 1500);
  textEl.focus();
});
"#;

#[cfg(test)]
mod tests {
    use super::super::store::ROLE_ASSISTANT;
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state() -> (WebState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = ChatStore::open(dir.path().join(super::super::store::DB_FILENAME)).unwrap();
        let state = WebState {
            store,
            base_path: "/chat".into(),
            nav_path: "".into(),
        };
        (state, dir)
    }

    fn router_of(state: &WebState) -> Router {
        router(state.store.clone(), state.base_path.clone())
    }

    async fn body(res: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn index_serves_the_page_with_base_prefix() {
        let (state, _d) = test_state();
        let app = router_of(&state);
        // The router's own routes are root-relative (`/`, `/send`, ...) -- callers
        // nest it under a prefix (e.g. `/chat`); `base_path` is what makes the page's
        // emitted URLs come out prefixed. Serving the router directly never matches a
        // `/chat/...` URI, so tests hit the internal paths.
        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&bytes).to_string();
        assert!(html.contains("Symphony"));
        assert!(html.contains(r#"<a href="/chat" class="active">Chat</a>"#));
        assert!(html.contains(r#"const BASE = "/chat";"#));
        assert!(html.contains("unified Q&amp;A"));
        // CHAT_SCRIPT is only ever run through plain `.replace()`, never `format!` --
        // a stray `{{`/`}}` written as if this were format!-escaped (as it once was)
        // ends up literally in the served <script>, which is invalid JS syntax.
        assert!(!html.contains("{{"));
        assert!(!html.contains("}}"));
    }

    async fn send_json(app: &Router, conv: i64, text: &str) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/send")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"conv": conv, "text": text}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn send_then_poll_round_trips_a_message() {
        let (state, _d) = test_state();
        let conv = state
            .store
            .create_conversation("web", None, WEB_USER, "New chat")
            .unwrap();
        let app = router_of(&state);
        let res = send_json(&app, conv, "draft a ticket for the auth rewrite").await;
        assert_eq!(res.status(), StatusCode::OK);
        let sent = body(res).await;
        assert_eq!(sent["ok"], true);

        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/messages?conv={conv}&since=0"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let msgs = body(res).await;
        assert_eq!(
            msgs["active"], true,
            "a pending user message means work in flight"
        );
        let list = msgs["messages"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["role"], "user");
        assert_eq!(list[0]["body"], "draft a ticket for the auth rewrite");
        assert_eq!(list[0]["status"], "pending");
    }

    #[tokio::test]
    async fn empty_send_is_rejected() {
        let (state, _d) = test_state();
        let conv = state
            .store
            .create_conversation("web", None, WEB_USER, "New chat")
            .unwrap();
        let app = router_of(&state);
        let res = send_json(&app, conv, "   ").await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn send_to_an_unknown_thread_is_rejected() {
        let (state, _d) = test_state();
        let app = router_of(&state);
        let res = send_json(&app, 999_999, "hello").await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn first_message_auto_titles_the_thread() {
        let (state, _d) = test_state();
        let conv = state
            .store
            .create_conversation("web", None, WEB_USER, NEW_THREAD_TITLE)
            .unwrap();
        let app = router_of(&state);
        send_json(&app, conv, "how does the dedup logic work?").await;
        assert_eq!(
            state.store.conversation(conv).unwrap().unwrap().title,
            "how does the dedup logic work?"
        );
        // A second message must not re-title an already-named thread.
        send_json(&app, conv, "and what about the retry path?").await;
        assert_eq!(
            state.store.conversation(conv).unwrap().unwrap().title,
            "how does the dedup logic work?"
        );
    }

    #[tokio::test]
    async fn threads_are_independent_and_listed_newest_first() {
        let (state, _d) = test_state();
        let app = router_of(&state);
        let a = state
            .store
            .create_conversation("web", None, WEB_USER, "thread a")
            .unwrap();
        let b = state
            .store
            .create_conversation("web", None, WEB_USER, "thread b")
            .unwrap();
        send_json(&app, a, "question in thread a").await;

        // Listed newest-updated first: `a` was just touched by the send above.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/threads")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let list = body(res).await;
        let threads = list["threads"].as_array().unwrap();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0]["id"], a);

        // `b` has no messages of its own -- its /messages must stay empty even
        // though `a` now has a pending question (each thread's state is isolated).
        let res = app
            .oneshot(
                Request::builder()
                    .uri(format!("/messages?conv={b}&since=0"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let msgs = body(res).await;
        assert_eq!(msgs["active"], false);
        assert_eq!(msgs["messages"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn post_threads_creates_a_new_thread() {
        let (state, _d) = test_state();
        let app = router_of(&state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/threads")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let created = body(res).await;
        assert_eq!(created["title"], NEW_THREAD_TITLE);
        let id = created["id"].as_i64().unwrap();
        assert!(state.store.conversation(id).unwrap().is_some());
    }

    #[tokio::test]
    async fn read_marks_only_delivered_assistant_messages() {
        let (state, _d) = test_state();
        let conv = state
            .store
            .create_conversation("web", None, WEB_USER, "New chat")
            .unwrap();
        let a = state
            .store
            .insert_message(conv, ROLE_ASSISTANT, "hi", "sent", &json!({}), None)
            .unwrap();
        let u = state
            .store
            .insert_message(conv, ROLE_USER, "hey", "processed", &json!({}), None)
            .unwrap();
        let app = router_of(&state);
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/read")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"conv":{conv},"ids":[{a},{u}]}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let msgs = state.store.messages_of_conversation(conv, 0).unwrap();
        let assistant = msgs.iter().find(|m| m.id == a).unwrap();
        assert_eq!(assistant.status, "read");
        let user = msgs.iter().find(|m| m.id == u).unwrap();
        assert_eq!(user.status, "processed");
    }
}
