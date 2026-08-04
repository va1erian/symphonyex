//! MS Teams chat-connector skeleton (compile-checked only under `--features teams`).
//!
//! This is **not a working connector** -- it's the concrete template that proves the
//! `ChatConnector` seam is enough to add a new platform, and the roadmap for the one
//! actually-named next connector. It implements the trait without touching any other
//! module (the worker and store are connector-agnostic by construction), so bringing
//! Teams up is entirely contained here plus two one-line registrations:
//!
//! 1. Add `"teams"` to `config::KNOWN_CHAT_CONNECTORS` so `swebot.chat.connectors:
//!    [web, teams]` validates.
//! 2. Construct a `TeamsChatConnector` in `chat::start`'s connector vec under this
//!    module's compile guard.
//!
//! Then replace the no-op bodies below with real plumbing:
//!
//! - **ingest**: pull `chat` messages for the bot from a Teams webhook *or* the
//!   Microsoft Graph API (`GET /users/{id}/chats/{chatId}/messages`), map each remote
//!   message id onto `store.upsert_remote_user_message(conv, <databaseId>, body)` and
//!   conversations onto `store.get_or_create_remote_conversation("teams", ...)` --
//!   exactly the pattern `super::github` follows.
//! - **deliver**: post assistant replies as Teams messages (`POST
//!   /chats/{chatId}/messages`), using `store.undelivered_assistant_sent("teams")` +
//!   `store.mark_delivered(...)` exactly like `super::github` does for discussion
//!   comments. Read receipts map onto the `read_at`/`read` status columns (delivered
//!   once, no marker scheme needed -- and unlike GitHub, Teams *does* have real read
//!   receipts, which is the one spot the current connector vocabulary has room for).
//!
//! The `ChatConnector` methods returning `Result<(), String>` keep this template
//! dependency-free (no reqwest calls here); the real implementation will add whatever
//! HTTP client and Graph/webhook plumbing it needs inside this module.

#![allow(dead_code)] // a compile-checked template; nothing constructs it yet

use super::connector::ChatConnector;
use super::store::ChatStore;

/// The connector name, and the `conversations.connector` key its rows use.
pub const CONNECTOR_NAME: &str = "teams";

pub struct TeamsChatConnector;

#[async_trait::async_trait]
impl ChatConnector for TeamsChatConnector {
    fn name(&self) -> &str {
        CONNECTOR_NAME
    }

    async fn ingest(&self, store: &ChatStore) -> Result<(), String> {
        // TODO: Graph/webhook pull of new bot chats; for each remote message call
        // store.upsert_remote_user_message(...) -- see super::github::ingest_category.
        let _ = store;
        Ok(())
    }

    async fn deliver(&self, store: &ChatStore) -> Result<(), String> {
        // TODO: store.undelivered_system_notices / undelivered_assistant_sent for
        // CONNECTOR_NAME, post each as a Teams message, then store.mark_delivered --
        // see super::github::deliver_*.
        let _ = store;
        Ok(())
    }
}
