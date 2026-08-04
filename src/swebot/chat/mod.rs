//! SweBot chat mode: the unified Q&A + ticket-drafting conversation pipeline
//! (README.md "SweBot chat mode"). One worker task answers `pending` user messages
//! with streaming turns (the must-notify rule: a turn slower than
//! `first_text_deadline_ms` tells the user so), and connectors bridge the shared
//! `ChatStore` to each platform:
//!
//! The whole pipeline (store, worker, and both connectors below) runs only under
//! `swebot.chat.enabled` -- see `start`'s own doc comment for the ownership split
//! with the standalone `qa`/`drafting` loops when it's off.
//!
//! - `github` (for a GitHub repo, when chat is on) -- GitHub Discussions Q&A/drafting,
//!   folded in from the old standalone loop: each discussion is a conversation,
//!   replies are delivered back as comments.
//! - `web` (always, when chat is on) -- the bundled chat UI served by the status
//!   dashboard.
//!
//! Future connectors (MS Teams, ...) implement the `ChatConnector` contract and are
//! constructed in `start` -- see `connector.rs`.
//!
//! Spawned from `orchestrator::run_inner` alongside the rest of SweBot, inside the
//! same process/lifecycle -- not a second daemon to deploy.

pub mod connector;
pub mod github;
pub mod store;
#[cfg(feature = "teams")]
pub mod teams;
pub mod web;
pub mod worker;

use crate::config::{EffectiveConfig, RepoProvider};
use crate::repo_host::github::GithubRepoHost;
use crate::tracker::TrackerAdapter;
use std::sync::Arc;

/// Handed back to the caller that started chat (the orchestrator, and through
/// `ProjectHandles` the multi-project service): the store backs both the web UI and
/// (for a GitHub repo) the discussions connector, and `web_enabled` says whether the
/// web UI should be served.
#[derive(Clone)]
pub struct ChatHandles {
    pub store: store::ChatStore,
    pub web_enabled: bool,
}

/// Start the chat pipeline for a project. Runs only under `swebot.chat.enabled`; when
/// it's off, the standalone `qa`/`drafting` loops in `swebot::run` own Q&A/drafting
/// for every provider (including GitHub), so there's nothing for chat to do and no
/// store gets created.
///
/// Ownership split when it IS on: chat's GitHub connector owns a *GitHub* repo's
/// Discussions (and `swebot::run` skips its standalone Q&A/drafting for GitHub),
/// while GitLab keeps the standalone loop -- chat has no non-GitHub connector today.
/// The bundled web UI is always enabled here.
///
/// Errors are logged and the project continues without chat (same best-effort posture
/// as `swebot::run`).
pub fn start(cfg: EffectiveConfig, tracker: Arc<dyn TrackerAdapter>) -> Option<ChatHandles> {
    if !cfg.swebot.enabled || !cfg.swebot.chat.enabled {
        return None;
    }
    let backend = crate::swebot::build_restricted_backend(&cfg)?;
    let store = match store::ChatStore::open(cfg.workflow_dir.join(store::DB_FILENAME)) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "swebot chat: failed to open chat store; chat disabled");
            return None;
        }
    };

    let mut connectors: Vec<Box<dyn connector::ChatConnector>> = Vec::new();

    // GitHub Discussions connector (GitHub only -- chat doesn't speak GitLab).
    if let Some(repo) = cfg
        .swebot_repo_config()
        .filter(|r| matches!(r.provider, RepoProvider::Github))
    {
        match GithubRepoHost::new(&repo) {
            Ok(host) => connectors.push(Box::new(github::GitHubChatConnector::new(
                host,
                cfg.swebot.qa_discussion_category.clone(),
                cfg.swebot.drafting_discussion_category.clone(),
            ))),
            Err(e) => {
                tracing::warn!(error = %e, "swebot chat: failed to build GitHub host; discussions will not run");
            }
        }
    }

    connectors.push(Box::new(web::WebChatConnector));

    tokio::spawn(worker::run_loop(
        cfg.clone(),
        backend,
        tracker,
        store.clone(),
        connectors,
    ));
    tracing::info!("swebot chat pipeline starting");
    Some(ChatHandles {
        store,
        web_enabled: true,
    })
}
