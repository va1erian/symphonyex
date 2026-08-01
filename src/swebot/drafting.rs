//! Ticket-drafting capability: turns a rough idea posted in the repo's `Ideas`-
//! category GitHub Discussions (`swebot.drafting.discussion_category`, default
//! `"Ideas"`) into a properly scoped issue through a clarifying dialogue.
//!
//! Ends by creating a **new** issue via `TrackerAdapter::create_issue` rather than
//! rewriting the discussion into one -- Discussions stays the messy conversational
//! space, Issues stays the clean actionable backlog Symphony's own dispatch loop
//! watches, and a half-drafted idea is never sitting in the tracker looking
//! dispatchable when it isn't.
//!
//! Each poll cycle starts a *fresh* `claude` session rather than resuming a previous
//! one (a human's next reply may come hours later, well past a SweBot restart) --
//! the full discussion transcript, reconstructed from GitHub's own stored comments
//! and sent as the prompt every time, carries the conversation's state instead. No
//! `--resume` needed, and nothing to persist beyond the same `swebot:answered`
//! marker `qa.rs` already uses.

use super::{
    PERSONA, answered_marker, extract_json_block, next_to_answer, run_turn_collect_text,
    transcript_up_to,
};
use crate::agent::AgentBackend;
use crate::config::EffectiveConfig;
use crate::repo_host::GithubRepoHost;
use crate::tracker::TrackerAdapter;
use std::path::Path;

pub async fn poll_once(
    cfg: &EffectiveConfig,
    host: &GithubRepoHost,
    backend: &dyn AgentBackend,
    shared_clone_dir: &Path,
    tracker: &dyn TrackerAdapter,
) -> Result<(), String> {
    let threads = host
        .list_discussions_in_category(&cfg.swebot.drafting_discussion_category)
        .await?;

    for thread in threads {
        let Some(marker_id) = next_to_answer(&thread) else {
            continue; // nothing new since SweBot's last reply, or already handed off to an issue
        };
        let transcript = transcript_up_to(&thread, marker_id);

        let prompt = format!(
            "{PERSONA}\n\nHelp turn this GitHub Discussion idea (\"{}\") into a properly \
             scoped ticket. Ask only the clarifying questions that actually change scope \
             -- acceptance criteria, edge cases, what's explicitly out of scope -- not a \
             generic checklist. You have read access to the repo, checked out at {}, so \
             ground questions in what's actually there rather than guessing.\n\n\
             Original idea and discussion so far:\n{transcript}\n\n\
             End your response with exactly one fenced ```json block: either \
             {{\"ready\": false, \"reply\": \"<your clarifying question>\"}} if you need \
             more information, or {{\"ready\": true, \"title\": \"<ticket title>\", \
             \"body\": \"<full ticket body: what, why, acceptance criteria>\"}} once you \
             have enough to write a properly scoped ticket.",
            thread.title,
            shared_clone_dir.display(),
        );

        let mut session = backend
            .start_session(
                shared_clone_dir,
                &thread.number.to_string(),
                &thread.title,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        let raw = match run_turn_collect_text(session.as_mut(), &prompt).await {
            Ok(text) => text,
            Err(e) => {
                session.stop().await;
                return Err(format!("discussion #{}: {e}", thread.number));
            }
        };
        session.stop().await;

        let parsed = match extract_json_block(&raw) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!(
                    "discussion #{}: {e} (raw response: {raw})",
                    thread.number
                ));
            }
        };
        let ready = parsed
            .get("ready")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let marker = answered_marker(marker_id);

        if !ready {
            let reply = parsed
                .get("reply")
                .and_then(|v| v.as_str())
                .unwrap_or("Could you say a bit more about what you're looking for?");
            let comment = format!("{marker}\n{reply}");
            host.post_discussion_comment(&thread.id, &comment).await?;
            tracing::info!(
                discussion = thread.number,
                url = %thread.url,
                "swebot: asked a clarifying question"
            );
            continue;
        }

        let title = parsed
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(&thread.title)
            .to_string();
        let body = parsed
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or(&thread.body)
            .to_string();

        // The project's own configured "starting" active state -- the same
        // vocabulary `tracker.active_states` already uses. First entry by
        // convention (mirrors how a project orders todo before in-progress etc.).
        let Some(initial_state) = cfg.active_states.first() else {
            return Err(format!(
                "discussion #{}: tracker.active_states is empty, nowhere to create the \
                 drafted issue into",
                thread.number
            ));
        };

        let issue = tracker
            .create_issue(&title, &body, initial_state)
            .await
            .map_err(|e| format!("discussion #{}: create_issue failed: {e}", thread.number))?;

        let comment = format!(
            "{marker}\nDrafted and created: **{}** ({})\n\nSymphony will pick this up on \
             its next poll.",
            title,
            issue.url.as_deref().unwrap_or(&issue.identifier),
        );
        let comment_id = host.post_discussion_comment(&thread.id, &comment).await?;
        tracing::info!(
            discussion = thread.number,
            issue = %issue.identifier,
            "swebot: drafted and created issue"
        );
        if let Err(e) = host.mark_discussion_comment_as_answer(&comment_id).await {
            tracing::debug!(discussion = thread.number, error = %e, "swebot: could not mark comment as answer (ignored)");
        }
    }
    Ok(())
}
