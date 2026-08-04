//! The pluggable connector boundary for chat mode: what a chat connector must do to
//! bridge a platform (a web page, GitHub Discussions, MS Teams, ...) to the shared
//! `ChatStore` the worker processes. `super::github` (the Discussions Q&A/drafting
//! surface) and `super::web` (the bundled chat UI) are the shipped implementations;
//! a future Teams connector implements the same two methods and is constructed from
//! `chat::start`.
//!
//! The seam is deliberately coarse -- poll-based, like everything else in this
//! codebase, rather than an event bus:
//!
//! - `ingest` pulls any new platform-side messages *into* the store (no-op for
//!   `web`, whose HTTP routes write to the store directly; for `github` it enqueues
//!   new discussion comments).
//! - `deliver` pushes assistant messages and status changes *out* to the platform
//!   (no-op for `web`, which reads the store directly; for `github` it posts replies
//!   back as discussion comments).
//!
//! The worker owns processing between the two, so a connector never talks to a model.
//! Construction lives in `chat::start`, which builds each connector with whatever
//! platform state it needs (a `GithubRepoHost`, none for `web`), so adding a
//! connector is: implement this trait + construct it in `start` -- no registry, no
//! config-driven dispatch.

use super::store::ChatStore;
use async_trait::async_trait;

#[async_trait]
pub trait ChatConnector: Send + Sync {
    /// Connector name (used in logs and as the conversations link key).
    fn name(&self) -> &str;

    /// Pull new remote messages into the store. Polled every chat tick before the
    /// worker processes pending messages; errors are logged and retried next tick.
    async fn ingest(&self, store: &ChatStore) -> Result<(), String>;

    /// Push assistant messages + status changes out to the platform. Polled every
    /// chat tick after the worker processes; errors are logged and retried.
    async fn deliver(&self, store: &ChatStore) -> Result<(), String>;
}
