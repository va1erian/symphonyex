//! Orchestrator: the single authority over dispatch, retry, and reconciliation state
//! (Section 7, 8, 16). One task owns all mutable scheduling state directly; worker
//! tasks only ever report back over a channel, never mutate shared state themselves.

use crate::agent::{AgentBackend, AgentEvent, AgentSession, TokenUsage, TurnOutcome, claude, codex};
use crate::config::{self, AgentBackendKind, EffectiveConfig};
use crate::domain::Issue;
use crate::envsub;
use crate::hooks;
use crate::metrics::Metrics;
use crate::status;
use crate::template;
use crate::tracker::{self, TrackerAdapter};
use crate::workflow;
use crate::workspace::WorkspaceManager;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const DEFAULT_PROMPT: &str = "You are working on an issue from the configured tracker.";

/// Everything the dispatch loop needs, rebuilt (not mutated in place) on a successful
/// `WORKFLOW.md` reload so an in-flight session's snapshot is never observed to change
/// underneath it (Section 10.5).
struct Shared {
    config: EffectiveConfig,
    prompt_template: String,
    tracker: Arc<dyn TrackerAdapter>,
    agent_backend: Arc<dyn AgentBackend>,
    workspace_mgr: Arc<WorkspaceManager>,
}

impl Shared {
    fn snapshot(&self) -> DispatchSnapshot {
        DispatchSnapshot {
            config: self.config.clone(),
            prompt_template: self.prompt_template.clone(),
            tracker: self.tracker.clone(),
            agent_backend: self.agent_backend.clone(),
            workspace_mgr: self.workspace_mgr.clone(),
        }
    }
}

#[derive(Clone)]
struct DispatchSnapshot {
    config: EffectiveConfig,
    prompt_template: String,
    tracker: Arc<dyn TrackerAdapter>,
    agent_backend: Arc<dyn AgentBackend>,
    workspace_mgr: Arc<WorkspaceManager>,
}

struct RunningEntry {
    issue: Issue,
    started_at: Instant,
    session_id: String,
    last_event: Option<String>,
    last_event_at: Option<Instant>,
    last_message: Option<String>,
    tokens: TokenUsage,
    turn_count: u32,
    tool_call_count: u32,
    /// The worker task's own `JoinHandle`, not just an `AbortHandle`: reconciliation
    /// needs to `.abort()` *and then await* it before touching the workspace itself
    /// (running `after_run`, deleting the directory), so the aborted task's `Drop`
    /// (which `kill_on_drop`s the agent subprocess) has actually finished first —
    /// otherwise a hook/cleanup racing a not-yet-dead subprocess can silently lose
    /// work (see the `AR-8` incident this fixed: `after_run` never ran on
    /// reconciliation-triggered termination, so a fully-verified attempt's git commit
    /// never happened before its workspace got deleted).
    handle: tokio::task::JoinHandle<()>,
    retry_attempt: Option<u32>,
}

struct RetryEntry {
    identifier: String,
    attempt: u32,
    due_at: Instant,
    error: Option<String>,
    generation: u64,
}

#[derive(Default)]
struct OrchestratorState {
    running: HashMap<String, RunningEntry>,
    claimed: HashSet<String>,
    retry_attempts: HashMap<String, RetryEntry>,
    #[allow(dead_code)]
    completed: HashSet<String>,
    metrics: Metrics,
    retry_generation: HashMap<String, u64>,
}

impl OrchestratorState {
    fn next_generation(&mut self, issue_id: &str) -> u64 {
        let g = self.retry_generation.entry(issue_id.to_string()).or_insert(0);
        *g += 1;
        *g
    }
}

enum ExitReason {
    Normal,
    Error(String),
}

enum OrchMsg {
    SessionStarted { issue_id: String, session_id: String },
    TurnStarted { issue_id: String },
    AgentEvent { issue_id: String, event: AgentEvent },
    WorkerExit { issue_id: String, reason: ExitReason },
    RetryFired { issue_id: String, generation: u64 },
}

/// Load workflow + resolve config + build adapters for the current file contents.
/// Used for both startup and hot reload.
fn build_shared(workflow_path: &Path) -> anyhow::Result<Shared> {
    let wf = workflow::load(workflow_path)?;
    let workflow_dir_raw = workflow_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // Must be absolute: it's later handed to the MCP tool-server subprocess (spawned by
    // `claude` with the workspace, not the workflow directory, as its cwd), which would
    // otherwise resolve tracker-relative paths against the wrong directory.
    let workflow_dir = envsub::normalize(&std::env::current_dir()?.join(workflow_dir_raw));
    let workflow_dir = workflow_dir.as_path();
    let cfg = config::resolve(&wf.config, workflow_dir)?;
    config::validate_for_dispatch(&cfg, tracker::SUPPORTED_TRACKER_KINDS)?;

    let tracker_adapter = tracker::build(&cfg.tracker_kind, &cfg.tracker_provider, workflow_dir)?;
    let mcp_wiring = if tracker_adapter.agent_tool_specs().is_empty() {
        None
    } else {
        Some(claude::McpToolWiring {
            tracker_kind: cfg.tracker_kind.clone(),
            tracker_provider_json: serde_json::to_string(&cfg.tracker_provider)?,
            workflow_dir: cfg.workflow_dir.clone(),
        })
    };
    let agent_backend: Arc<dyn AgentBackend> = match cfg.agent_backend {
        AgentBackendKind::Claude => Arc::new(claude::ClaudeBackend {
            command: cfg.claude.command.clone(),
            extra_args: cfg.claude.args.clone(),
            model: cfg.claude.model.clone(),
            permission_mode: cfg.claude.permission_mode.clone(),
            turn_timeout_ms: cfg.claude.turn_timeout_ms,
            mcp_wiring,
        }),
        AgentBackendKind::Codex => Arc::new(codex::CodexBackend {
            command: cfg.codex.command.clone(),
            approval_policy: cfg.codex.approval_policy.clone(),
            thread_sandbox: cfg.codex.thread_sandbox.clone(),
            turn_sandbox_policy: cfg.codex.turn_sandbox_policy.clone(),
            turn_timeout_ms: cfg.codex.turn_timeout_ms,
            read_timeout_ms: cfg.codex.read_timeout_ms,
        }),
    };

    let prompt_template = if wf.prompt_template.is_empty() {
        DEFAULT_PROMPT.to_string()
    } else {
        wf.prompt_template
    };

    let workspace_mgr = WorkspaceManager::new(cfg.workspace_root.clone());
    std::fs::create_dir_all(workspace_mgr.root())?;

    Ok(Shared {
        config: cfg,
        prompt_template,
        tracker: Arc::from(tracker_adapter),
        agent_backend,
        workspace_mgr: Arc::new(workspace_mgr),
    })
}

/// Section 8.6: remove workspaces for issues already in a terminal state at startup.
async fn startup_terminal_cleanup(shared: &Shared) {
    match shared.tracker.fetch_issues_by_states(&shared.config.terminal_states).await {
        Ok(issues) => {
            for issue in issues {
                shared
                    .workspace_mgr
                    .remove_for_issue(
                        &issue.identifier,
                        shared.config.hook_before_remove.as_deref(),
                        shared.config.hook_timeout_ms,
                    )
                    .await;
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "startup terminal-issue fetch failed; continuing startup");
        }
    }
}

pub async fn run(
    workflow_path: PathBuf,
    status_port: Option<u16>,
    report_path_override: Option<PathBuf>,
) -> anyhow::Result<()> {
    let mut shared = build_shared(&workflow_path)?;
    tracing::info!(
        tracker_kind = %shared.config.tracker_kind,
        agent_backend = ?shared.config.agent_backend,
        poll_interval_ms = shared.config.poll_interval_ms,
        "symphony starting"
    );

    let report_path = report_path_override
        .unwrap_or_else(|| shared.config.workflow_dir.join("symphony-report.html"));
    tracing::info!(path = %report_path.display(), "usage report will be written here");

    startup_terminal_cleanup(&shared).await;

    let mut last_attempted_mtime = std::fs::metadata(&workflow_path).ok().and_then(|m| m.modified().ok());

    let (tx, mut rx) = mpsc::unbounded_channel::<OrchMsg>();
    let mut state = OrchestratorState::default();

    let (status_tx, status_rx) = tokio::sync::watch::channel(status::StatusSnapshot::default());
    if let Some(port) = status_port {
        tokio::spawn(async move {
            if let Err(e) = status::serve(port, status_rx).await {
                tracing::error!(error = %e, "status server exited");
            }
        });
    }

    let mut interval = tokio::time::interval(Duration::from_millis(shared.config.poll_interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                maybe_reload(&workflow_path, &mut shared, &mut last_attempted_mtime, &mut interval);
                on_tick(&shared, &mut state, &tx).await;
                status_tx.send_replace(build_status_snapshot(&state));
                write_report(&report_path, &workflow_path, &state);
            }
            Some(msg) = rx.recv() => {
                handle_msg(&shared, &mut state, &tx, msg).await;
                status_tx.send_replace(build_status_snapshot(&state));
                write_report(&report_path, &workflow_path, &state);
            }
        }
    }
}

fn write_report(report_path: &Path, workflow_path: &Path, state: &OrchestratorState) {
    if let Err(e) = crate::metrics::write_report(report_path, workflow_path, &state.metrics) {
        tracing::warn!(error = %e, path = %report_path.display(), "failed to write usage report (ignored)");
    }
}

fn build_status_snapshot(state: &OrchestratorState) -> status::StatusSnapshot {
    let now = Instant::now();
    let running = state
        .running
        .values()
        .map(|e| status::RunningRow {
            identifier: e.issue.identifier.clone(),
            title: e.issue.title.clone(),
            session_id: e.session_id.clone(),
            started_secs_ago: now.duration_since(e.started_at).as_secs_f64(),
            turn_count: e.turn_count,
            tool_call_count: e.tool_call_count,
            last_event: e.last_event.clone(),
            last_message: e.last_message.clone(),
        })
        .collect();

    let retrying = state
        .retry_attempts
        .values()
        .map(|e| status::RetryRow {
            identifier: e.identifier.clone(),
            attempt: e.attempt,
            due_in_secs: e.due_at.saturating_duration_since(now).as_secs_f64(),
            error: e.error.clone(),
        })
        .collect();

    status::StatusSnapshot {
        generated_at: chrono::Utc::now().to_rfc3339(),
        running,
        retrying,
    }
}

fn maybe_reload(
    workflow_path: &Path,
    shared: &mut Shared,
    last_attempted_mtime: &mut Option<std::time::SystemTime>,
    interval: &mut tokio::time::Interval,
) {
    let current_mtime = std::fs::metadata(workflow_path).ok().and_then(|m| m.modified().ok());
    if current_mtime == *last_attempted_mtime {
        return;
    }
    *last_attempted_mtime = current_mtime;

    match build_shared(workflow_path) {
        Ok(new_shared) => {
            let interval_changed = new_shared.config.poll_interval_ms != shared.config.poll_interval_ms;
            tracing::info!("workflow reloaded");
            *shared = new_shared;
            if interval_changed {
                *interval = tokio::time::interval(Duration::from_millis(shared.config.poll_interval_ms));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "workflow reload failed; keeping last known good configuration");
        }
    }
}

async fn on_tick(shared: &Shared, state: &mut OrchestratorState, tx: &mpsc::UnboundedSender<OrchMsg>) {
    reconcile(shared, state, tx).await;

    if let Err(e) = config::validate_for_dispatch(&shared.config, tracker::SUPPORTED_TRACKER_KINDS) {
        tracing::error!(error = %e, "dispatch preflight validation failed; skipping dispatch this tick");
        return;
    }

    let issues = match shared.tracker.fetch_issues_by_states(&shared.config.active_states).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "candidate fetch failed; skipping dispatch this tick");
            return;
        }
    };

    let mut candidates: Vec<Issue> = issues
        .into_iter()
        .filter(|i| eligible_for_dispatch(i, &shared.config, state))
        .collect();
    sort_for_dispatch(&mut candidates);

    for issue in candidates {
        if state.running.len() as u32 >= shared.config.max_concurrent_agents {
            break;
        }
        let normalized = issue.normalized_state();
        let per_state_count = state
            .running
            .values()
            .filter(|e| e.issue.normalized_state() == normalized)
            .count() as u32;
        if per_state_count >= shared.config.concurrency_limit_for_state(&normalized) {
            continue;
        }
        dispatch_issue(shared, state, tx, issue, None).await;
    }
}

fn eligible_for_dispatch(issue: &Issue, cfg: &EffectiveConfig, state: &OrchestratorState) -> bool {
    if issue.id.is_empty() || issue.identifier.is_empty() || issue.title.is_empty() || issue.state.is_empty() {
        return false;
    }
    let normalized = issue.normalized_state();
    if !cfg.active_states.iter().any(|s| s.trim().to_lowercase() == normalized) {
        return false;
    }
    if cfg.terminal_states.iter().any(|s| s.trim().to_lowercase() == normalized) {
        return false;
    }
    if !issue.is_routable(&cfg.required_labels) {
        return false;
    }
    if state.running.contains_key(&issue.id) || state.claimed.contains(&issue.id) {
        return false;
    }
    true
}

/// Section 8.2 sorting order.
fn sort_for_dispatch(issues: &mut [Issue]) {
    issues.sort_by(|a, b| {
        priority_bucket(a)
            .cmp(&priority_bucket(b))
            .then_with(|| priority_value(a).cmp(&priority_value(b)))
            .then_with(|| created_at_key(a).cmp(&created_at_key(b)))
            .then_with(|| a.identifier.cmp(&b.identifier))
    });
}

fn priority_bucket(i: &Issue) -> u8 {
    match i.priority {
        Some(p) if (1..=4).contains(&p) => 0,
        _ => 1,
    }
}

fn priority_value(i: &Issue) -> i64 {
    match i.priority {
        Some(p) if (1..=4).contains(&p) => p,
        _ => i64::MAX,
    }
}

fn created_at_key(i: &Issue) -> i64 {
    i.created_at.map(|t| t.timestamp_millis()).unwrap_or(i64::MAX)
}

fn has_available_slot(cfg: &EffectiveConfig, state: &OrchestratorState, normalized_state: &str) -> bool {
    if state.running.len() as u32 >= cfg.max_concurrent_agents {
        return false;
    }
    let per_state_count = state
        .running
        .values()
        .filter(|e| e.issue.normalized_state() == normalized_state)
        .count() as u32;
    per_state_count < cfg.concurrency_limit_for_state(normalized_state)
}

async fn dispatch_issue(
    shared: &Shared,
    state: &mut OrchestratorState,
    tx: &mpsc::UnboundedSender<OrchMsg>,
    issue: Issue,
    attempt: Option<u32>,
) {
    let issue_id = issue.id.clone();
    let identifier = issue.identifier.clone();
    let title = issue.title.clone();
    let snapshot = shared.snapshot();
    let tx2 = tx.clone();
    let issue_for_worker = issue.clone();
    let issue_id_for_worker = issue_id.clone();

    let handle = tokio::spawn(async move {
        run_agent_attempt(issue_id_for_worker, issue_for_worker, attempt, snapshot, tx2).await;
    });

    state.running.insert(
        issue_id.clone(),
        RunningEntry {
            issue,
            started_at: Instant::now(),
            session_id: String::new(),
            last_event: None,
            last_event_at: None,
            last_message: None,
            tokens: TokenUsage::default(),
            turn_count: 0,
            tool_call_count: 0,
            handle,
            retry_attempt: attempt,
        },
    );
    state.claimed.insert(issue_id.clone());
    state.retry_attempts.remove(&issue_id);

    state.metrics.agents_spawned += 1;
    state.metrics.issue_entry(&identifier, &title).dispatch_count += 1;

    tracing::info!(issue_id = %issue_id, identifier = %identifier, attempt = ?attempt, "dispatched");
}

fn backoff_delay_ms(attempt: u32, max_backoff_ms: u64) -> u64 {
    let pow = 2u64.saturating_pow(attempt.saturating_sub(1).min(32));
    10_000u64.saturating_mul(pow).min(max_backoff_ms)
}

fn schedule_retry(
    state: &mut OrchestratorState,
    tx: &mpsc::UnboundedSender<OrchMsg>,
    issue_id: &str,
    identifier: &str,
    attempt: u32,
    delay_ms: u64,
    error: Option<String>,
) {
    let generation = state.next_generation(issue_id);
    let due_at = Instant::now() + Duration::from_millis(delay_ms);
    state.retry_attempts.insert(
        issue_id.to_string(),
        RetryEntry {
            identifier: identifier.to_string(),
            attempt,
            due_at,
            error,
            generation,
        },
    );
    state.claimed.insert(issue_id.to_string());

    let issue_id_owned = issue_id.to_string();
    let tx2 = tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let _ = tx2.send(OrchMsg::RetryFired {
            issue_id: issue_id_owned,
            generation,
        });
    });
}

async fn terminate_running(
    state: &mut OrchestratorState,
    shared: &Shared,
    issue_id: &str,
    cleanup_workspace: bool,
    outcome: &str,
) {
    if let Some(entry) = state.running.remove(issue_id) {
        finalize_issue_runtime(state, &entry, outcome);
        let identifier = entry.issue.identifier.clone();
        abort_and_run_after_run(shared, entry.handle, &identifier).await;
        if cleanup_workspace {
            shared
                .workspace_mgr
                .remove_for_issue(
                    &identifier,
                    shared.config.hook_before_remove.as_deref(),
                    shared.config.hook_timeout_ms,
                )
                .await;
        }
    }
    state.claimed.remove(issue_id);
    state.retry_attempts.remove(issue_id);
}

/// Abort a running worker and *wait for it to actually stop* (not just request
/// cancellation) before touching its workspace, then run `after_run` if configured —
/// Section 9.4 requires `after_run` to fire on cancellation, not just normal/error
/// exit, and it can't safely run (or workspace cleanup safely proceed) while the
/// aborted task's `Drop` (which `kill_on_drop`s the agent subprocess) might still be
/// in flight.
async fn abort_and_run_after_run(shared: &Shared, handle: tokio::task::JoinHandle<()>, identifier: &str) {
    handle.abort();
    let _ = handle.await; // resolves once the task has truly finished, Err(Cancelled) is expected

    let Some(script) = &shared.config.hook_after_run else {
        return;
    };
    let path = shared.workspace_mgr.path_for(identifier);
    if !path.is_dir() {
        return;
    }
    if let Err(e) = hooks::run_hook("after_run", script, &path, shared.config.hook_timeout_ms).await {
        tracing::warn!(%identifier, error = %e, "after_run hook failed (ignored) [reconciliation-triggered termination]");
    }
}

/// Turns/tool-calls/tokens are already folded into `state.metrics` live as they
/// happen (see `handle_msg`); this only adds what can't be known until the attempt
/// ends: wall-clock runtime, and (via `outcome`) a human-readable last-outcome note.
fn finalize_issue_runtime(state: &mut OrchestratorState, entry: &RunningEntry, outcome: &str) {
    let seconds = entry.started_at.elapsed().as_secs_f64();
    state.metrics.seconds_running += seconds;
    let issue_metrics = state.metrics.issue_entry(&entry.issue.identifier, &entry.issue.title);
    issue_metrics.seconds_running += seconds;
    issue_metrics.last_outcome = Some(outcome.to_string());
}

/// Section 8.5: stall detection (Part A) + tracker state refresh (Part B).
async fn reconcile(shared: &Shared, state: &mut OrchestratorState, tx: &mpsc::UnboundedSender<OrchMsg>) {
    reconcile_stalled(shared, state, tx).await;

    let running_ids: Vec<String> = state.running.keys().cloned().collect();
    if running_ids.is_empty() {
        return;
    }

    match shared.tracker.fetch_issues_by_ids(&running_ids).await {
        Ok(refreshed) => {
            let mut seen = HashSet::new();
            for issue in refreshed {
                seen.insert(issue.id.clone());
                let normalized = issue.normalized_state();
                let terminal = shared.config.terminal_states.iter().any(|s| s.trim().to_lowercase() == normalized);
                let active = shared.config.active_states.iter().any(|s| s.trim().to_lowercase() == normalized);
                let routable = issue.is_routable(&shared.config.required_labels);

                if terminal {
                    terminate_running(state, shared, &issue.id, true, "reached terminal tracker state").await;
                } else if active && routable {
                    if let Some(entry) = state.running.get_mut(&issue.id) {
                        entry.issue = issue;
                    }
                } else {
                    terminate_running(state, shared, &issue.id, false, "no longer active/routable").await;
                }
            }
            for missing_id in running_ids.iter().filter(|id| !seen.contains(*id)) {
                terminate_running(state, shared, missing_id, false, "no longer visible in tracker").await;
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "reconciliation refresh failed; keeping workers running");
        }
    }
}

async fn reconcile_stalled(shared: &Shared, state: &mut OrchestratorState, tx: &mpsc::UnboundedSender<OrchMsg>) {
    let stall_ms = shared.config.effective_stall_timeout_ms();
    if stall_ms <= 0 {
        return;
    }
    let now = Instant::now();
    let stalled: Vec<(String, String)> = state
        .running
        .iter()
        .filter_map(|(id, e)| {
            let reference = e.last_event_at.unwrap_or(e.started_at);
            if now.duration_since(reference).as_millis() as i64 > stall_ms {
                Some((id.clone(), e.issue.identifier.clone()))
            } else {
                None
            }
        })
        .collect();

    for (issue_id, identifier) in stalled {
        tracing::warn!(issue_id = %issue_id, %identifier, stall_timeout_ms = stall_ms, "stall timeout exceeded; terminating worker");
        let attempt = state
            .running
            .get(&issue_id)
            .and_then(|e| e.retry_attempt)
            .unwrap_or(0)
            + 1;
        if let Some(entry) = state.running.remove(&issue_id) {
            finalize_issue_runtime(state, &entry, "stalled: no activity");
            abort_and_run_after_run(shared, entry.handle, &identifier).await;
        }
        let delay = backoff_delay_ms(attempt, shared.config.max_retry_backoff_ms);
        schedule_retry(state, tx, &issue_id, &identifier, attempt, delay, Some("stalled: no activity".to_string()));
    }
}

async fn handle_retry_fired(shared: &Shared, state: &mut OrchestratorState, tx: &mpsc::UnboundedSender<OrchMsg>, issue_id: String, generation: u64) {
    let Some(entry) = state.retry_attempts.get(&issue_id) else {
        return;
    };
    if entry.generation != generation {
        return; // superseded by a newer retry schedule
    }
    let entry = state.retry_attempts.remove(&issue_id).unwrap();

    let refreshed = match shared.tracker.fetch_issues_by_ids(std::slice::from_ref(&issue_id)).await {
        Ok(r) => r,
        Err(_) => {
            let next_attempt = entry.attempt + 1;
            let delay = backoff_delay_ms(next_attempt, shared.config.max_retry_backoff_ms);
            schedule_retry(state, tx, &issue_id, &entry.identifier, next_attempt, delay, Some("retry refresh failed".to_string()));
            return;
        }
    };

    let Some(issue) = refreshed.into_iter().next() else {
        state.claimed.remove(&issue_id);
        return;
    };

    let normalized = issue.normalized_state();
    let active = shared.config.active_states.iter().any(|s| s.trim().to_lowercase() == normalized);
    let routable = issue.is_routable(&shared.config.required_labels);

    if !active || !routable {
        state.claimed.remove(&issue_id);
        return;
    }

    if has_available_slot(&shared.config, state, &normalized) {
        dispatch_issue(shared, state, tx, issue, Some(entry.attempt)).await;
    } else {
        let next_attempt = entry.attempt + 1;
        let delay = backoff_delay_ms(next_attempt, shared.config.max_retry_backoff_ms);
        schedule_retry(state, tx, &issue_id, &entry.identifier, next_attempt, delay, Some("no available orchestrator slots".to_string()));
    }
}

async fn handle_msg(shared: &Shared, state: &mut OrchestratorState, tx: &mpsc::UnboundedSender<OrchMsg>, msg: OrchMsg) {
    match msg {
        OrchMsg::SessionStarted { issue_id, session_id } => {
            if let Some(e) = state.running.get_mut(&issue_id) {
                e.session_id = session_id;
            }
        }
        OrchMsg::TurnStarted { issue_id } => {
            if let Some(e) = state.running.get_mut(&issue_id) {
                e.turn_count += 1;
                let (identifier, title) = (e.issue.identifier.clone(), e.issue.title.clone());
                state.metrics.turns_started += 1;
                state.metrics.issue_entry(&identifier, &title).turns += 1;
            }
        }
        OrchMsg::AgentEvent { issue_id, event } => {
            let session_id = state.running.get(&issue_id).map(|e| e.session_id.clone()).unwrap_or_default();
            tracing::info!(issue_id = %issue_id, session_id = %session_id, event = %event.event, at = %event.timestamp, message = ?event.message, "agent event");

            let is_tool_call = event.event == "tool_call";
            let tool_name = event.message.clone().unwrap_or_default();

            if let Some(e) = state.running.get_mut(&issue_id) {
                e.last_event = Some(event.event.clone());
                e.last_event_at = Some(Instant::now());
                if let Some(m) = &event.message {
                    e.last_message = Some(m.clone());
                }
                let (identifier, title) = (e.issue.identifier.clone(), e.issue.title.clone());

                if let Some(u) = &event.usage {
                    e.tokens.input_tokens += u.input_tokens;
                    e.tokens.output_tokens += u.output_tokens;
                    e.tokens.total_tokens += u.total_tokens;

                    state.metrics.input_tokens += u.input_tokens;
                    state.metrics.output_tokens += u.output_tokens;
                    state.metrics.total_tokens += u.total_tokens;
                    let im = state.metrics.issue_entry(&identifier, &title);
                    im.input_tokens += u.input_tokens;
                    im.output_tokens += u.output_tokens;
                }

                if is_tool_call {
                    e.tool_call_count += 1;
                    state.metrics.tool_calls += 1;
                    *state.metrics.tool_call_counts.entry(tool_name).or_insert(0) += 1;
                    state.metrics.issue_entry(&identifier, &title).tool_calls += 1;
                }
            }
        }
        OrchMsg::WorkerExit { issue_id, reason } => {
            let Some(entry) = state.running.remove(&issue_id) else {
                return;
            };
            match reason {
                ExitReason::Normal => {
                    finalize_issue_runtime(state, &entry, "worker exited normally");
                    state.completed.insert(issue_id.clone());
                    tracing::info!(issue_id = %issue_id, identifier = %entry.issue.identifier, "worker exited normally; scheduling continuation check");
                    schedule_retry(state, tx, &issue_id, &entry.issue.identifier, 1, 1_000, None);
                }
                ExitReason::Error(err) => {
                    finalize_issue_runtime(state, &entry, &format!("error: {err}"));
                    let next_attempt = entry.retry_attempt.unwrap_or(0) + 1;
                    let delay = backoff_delay_ms(next_attempt, shared.config.max_retry_backoff_ms);
                    tracing::warn!(issue_id = %issue_id, identifier = %entry.issue.identifier, error = %err, next_attempt, delay_ms = delay, "worker exited abnormally; scheduling retry");
                    schedule_retry(state, tx, &issue_id, &entry.issue.identifier, next_attempt, delay, Some(err));
                }
            }
        }
        OrchMsg::RetryFired { issue_id, generation } => {
            handle_retry_fired(shared, state, tx, issue_id, generation).await;
        }
    }
}

// ---------------------------------------------------------------------------------
// Worker: Section 16.5 run_agent_attempt, restructured so `after_run` always executes
// once the workspace exists, per Section 9.4 (the illustrative pseudocode omits the
// before_run-failure case; the normative hook contract does not).
// ---------------------------------------------------------------------------------

async fn run_agent_attempt(
    issue_id: String,
    issue: Issue,
    attempt: Option<u32>,
    snapshot: DispatchSnapshot,
    tx: mpsc::UnboundedSender<OrchMsg>,
) {
    let cfg = &snapshot.config;

    let workspace = match snapshot
        .workspace_mgr
        .create_for_issue(&issue.identifier, cfg.hook_after_create.as_deref(), cfg.hook_timeout_ms)
        .await
    {
        Ok(ws) => ws,
        Err(e) => {
            let _ = tx.send(OrchMsg::WorkerExit {
                issue_id,
                reason: ExitReason::Error(format!("workspace error: {e}")),
            });
            return;
        }
    };

    let reason = run_attempt_body(&issue_id, issue, attempt, &workspace.path, &snapshot, &tx).await;

    if let Some(script) = &cfg.hook_after_run
        && let Err(e) = hooks::run_hook("after_run", script, &workspace.path, cfg.hook_timeout_ms).await
    {
        tracing::warn!(issue_id = %issue_id, error = %e, "after_run hook failed (ignored)");
    }

    let _ = tx.send(OrchMsg::WorkerExit { issue_id, reason });
}

async fn run_attempt_body(
    issue_id: &str,
    mut issue: Issue,
    attempt: Option<u32>,
    workspace_path: &Path,
    snapshot: &DispatchSnapshot,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) -> ExitReason {
    let cfg = &snapshot.config;

    if let Some(script) = &cfg.hook_before_run
        && let Err(e) = hooks::run_hook("before_run", script, workspace_path, cfg.hook_timeout_ms).await
    {
        return ExitReason::Error(format!("before_run hook error: {e}"));
    }

    if let Err(e) = snapshot.workspace_mgr.validate_agent_cwd(workspace_path) {
        return ExitReason::Error(format!("workspace safety check failed: {e}"));
    }

    let title = format!("{}: {}", issue.identifier, issue.title);
    let mut session = match snapshot.agent_backend.start_session(workspace_path, &issue.id, &title).await {
        Ok(s) => s,
        Err(e) => return ExitReason::Error(format!("agent session startup error: {e}")),
    };

    let _ = tx.send(OrchMsg::SessionStarted {
        issue_id: issue_id.to_string(),
        session_id: session.session_id().to_string(),
    });

    let max_turns = cfg.max_turns;
    let mut turn_number: u32 = 1;
    let exit: ExitReason;

    loop {
        let prompt = match render_turn_prompt(&snapshot.prompt_template, &issue, attempt, turn_number, max_turns) {
            Ok(p) => p,
            Err(e) => {
                exit = ExitReason::Error(format!("prompt error: {e}"));
                break;
            }
        };

        let _ = tx.send(OrchMsg::TurnStarted { issue_id: issue_id.to_string() });

        let turn_result = run_one_turn(session.as_mut(), &prompt, issue_id, tx).await;

        match turn_result {
            Ok(TurnOutcome::Completed { .. }) => {}
            Ok(TurnOutcome::Failed { reason }) => {
                exit = ExitReason::Error(format!("agent turn error: {reason}"));
                break;
            }
            Err(e) => {
                exit = ExitReason::Error(format!("agent turn error: {e}"));
                break;
            }
        }

        let refreshed = match snapshot.tracker.fetch_issues_by_ids(&[issue.id.clone()]).await {
            Ok(r) => r,
            Err(e) => {
                exit = ExitReason::Error(format!("issue state refresh error: {e}"));
                break;
            }
        };
        let Some(next_issue) = refreshed.into_iter().next() else {
            exit = ExitReason::Normal;
            break;
        };
        issue = next_issue;

        let normalized = issue.normalized_state();
        let active = cfg.active_states.iter().any(|s| s.trim().to_lowercase() == normalized);
        if !active || !issue.is_routable(&cfg.required_labels) {
            exit = ExitReason::Normal;
            break;
        }
        if turn_number >= max_turns {
            exit = ExitReason::Normal;
            break;
        }
        turn_number += 1;
    }

    session.stop().await;
    exit
}

async fn run_one_turn(
    session: &mut dyn AgentSession,
    prompt: &str,
    issue_id: &str,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) -> Result<TurnOutcome, crate::agent::AgentError> {
    let (etx, mut erx) = mpsc::unbounded_channel::<AgentEvent>();
    let fwd_issue_id = issue_id.to_string();
    let fwd_tx = tx.clone();
    let forward = tokio::spawn(async move {
        while let Some(ev) = erx.recv().await {
            let _ = fwd_tx.send(OrchMsg::AgentEvent {
                issue_id: fwd_issue_id.clone(),
                event: ev,
            });
        }
    });
    let result = session.run_turn(prompt, etx).await;
    let _ = forward.await;
    result
}

fn render_turn_prompt(
    template_str: &str,
    issue: &Issue,
    attempt: Option<u32>,
    turn_number: u32,
    max_turns: u32,
) -> Result<String, template::TemplateError> {
    if turn_number == 1 {
        let ctx = json!({
            "issue": serde_json::to_value(issue).unwrap_or(serde_json::Value::Null),
            "attempt": attempt,
        });
        template::render(template_str, &ctx)
    } else {
        Ok(format!(
            "Continue working on {}: {}. This is turn {turn_number} of {max_turns}. \
             Re-check the issue tracker state and your prior progress, then continue or \
             report completion.",
            issue.identifier, issue.title
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_shared(hook_after_run: &str, workspace_root: PathBuf, tracker_dir: &std::path::Path) -> Shared {
        let cfg_yaml: serde_yaml::Value = serde_yaml::from_str("tracker:\n  kind: local\n").unwrap();
        let mut cfg = config::resolve(&cfg_yaml, Path::new(".")).unwrap();
        cfg.hook_after_run = Some(hook_after_run.to_string());
        cfg.workspace_root = workspace_root.clone();

        let provider: serde_yaml::Value =
            serde_yaml::from_str(&format!("dir: {:?}", tracker_dir)).unwrap();
        let tracker_adapter = tracker::build("local", &provider, Path::new(".")).unwrap();

        Shared {
            config: cfg,
            prompt_template: String::new(),
            tracker: Arc::from(tracker_adapter),
            agent_backend: Arc::new(claude::ClaudeBackend {
                command: "claude".to_string(),
                extra_args: Vec::new(),
                model: None,
                permission_mode: "bypassPermissions".to_string(),
                turn_timeout_ms: 1000,
                mcp_wiring: None,
            }),
            workspace_mgr: Arc::new(WorkspaceManager::new(workspace_root)),
        }
    }

    /// Regression test for the AR-8 incident: a running worker that gets aborted by
    /// reconciliation (terminal state, non-active state, or a stall timeout) must
    /// still get its `after_run` hook run before its workspace is touched — not just
    /// a worker that exits on its own via the normal WorkerExit message path.
    #[tokio::test]
    async fn abort_and_run_after_run_runs_the_hook_against_a_real_workspace() {
        let root = tempdir().unwrap();
        let tracker_dir = tempdir().unwrap();
        let identifier = "AR-TEST";
        let shared = test_shared(
            "echo ran > after_run_marker.txt",
            root.path().to_path_buf(),
            tracker_dir.path(),
        );
        let workspace = shared.workspace_mgr.path_for(identifier);
        std::fs::create_dir_all(&workspace).unwrap();

        // A task that would run forever if not aborted -- stands in for a worker
        // that's mid-turn (e.g. waiting on a `claude` subprocess) when reconciliation
        // decides to terminate it.
        let handle = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });

        abort_and_run_after_run(&shared, handle, identifier).await;

        assert!(
            workspace.join("after_run_marker.txt").exists(),
            "after_run hook should have run against the workspace after the abort"
        );
    }

    #[tokio::test]
    async fn abort_and_run_after_run_is_a_noop_without_a_configured_hook() {
        let root = tempdir().unwrap();
        let tracker_dir = tempdir().unwrap();
        let identifier = "AR-TEST-2";
        let mut shared = test_shared(
            "echo should-not-run > marker.txt",
            root.path().to_path_buf(),
            tracker_dir.path(),
        );
        shared.config.hook_after_run = None;
        let workspace = shared.workspace_mgr.path_for(identifier);
        std::fs::create_dir_all(&workspace).unwrap();

        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });

        abort_and_run_after_run(&shared, handle, identifier).await;

        assert!(!workspace.join("marker.txt").exists());
    }
}

