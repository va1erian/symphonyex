//! SQLite-backed persistence for SweBot chat mode: conversations and messages with
//! per-message status tracking -- the live "being processed" indicator and read
//! receipts the web UI renders -- plus the drafting `meta` the worker stores so a
//! later "create it" reply can file an already-prepared ticket.
//!
//! Lives next to `eventlog.rs`'s `symphony.db` as its own `symphony-chat.db`: separate
//! schema, separate concern (interactive conversation state vs. dispatch history) --
//! and the same "no pool, no long-lived shared connection" posture as `eventlog.rs`/
//! `registry.rs`: every method opens a short-lived WAL connection, does its tiny
//! amount of work, and closes it. Chat traffic is a handful of messages per minute
//! split between the worker task and the web server's handlers, so opening a
//! connection per op is not just fine, it sidesteps any shared-connection locking
//! between the two.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Default filename, joined onto `workflow_dir` -- same directory convention as
/// `eventlog::DB_FILENAME`, so it lands next to `symphony.db`.
pub const DB_FILENAME: &str = "symphony-chat.db";

/// Message `role` values.
pub const ROLE_USER: &str = "user";
pub const ROLE_ASSISTANT: &str = "assistant";
pub const ROLE_SYSTEM: &str = "system";

/// User-message statuses.
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_PROCESSING: &str = "processing";
pub const STATUS_PROCESSED: &str = "processed";
pub const STATUS_FAILED: &str = "failed";
/// Assistant-message statuses.
pub const STATUS_STREAMING: &str = "streaming";
pub const STATUS_SENT: &str = "sent";
pub const STATUS_READ: &str = "read";
/// System-row statuses: an in-flight "still working" notice vs. one already resolved.
pub const STATUS_NOTICE_ACTIVE: &str = "notice-active";
pub const STATUS_NOTICE_DONE: &str = "notice-done";
/// A final "this failed" notice: deliverable to non-web connectors (GitHub), but
/// *not* counted by `has_active_work` the way an in-flight `notice-active` row is,
/// so the web UI's typing banner doesn't hang on a failure.
pub const STATUS_NOTICE_ERROR: &str = "notice-error";

#[derive(Debug, Error)]
pub enum ChatError {
    #[error("chat store: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("chat store: {0}")]
    Msg(String),
}

impl From<String> for ChatError {
    fn from(s: String) -> Self {
        ChatError::Msg(s)
    }
}

/// Lets connector `ingest`/`deliver` bodies flow store errors out with `?` (the
/// `ChatConnector` methods return `Result<(), String>`, mirroring `repo_host`'s
/// loose error strings).
impl From<ChatError> for String {
    fn from(e: ChatError) -> Self {
        e.to_string()
    }
}

/// A conversation as stored. The multi-conversation API below
/// (`create_conversation`/`find_conversation_by_remote`/`get_or_create_remote_conversation`/
/// `conversation`/`list_conversations`) is exercised by the GitHub-Discussions connector
/// (Phase D); the bundled web connector only ever touches the singular default one.
#[allow(dead_code)] // fields read by the GitHub-Discussions connector (Phase D)
#[derive(Debug, Clone)]
pub struct ConversationRow {
    pub id: i64,
    pub connector: String,
    pub remote_id: Option<String>,
    pub user_handle: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One message with its status and drafting metadata already parsed out of the
/// stored JSON blob (bad meta JSON degrades to `{}` rather than failing the read).
#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: i64,
    pub conversation_id: i64,
    pub role: String,
    pub body: String,
    pub status: String,
    pub reply_to: Option<i64>,
    #[allow(dead_code)] // surfaced by the status vocabulary (read receipts)
    pub read_at: Option<String>,
    pub created_at: String,
    pub meta: Value,
    /// Connector-local id of the platform artifact this message maps to -- a GitHub
    /// comment `databaseId` (or `"0"` for a discussion's opening body), or the GraphQL
    /// node id of the comment a SweBot reply was posted as. `None` means "not backed
    /// by a remote artifact yet" (a streamed-but-undelivered reply, or a
    /// locally-enqueued web message). Used as the idempotency key so a connector
    /// poll never enqueues/delivers the same artifact twice. Text, not integer,
    /// because delivered replies are keyed by GitHub's base64 node id.
    pub remote_message_id: Option<String>,
}

/// Cheap clone handle over a single SQLite file path -- every method opens its own
/// short-lived connection (see the module doc comment).
#[derive(Debug, Clone)]
pub struct ChatStore {
    path: PathBuf,
}

impl ChatStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ChatError> {
        let path = path.into();
        ensure_schema(&path)?;
        // A crash (or an aborted process) between "claim a user message" and
        // "answer it" would otherwise leave it stuck in `processing` forever.
        reset_stale_processing(&path)?;
        Ok(Self { path })
    }

    fn conn(&self) -> Result<Connection, ChatError> {
        let c = Connection::open(&self.path)?;
        c.pragma_update(None, "journal_mode", "WAL")?;
        c.pragma_update(None, "foreign_keys", "ON")?;
        // The worker's ingest/process loop and its separately-spawned delivery loop
        // (worker::run_loop) both write to this file concurrently. WAL still
        // serializes writers, so a lock conflict without a busy_timeout returns
        // SQLITE_BUSY immediately instead of waiting a moment for the other loop's
        // short-lived connection to finish -- avoid the spurious failure/retry.
        c.pragma_update(None, "busy_timeout", 5_000)?;
        Ok(c)
    }

    // ---------------------------------------------------------------------------
    // Conversations
    // ---------------------------------------------------------------------------

    #[allow(dead_code)] // exercised by the GitHub-Discussions connector (Phase D)
    pub fn create_conversation(
        &self,
        connector: &str,
        remote_id: Option<&str>,
        user_handle: &str,
        title: &str,
    ) -> Result<i64, ChatError> {
        let c = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();
        c.execute(
            "INSERT INTO conversations (connector, remote_id, user_handle, title, created_at, \
             updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![connector, remote_id, user_handle, title, now],
        )?;
        Ok(c.last_insert_rowid())
    }

    #[allow(dead_code)] // exercised by the GitHub-Discussions connector (Phase D)
    pub fn find_conversation_by_remote(
        &self,
        connector: &str,
        remote_id: &str,
    ) -> Result<Option<ConversationRow>, ChatError> {
        let c = self.conn()?;
        Ok(c.query_row(
            "SELECT id, connector, remote_id, user_handle, title, created_at, updated_at \
             FROM conversations WHERE connector=?1 AND remote_id=?2",
            params![connector, remote_id],
            row_to_conversation,
        )
        .optional()?)
    }

    #[allow(dead_code)] // exercised by the GitHub-Discussions connector (Phase D)
    pub fn get_or_create_remote_conversation(
        &self,
        connector: &str,
        remote_id: &str,
        user_handle: &str,
        title: &str,
    ) -> Result<i64, ChatError> {
        if let Some(id) = self
            .find_conversation_by_remote(connector, remote_id)?
            .map(|c| c.id)
        {
            return Ok(id);
        }
        self.create_conversation(connector, Some(remote_id), user_handle, title)
    }

    /// The web connector's default conversation for a user handle: the earliest web
    /// one, created on first use.
    pub fn get_or_create_web_conversation(&self, user_handle: &str) -> Result<i64, ChatError> {
        let c = self.conn()?;
        if let Some(id) = c
            .query_row(
                "SELECT id FROM conversations WHERE connector='web' AND user_handle=?1 \
                 ORDER BY id LIMIT 1",
                params![user_handle],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            return Ok(id);
        }
        let now = chrono::Utc::now().to_rfc3339();
        c.execute(
            "INSERT INTO conversations (connector, remote_id, user_handle, title, created_at, \
             updated_at) VALUES ('web', NULL, ?1, ?2, ?3, ?3)",
            params![
                user_handle,
                format!("Chat with SweBot ({user_handle})"),
                now
            ],
        )?;
        Ok(c.last_insert_rowid())
    }

    #[allow(dead_code)] // exercised by the GitHub-Discussions connector (Phase D)
    pub fn conversation(&self, id: i64) -> Result<Option<ConversationRow>, ChatError> {
        let c = self.conn()?;
        Ok(c.query_row(
            "SELECT id, connector, remote_id, user_handle, title, created_at, updated_at \
             FROM conversations WHERE id=?1",
            params![id],
            row_to_conversation,
        )
        .optional()?)
    }

    #[allow(dead_code)] // exercised by the GitHub-Discussions connector (Phase D)
    pub fn list_conversations(&self) -> Result<Vec<ConversationRow>, ChatError> {
        let c = self.conn()?;
        let mut stmt = c.prepare(
            "SELECT id, connector, remote_id, user_handle, title, created_at, updated_at \
             FROM conversations WHERE archived=0 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_conversation)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn touch(&self, c: &Connection, conversation_id: i64) -> Result<(), ChatError> {
        c.execute(
            "UPDATE conversations SET updated_at=?1 WHERE id=?2",
            params![chrono::Utc::now().to_rfc3339(), conversation_id],
        )?;
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Messages
    // ---------------------------------------------------------------------------

    pub fn insert_message(
        &self,
        conversation_id: i64,
        role: &str,
        body: &str,
        status: &str,
        meta: &Value,
        reply_to: Option<i64>,
    ) -> Result<i64, ChatError> {
        let c = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();
        c.execute(
            "INSERT INTO messages (conversation_id, role, body, status, reply_to, created_at, \
             meta_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                conversation_id,
                role,
                body,
                status,
                reply_to,
                now,
                meta.to_string()
            ],
        )?;
        let id = c.last_insert_rowid();
        self.touch(&c, conversation_id)?;
        Ok(id)
    }

    /// Enqueue a platform message (e.g. a GitHub comment) as a `pending` user
    /// message, idempotently: if a row with the same `(conversation_id,
    /// remote_message_id)` already exists -- e.g. an earlier poll already enqueued
    /// it and the worker hasn't answered/delivered yet -- nothing is inserted.
    /// Returns whether this call actually inserted a new row. `remote_message_id` —
    /// a GitHub comment `databaseId`, or `0` for a discussion's opening body — is
    /// stored as its string form to keep the column type uniform with delivered
    /// replies' node ids.
    pub fn upsert_remote_user_message(
        &self,
        conversation_id: i64,
        remote_message_id: u64,
        body: &str,
    ) -> Result<bool, ChatError> {
        let c = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();
        let changed = c.execute(
            "INSERT INTO messages (conversation_id, role, body, status, reply_to, created_at, \
             meta_json, remote_message_id) VALUES (?1, 'user', ?2, 'pending', NULL, ?3, '{}', ?4) \
             ON CONFLICT (conversation_id, remote_message_id) DO NOTHING",
            params![conversation_id, body, now, remote_message_id.to_string()],
        )?;
        if changed > 0 {
            self.touch(&c, conversation_id)?;
        }
        Ok(changed > 0)
    }

    /// Record that `id` was successfully delivered to the platform as artifact
    /// `remote_message_id` (e.g. the GitHub node id of the comment that reply was
    /// posted as) -- the anti-redelivery marker for eventual-connector delivery.
    pub fn mark_delivered(&self, id: i64, remote_message_id: &str) -> Result<(), ChatError> {
        let c = self.conn()?;
        c.execute(
            "UPDATE messages SET remote_message_id=?1 WHERE id=?2",
            params![remote_message_id, id],
        )?;
        Ok(())
    }

    pub fn message(&self, id: i64) -> Result<Option<MessageRow>, ChatError> {
        let c = self.conn()?;
        Ok(c.query_row(
            "SELECT id, conversation_id, role, body, status, reply_to, read_at, created_at, \
             meta_json, remote_message_id FROM messages WHERE id=?1",
            params![id],
            row_to_message,
        )
        .optional()?)
    }

    /// System notices in `connector` conversations that haven't been delivered to
    /// the platform yet (no remote artifact id): in-flight `notice-active` rows (the
    /// mid-turn "still working" message) and final `notice-error` rows (a failed
    /// turn, which is exactly what the user on GitHub should see). `notice-done`
    /// rows are suppressed: those were either already delivered or superseded by a
    /// delivered reply.
    pub fn undelivered_system_notices(
        &self,
        connector: &str,
    ) -> Result<Vec<MessageRow>, ChatError> {
        let c = self.conn()?;
        let mut stmt = c.prepare(
            "SELECT m.id, m.conversation_id, m.role, m.body, m.status, m.reply_to, m.read_at, \
             m.created_at, m.meta_json, m.remote_message_id \
             FROM messages m JOIN conversations c ON c.id = m.conversation_id \
             WHERE c.connector=?1 AND m.role='system' \
               AND m.status IN ('notice-active', 'notice-error') \
               AND m.remote_message_id IS NULL \
             ORDER BY m.id",
        )?;
        let rows = stmt.query_map(params![connector], row_to_message)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Completed assistant replies in `connector` conversations that haven't been
    /// delivered to the platform yet (`sent` with non-empty body, no remote id).
    /// Read receipts are never delivered (GitHub has none): a `read` row doesn't
    /// need pushing, and `sent` is the deliverable state.
    pub fn undelivered_assistant_sent(
        &self,
        connector: &str,
    ) -> Result<Vec<MessageRow>, ChatError> {
        let c = self.conn()?;
        let mut stmt = c.prepare(
            "SELECT m.id, m.conversation_id, m.role, m.body, m.status, m.reply_to, m.read_at, \
             m.created_at, m.meta_json, m.remote_message_id \
             FROM messages m JOIN conversations c ON c.id = m.conversation_id \
             WHERE c.connector=?1 AND m.role='assistant' AND m.status='sent' \
               AND m.remote_message_id IS NULL AND trim(m.body) <> '' \
             ORDER BY m.id",
        )?;
        let rows = stmt.query_map(params![connector], row_to_message)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn set_message_body(&self, id: i64, body: &str) -> Result<(), ChatError> {
        let c = self.conn()?;
        c.execute("UPDATE messages SET body=?1 WHERE id=?2", params![body, id])?;
        Ok(())
    }

    pub fn set_message_status(&self, id: i64, status: &str) -> Result<(), ChatError> {
        let c = self.conn()?;
        c.execute(
            "UPDATE messages SET status=?1 WHERE id=?2",
            params![status, id],
        )?;
        Ok(())
    }

    pub fn set_message_meta(&self, id: i64, meta: &Value) -> Result<(), ChatError> {
        let c = self.conn()?;
        c.execute(
            "UPDATE messages SET meta_json=?1 WHERE id=?2",
            params![meta.to_string(), id],
        )?;
        Ok(())
    }

    /// Mark delivered assistant messages as read (the web UI calls this once the
    /// browser has actually shown them). Only touches assistant rows still in a
    /// deliverable state, so an already-read message isn't re-flagged.
    pub fn mark_read(&self, ids: &[i64]) -> Result<(), ChatError> {
        if ids.is_empty() {
            return Ok(());
        }
        let now = chrono::Utc::now().to_rfc3339();
        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "UPDATE messages SET status='{read}', read_at=?{n} \
             WHERE role='assistant' AND status IN ('sent','streaming') \
             AND id IN ({})",
            placeholders.join(","),
            read = STATUS_READ,
            n = ids.len() + 1,
        );
        let mut vals: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for id in ids {
            vals.push(Box::new(id.to_owned()));
        }
        vals.push(Box::new(now.to_owned()));
        let c = self.conn()?;
        c.execute(&sql, rusqlite::params_from_iter(vals.iter().map(|b| &**b)))?;
        Ok(())
    }

    pub fn messages_of_conversation(
        &self,
        conversation_id: i64,
        since_id: i64,
    ) -> Result<Vec<MessageRow>, ChatError> {
        let c = self.conn()?;
        let mut stmt = c.prepare(
            "SELECT id, conversation_id, role, body, status, reply_to, read_at, created_at, \
             meta_json, remote_message_id FROM messages WHERE conversation_id=?1 AND id>?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![conversation_id, since_id], row_to_message)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// The newest `limit` messages in a conversation, oldest-first -- the "refresh
    /// the last few in place" window the web UI uses to update statuses/read receipts
    /// on messages it already rendered (which the append-only `since_id` cursor
    /// deliberately never re-sends).
    pub fn recent_messages(
        &self,
        conversation_id: i64,
        limit: usize,
    ) -> Result<Vec<MessageRow>, ChatError> {
        let c = self.conn()?;
        let mut stmt = c.prepare(
            "SELECT id, conversation_id, role, body, status, reply_to, read_at, created_at, \
             meta_json, remote_message_id FROM messages WHERE conversation_id=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![conversation_id, limit as i64], row_to_message)?;
        let mut v: Vec<MessageRow> = rows.filter_map(|r| r.ok()).collect();
        v.reverse();
        Ok(v)
    }

    /// Requeue any user message a crashed worker run left claimed (`processing`).
    /// Called by the worker at startup; the store's `open` also does it so the web
    /// connector alone gets the same guarantee.
    pub fn requeue_stale_processing(&self) -> Result<(), ChatError> {
        reset_stale_processing(&self.path)
    }

    /// The oldest `pending` user message per conversation (an earlier unclaimed
    /// message in the same conversation blocks a later one, preserving order), up to
    /// `limit` total.
    pub fn pending_user_messages(&self, limit: usize) -> Result<Vec<MessageRow>, ChatError> {
        let c = self.conn()?;
        let mut stmt = c.prepare(
            "SELECT m.id, m.conversation_id, m.role, m.body, m.status, m.reply_to, m.read_at, \
             m.created_at, m.meta_json, m.remote_message_id \
             FROM messages m \
             WHERE m.role='user' AND m.status='pending' \
               AND NOT EXISTS ( \
                 SELECT 1 FROM messages m2 \
                 WHERE m2.conversation_id = m.conversation_id \
                   AND m2.role='user' \
                   AND m2.status IN ('pending','processing') \
                   AND m2.id < m.id \
               ) \
             ORDER BY m.id \
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_message)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Whether anything in `conversation_id` is still in flight -- a pending/processing
    /// user message, a streaming assistant reply, or an active "still working" notice.
    /// Drives the UI's typing indicator.
    pub fn has_active_work(&self, conversation_id: i64) -> Result<bool, ChatError> {
        let c = self.conn()?;
        let n: i64 = c.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id=?1 AND ( \
               (role='user' AND status IN ('pending','processing')) OR \
               (role='assistant' AND status='streaming') OR \
               (role='system' AND status='notice-active') \
             )",
            params![conversation_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    // ---------------------------------------------------------------------------
    // System notices + drafting meta (worker's vocabulary)
    // ---------------------------------------------------------------------------

    pub fn insert_system_notice(&self, conversation_id: i64, body: &str) -> Result<i64, ChatError> {
        self.insert_message(
            conversation_id,
            ROLE_SYSTEM,
            body,
            STATUS_NOTICE_ACTIVE,
            &json!({}),
            None,
        )
    }

    pub fn resolve_system_notices(&self, conversation_id: i64) -> Result<(), ChatError> {
        let c = self.conn()?;
        c.execute(
            "UPDATE messages SET status=?1 WHERE conversation_id=?2 AND role='system' AND \
             status=?3",
            params![STATUS_NOTICE_DONE, conversation_id, STATUS_NOTICE_ACTIVE],
        )?;
        Ok(())
    }

    /// The most recent assistant message in `conversation_id` that carries a finished
    /// draft (`meta.kind == "draft"`, not yet created), if any -- what a "create it"
    /// confirmation should file. Returns the message id and its meta.
    pub fn pending_draft_for(
        &self,
        conversation_id: i64,
    ) -> Result<Option<(i64, Value)>, ChatError> {
        let c = self.conn()?;
        let row = c
            .query_row(
                "SELECT id, meta_json FROM messages WHERE conversation_id=?1 AND role='assistant' \
                 ORDER BY id DESC LIMIT 1",
                params![conversation_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((id, meta_json)) = row else {
            return Ok(None);
        };
        let meta: Value = serde_json::from_str(&meta_json).unwrap_or_else(|_| json!({}));
        let is_draft = meta.get("kind").and_then(|k| k.as_str()) == Some("draft")
            && meta.get("created").and_then(|c| c.as_bool()) != Some(true);
        Ok(is_draft.then_some((id, meta)))
    }
}

fn ensure_schema(path: &Path) -> Result<(), ChatError> {
    let c = Connection::open(path)?;
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS conversations (
            id INTEGER PRIMARY KEY,
            connector TEXT NOT NULL,
            remote_id TEXT,
            user_handle TEXT NOT NULL DEFAULT '',
            title TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            archived INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY,
            conversation_id INTEGER NOT NULL REFERENCES conversations(id),
            role TEXT NOT NULL,
            body TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL,
            reply_to INTEGER REFERENCES messages(id),
            created_at TEXT NOT NULL,
            read_at TEXT,
            meta_json TEXT NOT NULL DEFAULT '{}',
            remote_message_id TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_conversations_remote
            ON conversations(connector, remote_id) WHERE remote_id IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_messages_conv ON messages(conversation_id, id);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_remote ON messages(conversation_id, remote_message_id);
        ",
    )?;
    // Migrations for databases created before the remote-message column existed
    // (`CREATE TABLE IF NOT EXISTS` won't add a column or alter an index):
    // - add the column,
    // - swap a partial unique index for the full one the `ON CONFLICT` clause in
    //   `upsert_remote_user_message` needs (SQLite's `ON CONFLICT` target can't
    //   match a partial index). NULLs don't collide in a unique index, so the full
    //   index is exactly as permissive.
    if !has_column(&c, "messages", "remote_message_id")? {
        c.execute("ALTER TABLE messages ADD COLUMN remote_message_id TEXT", [])?;
    }
    c.execute("DROP INDEX IF EXISTS idx_messages_remote", [])?;
    c.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_remote ON messages(conversation_id, remote_message_id)",
        [],
    )?;
    Ok(())
}

/// True if `table` already has a column named `column` (checked via `PRAGMA
/// table_info`, the only way to distinguish a column that exists but is empty from
/// one that was never created).
fn has_column(c: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = c.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in rows {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Requeue any user message a crashed run left claimed (`processing`). Called once at
/// `open`; the worker is the only thing that ever sets `processing`, and it's a
/// single task, so there is never a legitimately in-flight row at startup.
fn reset_stale_processing(path: &Path) -> Result<(), ChatError> {
    let c = Connection::open(path)?;
    c.execute(
        "UPDATE messages SET status=?1 WHERE role=?2 AND status=?3",
        params![STATUS_PENDING, ROLE_USER, STATUS_PROCESSING],
    )?;
    Ok(())
}

#[allow(dead_code)] // exercised by the GitHub-Discussions connector (Phase D)
fn row_to_conversation(row: &Row) -> rusqlite::Result<ConversationRow> {
    Ok(ConversationRow {
        id: row.get(0)?,
        connector: row.get(1)?,
        remote_id: row.get(2)?,
        user_handle: row.get(3)?,
        title: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn row_to_message(row: &Row) -> rusqlite::Result<MessageRow> {
    let meta_json: String = row.get(8)?;
    let meta = serde_json::from_str(&meta_json).unwrap_or_else(|_| json!({}));
    Ok(MessageRow {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        role: row.get(2)?,
        body: row.get(3)?,
        status: row.get(4)?,
        reply_to: row.get(5)?,
        read_at: row.get(6)?,
        created_at: row.get(7)?,
        meta,
        remote_message_id: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (ChatStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = ChatStore::open(dir.path().join(DB_FILENAME)).unwrap();
        (s, dir)
    }

    #[test]
    fn web_conversation_is_created_once_and_reused() {
        let (s, _d) = store();
        let a = s.get_or_create_web_conversation("you").unwrap();
        let b = s.get_or_create_web_conversation("you").unwrap();
        assert_eq!(a, b);
        assert_eq!(s.list_conversations().unwrap().len(), 1);
    }

    #[test]
    fn pending_query_returns_oldest_pending_per_conversation_in_order() {
        let (s, _d) = store();
        let c1 = s.create_conversation("test", None, "u", "t").unwrap();
        let c2 = s.create_conversation("test", None, "u", "t").unwrap();
        let m1 = s
            .insert_message(c1, ROLE_USER, "first", STATUS_PENDING, &json!({}), None)
            .unwrap();
        let m2 = s
            .insert_message(
                c2,
                ROLE_USER,
                "other conv",
                STATUS_PENDING,
                &json!({}),
                None,
            )
            .unwrap();
        let _m3 = s
            .insert_message(
                c1,
                ROLE_USER,
                "second, blocked by first",
                STATUS_PENDING,
                &json!({}),
                None,
            )
            .unwrap();
        let pending = s.pending_user_messages(10).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].id, m1);
        assert_eq!(pending[1].id, m2);
    }

    #[test]
    fn a_conversation_is_processed_one_message_at_a_time() {
        let (s, _d) = store();
        let c = s.create_conversation("test", None, "u", "t").unwrap();
        let m1 = s
            .insert_message(c, ROLE_USER, "a", STATUS_PENDING, &json!({}), None)
            .unwrap();
        let m2 = s
            .insert_message(c, ROLE_USER, "b", STATUS_PENDING, &json!({}), None)
            .unwrap();
        // m1 is claimed: m2 stays blocked (ordering) until m1 finishes -- no two
        // turns at once for the same conversation.
        s.set_message_status(m1, STATUS_PROCESSING).unwrap();
        assert!(s.pending_user_messages(10).unwrap().is_empty());
        s.set_message_status(m1, STATUS_PROCESSED).unwrap();
        let pending = s.pending_user_messages(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, m2);
    }

    #[test]
    fn reset_stale_processing_requeues_leftover_claims() {
        let (s, _d) = store();
        let c = s.create_conversation("test", None, "u", "t").unwrap();
        let m = s
            .insert_message(c, ROLE_USER, "a", STATUS_PENDING, &json!({}), None)
            .unwrap();
        s.set_message_status(m, STATUS_PROCESSING).unwrap();
        ChatStore::open(s.path.clone()).unwrap();
        let pending = s.pending_user_messages(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, STATUS_PENDING);
    }

    #[test]
    fn mark_read_only_touches_delivered_assistant_messages() {
        let (s, _d) = store();
        let c = s.create_conversation("test", None, "u", "t").unwrap();
        let a = s
            .insert_message(c, ROLE_ASSISTANT, "reply", STATUS_SENT, &json!({}), None)
            .unwrap();
        let sent_user = s
            .insert_message(c, ROLE_USER, "question", STATUS_PROCESSED, &json!({}), None)
            .unwrap();
        s.mark_read(&[a, sent_user]).unwrap();
        let msgs = s.messages_of_conversation(c, 0).unwrap();
        let assistant = msgs.iter().find(|m| m.id == a).unwrap();
        assert_eq!(assistant.status, STATUS_READ);
        assert!(assistant.read_at.is_some());
        // The user row is untouched: mark_read's WHERE only matches assistant rows.
        let user = msgs.iter().find(|m| m.id == sent_user).unwrap();
        assert_eq!(user.status, STATUS_PROCESSED);
    }

    #[test]
    fn pending_draft_for_finds_the_latest_uncreated_draft_only() {
        let (s, _d) = store();
        let c = s.create_conversation("test", None, "u", "t").unwrap();
        assert!(s.pending_draft_for(c).unwrap().is_none());
        s.insert_message(
            c,
            ROLE_ASSISTANT,
            "draft",
            STATUS_SENT,
            &json!({"kind": "draft", "title": "x", "body": "y", "created": false}),
            None,
        )
        .unwrap();
        let (id, meta) = s.pending_draft_for(c).unwrap().unwrap();
        assert_eq!(meta["title"], "x");
        s.set_message_meta(id, &json!({"kind": "draft", "created": true}))
            .unwrap();
        assert!(s.pending_draft_for(c).unwrap().is_none());
    }

    #[test]
    fn system_notices_are_resolvable_to_done() {
        let (s, _d) = store();
        let c = s.create_conversation("test", None, "u", "t").unwrap();
        s.insert_system_notice(c, "still working").unwrap();
        assert!(s.has_active_work(c).unwrap());
        s.resolve_system_notices(c).unwrap();
        assert!(!s.has_active_work(c).unwrap());
        let (n, _) = (s.messages_of_conversation(c, 0).unwrap().len(), ());
        assert_eq!(n, 1);
    }
}
