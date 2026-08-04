//! The bundled `web` chat connector: a browser chat UI plus the tiny JSON API it
//! talks to, both backed directly by the shared `ChatStore` (no remote platform to
//! poll, so `ingest`/`deliver` are no-ops -- the store *is* the platform here).
//!
//! Server-rendered like `status.rs`'s dashboard, with the same minimal-JS posture:
//! one static page, a narrow JSON API (`/send`, `/messages`, `/read`), small inline
//! JS that polls `/messages` roughly every 1.5s and renders into the page in place.
//! The status vocabulary the UI renders:
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

/// The web conversation's fixed user handle -- a single-operator local surface, so
/// one default conversation is enough (see `store::get_or_create_web_conversation`).
const WEB_USER: &str = "web";

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
    text: String,
}

#[derive(Deserialize)]
struct ReadForm {
    ids: Vec<i64>,
}

#[derive(Deserialize)]
struct MessagesQuery {
    /// Append-only cursor: return messages newer than this id.
    since: Option<i64>,
    /// When set, also return the newest `recent` messages (for in-place status/read
    /// receipt updates -- ids the client already rendered but whose status moved on).
    recent: Option<usize>,
}

fn internal(e: ChatError) -> (axum::http::StatusCode, String) {
    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

async fn index(State(state): State<WebState>) -> Html<String> {
    let page = PAGE_TEMPLATE
        .replace("{STYLE}", STYLE)
        .replace("{base}", &state.base_path)
        .replace("{navbase}", &state.nav_path);
    Html(page)
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
    let conv = state
        .store
        .get_or_create_web_conversation(WEB_USER)
        .map_err(internal)?;
    let id = state
        .store
        .insert_message(conv, ROLE_USER, text, STATUS_PENDING, &json!({}), None)
        .map_err(internal)?;
    Ok(Json(
        json!({ "ok": true, "conversation_id": conv, "message_id": id }),
    ))
}

async fn messages(
    State(state): State<WebState>,
    Query(q): Query<MessagesQuery>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let conv = state
        .store
        .get_or_create_web_conversation(WEB_USER)
        .map_err(internal)?;
    let since = q.since.unwrap_or(0).max(0);
    let appended: Vec<Value> = state
        .store
        .messages_of_conversation(conv, since)
        .map_err(internal)?
        .iter()
        .map(message_json)
        .collect();
    let recent: Vec<Value> = match q.recent {
        Some(n) if n > 0 => state
            .store
            .recent_messages(conv, n)
            .map_err(internal)?
            .iter()
            .map(message_json)
            .collect(),
        _ => Vec::new(),
    };
    let active = state.store.has_active_work(conv).map_err(internal)?;
    Ok(Json(json!({
        "conversation_id": conv,
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
    state.store.mark_read(&form.ids).map_err(internal)?;
    Ok(Json(json!({})))
}

const STYLE: &str = r#"
  body { font-family: system-ui, sans-serif; background: #111; color: #eee; margin: 0; padding: 24px; max-width: 860px; margin: 0 auto; }
  h1 { font-size: 1.1rem; font-weight: 600; color: #9cf; margin: 0 0 4px; }
  .meta { color: #888; font-size: 0.8rem; margin-bottom: 12px; }
  nav a { color: #9cf; text-decoration: none; margin-right: 16px; font-size: 0.85rem; }
  nav a:hover { text-decoration: underline; }
  #messages { display: flex; flex-direction: column; gap: 10px; margin-top: 16px; }
  .msg { max-width: 78%; }
  .msg.user { align-self: flex-end; }
  .msg.assistant { align-self: flex-start; }
  .msg.system { align-self: center; max-width: 100%; }
  .bubble { border-radius: 10px; padding: 8px 12px; font-size: 0.9rem; line-height: 1.4; white-space: pre-wrap; word-break: break-word; }
  .msg.user .bubble { background: #1c3a5f; border: 1px solid #2a5a8f; }
  .msg.assistant .bubble { background: #242424; border: 1px solid #383838; }
  .msg.system .bubble { color: #aaa; background: #181818; font-style: italic; border: 1px dashed #333; }
  .status { font-size: 0.72rem; color: #777; margin-top: 3px; }
  .status .read { color: #8c8; }
  #typing { color: #9cf; font-size: 0.8rem; font-style: italic; min-height: 1.2em; margin-top: 14px; }
  form#chat-form { display: flex; gap: 8px; margin-top: 8px; }
  form#chat-form input { flex: 1; background: #1c1c1c; border: 1px solid #333; color: #eee; padding: 10px 12px; border-radius: 6px; font-size: 0.9rem; }
  form#chat-form button { background: #2d4d2d; border: 1px solid #3a6a3a; color: #9f9; padding: 10px 18px; border-radius: 6px; font-size: 0.9rem; cursor: pointer; }
  form#chat-form button:disabled { opacity: 0.5; cursor: default; }
  .empty { color: #666; font-style: italic; }
"#;

const PAGE_TEMPLATE: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>Symphony &mdash; SweBot chat</title>
<style>{STYLE}</style>
<script>
    const BASE = "{base}";
let NAVBASE = "{navbase}";
let lastId = 0;

function esc(s) {{
  return (s == null ? "" : String(s))
    .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}}

function statusLine(m) {{
  if (m.role === "user") {{
    if (m.status === "pending") return "sending&hellip;";
    if (m.status === "processing") return "processing&hellip;";
    if (m.status === "processed") return "&check;";
    if (m.status === "failed") return "failed";
    return "";
  }}
  if (m.role === "assistant") {{
    if (m.status === "streaming") return "&hellip;";
    if (m.status === "read") return '<span class="read">read &check;&check;</span>';
    return "";
  }}
  return "";
}}

function render(m) {{
  const body = esc(m.body) || (m.role === "assistant" && m.status === "streaming" ? "…" : "");
  return '<div class="msg ' + m.role + '" id="msg-' + m.id + '">' +
    '<div class="bubble">' + body + '</div>' +
    '<div class="status">' + statusLine(m) + '</div>' +
    '</div>';
}}

function updateNode(m) {{
  const node = document.getElementById("msg-" + m.id);
  if (node) node.outerHTML = render(m);
}}

async function poll() {{
  let res;
  try {{
    res = await fetch(BASE + "/messages?since=" + lastId + "&recent=6");
  }} catch (e) {{
    return;
  }}
  const data = await res.json();
  const out = document.getElementById("messages");
  const typing = document.getElementById("typing");

  for (const m of (data.recent || [])) {{ updateNode(m); }}

  const readIds = [];
  for (const m of (data.messages || [])) {{
    if (m.id > lastId) {{
      out.insertAdjacentHTML("beforeend", render(m));
      lastId = m.id;
      if (m.role === "assistant" && (m.status === "sent" || m.status === "streaming")) {{
        readIds.push(m.id);
      }}
    }}
  }}
  if (data.messages && data.messages.length) {{
    const last = out.lastElementChild;
    if (last) last.scrollIntoView({{behavior: "smooth", block: "nearest"}});
  }}
  typing.style.display = data.active ? "block" : "none";
  typing.innerHTML = data.active ? "SweBot is working&hellip;" : "";
  if (readIds.length) {{
    fetch(BASE + "/read", {{
      method: "POST",
      headers: {{"Content-Type": "application/json"}},
      body: JSON.stringify({{ids: readIds}})
    }});
  }}
}}

async function sendMessage(ev) {{
  ev.preventDefault();
  const input = document.getElementById("text");
  const text = input.value.trim();
  if (!text) return;
  input.disabled = true;
  const btn = document.getElementById("sendBtn");
  btn.disabled = true;
  try {{
    const res = await fetch(BASE + "/send", {{
      method: "POST",
      headers: {{"Content-Type": "application/json"}},
      body: JSON.stringify({{text: text}})
    }});
    if (res.ok) {{
      input.value = "";
      lastId = 0;
      const out = document.getElementById("messages");
      out.innerHTML = "";
      await poll();
    }}
  }} finally {{
    input.disabled = false;
    btn.disabled = false;
    input.focus();
  }}
}}

window.addEventListener("DOMContentLoaded", () => {{
  document.getElementById("chat-form").addEventListener("submit", sendMessage);
  poll();
  setInterval(poll, 1500);
  document.getElementById("text").focus();
}});
</script>
</head>
<body>
<h1>SweBot chat</h1>
<nav><a href="{navbase}/">Status</a></nav>
<div class="meta">unified Q&amp;A and ticket drafting &middot; live updates every 1.5s, in place</div>
<div id="messages"><p class="empty">No messages yet &mdash; ask a question about the repo, or ask SweBot to draft a ticket.</p></div>
<div id="typing" style="display:none"></div>
<form id="chat-form">
  <input id="text" autocomplete="off" placeholder="Ask something, or: draft a ticket for &hellip;">
  <button id="sendBtn" type="submit">Send</button>
</form>
</body>
</html>
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
        assert!(html.contains("SweBot chat"));
        assert!(html.contains(r#"const BASE = "/chat";"#));
        assert!(html.contains("unified Q&amp;A"));
    }

    #[tokio::test]
    async fn send_then_poll_round_trips_a_message() {
        let (state, _d) = test_state();
        let app = router_of(&state);
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/send")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"text":"draft a ticket for the auth rewrite"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let sent = body(res).await;
        assert_eq!(sent["ok"], true);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/messages?since=0")
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
        let app = router_of(&state);
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/send")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"text":"   "}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn read_marks_only_delivered_assistant_messages() {
        let (state, _d) = test_state();
        let conv = state
            .store
            .get_or_create_web_conversation(WEB_USER)
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
                    .body(Body::from(format!(r#"{{"ids":[{a},{u}]}}"#)))
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
