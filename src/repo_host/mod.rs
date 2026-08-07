//! Code-host abstraction: pull/merge-request automation (`repo.pull_request: true`)
//! and, for SweBot (README.md "SweBot"), the Q&A/drafting conversational surface and
//! PR/MR review -- abstracted over the underlying host (GitHub or GitLab) behind the
//! `RepoHost` trait below, the same pattern `tracker::TrackerAdapter` already
//! established for the issue-tracker side. The Q&A/drafting half is further split
//! out into its own `DiscussionHost` supertrait -- see `DiscussionHost`'s own doc
//! comment.
//!
//! Deliberately independent of `tracker::TrackerAdapter`: these are properties of
//! `repo:` (the code host), not `tracker:` (the issue board). They usually point at
//! the same repo in practice, but nothing requires that -- `repo.token` is already
//! kept separate from the tracker's own token for the same reason (see
//! `config::RepoConfig::token_env`'s doc comment).
//!
//! `SymphonyPullRequest`, `DiscussionThread`, `DiscussionComment`, and
//! `extract_closes_issue_number` live here rather than in `github`/`gitlab` because
//! they're already provider-agnostic: every field either host can populate, and
//! `swebot::mod`'s marker-scanning logic (`next_to_answer`, `transcript_up_to`, ...)
//! operates on `DiscussionThread`/`DiscussionComment` generically, needing no
//! provider-specific knowledge at all.

pub mod github;
pub mod gitlab;

// Re-exported so `service.rs` (the multi-repo `symphony serve` registration path,
// deferred/out of scope for the RepoHost trait itself -- see the GitLab integration
// plan's Stage 5) can keep calling these by their pre-split path without updating
// every call site for a refactor that doesn't otherwise touch it.
pub use github::{fetch_file, parse_github_owner_repo};

use crate::config::{RepoConfig, RepoProvider};
use crate::tracker::{ToolResult, ToolSpec};
use async_trait::async_trait;
use std::path::Path;

/// The verdict SweBot's review driver (`swebot::review`) asks a host to post.
/// Provider-agnostic: `RequestChanges` has no formal counterpart on GitLab core (see
/// `gitlab::GitlabRepoHost::post_pr_review`'s doc comment), but the caller-facing
/// vocabulary stays the same across both hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    Comment,
}

/// What a `post_pr_review` call actually posted -- may differ from the requested
/// `ReviewVerdict` (e.g. `Approve` requested but posted as `Comment` because the
/// reviewing identity authored the PR/MR itself and the host refuses a self-approval).
/// `review.rs` logs this, not the originally-requested verdict, so the log reflects
/// what a human reading the PR/MR would actually see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewOutcome {
    pub posted_as: ReviewVerdict,
}

/// The Q&A/drafting conversational surface `swebot::qa`/`swebot::drafting` poll and
/// post into -- deliberately its own trait, separate from `RepoHost` below. Every
/// `RepoHost` is a `DiscussionHost` (see the supertrait bound below); `swebot::run`
/// picks the right selector/field for the configured `repo.provider`.
#[async_trait]
pub trait DiscussionHost: Send + Sync {
    /// `selector` is a Discussions category name on GitHub or an issue label on
    /// GitLab -- interpreted per-impl; callers pick the right config field/value
    /// (see `swebot::run`).
    async fn list_swebot_threads(&self, selector: &str) -> Result<Vec<DiscussionThread>, String>;
    async fn post_discussion_comment(&self, thread_id: &str, body: &str) -> Result<String, String>;
    /// Mark a Q&A-category discussion comment as the accepted answer. Default: a
    /// silent no-op -- GitLab has no equivalent concept, and this must not surface as
    /// an error `qa.rs` has to swallow on every single answer.
    async fn mark_discussion_comment_as_answer(&self, _comment_id: &str) -> Result<(), String> {
        Ok(())
    }
    /// Close the source thread once ticket drafting has promoted it to a real tracker
    /// issue. Default: a no-op -- GitHub Discussions were never closed by this flow
    /// (Discussions and Issues are already visually separate there); GitLab overrides
    /// this to close/mark-closed the source thread (see
    /// `gitlab::GitlabRepoHost::close_thread`).
    async fn close_thread(&self, _thread_id: &str) -> Result<(), String> {
        Ok(())
    }
}

/// A code host's abstraction for PR/MR automation and SweBot PR/MR review. One
/// implementor per provider (`github::GithubRepoHost`, `gitlab::GitlabRepoHost`);
/// `build` below picks the right one from `RepoConfig::provider`. A supertrait of
/// `DiscussionHost` since every real code host is *also* a valid (if not always the
/// chosen) Q&A/drafting conversational surface -- see that trait's own doc comment
/// for why the split exists.
#[async_trait]
pub trait RepoHost: DiscussionHost {
    fn provider_kind(&self) -> RepoProvider;

    // --- PR/MR automation (repo.pull_request), and evidence attachment
    // (repo.evidence) -- both routed through the same tool-name dispatch. `workspace_dir`
    // is this turn's workspace path exactly as seen by the `__mcp_tool_server` process
    // itself (the in-container path in Docker mode, the host path otherwise -- mirroring
    // `--workflow-dir`'s own container-aware resolution), needed only by `attach_evidence`
    // to resolve the agent-supplied image path; every other tool ignores it.
    fn agent_tool_specs(&self) -> Vec<ToolSpec>;
    async fn execute_agent_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
        issue_id: &str,
        workspace_dir: &Path,
    ) -> ToolResult;

    // --- AIR-9: release evidence consolidation (repo.release_evidence) ---
    /// Overwrite an already-open PR/MR's body (title untouched) -- used to append
    /// the evidence bundle onto whatever the agent itself wrote, without needing the
    /// original title (which `execute_agent_tool`'s `open_pull_request` path doesn't
    /// hand back -- see `SymphonyPullRequest`, which carries `body` but not `title`).
    async fn update_pr_body(&self, pr_number: u64, body: &str) -> Result<(), String>;
    /// Upload arbitrary bytes (not just an image) to this repo's ticket branch via
    /// the same content-hashed path convention `attach_evidence` uses, and hand back
    /// the raw content URL (not a markdown snippet -- callers decide how to present
    /// it, e.g. a plain link for an oversized coverage report or the durable copy of
    /// a whole `release::EvidenceBundle`). Both implementations share their
    /// `attach_evidence`'s own upload plumbing rather than duplicating it.
    async fn upload_artifact(
        &self,
        issue_id: &str,
        file_name: &str,
        bytes: &[u8],
        commit_message: &str,
    ) -> Result<String, String>;

    // --- SweBot: PR/MR review ---
    async fn list_open_symphony_prs(&self) -> Result<Vec<SymphonyPullRequest>, String>;
    async fn has_reviewed_sha(&self, pr_number: u64, head_sha: &str) -> Result<bool, String>;
    async fn post_pr_review(
        &self,
        pr_number: u64,
        head_sha: &str,
        verdict: ReviewVerdict,
        summary: &str,
    ) -> Result<ReviewOutcome, String>;
}

/// Construct the configured repo host from `repo.provider`.
pub fn build(repo: &RepoConfig) -> Result<Box<dyn RepoHost>, String> {
    match repo.provider {
        RepoProvider::Github => Ok(Box::new(github::GithubRepoHost::new(repo)?)),
        RepoProvider::Gitlab => Ok(Box::new(gitlab::GitlabRepoHost::new(repo)?)),
    }
}

/// Open pull/merge requests whose head branch matches Symphony's own per-ticket
/// convention (`issue-<identifier>`, produced by `synthesize_repo_hooks` /
/// `open_pull_request`'s `head` computation) -- i.e. ones Symphony's *own* coding
/// agents opened, not every PR/MR a human might have opened by hand on this repo.
/// `number` is the provider-native numeric identifier: a GitHub PR number or a
/// GitLab MR `iid`.
#[derive(Debug, Clone)]
pub struct SymphonyPullRequest {
    pub number: u64,
    pub html_url: String,
    pub head_ref: String,
    pub head_sha: String,
    pub body: String,
}

/// A GitHub Discussion or a GitLab label-filtered Issue, normalized to the shape
/// `swebot::mod`'s marker-scanning logic needs: an opening `body` plus a flat,
/// chronologically-ordered list of `comments`.
#[derive(Debug, Clone)]
pub struct DiscussionThread {
    pub id: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub comments: Vec<DiscussionComment>,
}

#[derive(Debug, Clone)]
pub struct DiscussionComment {
    pub database_id: u64,
    pub body: String,
    pub author_login: Option<String>,
}

/// Case-insensitive scan for issue-closing keywords (`close(s|d)`, `fix(es|ed)`,
/// `resolve(s|d)`) followed by `#<number>` -- the same convention both
/// `open_pull_request`'s tool description and GitLab's own MR-closing-keyword
/// convention ask the agent to use. Used by SweBot's review driver to find the
/// ticket a PR/MR is meant to resolve, so a review can be judged against the
/// *original* acceptance criteria. Manual scan rather than a new `regex` dependency
/// -- mirrors `tracker::depends_on::parse_depends_on`'s own style for the same reason.
pub fn extract_closes_issue_number(body: &str) -> Option<u64> {
    const KEYWORDS: &[&str] = &[
        "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
    ];
    let lower = body.to_lowercase();
    for keyword in KEYWORDS {
        let mut search_from = 0;
        while let Some(pos) = lower[search_from..].find(keyword) {
            let start = search_from + pos;
            let after = &body[start + keyword.len()..];
            let after = after.trim_start();
            if let Some(rest) = after.strip_prefix('#') {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if !digits.is_empty()
                    && let Ok(n) = digits.parse::<u64>()
                {
                    return Some(n);
                }
            }
            search_from = start + keyword.len();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_closes_issue_number_case_insensitively_and_across_keywords() {
        assert_eq!(extract_closes_issue_number("Closes #42"), Some(42));
        assert_eq!(
            extract_closes_issue_number("fixes #7 and more text"),
            Some(7)
        );
        assert_eq!(extract_closes_issue_number("RESOLVED #100"), Some(100));
        assert_eq!(extract_closes_issue_number("no keyword here, #9"), None);
        assert_eq!(extract_closes_issue_number("closes issue 9, no hash"), None);
    }
}
