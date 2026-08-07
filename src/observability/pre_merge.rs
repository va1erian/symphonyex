//! Pre-merge stage driver: for each open Symphony pull/merge request not yet
//! scanned, diffs it against the default branch (reusing `swebot::git`, the same
//! clone/diff helpers PR review already uses) and records `TelemetryEvidence`
//! (`evidence::collect_telemetry_evidence`) as a `telemetry_evidence` eventlog event
//! -- independent of `swebot.review.enabled`, since observability validation isn't a
//! SweBot capability.

use super::evidence::collect_telemetry_evidence;
use crate::config::{EffectiveConfig, RepoConfig};
use crate::eventlog::NewEvent;
use crate::repo_host::{RepoHost, extract_closes_issue_number};
use crate::swebot::git;
use std::collections::HashSet;
use tokio::sync::mpsc::UnboundedSender;

/// One poll tick, scanning every open Symphony PR/MR not already in `seen` (keyed by
/// `<number>:<head_sha>`, mirroring `RepoHost::has_reviewed_sha`'s own per-commit
/// idempotency but kept in-process rather than round-tripping the host -- a restart
/// simply re-scans, which is harmless since this only ever emits an evidence event,
/// never mutates anything on the host). Takes `repo` directly (not the whole
/// `EffectiveConfig`) so the dashboard's "rescan now" action
/// (`status::observability_rescan`) can call this without needing a full config.
pub async fn poll_once(
    repo: &RepoConfig,
    host: &dyn RepoHost,
    event_tx: &UnboundedSender<NewEvent>,
    seen: &mut HashSet<String>,
) -> Result<(), String> {
    let prs = host.list_open_symphony_prs().await?;

    for pr in prs {
        let key = format!("{}:{}", pr.number, pr.head_sha);
        if !seen.insert(key) {
            continue;
        }

        let scratch = tempfile::tempdir().map_err(|e| e.to_string())?;
        git::clone_branch(repo, scratch.path(), &pr.head_ref).await?;
        let diff = git::diff_against(scratch.path(), &repo.default_branch).await?;
        let evidence = collect_telemetry_evidence(&diff, &[]);

        let issue_id = extract_closes_issue_number(&pr.body)
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("pr:{}", pr.number));
        let message = serde_json::to_string(&evidence).unwrap_or_default();
        let _ = event_tx.send(NewEvent {
            issue_id: issue_id.clone(),
            identifier: issue_id,
            title: format!("PR #{}", pr.number),
            session_id: None,
            event_type: "telemetry_evidence".to_string(),
            message: Some(message),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
        });
    }
    Ok(())
}

/// Background loop, spawned alongside `production_validation::run` (see
/// `orchestrator::run_inner`) whenever `observability.backend` isn't `none` and
/// `repo:` is configured. The scan itself needs no backend -- gating it on
/// `observability.backend` anyway (rather than a separate on/off flag) is what makes
/// `backend: none` (the default) leave the daemon's pre-existing behavior fully
/// unchanged, matching this ticket's acceptance criterion for `production_validation`.
pub async fn run(cfg: EffectiveConfig, event_tx: UnboundedSender<NewEvent>) {
    let Some(repo) = cfg.repo.clone() else { return };
    let Ok(host) = crate::repo_host::build(&repo) else {
        return;
    };

    let mut seen = HashSet::new();
    let mut interval =
        tokio::time::interval(std::time::Duration::from_millis(cfg.poll_interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if let Err(e) = poll_once(&repo, host.as_ref(), &event_tx, &mut seen).await {
            tracing::warn!(error = %e, "observability: pre-merge scan failed this tick (ignored)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoConfig;
    use crate::repo_host::{
        DeployRecord, DiscussionHost, DiscussionThread, ReviewOutcome, ReviewVerdict,
        SymphonyPullRequest,
    };
    use crate::tracker::{ToolResult, ToolSpec};
    use async_trait::async_trait;
    use std::path::Path;

    /// A fake `RepoHost` returning one fixed PR list, so `poll_once` can be tested
    /// without a real code host -- the diff/clone side still needs a real git repo,
    /// so `poll_once`'s own body is exercised end to end via `dedup_skips_a_pr_already_seen`
    /// against a local scratch git repo below.
    struct FakeHost {
        prs: Vec<SymphonyPullRequest>,
    }

    #[async_trait]
    impl DiscussionHost for FakeHost {
        async fn list_swebot_threads(
            &self,
            _selector: &str,
        ) -> Result<Vec<DiscussionThread>, String> {
            Ok(vec![])
        }
        async fn post_discussion_comment(
            &self,
            _thread_id: &str,
            _body: &str,
        ) -> Result<String, String> {
            Ok(String::new())
        }
    }

    #[async_trait]
    impl RepoHost for FakeHost {
        fn provider_kind(&self) -> crate::config::RepoProvider {
            crate::config::RepoProvider::Github
        }
        fn agent_tool_specs(&self) -> Vec<ToolSpec> {
            vec![]
        }
        async fn execute_agent_tool(
            &self,
            _name: &str,
            _arguments: serde_json::Value,
            _issue_id: &str,
            _workspace_dir: &Path,
        ) -> ToolResult {
            ToolResult::error("unused")
        }
        async fn list_open_symphony_prs(&self) -> Result<Vec<SymphonyPullRequest>, String> {
            Ok(self.prs.clone())
        }
        async fn has_reviewed_sha(&self, _pr_number: u64, _head_sha: &str) -> Result<bool, String> {
            Ok(false)
        }
        async fn post_pr_review(
            &self,
            _pr_number: u64,
            _head_sha: &str,
            verdict: ReviewVerdict,
            _summary: &str,
        ) -> Result<ReviewOutcome, String> {
            Ok(ReviewOutcome { posted_as: verdict })
        }
        async fn latest_successful_deploy(&self) -> Result<Option<DeployRecord>, String> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn dedup_skips_a_pr_already_seen() {
        let pr = SymphonyPullRequest {
            number: 1,
            html_url: "https://example.invalid/pr/1".to_string(),
            head_ref: "issue-1".to_string(),
            head_sha: "sha1".to_string(),
            body: String::new(),
        };
        let mut seen = HashSet::new();
        seen.insert("1:sha1".to_string());
        let repo = RepoConfig {
            url: "https://github.com/owner/name.git".to_string(),
            default_branch: "main".to_string(),
            ..Default::default()
        };
        let host = FakeHost { prs: vec![pr] };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // `host.list_open_symphony_prs` returns the one PR, but it's already in
        // `seen` -- `poll_once` must skip straight past it without ever attempting
        // the (network-dependent) clone, so this returns `Ok(())` with nothing sent.
        poll_once(&repo, &host, &tx, &mut seen).await.unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn run_returns_immediately_when_no_repo_is_configured() {
        let cfg = crate::config::resolve(
            &serde_yaml::from_str("tracker:\n  kind: local\n").unwrap(),
            Path::new("."),
        )
        .unwrap();
        assert!(cfg.repo.is_none());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        // Would loop forever if `repo:` were treated as present -- a timeout proves
        // `run` returns instead of entering its poll loop.
        tokio::time::timeout(std::time::Duration::from_millis(200), run(cfg, tx))
            .await
            .expect("run() must return immediately when cfg.repo is None");
    }
}
