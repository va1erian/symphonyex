//! Orchestrator: the single authority over dispatch, retry, and reconciliation state
//! (Section 7, 8, 16). One task owns all mutable scheduling state directly; worker
//! tasks only ever report back over a channel, never mutate shared state themselves.

use crate::agent::{
    AgentBackend, AgentEvent, AgentSession, TokenUsage, TurnOutcome, claude, codex, opencode,
};
use crate::config::{self, AgentBackendKind, EffectiveConfig, StageFailureAction};
use crate::container::{self, ContainerHandle};
use crate::domain::Issue;
use crate::envsub;
use crate::hooks;
use crate::metrics::Metrics;
use crate::status;
use crate::template;
use crate::tracker::{self, TrackerAdapter};
use crate::workflow;
use crate::workspace::{DockerContext, WorkspaceManager};
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
    /// Feeds the SQLite event log's dedicated writer task (`eventlog::spawn_writer`) --
    /// see that module's doc comment for why this is a channel rather than a shared
    /// connection. Not part of `DispatchSnapshot`: only `handle_msg`/`dispatch_issue`
    /// (which already own `shared`) ever record events, not the per-issue worker task
    /// itself.
    event_tx: mpsc::UnboundedSender<crate::eventlog::NewEvent>,
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

/// Best-effort: a full channel or a writer task that's already exited just means one
/// history row is lost, never something worth propagating into the dispatch path.
fn record_event(shared: &Shared, ev: crate::eventlog::NewEvent) {
    let _ = shared.event_tx.send(ev);
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
    /// Current pipeline stage id (`pipeline.stages[].id`), when `pipeline.enabled` --
    /// `None` for the legacy single-stage path or before the first stage has started.
    /// Surfaced on the dashboard (`status::RunningRow::stage`) the same way `last_event`
    /// already is, so a human watching a multi-stage cycle can see which stage is
    /// running without opening `/events`.
    current_stage: Option<String>,
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
    /// Set when a turn failure was classified as a plan usage-limit hit
    /// (`is_plan_rate_limited`); while `Instant::now()` is before this, `on_tick` skips
    /// dispatching *new* candidates -- every concurrently running issue shares the same
    /// account, so a fresh dispatch during the pause would just fail immediately too.
    /// Already-scheduled retries aren't gated by this (they were rescheduled with the
    /// same `rate_limit_pause_ms` delay when the pause was set, so they naturally land
    /// around when it's expected to lift); this only stops *additional* new dispatch in
    /// the meantime. Cleared automatically once the deadline passes.
    rate_limited_until: Option<Instant>,
}

impl OrchestratorState {
    fn next_generation(&mut self, issue_id: &str) -> u64 {
        let g = self
            .retry_generation
            .entry(issue_id.to_string())
            .or_insert(0);
        *g += 1;
        *g
    }
}

enum ExitReason {
    Normal,
    Error(String),
}

enum OrchMsg {
    SessionStarted {
        issue_id: String,
        session_id: String,
    },
    TurnStarted {
        issue_id: String,
    },
    AgentEvent {
        issue_id: String,
        event: AgentEvent,
    },
    WorkerExit {
        issue_id: String,
        reason: ExitReason,
    },
    RetryFired {
        issue_id: String,
        generation: u64,
    },
    /// A pipeline stage began running (`pipeline.enabled` only -- see `run_pipeline`).
    StageStarted {
        issue_id: String,
        stage_id: String,
    },
    /// A pipeline stage finished, however it finished (`outcome` is a short
    /// human-readable label: "completed", "ended by issue state", "failed", or
    /// "failed, skipped"). Recorded as a `stage_finished` event regardless of outcome,
    /// so the pipeline's progress is fully visible on `/events` even for a cycle that's
    /// ultimately blocked or retried.
    StageFinished {
        issue_id: String,
        stage_id: String,
        outcome: String,
    },
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
    // `repo.pull_request` needs the MCP subprocess spawned to expose open_pull_request
    // (and, when `repo.evidence` is also on, attach_evidence too -- `config::resolve`
    // already rejects `evidence: true` without `pull_request: true`, so gating on
    // `pull_request` alone here is enough) even when the tracker itself has no tools of
    // its own (e.g. tracker.kind: local), since these are properties of repo: config,
    // not of the tracker.
    let repo_pr_json = cfg
        .repo
        .as_ref()
        .filter(|r| r.pull_request)
        .map(serde_json::to_string)
        .transpose()?;
    let mcp_wiring = if tracker_adapter.agent_tool_specs().is_empty() && repo_pr_json.is_none() {
        None
    } else {
        Some(claude::McpToolWiring {
            tracker_kind: cfg.tracker_kind.clone(),
            tracker_provider_json: serde_json::to_string(&cfg.tracker_provider)?,
            workflow_dir: cfg.workflow_dir.clone(),
            repo_pr_json,
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
            workflow_dir: cfg.workflow_dir.clone(),
        }),
        AgentBackendKind::Codex => Arc::new(codex::CodexBackend {
            command: cfg.codex.command.clone(),
            approval_policy: cfg.codex.approval_policy.clone(),
            thread_sandbox: cfg.codex.thread_sandbox.clone(),
            turn_sandbox_policy: cfg.codex.turn_sandbox_policy.clone(),
            turn_timeout_ms: cfg.codex.turn_timeout_ms,
            read_timeout_ms: cfg.codex.read_timeout_ms,
        }),
        AgentBackendKind::OpenCode => Arc::new(opencode::OpenCodeBackend {
            command: cfg.opencode.command.clone(),
            model: cfg.opencode.model.clone(),
            extra_args: cfg.opencode.args.clone(),
            auto_approve: cfg.opencode.auto_approve,
            turn_timeout_ms: cfg.opencode.turn_timeout_ms,
            permission_config: None,
            mcp_wiring,
            workflow_dir: cfg.workflow_dir.clone(),
        }),
    };

    let prompt_template = if wf.prompt_template.is_empty() {
        DEFAULT_PROMPT.to_string()
    } else {
        wf.prompt_template
    };

    let docker_ctx = if cfg.docker.enabled {
        // `SYMPHONY_DAEMON_VOLUME`: set by `symphony daemon start` when Symphony
        // itself is running inside its own container (Docker-outside-of-Docker) --
        // per-ticket sibling containers must then mount this named volume rather than
        // bind-mount a host path, since a path meaningful only inside *this*
        // container's own mount namespace (e.g. `/project`) wouldn't resolve to
        // anything on the host, where sibling containers are actually created. See
        // `container::MountSource`'s doc comment for the full explanation.
        let mount = match std::env::var("SYMPHONY_DAEMON_VOLUME") {
            Ok(volume) if !volume.trim().is_empty() => container::MountSource::NamedVolume(volume),
            _ => container::MountSource::HostPath(cfg.workflow_dir.clone()),
        };
        // Forward exactly the secrets this config references (repo.token, the
        // tracker's own token, anything else `$VAR`-shaped) into per-ticket
        // containers -- see `envsub::collect_var_refs`'s doc comment for why this is
        // necessary at all: Docker doesn't inherit the host environment into a
        // container the way a plain child process would.
        let env_passthrough = envsub::collect_var_refs(&wf.config);
        // Only actually resolve the host's Claude Code login when the operator opted
        // in via `workspace.docker.mount_claude_credentials` -- see that field's doc
        // comment in config.rs. `resolve_claude_credentials_path` itself already
        // handles the daemonized-vs-direct distinction (checks
        // `SYMPHONY_HOST_CLAUDE_CREDENTIALS`, forwarded by `daemon::start`, before
        // falling back to this process's own `USERPROFILE`/`HOME`).
        let claude_credentials_path = cfg
            .docker
            .mount_claude_credentials
            .then(envsub::resolve_claude_credentials_path)
            .flatten();
        if cfg.docker.mount_claude_credentials && claude_credentials_path.is_none() {
            tracing::warn!(
                "workspace.docker.mount_claude_credentials is enabled but no Claude Code \
                 credentials file was found -- containers will run unauthenticated"
            );
        }
        Some(DockerContext {
            workflow_dir: cfg.workflow_dir.clone(),
            mount,
            env_passthrough,
            claude_credentials_path,
            config: cfg.docker.clone(),
        })
    } else {
        None
    };
    let workspace_mgr = WorkspaceManager::new(cfg.workspace_root.clone()).with_docker(docker_ctx);
    std::fs::create_dir_all(workspace_mgr.root())?;

    // Same directory convention `symphony-report.html` already defaults to, so the
    // event log lands in the same place (including inside a daemonized Symphony's own
    // persistent volume) with no new config needed.
    let event_tx =
        crate::eventlog::spawn_writer(cfg.workflow_dir.join(crate::eventlog::DB_FILENAME));

    Ok(Shared {
        config: cfg,
        prompt_template,
        tracker: Arc::from(tracker_adapter),
        agent_backend,
        workspace_mgr: Arc::new(workspace_mgr),
        event_tx,
    })
}

/// Section 8.6: remove workspaces for issues already in a terminal state at startup.
async fn startup_terminal_cleanup(shared: &Shared) {
    match shared
        .tracker
        .fetch_issues_by_states(&shared.config.terminal_states)
        .await
    {
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

/// Handed back to a caller that spawned `run_managed` (the multi-project service
/// manager, `src/service.rs`) so it can nest this project's own status/events/usage
/// pages into its own aggregate web UI, the same data `run`'s own `status_port`
/// branch below feeds directly into `status::serve`.
pub struct ProjectHandles {
    pub status_rx: tokio::sync::watch::Receiver<status::StatusSnapshot>,
    /// `EffectiveConfig::workflow_dir` -- `status::router` derives the eventlog db
    /// path from this itself.
    pub workflow_dir: PathBuf,
    /// Present when `swebot.chat.enabled` -- chat mode's store+worker. The
    /// multi-project service nests the web chat UI under `/projects/<id>/chat` only
    /// when `web_enabled` is set (the single-project status server serves it at
    /// `/chat` on the same condition).
    pub chat: Option<crate::swebot::chat::ChatHandles>,
    /// Backs the nested `/observability` page's "rescan now" action -- see
    /// `status::ObservabilityHandle`.
    pub observability: status::ObservabilityHandle,
}

/// Single-project entry point (Section 5): runs until the process is killed or the
/// workflow can't be loaded. Used directly by the CLI (`main.rs`) -- never
/// externally cancellable, matching today's "ctrl_c kills the whole process" model.
pub async fn run(
    workflow_path: PathBuf,
    status_port: Option<u16>,
    report_path_override: Option<PathBuf>,
) -> anyhow::Result<()> {
    // Never fired: `run`'s caller has no shutdown concept of its own, so this receiver
    // simply stays pending for the process lifetime, same as before this arm existed.
    let (_never_fired, shutdown) = tokio::sync::oneshot::channel();
    run_inner(
        workflow_path,
        status_port,
        report_path_override,
        shutdown,
        None,
    )
    .await
}

/// Multi-project entry point (`src/service.rs`): like `run`, but never opens its own
/// status port (the service aggregates status itself) and can be stopped from the
/// outside via `shutdown` when a project is deregistered, without killing the whole
/// service process. `handles_tx` delivers this project's `ProjectHandles` back to the
/// caller once they're available (right after startup), so the caller can nest this
/// project's dashboard into its own router before this function's polling loop --
/// which runs for as long as the project stays registered -- returns.
pub async fn run_managed(
    workflow_path: PathBuf,
    report_path_override: Option<PathBuf>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    handles_tx: tokio::sync::oneshot::Sender<ProjectHandles>,
) -> anyhow::Result<()> {
    run_inner(
        workflow_path,
        None,
        report_path_override,
        shutdown,
        Some(handles_tx),
    )
    .await
}

async fn run_inner(
    workflow_path: PathBuf,
    status_port: Option<u16>,
    report_path_override: Option<PathBuf>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
    handles_tx: Option<tokio::sync::oneshot::Sender<ProjectHandles>>,
) -> anyhow::Result<()> {
    let mut shared = build_shared(&workflow_path)?;
    tracing::info!(
        tracker_kind = %shared.config.tracker_kind,
        agent_backend = ?shared.config.agent_backend,
        poll_interval_ms = shared.config.poll_interval_ms,
        "symphony starting"
    );

    if shared.config.docker.enabled && !container::docker_available().await {
        anyhow::bail!(
            "workspace.docker.enabled is true but `docker` isn't reachable -- \
             is Docker Desktop running? (see README.md \"Docker mode\")"
        );
    }

    let report_path = report_path_override
        .unwrap_or_else(|| shared.config.workflow_dir.join("symphony-report.html"));
    tracing::info!(path = %report_path.display(), "usage report will be written here");

    startup_terminal_cleanup(&shared).await;

    let mut last_attempted_mtime = std::fs::metadata(&workflow_path)
        .ok()
        .and_then(|m| m.modified().ok());

    let (tx, mut rx) = mpsc::unbounded_channel::<OrchMsg>();
    let mut state = OrchestratorState::default();

    let (status_tx, status_rx) = tokio::sync::watch::channel(status::StatusSnapshot::default());
    let workflow_dir = shared.config.workflow_dir.clone();

    // Chat mode runs its worker headlessly even without --port (only the web UI's
    // HTTP surface is gated on a port).
    let chat = crate::swebot::chat::start(shared.config.clone(), shared.tracker.clone());

    // Backs `/observability`'s "rescan now" action -- see `status::ObservabilityHandle`.
    let observability_handle = status::ObservabilityHandle {
        repo: shared.config.repo.clone(),
        event_tx: shared.event_tx.clone(),
    };

    if let Some(port) = status_port {
        // Same daemonized-Symphony signal used for MountSource above: inside its own
        // container, loopback-only binding would make the dashboard unreachable even
        // with the port published (see status::serve_composite's doc comment for why).
        let bind_all_interfaces =
            std::env::var("SYMPHONY_DAEMON_VOLUME").is_ok_and(|v| !v.trim().is_empty());
        let status_rx_for_serve = status_rx.clone();
        let workflow_dir_for_serve = workflow_dir.clone();
        // Composite dashboard: the status router at the root, chat's UI nested under
        // /chat -- only when the web connector is enabled.
        let chat_router = chat
            .as_ref()
            .filter(|handles| handles.web_enabled)
            .map(|handles| crate::swebot::chat::web::router(handles.store.clone(), "/chat"));
        let observability_for_serve = observability_handle.clone();
        tokio::spawn(async move {
            if let Err(e) = status::serve_composite(
                port,
                bind_all_interfaces,
                status_rx_for_serve,
                workflow_dir_for_serve,
                chat_router,
                Some(observability_for_serve),
            )
            .await
            {
                tracing::error!(error = %e, "status server exited");
            }
        });
    }
    if let Some(handles_tx) = handles_tx {
        let _ = handles_tx.send(ProjectHandles {
            status_rx: status_rx.clone(),
            workflow_dir: workflow_dir.clone(),
            chat,
            observability: observability_handle,
        });
    }

    // Spawned once against this startup's config snapshot, like the status
    // dashboard above -- a `swebot:` config change (unlike most of `EffectiveConfig`)
    // needs a restart to take effect, not picked up by `maybe_reload`'s hot reload.
    if shared.config.swebot.enabled {
        let swebot_cfg = shared.config.clone();
        let swebot_tracker = shared.tracker.clone();
        tokio::spawn(async move {
            crate::swebot::run(swebot_cfg, swebot_tracker).await;
        });
    }

    // AIR-10 Observability Agent: both halves are gated on `observability.backend`
    // not being `none` (the default) -- see `observability::pre_merge::run`'s doc
    // comment for why the pre-merge scan (which needs no backend of its own) is
    // gated the same way as post-deploy validation (which does).
    if shared.config.observability.backend != crate::config::ObservabilityBackendKind::None {
        let pre_merge_cfg = shared.config.clone();
        let pre_merge_events = shared.event_tx.clone();
        tokio::spawn(async move {
            crate::observability::pre_merge::run(pre_merge_cfg, pre_merge_events).await;
        });

        let validation_cfg = shared.config.clone();
        let validation_events = shared.event_tx.clone();
        tokio::spawn(async move {
            crate::observability::production_validation::run(validation_cfg, validation_events)
                .await;
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
            _ = &mut shutdown => {
                tracing::info!(path = %workflow_path.display(), "project stopped (removed from service)");
                return Ok(());
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
        .iter()
        .map(|(issue_id, e)| status::RunningRow {
            issue_id: issue_id.clone(),
            identifier: e.issue.identifier.clone(),
            title: e.issue.title.clone(),
            session_id: e.session_id.clone(),
            started_secs_ago: now.duration_since(e.started_at).as_secs_f64(),
            turn_count: e.turn_count,
            tool_call_count: e.tool_call_count,
            last_event: e.last_event.clone(),
            last_message: e.last_message.clone(),
            stage: e.current_stage.clone(),
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
    let current_mtime = std::fs::metadata(workflow_path)
        .ok()
        .and_then(|m| m.modified().ok());
    if current_mtime == *last_attempted_mtime {
        return;
    }
    *last_attempted_mtime = current_mtime;

    match build_shared(workflow_path) {
        Ok(new_shared) => {
            let interval_changed =
                new_shared.config.poll_interval_ms != shared.config.poll_interval_ms;
            tracing::info!("workflow reloaded");
            *shared = new_shared;
            if interval_changed {
                *interval =
                    tokio::time::interval(Duration::from_millis(shared.config.poll_interval_ms));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "workflow reload failed; keeping last known good configuration");
        }
    }
}

async fn on_tick(
    shared: &Shared,
    state: &mut OrchestratorState,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) {
    reconcile(shared, state, tx).await;

    if let Some(until) = state.rate_limited_until {
        if Instant::now() < until {
            // Still paused after a plan usage-limit hit: skip dispatching *new*
            // candidates this tick (already-scheduled retries are unaffected -- they
            // carry their own `rate_limit_pause_ms` delay from when the pause was set).
            return;
        }
        state.rate_limited_until = None;
    }

    if let Err(e) = config::validate_for_dispatch(&shared.config, tracker::SUPPORTED_TRACKER_KINDS)
    {
        tracing::error!(error = %e, "dispatch preflight validation failed; skipping dispatch this tick");
        return;
    }

    let issues = match shared
        .tracker
        .fetch_issues_by_states(&shared.config.active_states)
        .await
    {
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
    if issue.id.is_empty()
        || issue.identifier.is_empty()
        || issue.title.is_empty()
        || issue.state.is_empty()
    {
        return false;
    }
    let normalized = issue.normalized_state();
    if !cfg
        .active_states
        .iter()
        .any(|s| s.trim().to_lowercase() == normalized)
    {
        return false;
    }
    if cfg
        .terminal_states
        .iter()
        .any(|s| s.trim().to_lowercase() == normalized)
    {
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
    i.created_at
        .map(|t| t.timestamp_millis())
        .unwrap_or(i64::MAX)
}

fn has_available_slot(
    cfg: &EffectiveConfig,
    state: &OrchestratorState,
    normalized_state: &str,
) -> bool {
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
        run_agent_attempt(
            issue_id_for_worker,
            issue_for_worker,
            attempt,
            snapshot,
            tx2,
        )
        .await;
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
            current_stage: None,
        },
    );
    state.claimed.insert(issue_id.clone());
    state.retry_attempts.remove(&issue_id);

    state.metrics.agents_spawned += 1;
    state
        .metrics
        .issue_entry(&identifier, &title)
        .dispatch_count += 1;

    tracing::info!(issue_id = %issue_id, identifier = %identifier, attempt = ?attempt, "dispatched");
    record_event(
        shared,
        crate::eventlog::NewEvent {
            issue_id,
            identifier,
            title,
            session_id: None,
            event_type: "dispatched".to_string(),
            message: attempt.map(|a| format!("attempt {a}")),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
        },
    );
}

fn backoff_delay_ms(attempt: u32, max_backoff_ms: u64) -> u64 {
    let pow = 2u64.saturating_pow(attempt.saturating_sub(1).min(32));
    10_000u64.saturating_mul(pow).min(max_backoff_ms)
}

/// Recognizes the `claude` CLI's own plan/account usage-limit message (observed live,
/// verbatim: `"You've hit your session limit · resets 12:30am (Europe/Paris)"`) so it
/// can be handled as a coordinated, much-longer pause instead of the normal per-issue
/// exponential backoff (see the `ExitReason::Error` handling in `handle_msg`). Matched
/// on the stable phrases rather than the whole sentence (which varies by exact reset
/// time/timezone) or a generic "rate limit" substring (which could also describe an
/// unrelated transient 429 that *should* use the normal short backoff).
fn is_plan_rate_limited(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains("session limit") || lower.contains("usage limit")
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
async fn abort_and_run_after_run(
    shared: &Shared,
    handle: tokio::task::JoinHandle<()>,
    identifier: &str,
) {
    handle.abort();
    let _ = handle.await; // resolves once the task has truly finished, Err(Cancelled) is expected

    // No live `Workspace` handle here (only `identifier`), so the container -- if
    // Docker mode is enabled -- is re-derived by name rather than looked up; this is
    // safe because the name is deterministic (`container::derive_container_name`) and
    // the container's lifecycle is still owned by `WorkspaceManager`, which hasn't
    // torn it down yet at this point in `terminate_running`/`reconcile_stalled`.
    let container = docker_container_for(shared, identifier);

    // In container mode, `handle.await` above only guarantees the *task* finished --
    // the agent turn's own `ContainerKillGuard` cleanup (for the in-container `claude`
    // process, as opposed to the host-side `docker exec` client `kill_on_drop`
    // already handles) runs from that guard's `Drop`, which can only `tokio::spawn` a
    // detached fire-and-forget kill since `Drop` can't be `async`. That gives no
    // guarantee the in-container process is actually dead yet -- issue our own
    // explicitly-awaited kill here too before touching anything that assumes it is
    // (namely `after_run` below, e.g. `git commit && git push`), closing the same
    // class of race this function's own `handle.await` above already closes at the
    // host level (see this function's top-level doc comment and the AR-8 incident it
    // references).
    if let Some(c) = &container {
        container::kill_process_by_name(&c.name, shared.config.effective_command()).await;
    }

    let Some(script) = &shared.config.hook_after_run else {
        return;
    };
    let path = shared.workspace_mgr.path_for(identifier);
    if !path.is_dir() {
        return;
    }
    if let Err(e) = hooks::run_hook_maybe_containerized(
        "after_run",
        script,
        &shared.config.workflow_dir,
        &path,
        shared.config.hook_timeout_ms,
        container.as_ref(),
    )
    .await
    {
        tracing::warn!(%identifier, error = %e, "after_run hook failed (ignored) [reconciliation-triggered termination]");
    }
}

/// Deterministically re-derive the Docker-mode container handle for `identifier`
/// without needing a live `Workspace` (see `abort_and_run_after_run`'s doc comment).
fn docker_container_for(shared: &Shared, identifier: &str) -> Option<ContainerHandle> {
    if !shared.config.docker.enabled {
        return None;
    }
    Some(ContainerHandle {
        name: container::derive_container_name(&shared.config.workflow_dir, identifier),
        container_root: Path::new(container::CONTAINER_PROJECT_ROOT).to_path_buf(),
    })
}

/// Turns/tool-calls/tokens are already folded into `state.metrics` live as they
/// happen (see `handle_msg`); this only adds what can't be known until the attempt
/// ends: wall-clock runtime, and (via `outcome`) a human-readable last-outcome note.
fn finalize_issue_runtime(state: &mut OrchestratorState, entry: &RunningEntry, outcome: &str) {
    let seconds = entry.started_at.elapsed().as_secs_f64();
    state.metrics.seconds_running += seconds;
    let issue_metrics = state
        .metrics
        .issue_entry(&entry.issue.identifier, &entry.issue.title);
    issue_metrics.seconds_running += seconds;
    issue_metrics.last_outcome = Some(outcome.to_string());
}

/// Section 8.5: stall detection (Part A) + tracker state refresh (Part B).
async fn reconcile(
    shared: &Shared,
    state: &mut OrchestratorState,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) {
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
                let terminal = shared
                    .config
                    .terminal_states
                    .iter()
                    .any(|s| s.trim().to_lowercase() == normalized);
                let active = shared
                    .config
                    .active_states
                    .iter()
                    .any(|s| s.trim().to_lowercase() == normalized);
                let routable = issue.is_routable(&shared.config.required_labels);

                if terminal {
                    terminate_running(
                        state,
                        shared,
                        &issue.id,
                        true,
                        "reached terminal tracker state",
                    )
                    .await;
                } else if active && routable {
                    if let Some(entry) = state.running.get_mut(&issue.id) {
                        entry.issue = issue;
                    }
                } else {
                    terminate_running(state, shared, &issue.id, false, "no longer active/routable")
                        .await;
                }
            }
            for missing_id in running_ids.iter().filter(|id| !seen.contains(*id)) {
                terminate_running(
                    state,
                    shared,
                    missing_id,
                    false,
                    "no longer visible in tracker",
                )
                .await;
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "reconciliation refresh failed; keeping workers running");
        }
    }
}

async fn reconcile_stalled(
    shared: &Shared,
    state: &mut OrchestratorState,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) {
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
        // Everything already tracked on the entry, bundled into the one line that
        // actually fires when a stall happens -- diagnosing "why did this go quiet"
        // after the fact otherwise means grepping back through the full "agent
        // event" log by hand for this issue_id/session_id to reconstruct the same
        // picture (last event type/text, how long it had been silent, how much work
        // it had done before going quiet).
        if let Some(e) = state.running.get(&issue_id) {
            let silent_for_ms = e
                .last_event_at
                .unwrap_or(e.started_at)
                .elapsed()
                .as_millis();
            tracing::warn!(
                issue_id = %issue_id,
                %identifier,
                stall_timeout_ms = stall_ms,
                session_id = %e.session_id,
                silent_for_ms,
                running_for_ms = e.started_at.elapsed().as_millis(),
                turn_count = e.turn_count,
                tool_call_count = e.tool_call_count,
                last_event = ?e.last_event,
                last_message = ?e.last_message,
                total_tokens = e.tokens.total_tokens,
                "stall timeout exceeded; terminating worker"
            );
        }
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
        schedule_retry(
            state,
            tx,
            &issue_id,
            &identifier,
            attempt,
            delay,
            Some("stalled: no activity".to_string()),
        );
    }
}

async fn handle_retry_fired(
    shared: &Shared,
    state: &mut OrchestratorState,
    tx: &mpsc::UnboundedSender<OrchMsg>,
    issue_id: String,
    generation: u64,
) {
    let Some(entry) = state.retry_attempts.get(&issue_id) else {
        return;
    };
    if entry.generation != generation {
        return; // superseded by a newer retry schedule
    }
    let entry = state.retry_attempts.remove(&issue_id).unwrap();
    // RetryEntry has no title field (it's a pure scheduling record) -- identifier
    // stands in for title here rather than an extra tracker fetch just for a log row;
    // the eventual dispatch (if one happens) logs the real title via its own
    // "dispatched" event.
    record_event(
        shared,
        crate::eventlog::NewEvent {
            issue_id: issue_id.clone(),
            identifier: entry.identifier.clone(),
            title: entry.identifier.clone(),
            session_id: None,
            event_type: "retry_fired".to_string(),
            message: Some(format!("attempt {}", entry.attempt)),
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
        },
    );

    let refreshed = match shared
        .tracker
        .fetch_issues_by_ids(std::slice::from_ref(&issue_id))
        .await
    {
        Ok(r) => r,
        Err(_) => {
            let next_attempt = entry.attempt + 1;
            let delay = backoff_delay_ms(next_attempt, shared.config.max_retry_backoff_ms);
            schedule_retry(
                state,
                tx,
                &issue_id,
                &entry.identifier,
                next_attempt,
                delay,
                Some("retry refresh failed".to_string()),
            );
            return;
        }
    };

    let Some(issue) = refreshed.into_iter().next() else {
        state.claimed.remove(&issue_id);
        return;
    };

    let normalized = issue.normalized_state();
    let active = shared
        .config
        .active_states
        .iter()
        .any(|s| s.trim().to_lowercase() == normalized);
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
        schedule_retry(
            state,
            tx,
            &issue_id,
            &entry.identifier,
            next_attempt,
            delay,
            Some("no available orchestrator slots".to_string()),
        );
    }
}

async fn handle_msg(
    shared: &Shared,
    state: &mut OrchestratorState,
    tx: &mpsc::UnboundedSender<OrchMsg>,
    msg: OrchMsg,
) {
    match msg {
        OrchMsg::SessionStarted {
            issue_id,
            session_id,
        } => {
            if let Some(e) = state.running.get_mut(&issue_id) {
                e.session_id = session_id.clone();
                let (identifier, title) = (e.issue.identifier.clone(), e.issue.title.clone());
                record_event(
                    shared,
                    crate::eventlog::NewEvent {
                        issue_id,
                        identifier,
                        title,
                        session_id: Some(session_id),
                        event_type: "session_started".to_string(),
                        message: None,
                        input_tokens: None,
                        output_tokens: None,
                        total_tokens: None,
                    },
                );
            }
        }
        OrchMsg::TurnStarted { issue_id } => {
            if let Some(e) = state.running.get_mut(&issue_id) {
                e.turn_count += 1;
                let (identifier, title) = (e.issue.identifier.clone(), e.issue.title.clone());
                let session_id = e.session_id.clone();
                state.metrics.turns_started += 1;
                state.metrics.issue_entry(&identifier, &title).turns += 1;
                record_event(
                    shared,
                    crate::eventlog::NewEvent {
                        issue_id,
                        identifier,
                        title,
                        session_id: Some(session_id),
                        event_type: "turn_started".to_string(),
                        message: None,
                        input_tokens: None,
                        output_tokens: None,
                        total_tokens: None,
                    },
                );
            }
        }
        OrchMsg::AgentEvent { issue_id, event } => {
            let session_id = state
                .running
                .get(&issue_id)
                .map(|e| e.session_id.clone())
                .unwrap_or_default();
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

                record_event(
                    shared,
                    crate::eventlog::NewEvent {
                        issue_id,
                        identifier,
                        title,
                        session_id: Some(session_id),
                        event_type: event.event,
                        message: event.message,
                        input_tokens: event.usage.as_ref().map(|u| u.input_tokens),
                        output_tokens: event.usage.as_ref().map(|u| u.output_tokens),
                        total_tokens: event.usage.as_ref().map(|u| u.total_tokens),
                    },
                );
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
                    record_event(
                        shared,
                        crate::eventlog::NewEvent {
                            issue_id: issue_id.clone(),
                            identifier: entry.issue.identifier.clone(),
                            title: entry.issue.title.clone(),
                            session_id: Some(entry.session_id.clone()),
                            event_type: "worker_exit".to_string(),
                            message: Some("worker exited normally".to_string()),
                            input_tokens: None,
                            output_tokens: None,
                            total_tokens: None,
                        },
                    );
                    schedule_retry(
                        state,
                        tx,
                        &issue_id,
                        &entry.issue.identifier,
                        1,
                        1_000,
                        None,
                    );
                }
                ExitReason::Error(err) => {
                    finalize_issue_runtime(state, &entry, &format!("error: {err}"));
                    let rate_limited = is_plan_rate_limited(&err);
                    // A plan usage-limit failure isn't the ticket's fault and won't
                    // resolve itself on the normal few-minutes exponential curve --
                    // don't escalate the attempt counter for it, and wait the fixed,
                    // much longer `rate_limit_pause_ms` instead of `backoff_delay_ms`.
                    // Also pause *new* dispatch account-wide (`rate_limited_until`,
                    // checked in `on_tick`): every concurrently running issue shares the
                    // same underlying account, so a fresh dispatch would just hit the
                    // same wall immediately.
                    let next_attempt = if rate_limited {
                        entry.retry_attempt.unwrap_or(0).max(1)
                    } else {
                        entry.retry_attempt.unwrap_or(0) + 1
                    };
                    let delay = if rate_limited {
                        state.rate_limited_until = Some(
                            Instant::now()
                                + Duration::from_millis(shared.config.rate_limit_pause_ms),
                        );
                        shared.config.rate_limit_pause_ms
                    } else {
                        backoff_delay_ms(next_attempt, shared.config.max_retry_backoff_ms)
                    };
                    tracing::warn!(issue_id = %issue_id, identifier = %entry.issue.identifier, error = %err, next_attempt, delay_ms = delay, rate_limited, "worker exited abnormally; scheduling retry");
                    record_event(
                        shared,
                        crate::eventlog::NewEvent {
                            issue_id: issue_id.clone(),
                            identifier: entry.issue.identifier.clone(),
                            title: entry.issue.title.clone(),
                            session_id: Some(entry.session_id.clone()),
                            event_type: "worker_exit".to_string(),
                            message: Some(format!("error: {err}")),
                            input_tokens: None,
                            output_tokens: None,
                            total_tokens: None,
                        },
                    );
                    schedule_retry(
                        state,
                        tx,
                        &issue_id,
                        &entry.issue.identifier,
                        next_attempt,
                        delay,
                        Some(err),
                    );
                }
            }
        }
        OrchMsg::RetryFired {
            issue_id,
            generation,
        } => {
            handle_retry_fired(shared, state, tx, issue_id, generation).await;
        }
        OrchMsg::StageStarted { issue_id, stage_id } => {
            if let Some(e) = state.running.get_mut(&issue_id) {
                e.current_stage = Some(stage_id.clone());
                let (identifier, title) = (e.issue.identifier.clone(), e.issue.title.clone());
                let session_id = e.session_id.clone();
                record_event(
                    shared,
                    crate::eventlog::NewEvent {
                        issue_id,
                        identifier,
                        title,
                        session_id: Some(session_id),
                        event_type: "stage_started".to_string(),
                        message: Some(stage_id),
                        input_tokens: None,
                        output_tokens: None,
                        total_tokens: None,
                    },
                );
            }
        }
        OrchMsg::StageFinished {
            issue_id,
            stage_id,
            outcome,
        } => {
            if let Some(e) = state.running.get(&issue_id) {
                let (identifier, title) = (e.issue.identifier.clone(), e.issue.title.clone());
                let session_id = e.session_id.clone();
                record_event(
                    shared,
                    crate::eventlog::NewEvent {
                        issue_id,
                        identifier,
                        title,
                        session_id: Some(session_id),
                        event_type: "stage_finished".to_string(),
                        message: Some(format!("{stage_id}: {outcome}")),
                        input_tokens: None,
                        output_tokens: None,
                        total_tokens: None,
                    },
                );
            }
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
        .create_for_issue(
            &issue.identifier,
            cfg.hook_after_create.as_deref(),
            cfg.hook_timeout_ms,
        )
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

    let reason = run_attempt_body(
        &issue_id,
        issue,
        attempt,
        &workspace.path,
        workspace.container.as_ref(),
        &snapshot,
        &tx,
    )
    .await;

    if let Some(script) = &cfg.hook_after_run
        && let Err(e) = hooks::run_hook_maybe_containerized(
            "after_run",
            script,
            &cfg.workflow_dir,
            &workspace.path,
            cfg.hook_timeout_ms,
            workspace.container.as_ref(),
        )
        .await
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
    container: Option<&ContainerHandle>,
    snapshot: &DispatchSnapshot,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) -> ExitReason {
    let cfg = &snapshot.config;

    if let Some(script) = &cfg.hook_before_run
        && let Err(e) = hooks::run_hook_maybe_containerized(
            "before_run",
            script,
            &cfg.workflow_dir,
            workspace_path,
            cfg.hook_timeout_ms,
            container,
        )
        .await
    {
        return ExitReason::Error(format!("before_run hook error: {e}"));
    }

    if let Err(e) = snapshot.workspace_mgr.validate_agent_cwd(workspace_path) {
        return ExitReason::Error(format!("workspace safety check failed: {e}"));
    }

    let title = format!("{}: {}", issue.identifier, issue.title);
    let mut session = match snapshot
        .agent_backend
        .start_session(workspace_path, &issue.id, &title, container)
        .await
    {
        Ok(s) => s,
        Err(e) => return ExitReason::Error(format!("agent session startup error: {e}")),
    };

    let _ = tx.send(OrchMsg::SessionStarted {
        issue_id: issue_id.to_string(),
        session_id: session.session_id().to_string(),
    });

    let exit = if cfg.pipeline.enabled {
        run_pipeline(
            issue_id,
            &mut issue,
            attempt,
            session.as_mut(),
            snapshot,
            tx,
        )
        .await
    } else {
        match run_turn_loop(
            session.as_mut(),
            &snapshot.tracker,
            &cfg.active_states,
            &cfg.required_labels,
            &snapshot.prompt_template,
            &mut issue,
            attempt,
            cfg.max_turns,
            issue_id,
            tx,
        )
        .await
        {
            LoopExit::Completed | LoopExit::EndedByIssueState => ExitReason::Normal,
            LoopExit::Error(e) => ExitReason::Error(e),
        }
    };

    session.stop().await;
    exit
}

/// How a fixed-turn-budget run of `run_turn_loop` ended. `Completed` and
/// `EndedByIssueState` both meant `ExitReason::Normal` in the pre-pipeline single-stage
/// path (see `run_attempt_body`'s non-pipeline branch above, unchanged); the pipeline
/// path (`run_pipeline` below) distinguishes them because only `EndedByIssueState`
/// (the issue itself left the active/routable state -- e.g. the agent called
/// `update_issue_state` to a terminal state) means the whole cycle is done, whereas
/// `Completed` (the stage simply ran out its own turn budget) means "move on to the
/// next stage."
enum LoopExit {
    Completed,
    EndedByIssueState,
    Error(String),
}

/// Runs 1..=`max_turns` turns of `session` against `issue`, refreshing tracker state
/// after each turn and stopping early if the issue leaves the active/routable state --
/// exactly the loop `run_attempt_body` always ran (Section 16.5), generalized so it can
/// be invoked once per pipeline stage (each with its own turn budget) as well as once
/// for a whole attempt (the legacy, and still default, single-stage behavior).
#[allow(clippy::too_many_arguments)]
async fn run_turn_loop(
    session: &mut dyn AgentSession,
    tracker: &Arc<dyn TrackerAdapter>,
    active_states: &[String],
    required_labels: &[String],
    prompt_template: &str,
    issue: &mut Issue,
    attempt: Option<u32>,
    max_turns: u32,
    issue_id: &str,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) -> LoopExit {
    let mut turn_number: u32 = 1;
    loop {
        let prompt =
            match render_turn_prompt(prompt_template, issue, attempt, turn_number, max_turns) {
                Ok(p) => p,
                Err(e) => return LoopExit::Error(format!("prompt error: {e}")),
            };

        let _ = tx.send(OrchMsg::TurnStarted {
            issue_id: issue_id.to_string(),
        });

        match run_one_turn(session, &prompt, issue_id, tx).await {
            Ok(TurnOutcome::Completed { .. }) => {}
            Ok(TurnOutcome::Failed { reason }) => {
                return LoopExit::Error(format!("agent turn error: {reason}"));
            }
            Err(e) => return LoopExit::Error(format!("agent turn error: {e}")),
        }

        let refreshed = match tracker
            .fetch_issues_by_ids(std::slice::from_ref(&issue.id))
            .await
        {
            Ok(r) => r,
            Err(e) => return LoopExit::Error(format!("issue state refresh error: {e}")),
        };
        let Some(next_issue) = refreshed.into_iter().next() else {
            return LoopExit::EndedByIssueState;
        };
        *issue = next_issue;

        let normalized = issue.normalized_state();
        let active = active_states
            .iter()
            .any(|s| s.trim().to_lowercase() == normalized);
        if !active || !issue.is_routable(required_labels) {
            return LoopExit::EndedByIssueState;
        }
        if turn_number >= max_turns {
            return LoopExit::Completed;
        }
        turn_number += 1;
    }
}

/// Drives one delivery cycle through `pipeline.stages` in order, within the one
/// workspace/session `run_attempt_body` already set up (AIR-1: a stage boundary is not
/// a workspace boundary). Each stage's own turn budget runs via `run_turn_loop`;
/// `StageStarted`/`StageFinished` bracket it so `/events` shows per-stage progress
/// regardless of how the stage (or the whole cycle) ends.
async fn run_pipeline(
    issue_id: &str,
    issue: &mut Issue,
    attempt: Option<u32>,
    session: &mut dyn AgentSession,
    snapshot: &DispatchSnapshot,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) -> ExitReason {
    let cfg = &snapshot.config;

    for stage in &cfg.pipeline.stages {
        let _ = tx.send(OrchMsg::StageStarted {
            issue_id: issue_id.to_string(),
            stage_id: stage.id.clone(),
        });

        let mut outcome = run_turn_loop(
            session,
            &snapshot.tracker,
            &cfg.active_states,
            &cfg.required_labels,
            &snapshot.prompt_template,
            issue,
            attempt,
            stage.max_turns,
            issue_id,
            tx,
        )
        .await;

        // `retry` gets exactly one extra attempt at the same stage before falling
        // through to the same handling `escalate` gets -- a bounded retry, not
        // unbounded looping.
        if matches!(outcome, LoopExit::Error(_)) && stage.on_failure == StageFailureAction::Retry {
            outcome = run_turn_loop(
                session,
                &snapshot.tracker,
                &cfg.active_states,
                &cfg.required_labels,
                &snapshot.prompt_template,
                issue,
                attempt,
                stage.max_turns,
                issue_id,
                tx,
            )
            .await;
        }

        let outcome_label = match &outcome {
            LoopExit::Completed => "completed".to_string(),
            LoopExit::EndedByIssueState => "ended by issue state".to_string(),
            LoopExit::Error(reason) if stage.on_failure == StageFailureAction::Skip => {
                format!("failed, skipped: {reason}")
            }
            LoopExit::Error(reason) => format!("failed: {reason}"),
        };
        let _ = tx.send(OrchMsg::StageFinished {
            issue_id: issue_id.to_string(),
            stage_id: stage.id.clone(),
            outcome: outcome_label,
        });

        match outcome {
            LoopExit::Completed => continue,
            LoopExit::EndedByIssueState => return ExitReason::Normal,
            LoopExit::Error(reason) => {
                if stage.on_failure == StageFailureAction::Skip {
                    continue;
                }
                // A plan usage-limit hit isn't a genuine exit-criteria failure -- it
                // will very likely succeed on a later retry once the account's own
                // limit resets, so a `blocking` stage must not park the issue over it
                // (that's for real judged failures). Fall through to the same
                // whole-attempt retry every non-blocking failure already takes;
                // `handle_msg`'s `ExitReason::Error` handling recognizes the same
                // phrase and schedules the long, coordinated pause instead of the
                // normal short backoff.
                if stage.blocking && !is_plan_rate_limited(&reason) {
                    block_issue(snapshot, issue_id, &cfg.pipeline.blocked_state).await;
                    return ExitReason::Normal;
                }
                return ExitReason::Error(format!("stage '{}' failed: {reason}", stage.id));
            }
        }
    }
    ExitReason::Normal
}

/// Parks `issue_id` in `pipeline.blocked_state` after a blocking stage's failure --
/// deliberately host-side (`TrackerAdapter::set_issue_state`, not the agent-facing
/// `update_issue_state` tool): the decision to stop the cycle is the orchestrator's,
/// not something to ask the (just-failed) agent to report on its own behalf. A tracker
/// adapter that doesn't support this (the default) logs a warning rather than failing
/// the cycle outright -- the cycle still stops via `ExitReason::Normal` either way,
/// this only affects whether the tracker's own state reflects why.
async fn block_issue(snapshot: &DispatchSnapshot, issue_id: &str, blocked_state: &str) {
    if let Err(e) = snapshot
        .tracker
        .set_issue_state(issue_id, blocked_state)
        .await
    {
        tracing::warn!(
            issue_id = %issue_id,
            blocked_state = %blocked_state,
            error = %e,
            "failed to park issue in pipeline.blocked_state after a blocking stage failure"
        );
    }
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

    fn test_shared(
        hook_after_run: &str,
        workspace_root: PathBuf,
        tracker_dir: &std::path::Path,
    ) -> Shared {
        let cfg_yaml: serde_yaml::Value =
            serde_yaml::from_str("tracker:\n  kind: local\n").unwrap();
        let mut cfg = config::resolve(&cfg_yaml, Path::new(".")).unwrap();
        cfg.hook_after_run = Some(hook_after_run.to_string());
        cfg.workspace_root = workspace_root.clone();

        let provider: serde_yaml::Value =
            serde_yaml::from_str(&format!("dir: {:?}", tracker_dir)).unwrap();
        let tracker_adapter = tracker::build("local", &provider, Path::new(".")).unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();

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
                workflow_dir: Path::new(".").to_path_buf(),
            }),
            workspace_mgr: Arc::new(WorkspaceManager::new(workspace_root)),
            event_tx,
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

    // -----------------------------------------------------------------------------
    // AIR-1: delivery pipeline (`run_pipeline`/`run_turn_loop`)
    // -----------------------------------------------------------------------------

    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A canned-outcome `AgentBackend`/`AgentSession` pair for pipeline tests: every
    /// turn across the whole attempt (however many stages it spans) increments one
    /// shared counter, so a test can script "fail on the Nth turn" and assert on
    /// exactly how many turns ran in total -- the thing that distinguishes "two stages
    /// ran in order in one session" from "only one ran."
    struct ScriptedBackend {
        calls: Arc<Mutex<u32>>,
        failures: HashMap<u32, String>,
    }

    #[async_trait::async_trait]
    impl AgentBackend for ScriptedBackend {
        async fn start_session(
            &self,
            _workspace: &Path,
            _issue_id: &str,
            _title: &str,
            _container: Option<&ContainerHandle>,
        ) -> Result<Box<dyn AgentSession>, crate::agent::AgentError> {
            Ok(Box::new(ScriptedSession {
                calls: self.calls.clone(),
                failures: self.failures.clone(),
            }))
        }
    }

    struct ScriptedSession {
        calls: Arc<Mutex<u32>>,
        failures: HashMap<u32, String>,
    }

    #[async_trait::async_trait]
    impl AgentSession for ScriptedSession {
        fn session_id(&self) -> &str {
            "scripted-session"
        }

        async fn run_turn(
            &mut self,
            _prompt: &str,
            events: mpsc::UnboundedSender<AgentEvent>,
        ) -> Result<TurnOutcome, crate::agent::AgentError> {
            let n = {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                *calls
            };
            let _ = events.send(AgentEvent::new("turn_completed"));
            match self.failures.get(&n) {
                Some(reason) => Ok(TurnOutcome::Failed {
                    reason: reason.clone(),
                }),
                None => Ok(TurnOutcome::Completed { usage: None }),
            }
        }

        async fn stop(self: Box<Self>) {}
    }

    fn write_pipeline_issue(dir: &Path, identifier: &str) {
        std::fs::write(
            dir.join(format!("{identifier}.md")),
            format!("---\nidentifier: {identifier}\ntitle: Test issue\nstate: todo\n---\nbody\n"),
        )
        .unwrap();
    }

    /// Builds a `DispatchSnapshot` wired to a real `LocalTrackerAdapter` (so blocking
    /// failures' `set_issue_state` calls are observable) and the given `ScriptedBackend`,
    /// with `pipeline.enabled: true` and the supplied stage config appended as raw YAML.
    fn pipeline_snapshot(
        tracker_dir: &Path,
        stages_yaml: &str,
        backend: ScriptedBackend,
    ) -> DispatchSnapshot {
        let cfg_yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
            "tracker:\n  kind: local\n  active_states: [todo]\n  terminal_states: [done]\n\
             pipeline:\n  enabled: true\n  blocked_state: blocked\n  stages:\n{stages_yaml}"
        ))
        .unwrap();
        let cfg = config::resolve(&cfg_yaml, Path::new(".")).unwrap();

        let provider: serde_yaml::Value =
            serde_yaml::from_str(&format!("dir: {:?}", tracker_dir)).unwrap();
        let tracker_adapter = tracker::build("local", &provider, Path::new(".")).unwrap();

        DispatchSnapshot {
            config: cfg,
            prompt_template: String::new(),
            tracker: Arc::from(tracker_adapter),
            agent_backend: Arc::new(backend),
            // Never touched by these tests (`run_pipeline` doesn't create workspaces
            // itself -- that's `run_agent_attempt`'s job), so an unused placeholder
            // path is fine; `WorkspaceManager::new` does no I/O at construction.
            workspace_mgr: Arc::new(WorkspaceManager::new(PathBuf::from("unused"))),
        }
    }

    async fn drain(mut rx: mpsc::UnboundedReceiver<OrchMsg>) -> Vec<OrchMsg> {
        rx.close();
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            out.push(msg);
        }
        out
    }

    fn stage_events(msgs: &[OrchMsg]) -> Vec<(&str, &str)> {
        msgs.iter()
            .filter_map(|m| match m {
                OrchMsg::StageStarted { stage_id, .. } => Some(("started", stage_id.as_str())),
                OrchMsg::StageFinished { stage_id, .. } => Some(("finished", stage_id.as_str())),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn pipeline_runs_stages_in_order_within_one_session() {
        let tracker_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-1");
        let calls = Arc::new(Mutex::new(0));
        let snapshot = pipeline_snapshot(
            tracker_dir.path(),
            "  - id: requirements\n    role: requirements\n    max_turns: 1\n\
             \x20\x20- id: implement\n    role: developer\n    max_turns: 2\n",
            ScriptedBackend {
                calls: calls.clone(),
                failures: HashMap::new(),
            },
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-1".to_string()])
            .await
            .unwrap()
            .remove(0);

        let mut session = snapshot
            .agent_backend
            .start_session(Path::new("."), &issue.id, "t", None)
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));
        // requirements (1 turn) + implement (2 turns) = 3 turns total, in one session.
        assert_eq!(*calls.lock().unwrap(), 3);

        let msgs = drain(rx).await;
        let stages = stage_events(&msgs);
        assert_eq!(
            stages,
            vec![
                ("started", "requirements"),
                ("finished", "requirements"),
                ("started", "implement"),
                ("finished", "implement"),
            ]
        );
    }

    #[tokio::test]
    async fn blocking_stage_failure_parks_the_issue_in_blocked_state() {
        let tracker_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-2");
        let mut failures = HashMap::new();
        failures.insert(1, "boom".to_string()); // the only stage's only turn fails
        let snapshot = pipeline_snapshot(
            tracker_dir.path(),
            "  - id: review\n    role: reviewer\n    max_turns: 1\n    blocking: true\n",
            ScriptedBackend {
                calls: Arc::new(Mutex::new(0)),
                failures,
            },
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-2".to_string()])
            .await
            .unwrap()
            .remove(0);
        let mut session = snapshot
            .agent_backend
            .start_session(Path::new("."), &issue.id, "t", None)
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            &tx,
        )
        .await;
        // Blocking failure ends the cycle cleanly (not an attempt-level error) --
        // the issue's own state is what now says it stopped, and why.
        assert!(matches!(exit, ExitReason::Normal));

        let refreshed = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-2".to_string()])
            .await
            .unwrap();
        assert_eq!(refreshed[0].state, "blocked");

        let msgs = drain(rx).await;
        assert!(msgs.iter().any(
            |m| matches!(m, OrchMsg::StageFinished { outcome, .. } if outcome.starts_with("failed"))
        ));
    }

    #[tokio::test]
    async fn on_failure_skip_continues_to_the_next_stage() {
        let tracker_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-3");
        let mut failures = HashMap::new();
        failures.insert(1, "boom".to_string()); // first stage's only turn fails
        let snapshot = pipeline_snapshot(
            tracker_dir.path(),
            "  - id: requirements\n    role: requirements\n    max_turns: 1\n    on_failure: skip\n\
             \x20\x20- id: implement\n    role: developer\n    max_turns: 1\n",
            ScriptedBackend {
                calls: Arc::new(Mutex::new(0)),
                failures,
            },
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-3".to_string()])
            .await
            .unwrap()
            .remove(0);
        let mut session = snapshot
            .agent_backend
            .start_session(Path::new("."), &issue.id, "t", None)
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));

        let msgs = drain(rx).await;
        let stages = stage_events(&msgs);
        // Both stages ran even though the first failed -- `skip` moved on.
        assert_eq!(
            stages,
            vec![
                ("started", "requirements"),
                ("finished", "requirements"),
                ("started", "implement"),
                ("finished", "implement"),
            ]
        );
    }

    #[tokio::test]
    async fn on_failure_retry_gives_one_extra_attempt_before_giving_up() {
        let tracker_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-4");
        let mut failures = HashMap::new();
        failures.insert(1, "boom".to_string()); // first attempt at the stage fails...
        // ...second attempt (turn 2) is left unscripted, so it succeeds.
        let calls = Arc::new(Mutex::new(0));
        let snapshot = pipeline_snapshot(
            tracker_dir.path(),
            "  - id: implement\n    role: developer\n    max_turns: 1\n    on_failure: retry\n",
            ScriptedBackend {
                calls: calls.clone(),
                failures,
            },
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-4".to_string()])
            .await
            .unwrap()
            .remove(0);
        let mut session = snapshot
            .agent_backend
            .start_session(Path::new("."), &issue.id, "t", None)
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));
        assert_eq!(
            *calls.lock().unwrap(),
            2,
            "the failed turn plus one retry turn"
        );

        let msgs = drain(rx).await;
        assert!(msgs.iter().any(
            |m| matches!(m, OrchMsg::StageFinished { outcome, .. } if outcome == "completed")
        ));
    }

    #[tokio::test]
    async fn on_failure_escalate_stops_a_non_blocking_stage_with_an_error() {
        let tracker_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-5");
        let mut failures = HashMap::new();
        failures.insert(1, "boom".to_string());
        let snapshot = pipeline_snapshot(
            tracker_dir.path(),
            "  - id: implement\n    role: developer\n    max_turns: 1\n\
             \x20\x20- id: review\n    role: reviewer\n    max_turns: 1\n",
            ScriptedBackend {
                calls: Arc::new(Mutex::new(0)),
                failures,
            },
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-5".to_string()])
            .await
            .unwrap()
            .remove(0);
        let mut session = snapshot
            .agent_backend
            .start_session(Path::new("."), &issue.id, "t", None)
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Error(_)));

        let msgs = drain(rx).await;
        let stages = stage_events(&msgs);
        // The second stage never starts -- default `escalate` stops the whole cycle.
        assert_eq!(
            stages,
            vec![("started", "implement"), ("finished", "implement")]
        );
    }

    #[test]
    fn pipeline_absent_resolves_disabled_with_no_stages() {
        let cfg_yaml: serde_yaml::Value =
            serde_yaml::from_str("tracker:\n  kind: local\n").unwrap();
        let cfg = config::resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(!cfg.pipeline.enabled);
        assert!(cfg.pipeline.stages.is_empty());
    }

    // -----------------------------------------------------------------------------
    // BUG-3: plan usage-limit ("session limit") handling
    // -----------------------------------------------------------------------------

    #[test]
    fn is_plan_rate_limited_recognizes_the_real_captured_message() {
        // Verbatim from a live dogfood run against the real `claude` CLI.
        assert!(is_plan_rate_limited(
            "You've hit your session limit \u{b7} resets 12:30am (Europe/Paris)"
        ));
        // The pipeline wraps the reason with a stage-id prefix -- must still match.
        assert!(is_plan_rate_limited(
            "stage 'implement' failed: agent turn error: You've hit your session limit \u{b7} resets 12:30am (Europe/Paris)"
        ));
        assert!(is_plan_rate_limited(
            "You've hit your usage limit for today"
        ));
    }

    #[test]
    fn is_plan_rate_limited_does_not_misclassify_generic_errors() {
        assert!(!is_plan_rate_limited("agent turn error: connection reset"));
        assert!(!is_plan_rate_limited("prompt error: unknown variable 'x'"));
        assert!(!is_plan_rate_limited("subprocess exited with code 1"));
    }

    #[tokio::test]
    async fn rate_limited_worker_exit_pauses_dispatch_without_escalating_attempt() {
        let root = tempdir().unwrap();
        let tracker_dir = tempdir().unwrap();
        let shared = test_shared("true", root.path().to_path_buf(), tracker_dir.path());
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut state = OrchestratorState::default();

        let issue_id = "RL-1".to_string();
        let handle = tokio::spawn(async {});
        state.running.insert(
            issue_id.clone(),
            RunningEntry {
                issue: Issue {
                    id: issue_id.clone(),
                    native_ref: None,
                    identifier: "RL-1".to_string(),
                    title: "Rate limit test".to_string(),
                    description: None,
                    priority: None,
                    state: "todo".to_string(),
                    branch_name: None,
                    url: None,
                    assignee_id: None,
                    labels: Vec::new(),
                    blocked_by: Vec::new(),
                    dispatchable: true,
                    created_at: None,
                    updated_at: None,
                },
                started_at: Instant::now(),
                session_id: String::new(),
                last_event: None,
                last_event_at: None,
                last_message: None,
                tokens: TokenUsage::default(),
                turn_count: 0,
                tool_call_count: 0,
                handle,
                retry_attempt: None,
                current_stage: None,
            },
        );

        handle_msg(
            &shared,
            &mut state,
            &tx,
            OrchMsg::WorkerExit {
                issue_id: issue_id.clone(),
                reason: ExitReason::Error(
                    "You've hit your session limit \u{b7} resets 12:30am (Europe/Paris)"
                        .to_string(),
                ),
            },
        )
        .await;

        assert!(
            state.rate_limited_until.is_some(),
            "a session-limit failure must set the account-wide pause"
        );
        let retry = state
            .retry_attempts
            .get(&issue_id)
            .expect("a retry must still be scheduled for the affected issue");
        assert_eq!(
            retry.attempt, 1,
            "attempt must not escalate for a rate-limit failure (not the ticket's fault)"
        );
    }

    #[tokio::test]
    async fn on_tick_skips_new_dispatch_while_rate_limited_and_resumes_after() {
        let root = tempdir().unwrap();
        let tracker_dir = tempdir().unwrap();
        std::fs::write(
            tracker_dir.path().join("RL-2.md"),
            "---\nidentifier: RL-2\ntitle: Candidate\nstate: todo\n---\nbody\n",
        )
        .unwrap();

        let cfg_yaml: serde_yaml::Value = serde_yaml::from_str(
            "tracker:\n  kind: local\n  active_states: [todo]\n  terminal_states: [done]\n",
        )
        .unwrap();
        let mut cfg = config::resolve(&cfg_yaml, Path::new(".")).unwrap();
        cfg.workspace_root = root.path().to_path_buf();

        let provider: serde_yaml::Value =
            serde_yaml::from_str(&format!("dir: {:?}", tracker_dir.path())).unwrap();
        let tracker_adapter = tracker::build("local", &provider, Path::new(".")).unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let shared = Shared {
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
                workflow_dir: Path::new(".").to_path_buf(),
            }),
            workspace_mgr: Arc::new(WorkspaceManager::new(root.path().to_path_buf())),
            event_tx,
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut state = OrchestratorState {
            rate_limited_until: Some(Instant::now() + Duration::from_secs(60)),
            ..Default::default()
        };

        on_tick(&shared, &mut state, &tx).await;
        assert!(
            state.running.is_empty(),
            "dispatch must be skipped while the account-wide pause is still in effect"
        );
        assert!(state.rate_limited_until.is_some());

        state.rate_limited_until = Some(Instant::now() - Duration::from_secs(1));
        on_tick(&shared, &mut state, &tx).await;
        assert!(
            !state.running.is_empty(),
            "dispatch must resume once the pause deadline has passed"
        );
        assert!(
            state.rate_limited_until.is_none(),
            "an expired pause must be cleared"
        );
    }
}
