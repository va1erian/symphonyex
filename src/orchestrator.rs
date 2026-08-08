//! Orchestrator: the single authority over dispatch, retry, and reconciliation state
//! (Section 7, 8, 16). One task owns all mutable scheduling state directly; worker
//! tasks only ever report back over a channel, never mutate shared state themselves.

use crate::agent::{
    AgentBackend, AgentEvent, AgentSession, TokenUsage, TurnOutcome, claude, codex, opencode,
};
use crate::approvals;
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
use serde_json::{Value, json};
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
    /// (which `kill_on_drop`s the agent subprocess) has actually finished first â€”
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
        /// AIR-2: human-readable summary of the resolved role driving this stage --
        /// `"<role>"` when it runs on `agent.backend` like everything else, or
        /// `"<role> (opencode/fireworks/x)"` when the role overrides backend/model.
        /// Surfaced on the dashboard (`status::RunningRow::stage`) so a human watching
        /// a multi-stage cycle can see *which* role/backend is running, not just which
        /// stage id.
        role_summary: String,
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
    /// AIR-5: a `requires_approval` stage finished and is now parked, waiting on a
    /// human decision (`approvals::ApprovalRow` id `approval_id`).
    ApprovalRequested {
        issue_id: String,
        stage_id: String,
        approval_id: i64,
    },
    /// AIR-5: a `requires_approval` stage finished and `pipeline.approval.
    /// auto_approve_when` matched -- no pending row was ever created, the cycle just
    /// moved on to the next stage.
    ApprovalAutoApproved {
        issue_id: String,
        stage_id: String,
    },
    /// A `security` stage finished evaluating its artifact (model output + scanners),
    /// whether or not it ended up blocking -- recorded unconditionally so the
    /// dashboard's `/security` page has something to show for a clean cycle too, not
    /// just blocked ones. `findings_json` is the fully redacted, scanner-merged
    /// `security::SecurityFindings` artifact.
    SecurityEvaluated {
        issue_id: String,
        stage_id: String,
        risk: String,
        findings_json: String,
    },
    /// The security stage's findings breached `pipeline.security.block_on` and no
    /// pending human override was found -- the cycle is about to be parked.
    SecurityBlocked {
        issue_id: String,
        reason: String,
    },
    /// A previously-recorded human override (`status.rs`'s `/security/override`) was
    /// applied to unblock this evaluation -- one-shot, see
    /// `eventlog::pending_override`.
    SecurityOverrideConsumed {
        issue_id: String,
        reason: String,
    },
    /// A release evidence bundle was assembled and (if a Symphony-opened PR/MR was
    /// open) consolidated into its body (`repo.release_evidence`, AIR-9). `summary`
    /// is the verdict plus the rule(s) that produced it (`release::explain_verdict`),
    /// so `/events` shows *why*, not just the verdict word.
    ReleaseEvidenceReady {
        issue_id: String,
        summary: String,
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
    // AIR-3: `pipeline.enabled` on its own also needs the MCP subprocess wired up, even
    // when the tracker has no tools of its own and `repo.pull_request` is off --
    // `record_artifact` is a property of the cycle, not of either of those.
    let mcp_wiring = if tracker_adapter.agent_tool_specs().is_empty()
        && repo_pr_json.is_none()
        && !cfg.pipeline.enabled
    {
        None
    } else {
        Some(claude::McpToolWiring {
            tracker_kind: cfg.tracker_kind.clone(),
            tracker_provider_json: serde_json::to_string(&cfg.tracker_provider)?,
            workflow_dir: cfg.workflow_dir.clone(),
            repo_pr_json,
            pipeline_enabled: cfg.pipeline.enabled,
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
    /// AIR-8: lets the multi-project service (`src/service.rs`) wire the same
    /// tracker-backed `/security` override action into this project's nested
    /// dashboard that the single-project path gets above.
    pub security: status::SecurityContext,
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
        let security_context = status::SecurityContext {
            tracker: shared.tracker.clone(),
            blocked_state: shared.config.pipeline.blocked_state.clone(),
            resume_state: shared.config.active_states.first().cloned(),
        };
        let observability_for_serve = observability_handle.clone();
        tokio::spawn(async move {
            if let Err(e) = status::serve_composite(
                port,
                bind_all_interfaces,
                status_rx_for_serve,
                workflow_dir_for_serve,
                chat_router,
                Some(security_context),
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
            security: status::SecurityContext {
                tracker: shared.tracker.clone(),
                blocked_state: shared.config.pipeline.blocked_state.clone(),
                resume_state: shared.config.active_states.first().cloned(),
            },
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
    poll_approval_comments(shared).await;
    apply_resolved_approvals(shared).await;

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
/// cancellation) before touching its workspace, then run `after_run` if configured â€”
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

/// AIR-5's issue-comment approval channel: for every still-pending approval, scan the
/// issue thread's comments for `/approve`, `/changes <reason>` or `/reject [reason]`
/// past whatever was already scanned (`approvals::last_seen_comment_id`), and record a
/// decision on the first match -- `apply_resolved_approvals` (called right after this,
/// same tick) then applies it exactly the same way a dashboard-recorded decision is
/// applied. Unsupported trackers (`TrackerAdapter::fetch_issue_comments`'s default)
/// simply never surface a comment here, so this is safe to run unconditionally.
async fn poll_approval_comments(shared: &Shared) {
    let db_path = approvals_db_path(&shared.config);
    let pending = match approvals::list_pending(&db_path) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "failed to list pending approvals; skipping comment poll this tick");
            return;
        }
    };
    for row in pending {
        let comments = match shared.tracker.fetch_issue_comments(&row.issue_id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(issue_id = %row.issue_id, error = %e, "fetch_issue_comments failed; will retry next tick");
                continue;
            }
        };
        let last_seen = approvals::last_seen_comment_id(&db_path, row.id).unwrap_or(0);
        let mut max_seen = last_seen;
        let mut decided = false;
        for c in comments.iter().filter(|c| c.id > last_seen) {
            max_seen = max_seen.max(c.id);
            if decided {
                continue; // first matching command wins; keep scanning only to advance the cursor
            }
            let Some((decision, reason)) = parse_approval_command(&c.body) else {
                continue;
            };
            let actor = c
                .author
                .clone()
                .unwrap_or_else(|| "issue-comment".to_string());
            match approvals::resolve(&db_path, row.id, decision, &actor, reason.as_deref()) {
                Ok(true) => decided = true,
                Ok(false) => {} // already resolved (e.g. via the dashboard) moments earlier
                Err(e) => {
                    tracing::warn!(approval_id = row.id, error = %e, "failed to record comment-driven approval decision");
                }
            }
        }
        if max_seen > last_seen
            && let Err(e) = approvals::set_last_seen_comment_id(&db_path, row.id, max_seen)
        {
            tracing::warn!(approval_id = row.id, error = %e, "failed to advance approval comment cursor");
        }
    }
}

/// `/approve`, `/changes <reason>`, `/reject [reason]` (case-insensitive command,
/// original-case reason) -- `None` for anything else, including a comment that merely
/// mentions one of these words in passing.
fn parse_approval_command(body: &str) -> Option<(approvals::Decision, Option<String>)> {
    let trimmed = body.trim();
    let lower = trimmed.to_lowercase();
    for (cmd, decision) in [
        ("/approve", approvals::Decision::Approve),
        ("/changes", approvals::Decision::RequestChanges),
        ("/reject", approvals::Decision::Reject),
    ] {
        if lower == cmd || lower.starts_with(&format!("{cmd} ")) {
            let reason = trimmed[cmd.len()..].trim();
            let reason = if reason.is_empty() {
                None
            } else {
                Some(reason.to_string())
            };
            return Some((decision, reason));
        }
    }
    None
}

/// Applies every approval decision resolved since the last tick -- whichever channel
/// recorded it (dashboard POST, `poll_approval_comments` above) -- to tracker state and
/// the event log. The sole place either channel's decision actually takes effect: this
/// is the orchestrator's own tick loop, the one thing in Symphony with standing
/// authority to mutate tracker state (`orchestrator.rs`'s module doc comment), so
/// neither channel does it directly.
async fn apply_resolved_approvals(shared: &Shared) {
    let db_path = approvals_db_path(&shared.config);
    let unapplied = match approvals::take_unapplied(&db_path) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(error = %e, "failed to list unapplied approval decisions; skipping this tick");
            return;
        }
    };
    for row in unapplied {
        let Some(decision) = row.decision.as_deref().and_then(approvals::Decision::parse) else {
            tracing::warn!(approval_id = row.id, decision = ?row.decision, "resolved approval row has an unrecognized decision; leaving unapplied");
            continue;
        };
        let default_active_state = shared
            .config
            .active_states
            .first()
            .cloned()
            .unwrap_or_else(|| "todo".to_string());
        let (target_state, resume_stage_id): (String, Option<String>) = match decision {
            approvals::Decision::Approve => (default_active_state, row.next_stage_id.clone()),
            approvals::Decision::RequestChanges => {
                (default_active_state, Some(row.stage_id.clone()))
            }
            approvals::Decision::Reject => (shared.config.pipeline.blocked_state.clone(), None),
        };

        if let Err(e) = shared
            .tracker
            .set_issue_state(&row.issue_id, &target_state)
            .await
        {
            tracing::warn!(
                approval_id = row.id,
                issue_id = %row.issue_id,
                target_state = %target_state,
                error = %e,
                "failed to move issue after an approval decision; will retry next tick"
            );
            continue;
        }

        record_event(
            shared,
            crate::eventlog::NewEvent {
                issue_id: row.issue_id.clone(),
                identifier: row.identifier.clone(),
                title: row.title.clone(),
                session_id: None,
                event_type: "approval_decided".to_string(),
                message: Some(format!(
                    "stage '{}': {} by {}{}",
                    row.stage_id,
                    decision.as_str(),
                    row.actor.as_deref().unwrap_or("unknown"),
                    row.comment
                        .as_deref()
                        .map(|c| format!(" -- {c}"))
                        .unwrap_or_default()
                )),
                input_tokens: None,
                output_tokens: None,
                total_tokens: None,
            },
        );

        if let Err(e) = approvals::mark_applied(&db_path, row.id, resume_stage_id.as_deref()) {
            tracing::warn!(approval_id = row.id, error = %e, "failed to mark approval decision applied (tracker state was already moved; this may reapply harmlessly next tick)");
        }
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

            // `test_report`/`coverage`'s `message` is the full JSON blob (AIR-6) --
            // worth persisting (below) and browsing on `/events`, but not worth
            // clobbering the running card's "last event" line with; the one-line
            // `test_summary`/`coverage_summary` companions carry the human-readable
            // version of the same data and update it normally.
            let is_full_report = matches!(event.event.as_str(), "test_report" | "coverage");

            if let Some(e) = state.running.get_mut(&issue_id) {
                if !is_full_report {
                    e.last_event = Some(event.event.clone());
                    e.last_event_at = Some(Instant::now());
                    if let Some(m) = &event.message {
                        e.last_message = Some(m.clone());
                    }
                }
                let (identifier, title) = (e.issue.identifier.clone(), e.issue.title.clone());

                match event.event.as_str() {
                    "test_summary" => {
                        state
                            .metrics
                            .issue_entry(&identifier, &title)
                            .last_test_summary = event.message.clone();
                    }
                    "coverage_summary" => {
                        state.metrics.issue_entry(&identifier, &title).last_coverage =
                            event.message.clone();
                    }
                    _ => {}
                }

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
        OrchMsg::StageStarted {
            issue_id,
            stage_id,
            role_summary,
        } => {
            if let Some(e) = state.running.get_mut(&issue_id) {
                e.current_stage = Some(role_summary);
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
        OrchMsg::ApprovalRequested {
            issue_id,
            stage_id,
            approval_id,
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
                        event_type: "approval_requested".to_string(),
                        message: Some(format!(
                            "stage '{stage_id}' is awaiting approval (#{approval_id})"
                        )),
                        input_tokens: None,
                        output_tokens: None,
                        total_tokens: None,
                    },
                );
            }
        }
        OrchMsg::ReleaseEvidenceReady { issue_id, summary } => {
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
                        event_type: "release_evidence_ready".to_string(),
                        message: Some(summary),
                        input_tokens: None,
                        output_tokens: None,
                        total_tokens: None,
                    },
                );
            }
        }
        OrchMsg::ApprovalAutoApproved { issue_id, stage_id } => {
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
                        event_type: "approval_auto_approved".to_string(),
                        message: Some(format!(
                            "stage '{stage_id}' matched pipeline.approval.auto_approve_when"
                        )),
                        input_tokens: None,
                        output_tokens: None,
                        total_tokens: None,
                    },
                );
            }
        }
        OrchMsg::SecurityEvaluated {
            issue_id,
            stage_id,
            risk,
            findings_json,
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
                        event_type: "security_findings".to_string(),
                        // First line is `stage=<id> risk=<risk>` for a quick glance in
                        // `/events`; everything after the first newline is the raw
                        // `security::SecurityFindings` JSON artifact, parsed back out by
                        // `status.rs`'s `/security` page.
                        message: Some(format!("stage={stage_id} risk={risk}\n{findings_json}")),
                        input_tokens: None,
                        output_tokens: None,
                        total_tokens: None,
                    },
                );
            }
        }
        OrchMsg::SecurityBlocked { issue_id, reason } => {
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
                        event_type: "security_blocked".to_string(),
                        message: Some(reason),
                        input_tokens: None,
                        output_tokens: None,
                        total_tokens: None,
                    },
                );
            }
        }
        OrchMsg::SecurityOverrideConsumed { issue_id, reason } => {
            // Already persisted synchronously by `evaluate_security_stage`, before
            // this message was even sent -- consuming an override has to be
            // immediate (never dependent on this deferred handler running first),
            // or two evaluations racing `pending_override` could both consume the
            // same one. Only a log line here, not a second `eventlog` row.
            tracing::info!(issue_id = %issue_id, reason = %reason, "security override consumed, cycle resumed");
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
        .start_session(
            workspace_path,
            &issue.id,
            &title,
            container,
            &crate::agent::ToolPolicy::default(),
        )
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
            workspace_path,
            container,
            tx,
        )
        .await
    } else {
        let (outcome, _last_message) = run_turn_loop(
            session.as_mut(),
            &snapshot.tracker,
            &cfg.active_states,
            &cfg.required_labels,
            &snapshot.prompt_template,
            &mut issue,
            attempt,
            cfg.max_turns,
            issue_id,
            None,
            tx,
            None,
        )
        .await;
        match outcome {
            LoopExit::Completed | LoopExit::EndedByIssueState => ExitReason::Normal,
            LoopExit::Error(e) => ExitReason::Error(e),
            // `raise_clarification` is only exposed when `pipeline.enabled` (gated in
            // `mcp.rs`), so this arm is unreachable on the legacy single-stage path --
            // handled rather than `unreachable!()` since a future tool-exposure change
            // elsewhere shouldn't be able to panic the whole worker over this.
            LoopExit::Blocked(question) => {
                tracing::warn!(issue_id = %issue_id, question = %question, "raise_clarification called outside the delivery pipeline; ignoring");
                ExitReason::Normal
            }
        }
    };

    if matches!(exit, ExitReason::Normal) {
        finalize_release_evidence(issue_id, &issue, cfg, tx).await;
    }

    session.stop().await;
    exit
}

/// AIR-9: after a cycle ends normally, with `repo.release_evidence` on, assemble an
/// evidence bundle from what's actually recorded for this issue (its own description
/// for requirements/AC, `eventlog` for the timeline and token totals -- see
/// `release.rs`'s own doc comment on why most other sections are still gaps today),
/// persist it (locally for `/evidence/<key>`, and durably via `upload_artifact` so it
/// survives the PR/MR being closed later), and -- if this cycle opened or already had
/// a Symphony-authored PR/MR open -- rewrite its body to lead with the agent's own
/// narrative followed by the evidence sections.
///
/// Best-effort throughout (mirrors `after_run`'s own "log and ignore" stance just
/// above this function's call site): a release-evidence failure must never fail the
/// cycle itself, since `repo.pull_request`'s own success already happened inside the
/// turn loop this runs after.
async fn finalize_release_evidence(
    issue_id: &str,
    issue: &Issue,
    cfg: &EffectiveConfig,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) {
    let Some(repo_cfg) = cfg.repo.as_ref().filter(|r| r.release_evidence) else {
        return;
    };
    let repo_host = match crate::repo_host::build(repo_cfg) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(issue_id = %issue_id, error = %e, "release evidence: failed to build repo host (ignored)");
            return;
        }
    };

    let db_path = cfg.workflow_dir.join(crate::eventlog::DB_FILENAME);
    let filter = crate::eventlog::EventFilter {
        issue_id: Some(issue_id.to_string()),
        ..Default::default()
    };
    let events = crate::eventlog::recent_events(&db_path, &filter, 5_000, 0).unwrap_or_default();
    let usage = crate::eventlog::usage_by_issue(&db_path)
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.issue_id == issue_id);

    let bundle = crate::release::assemble(issue, &events, usage.as_ref());
    let verdict = crate::release::compute_verdict(&bundle);
    let matrix = crate::release::build_traceability_matrix(&bundle);
    let reasons = crate::release::explain_verdict(&bundle);

    let key = crate::workspace::derive_workspace_key(issue_id);
    let dir = cfg.workflow_dir.join(".symphony").join("release");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(issue_id = %issue_id, error = %e, "release evidence: failed to create local artifact dir (ignored)");
    } else if let Ok(json) = serde_json::to_vec_pretty(&bundle)
        && let Err(e) = std::fs::write(dir.join(format!("{key}.json")), &json)
    {
        tracing::warn!(issue_id = %issue_id, error = %e, "release evidence: failed to persist bundle locally (ignored)");
    }

    let mut artifact_links = std::collections::BTreeMap::new();
    if let Ok(json) = serde_json::to_vec_pretty(&bundle) {
        match repo_host
            .upload_artifact(
                issue_id,
                "evidence-bundle.json",
                &json,
                "Persist release evidence bundle",
            )
            .await
        {
            Ok(url) => {
                artifact_links.insert("evidence-bundle.json".to_string(), url);
            }
            Err(e) => {
                tracing::warn!(issue_id = %issue_id, error = %e, "release evidence: failed to upload bundle artifact (ignored)")
            }
        }
    }
    for artifact in &bundle.large_artifacts {
        if artifact.content.len() > crate::release::INLINE_ARTIFACT_LIMIT {
            match repo_host
                .upload_artifact(
                    issue_id,
                    &artifact.name,
                    artifact.content.as_bytes(),
                    &format!("Attach release artifact: {}", artifact.name),
                )
                .await
            {
                Ok(url) => {
                    artifact_links.insert(artifact.name.clone(), url);
                }
                Err(e) => {
                    tracing::warn!(issue_id = %issue_id, artifact = %artifact.name, error = %e, "release evidence: failed to upload oversized artifact (ignored)")
                }
            }
        }
    }

    let rendered = crate::release::render_markdown(&bundle, verdict, &matrix, &artifact_links);

    match repo_host.list_open_symphony_prs().await {
        Ok(prs) => {
            let head = format!("issue-{key}");
            if let Some(pr) = prs.iter().find(|p| p.head_ref == head) {
                let composed = crate::release::compose_pr_body(&pr.body, &rendered);
                if let Err(e) = repo_host.update_pr_body(pr.number, &composed).await {
                    tracing::warn!(issue_id = %issue_id, error = %e, "release evidence: failed to update PR/MR body (ignored)");
                }
            }
        }
        Err(e) => {
            tracing::warn!(issue_id = %issue_id, error = %e, "release evidence: failed to list open PRs/MRs (ignored)");
        }
    }

    let summary = format!("{}: {}", verdict.as_str(), reasons.join("; "));
    let _ = tx.send(OrchMsg::ReleaseEvidenceReady {
        issue_id: issue_id.to_string(),
        summary,
    });
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
    /// A turn called `raise_clarification` with `blocking: true` (AIR-4): the cycle
    /// stops right away rather than running out the stage's remaining turn budget,
    /// same as a blocking stage failure -- `run_pipeline` parks the issue in
    /// `pipeline.blocked_state` via the same `block_issue` path. Carries the question
    /// only for the `stage_finished` event message.
    Blocked(String),
}

/// Runs 1..=`max_turns` turns of `session` against `issue`, refreshing tracker state
/// after each turn and stopping early if the issue leaves the active/routable state --
/// exactly the loop `run_attempt_body` always ran (Section 16.5), generalized so it can
/// be invoked once per pipeline stage (each with its own turn budget) as well as once
/// for a whole attempt (the legacy, and still default, single-stage behavior).
///
/// Returns the last turn's final text message alongside the `LoopExit` -- AIR-5's
/// approval gate uses it as a `requires_approval` stage's plan content
/// (`handle_stage_approval`); every other caller ignores it.
///
/// `resume_note`, when set, is appended to *only* the first turn's prompt -- a human
/// reviewer's "request changes" comment, injected back into the same stage's next
/// attempt (`run_pipeline`'s resume handling).
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
    // AIR-2: the `cycle.*` template namespace (id/stage/artifacts/previous_stage_summary)
    // for a pipeline stage's own role prompt. `None` for the legacy single-stage path,
    // whose `WORKFLOW.md` prompt never references `cycle.*` -- `template::render`'s
    // strict mode only errors on a *referenced* unknown variable, so omitting the key
    // entirely is safe there. AIR-3's real per-cycle artifact index is folded into this
    // object's `"artifacts"` field by `run_pipeline` before it's passed down here,
    // rather than threaded through as a separate parameter.
    cycle: Option<&serde_json::Value>,
    tx: &mpsc::UnboundedSender<OrchMsg>,
    resume_note: Option<&str>,
) -> (LoopExit, Option<String>) {
    let mut turn_number: u32 = 1;
    let mut last_message: Option<String> = None;
    loop {
        let mut prompt = match render_turn_prompt(
            prompt_template,
            issue,
            attempt,
            turn_number,
            max_turns,
            cycle,
        ) {
            Ok(p) => p,
            Err(e) => return (LoopExit::Error(format!("prompt error: {e}")), last_message),
        };
        if turn_number == 1
            && let Some(note) = resume_note
        {
            prompt.push_str(&format!(
                "\n\n---\nA human reviewer requested changes to this stage's prior output: \
                 {note}\nAddress this feedback in your revised output.\n"
            ));
        }

        let _ = tx.send(OrchMsg::TurnStarted {
            issue_id: issue_id.to_string(),
        });

        let (outcome, question, msg) = run_one_turn(session, &prompt, issue_id, tx).await;
        if msg.is_some() {
            last_message = msg;
        }
        if let Some(question) = question {
            return (LoopExit::Blocked(question), last_message);
        }
        match outcome {
            Ok(TurnOutcome::Completed { .. }) => {}
            Ok(TurnOutcome::Failed { reason }) => {
                return (
                    LoopExit::Error(format!("agent turn error: {reason}")),
                    last_message,
                );
            }
            Err(e) => {
                return (
                    LoopExit::Error(format!("agent turn error: {e}")),
                    last_message,
                );
            }
        }

        let refreshed = match tracker
            .fetch_issues_by_ids(std::slice::from_ref(&issue.id))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return (
                    LoopExit::Error(format!("issue state refresh error: {e}")),
                    last_message,
                );
            }
        };
        let Some(next_issue) = refreshed.into_iter().next() else {
            return (LoopExit::EndedByIssueState, last_message);
        };
        *issue = next_issue;

        let normalized = issue.normalized_state();
        let active = active_states
            .iter()
            .any(|s| s.trim().to_lowercase() == normalized);
        if !active || !issue.is_routable(required_labels) {
            return (LoopExit::EndedByIssueState, last_message);
        }
        if turn_number >= max_turns {
            return (LoopExit::Completed, last_message);
        }
        turn_number += 1;
    }
}

/// Drives one delivery cycle through `pipeline.stages` in order, within the one
/// workspace `run_attempt_body` already set up (AIR-1: a stage boundary is not a
/// workspace boundary). Each stage's own turn budget runs via `run_turn_loop`;
/// `StageStarted`/`StageFinished` bracket it so `/events` shows per-stage progress
/// regardless of how the stage (or the whole cycle) ends.
///
/// AIR-2: each stage resolves its own role (`roles::resolve`) for its prompt, tool
/// policy, and optionally a different backend/model than `agent.backend`. A stage whose
/// role doesn't override backend/model keeps running on `session` -- the one shared
/// session `run_attempt_body` started, preserving conversational continuity across
/// stages exactly as before this ticket. A stage that *does* override gets its own
/// freshly-started session (same workspace/container) for just that stage, stopped once
/// the stage finishes.
#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    issue_id: &str,
    issue: &mut Issue,
    attempt: Option<u32>,
    session: &mut dyn AgentSession,
    snapshot: &DispatchSnapshot,
    workspace_path: &Path,
    container: Option<&ContainerHandle>,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) -> ExitReason {
    let cfg = &snapshot.config;
    // AIR-2's own display-facing id (used only in `cycle.id`, e.g. distinguishing
    // retried attempts of the same issue in a rendered prompt). The artifact store
    // (AIR-3) deliberately keys on plain `issue_id` instead, matching what
    // `record_artifact`'s MCP wiring (`src/mcp.rs`) actually threads through today --
    // one shared artifact index per issue, not one per attempt, so a retry still sees
    // what earlier attempts already recorded rather than starting over.
    let cycle_id = format!("{issue_id}-{}", attempt.unwrap_or(1));
    let mut previous_stage_summary = String::new();
    let artifacts_db_path = cfg.workflow_dir.join(crate::eventlog::DB_FILENAME);

    // AIR-5: a prior cycle may have parked at a `requires_approval` stage and just
    // been resumed (approved -> the stage after it; "request changes" -> the same
    // stage again, with the reviewer's comment). `take_resume` hands this out at most
    // once, so a later dispatch of the same issue for an unrelated reason (retry,
    // manual re-trigger) doesn't replay it. An unresolvable stage id (stale config
    // between the decision and this dispatch) falls back to starting over from stage 0
    // rather than silently skipping the whole pipeline.
    let resume = match approvals::take_resume(&approvals_db_path(cfg), issue_id) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(issue_id = %issue_id, error = %e, "failed to read approval resume point; starting from stage 0");
            None
        }
    };
    let start_idx = resume
        .as_ref()
        .and_then(|r| cfg.pipeline.stages.iter().position(|s| s.id == r.stage_id))
        .unwrap_or(0);
    let resume_comment = resume.and_then(|r| r.reviewer_comment);

    // AIR-7: an index, not a `for`-loop over `&cfg.pipeline.stages`, so a Reviewer
    // stage's `request_changes` can rewind to an earlier Developer stage instead of
    // simply advancing (see the rework handling in the `LoopExit::Completed` arm
    // below) -- and so AIR-5's approval resume above can start mid-pipeline the same
    // way the old `.skip(start_idx)` did.
    let mut stage_idx: usize = start_idx;
    while stage_idx < cfg.pipeline.stages.len() {
        let stage = &cfg.pipeline.stages[stage_idx];
        let resume_note = if stage_idx == start_idx {
            resume_comment.as_deref()
        } else {
            None
        };

        // The test stage (AIR-6) is identified by convention (`id: test` plus a
        // configured `pipeline.test` block), the same way every other stage's `role`
        // is just a string until AIR-2 gives roles their own resolved behavior --
        // no new "stage kind" concept, just a naming convention two config keys agree
        // on. "Before touching anything" (the ticket's own words for the baseline run)
        // means literally before this stage's agent turns start writing tests.
        let is_test_stage = stage.id.eq_ignore_ascii_case("test") && cfg.pipeline.test.is_some();
        let baseline = if is_test_stage {
            match (&cfg.pipeline.test, &cfg.repo) {
                (Some(test_cfg), Some(repo)) => {
                    crate::quality::collect_baseline(
                        test_cfg,
                        &cfg.workflow_dir,
                        workspace_path,
                        &repo.default_branch,
                        cfg.hook_timeout_ms,
                        container,
                    )
                    .await
                }
                _ => None,
            }
        } else {
            None
        };

        // AIR-4: an `optional: true` stage can be opted out of per-issue by labeling
        // the issue `skip-<stage-id>` -- checked before role resolution, since a
        // skipped stage never actually runs a role.
        let skip_label = format!("skip-{}", stage.id.trim().to_lowercase());
        if stage.optional
            && issue
                .labels
                .iter()
                .any(|l| l.trim().to_lowercase() == skip_label)
        {
            let _ = tx.send(OrchMsg::StageStarted {
                issue_id: issue_id.to_string(),
                stage_id: stage.id.clone(),
                role_summary: format!("{} — skipped", stage.id),
            });
            let _ = tx.send(OrchMsg::StageFinished {
                issue_id: issue_id.to_string(),
                stage_id: stage.id.clone(),
                outcome: format!("skipped: issue labeled 'skip-{}'", stage.id),
            });
            stage_idx += 1;
            continue;
        }

        let role = match crate::roles::resolve(&stage.role, cfg) {
            Ok(r) => r,
            Err(e) => return ExitReason::Error(format!("stage '{}' role error: {e}", stage.id)),
        };
        let role_key = stage.role.trim().to_lowercase();
        let role_summary = if role.overrides_backend {
            format!(
                "{} — {} ({}{})",
                stage.id,
                stage.role,
                backend_label(role.backend),
                role.model
                    .as_ref()
                    .map(|m| format!("/{m}"))
                    .unwrap_or_default()
            )
        } else {
            format!("{} — {}", stage.id, stage.role)
        };
        let _ = tx.send(OrchMsg::StageStarted {
            issue_id: issue_id.to_string(),
            stage_id: stage.id.clone(),
            role_summary,
        });

        // AIR-3: tag the workspace with the stage now running (read back by
        // `record_artifact`, executing in the separate `__mcp_tool_server`
        // subprocess) and refresh its `.symphony/artifacts/` copies with whatever
        // earlier stages of this cycle have recorded so far -- covers a workspace
        // recreated since the last stage ran, not just a continuously-running one.
        crate::artifacts::prepare_workspace_for_stage(
            &artifacts_db_path,
            &cfg.workflow_dir,
            issue_id,
            workspace_path,
            &stage.id,
        );
        let cycle_artifacts = crate::artifacts::list_index(&artifacts_db_path, issue_id);
        // AIR-7: the Reviewer stage needs the working diff, but must not itself be
        // able to write files (`ToolPolicy::SWEBOT`) -- computed here, host-side, via
        // the hook plumbing (works identically against a plain host checkout or a
        // containerized one) and handed over as a workspace-relative path, the same
        // write-then-reference convention `swebot::review` already uses for the same
        // "a real diff can exceed the command line" reason.
        let diff_path = if role_key == "reviewer" {
            write_review_diff(cfg, workspace_path, container).await
        } else {
            None
        };
        let cycle_ctx = json!({
            "id": cycle_id,
            "stage": stage.id,
            "artifacts": cycle_artifacts,
            "previous_stage_summary": previous_stage_summary,
            "diff_path": diff_path.unwrap_or_default(),
        });

        let mut fresh_session: Option<Box<dyn AgentSession>> = None;
        if role.overrides_backend {
            let backend = crate::roles::build_backend(&role, cfg);
            let title = format!("{}: {} [{}]", issue.identifier, issue.title, stage.id);
            match backend
                .start_session(
                    workspace_path,
                    issue_id,
                    &title,
                    container,
                    &role.tool_policy,
                )
                .await
            {
                Ok(s) => fresh_session = Some(s),
                Err(e) => {
                    return ExitReason::Error(format!(
                        "stage '{}' session startup error: {e}",
                        stage.id
                    ));
                }
            }
        }
        let active_session: &mut dyn AgentSession = match &mut fresh_session {
            Some(s) => s.as_mut(),
            None => &mut *session,
        };

        let (mut outcome, mut last_message) = run_turn_loop(
            active_session,
            &snapshot.tracker,
            &cfg.active_states,
            &cfg.required_labels,
            &role.prompt,
            issue,
            attempt,
            stage.max_turns,
            issue_id,
            Some(&cycle_ctx),
            tx,
            resume_note,
        )
        .await;

        // `retry` gets exactly one extra attempt at the same stage before falling
        // through to the same handling `escalate` gets -- a bounded retry, not
        // unbounded looping.
        if matches!(outcome, LoopExit::Error(_)) && stage.on_failure == StageFailureAction::Retry {
            (outcome, last_message) = run_turn_loop(
                active_session,
                &snapshot.tracker,
                &cfg.active_states,
                &cfg.required_labels,
                &role.prompt,
                issue,
                attempt,
                stage.max_turns,
                issue_id,
                Some(&cycle_ctx),
                tx,
                resume_note,
            )
            .await;
        }

        if let Some(s) = fresh_session {
            s.stop().await;
        }

        // Run the configured suites/coverage for real once the agent's own turns are
        // done writing tests (skipped if the stage's turns errored out -- that failure
        // is handled by the normal `on_failure` path below, running tests against a
        // possibly half-written change adds nothing). A blocking coverage-gate miss is
        // handled exactly like a blocking stage failure: park the issue and stop, same
        // `block_issue` path `on_failure: escalate` already uses.
        if is_test_stage && !matches!(outcome, LoopExit::Error(_)) {
            let test_cfg = cfg
                .pipeline
                .test
                .as_ref()
                .expect("is_test_stage implies Some");
            let stage_outcome = crate::quality::run_test_stage(
                test_cfg,
                &cfg.workflow_dir,
                workspace_path,
                cfg.hook_timeout_ms,
                container,
                baseline.as_deref(),
                stage.blocking,
            )
            .await;
            emit_test_stage_events(tx, issue_id, &stage_outcome);

            if let crate::quality::CoverageGate::Blocking {
                percent,
                min_percent,
            } = stage_outcome.gate
            {
                let _ = tx.send(OrchMsg::StageFinished {
                    issue_id: issue_id.to_string(),
                    stage_id: stage.id.clone(),
                    outcome: format!(
                        "blocked: coverage {percent:.1}% below required {min_percent:.1}%"
                    ),
                });
                block_issue(snapshot, issue_id, &cfg.pipeline.blocked_state, None).await;
                return ExitReason::Normal;
            }
        }

        // AIR-8: a `security` stage's turns completing successfully is not the same
        // as the change passing security review -- evaluate the artifact the stage's
        // rubric (`roles::builtin::prompt_for("security")`) instructs the agent to
        // write, plus any configured scanners, and turn a threshold breach into the
        // same `LoopExit::Error` any other stage failure produces, so the existing
        // `on_failure`/`blocking` handling below (unchanged) is what actually parks
        // the issue.
        if stage.role.eq_ignore_ascii_case("security")
            && matches!(outcome, LoopExit::Completed | LoopExit::EndedByIssueState)
        {
            outcome =
                evaluate_security_stage(issue_id, &stage.id, workspace_path, container, cfg, tx)
                    .await
                    .unwrap_or(outcome);
        }

        let outcome_label = match &outcome {
            LoopExit::Completed => "completed".to_string(),
            LoopExit::EndedByIssueState => "ended by issue state".to_string(),
            LoopExit::Error(reason) if stage.on_failure == StageFailureAction::Skip => {
                format!("failed, skipped: {reason}")
            }
            LoopExit::Error(reason) => format!("failed: {reason}"),
            LoopExit::Blocked(question) => {
                format!("blocked: clarification needed: {question}")
            }
        };
        previous_stage_summary = format!("{}: {outcome_label}", stage.id);
        let _ = tx.send(OrchMsg::StageFinished {
            issue_id: issue_id.to_string(),
            stage_id: stage.id.clone(),
            outcome: outcome_label,
        });

        match outcome {
            LoopExit::Completed => {
                if stage.requires_approval {
                    let next_stage_id =
                        cfg.pipeline.stages.get(stage_idx + 1).map(|s| s.id.clone());
                    let auto_approved = handle_stage_approval(
                        issue_id,
                        issue,
                        stage,
                        next_stage_id,
                        last_message,
                        snapshot,
                        tx,
                    )
                    .await;
                    if auto_approved {
                        stage_idx += 1;
                        continue;
                    }
                    return ExitReason::Normal; // parked awaiting approval
                }
                // AIR-7: a Reviewer stage's `request_changes` recommendation sends the
                // cycle back to the nearest earlier Developer stage instead of simply
                // advancing -- a measured rework loop (roadmap §11: rework is a
                // recorded quantity, not silent looping), bounded by
                // `pipeline.review.max_rework_rounds`.
                if role_key == "reviewer"
                    && let Some((recommendation, summary)) = latest_review_recommendation(
                        &cfg.workflow_dir,
                        &artifacts_db_path,
                        issue_id,
                        &stage.id,
                    )
                    && recommendation == "request_changes"
                {
                    let round = crate::eventlog::record_rework_round(
                        &artifacts_db_path,
                        &crate::eventlog::NewReworkRound {
                            issue_id,
                            identifier: &issue.identifier,
                            title: &issue.title,
                            stage_id: &stage.id,
                            recommendation: &recommendation,
                            summary: &summary,
                            escalated: round_exceeds_limit(
                                &artifacts_db_path,
                                issue_id,
                                cfg.pipeline.review.max_rework_rounds,
                            ),
                        },
                    )
                    .unwrap_or(1);
                    if round > cfg.pipeline.review.max_rework_rounds as i64 {
                        block_issue(
                            snapshot,
                            issue_id,
                            &cfg.pipeline.blocked_state,
                            Some(&format!(
                                "Reviewer stage '{}' requested changes {round} times, exceeding \
                                 pipeline.review.max_rework_rounds ({}) -- escalating instead of \
                                 reworking again. Last review: {summary}",
                                stage.id, cfg.pipeline.review.max_rework_rounds
                            )),
                        )
                        .await;
                        return ExitReason::Normal;
                    }
                    match developer_stage_index(&cfg.pipeline.stages, stage_idx) {
                        Some(dev_idx) => {
                            previous_stage_summary = format!(
                                "{}: request_changes (rework round {round}) — {summary}",
                                stage.id
                            );
                            stage_idx = dev_idx;
                            continue;
                        }
                        None => {
                            block_issue(
                                snapshot,
                                issue_id,
                                &cfg.pipeline.blocked_state,
                                Some(&format!(
                                    "Reviewer stage '{}' requested changes but no earlier \
                                     'developer'-role stage exists to rework: {summary}",
                                    stage.id
                                )),
                            )
                            .await;
                            return ExitReason::Normal;
                        }
                    }
                }
                stage_idx += 1;
                continue;
            }
            LoopExit::EndedByIssueState => return ExitReason::Normal,
            // A blocking clarification stops the cycle exactly like a blocking stage
            // failure (AIR-4) -- same `block_issue` park, regardless of `stage.blocking`,
            // since this is the agent explicitly asking a human rather than an error.
            LoopExit::Blocked(question) => {
                block_issue(
                    snapshot,
                    issue_id,
                    &cfg.pipeline.blocked_state,
                    Some(&question),
                )
                .await;
                return ExitReason::Normal;
            }
            LoopExit::Error(reason) => {
                if stage.on_failure == StageFailureAction::Skip {
                    stage_idx += 1;
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
                    block_issue(snapshot, issue_id, &cfg.pipeline.blocked_state, None).await;
                    return ExitReason::Normal;
                }
                return ExitReason::Error(format!("stage '{}' failed: {reason}", stage.id));
            }
        }
    }
    ExitReason::Normal
}

/// `git diff <merge-base>..HEAD` in `workspace_path`, written to
/// `.symphony/review.diff` and returned as that workspace-relative path (AIR-7) --
/// run through the hook plumbing (`hooks::run_hook_maybe_containerized`) so it works
/// identically against a plain host checkout or a containerized one, rather than a
/// direct `Command::new("git")` that would only ever see the host side. `None` if the
/// script itself couldn't be launched at all; a missing/empty diff (no `repo:`
/// configured, nothing to diff against) still writes an empty file rather than
/// failing, since a Reviewer stage should still run -- just with less to go on.
async fn write_review_diff(
    cfg: &EffectiveConfig,
    workspace_path: &Path,
    container: Option<&ContainerHandle>,
) -> Option<String> {
    let default_branch = cfg
        .repo
        .as_ref()
        .map(|r| r.default_branch.clone())
        .unwrap_or_else(|| "main".to_string());
    let script = format!(
        "mkdir -p .symphony\n\
         base=\"{default_branch}\"\n\
         git rev-parse --verify \"$base\" >/dev/null 2>&1 || base=\"origin/{default_branch}\"\n\
         merge_base=$(git merge-base HEAD \"$base\" 2>/dev/null || echo \"$base\")\n\
         git diff \"$merge_base\"..HEAD > .symphony/review.diff 2>/dev/null || true\n"
    );
    match hooks::run_hook_maybe_containerized(
        "review_diff",
        &script,
        &cfg.workflow_dir,
        workspace_path,
        cfg.hook_timeout_ms,
        container,
    )
    .await
    {
        Ok(()) => Some(".symphony/review.diff".to_string()),
        Err(e) => {
            tracing::warn!(error = %e, "failed to compute the working diff for the reviewer stage (it will run without one)");
            None
        }
    }
}

/// Index of the nearest `role: developer` stage at or before `before_idx` -- AIR-7's
/// rework loop resends the cycle to the Developer stage that most recently fed this
/// Reviewer run, not always the pipeline's first one (a later hardening pass could
/// also be `role: developer`).
fn developer_stage_index(stages: &[config::StageConfig], before_idx: usize) -> Option<usize> {
    stages[..before_idx]
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| s.role.trim().to_lowercase() == "developer")
        .map(|(i, _)| i)
}

/// Whether recording one more rework round for `issue_id` would exceed
/// `max_rework_rounds` -- computed *before* the round is actually recorded so
/// `eventlog::record_rework_round`'s stored `escalated` flag reflects the outcome this
/// round produced, for the `/reviews` dashboard's explanation surface.
fn round_exceeds_limit(db_path: &Path, issue_id: &str, max_rework_rounds: u32) -> bool {
    let already = crate::eventlog::rework_rounds_for_issue(db_path, issue_id)
        .map(|rows| rows.len())
        .unwrap_or(0);
    (already as u32 + 1) > max_rework_rounds
}

/// The most recent `review_findings` artifact `stage_id` recorded this cycle, if any:
/// `(recommendation, summary)`. `summary` is the artifact's own human-readable
/// one-liner (`record_artifact`'s `summary` argument) rather than something re-derived
/// from the findings array -- already exactly what a human/the next Developer turn
/// needs, no second summarization step.
fn latest_review_recommendation(
    workflow_dir: &Path,
    db_path: &Path,
    issue_id: &str,
    stage_id: &str,
) -> Option<(String, String)> {
    let row = crate::artifacts::list_for_cycle(db_path, issue_id)
        .into_iter()
        .rfind(|r| r.kind == "review_findings" && r.stage_id.as_deref() == Some(stage_id))?;
    let bytes = crate::artifacts::read_content(workflow_dir, &row).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let recommendation = value.get("recommendation")?.as_str()?.to_string();
    Some((recommendation, row.summary))
}

fn backend_label(backend: AgentBackendKind) -> &'static str {
    match backend {
        AgentBackendKind::Claude => "claude",
        AgentBackendKind::Codex => "codex",
        AgentBackendKind::OpenCode => "opencode",
    }
}

/// Parks `issue_id` in `pipeline.blocked_state` after a blocking stage's failure or a
/// blocking clarification (AIR-4) -- deliberately host-side
/// (`TrackerAdapter::set_issue_state`, not the agent-facing `update_issue_state`
/// tool): the decision to stop the cycle is the orchestrator's, not something to ask
/// the (just-failed) agent to report on its own behalf. A tracker adapter that doesn't
/// support this (the default) logs a warning rather than failing the cycle outright --
/// the cycle still stops via `ExitReason::Normal` either way, this only affects
/// whether the tracker's own state reflects why.
///
/// `clarification` is `Some(question)` for a blocking `raise_clarification` call, in
/// which case the question is also posted as a comment on the issue where the tracker
/// adapter supports it (`TrackerAdapter::post_comment`, also default-unsupported --
/// the question is still visible either way via the `clarification_raised` event
/// already recorded when the tool was called). AIR-5 reuses this same function (with
/// `clarification: None`) to park a cycle in `pipeline.awaiting_approval_state` too --
/// same host-side "the orchestrator decided this, not the agent" shape, just a
/// different target state and no clarification text to post.
async fn block_issue(
    snapshot: &DispatchSnapshot,
    issue_id: &str,
    blocked_state: &str,
    clarification: Option<&str>,
) {
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
    if let Some(question) = clarification {
        let body = format!(
            "**Symphony: clarification needed before this cycle can continue.**\n\n\
             {question}\n\n\
             Reply on this issue (or edit its file directly, for the local tracker) \
             with an answer, then move it back to an active state to resume."
        );
        if let Err(e) = snapshot.tracker.post_comment(issue_id, &body).await {
            tracing::info!(
                issue_id = %issue_id,
                error = %e,
                "tracker adapter does not support post_comment; clarification is recorded in the event log/dashboard only"
            );
        }
    }
}

/// `symphony.db` lives alongside the eventlog, keyed by `workflow_dir` the same way
/// `eventlog::spawn_writer`'s caller resolves it -- one SQLite file, several tables,
/// not a second database to keep track of.
fn approvals_db_path(cfg: &EffectiveConfig) -> PathBuf {
    cfg.workflow_dir.join(crate::eventlog::DB_FILENAME)
}

/// A `requires_approval` stage just completed successfully. Either it matches
/// `pipeline.approval.auto_approve_when` (no human needed -- returns `true`, the
/// caller moves straight to the next stage) or it's parked: a pending-approval row is
/// recorded and the issue is moved to `pipeline.awaiting_approval_state` (host-side,
/// like `block_issue` -- this is the orchestrator's decision, not the agent's), and
/// the caller returns `ExitReason::Normal` to release the worker slot.
async fn handle_stage_approval(
    issue_id: &str,
    issue: &Issue,
    stage: &config::StageConfig,
    next_stage_id: Option<String>,
    last_message: Option<String>,
    snapshot: &DispatchSnapshot,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) -> bool {
    let cfg = &snapshot.config;
    let plan_json = last_message.as_deref().and_then(extract_plan_json);

    if let Some(cond) = &cfg.pipeline.approval.auto_approve_when
        && evaluate_auto_approve(plan_json.as_deref(), cond)
    {
        let _ = tx.send(OrchMsg::ApprovalAutoApproved {
            issue_id: issue_id.to_string(),
            stage_id: stage.id.clone(),
        });
        return true;
    }

    let db_path = approvals_db_path(cfg);
    let new = approvals::NewApproval {
        issue_id: issue_id.to_string(),
        identifier: issue.identifier.clone(),
        title: issue.title.clone(),
        stage_id: stage.id.clone(),
        next_stage_id,
        plan_text: last_message,
        plan_json,
    };
    let approval_id = match approvals::create_pending(&db_path, &new) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(issue_id = %issue_id, stage_id = %stage.id, error = %e, "failed to record pending approval; falling back to the plain blocking-stage park so the cycle doesn't silently spin");
            block_issue(
                snapshot,
                issue_id,
                &cfg.pipeline.awaiting_approval_state,
                None,
            )
            .await;
            return false;
        }
    };
    if let Err(e) = snapshot
        .tracker
        .set_issue_state(issue_id, &cfg.pipeline.awaiting_approval_state)
        .await
    {
        tracing::warn!(
            issue_id = %issue_id,
            awaiting_approval_state = %cfg.pipeline.awaiting_approval_state,
            error = %e,
            "failed to park issue in pipeline.awaiting_approval_state after a requires_approval stage"
        );
    }
    let _ = tx.send(OrchMsg::ApprovalRequested {
        issue_id: issue_id.to_string(),
        stage_id: stage.id.clone(),
        approval_id,
    });
    false
}

/// A `requires_approval` stage's output, as far as `auto_approve_when` can see it --
/// parsed from a fenced ```json block in the stage's last turn message (the same
/// convention `swebot`'s `qa`/`drafting`/`review` drivers already use to get
/// structured output from a free-text turn). Fields absent or unparseable never
/// satisfy a condition that checks them (see `evaluate_auto_approve`), so a stage that
/// didn't emit structured output simply never auto-approves.
#[derive(Debug, Default, serde::Deserialize)]
struct PlanSummary {
    #[serde(default)]
    risk: Option<String>,
    #[serde(default)]
    impacted_components: Vec<String>,
    #[serde(default)]
    estimate_turns: Option<u32>,
}

fn extract_plan_json(text: &str) -> Option<String> {
    let v = crate::swebot::extract_json_block(text).ok()?;
    serde_json::to_string_pretty(&v).ok()
}

/// Every condition set on `cond` (`None` fields are simply not checked) must hold
/// against `plan_json` for a `requires_approval` stage to skip the human. `plan_json`
/// being absent/unparseable fails every *set* condition -- never auto-approve on
/// missing information.
fn evaluate_auto_approve(plan_json: Option<&str>, cond: &config::AutoApproveWhen) -> bool {
    let plan: PlanSummary = plan_json
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();

    if let Some(want) = &cond.risk {
        match &plan.risk {
            Some(r) if r.trim().eq_ignore_ascii_case(want.trim()) => {}
            _ => return false,
        }
    }
    if let Some(allowlist) = &cond.impacted_components_allowlist {
        let allowed: HashSet<String> = allowlist.iter().map(|s| s.trim().to_lowercase()).collect();
        if !plan
            .impacted_components
            .iter()
            .all(|c| allowed.contains(&c.trim().to_lowercase()))
        {
            return false;
        }
    }
    if let Some(max_turns) = cond.max_estimate_turns {
        match plan.estimate_turns {
            Some(t) if t <= max_turns => {}
            _ => return false,
        }
    }
    true
}

/// Feeds the test stage's results into the same `OrchMsg::AgentEvent` pipe every other
/// agent event flows through (`run_one_turn`'s forwarder above) -- no new persistence
/// or dashboard-refresh mechanism: `handle_msg` already records every `AgentEvent` to
/// the event log and republishes the live status snapshot, and `/events` already lets
/// an operator browse by `event_type`. Three events, cheapest-to-richest: a one-line
/// summary for the dashboard/report column, then the full `test_report` and `coverage`
/// JSON for anyone who clicks through to see the per-suite/per-AC evidence.
fn emit_test_stage_events(
    tx: &mpsc::UnboundedSender<OrchMsg>,
    issue_id: &str,
    outcome: &crate::quality::TestStageOutcome,
) {
    let mut summary = AgentEvent::new("test_summary");
    summary.message = Some(outcome.report.summary_line());
    let _ = tx.send(OrchMsg::AgentEvent {
        issue_id: issue_id.to_string(),
        event: summary,
    });

    let mut coverage_summary = AgentEvent::new("coverage_summary");
    coverage_summary.message = Some(match outcome.coverage.line_percent() {
        Some(p) => format!("{p:.1}%"),
        None => "not measured".to_string(),
    });
    let _ = tx.send(OrchMsg::AgentEvent {
        issue_id: issue_id.to_string(),
        event: coverage_summary,
    });

    if let Ok(report_json) = serde_json::to_string(&outcome.report) {
        let mut ev = AgentEvent::new("test_report");
        ev.message = Some(report_json);
        let _ = tx.send(OrchMsg::AgentEvent {
            issue_id: issue_id.to_string(),
            event: ev,
        });
    }
    if let Ok(coverage_json) = serde_json::to_string(&outcome.coverage) {
        let mut ev = AgentEvent::new("coverage");
        ev.message = Some(coverage_json);
        let _ = tx.send(OrchMsg::AgentEvent {
            issue_id: issue_id.to_string(),
            event: ev,
        });
    }
}

/// AIR-8: evaluate a `security` stage's output after its turns complete. Reads the
/// `security_findings` artifact the stage's rubric (the built-in `security` role
/// prompt, `roles::builtin::prompt_for("security")`) instructs the agent to write to
/// `.symphony/security_findings.json` inside the workspace, folds in any configured
/// deterministic scanners, and decides whether the result breaches
/// `pipeline.security.block_on`.
///
/// Returns `None` when the stage should be treated as it already was (no threshold
/// breach); `Some(LoopExit::Error(reason))` when it should instead be treated as a
/// stage failure -- letting the existing `on_failure`/`blocking` handling in
/// `run_pipeline` decide what that means for the cycle, exactly like a turn error
/// would. A missing/invalid artifact is itself a failure: the stage's whole job is to
/// produce it.
async fn evaluate_security_stage(
    issue_id: &str,
    stage_id: &str,
    workspace_path: &Path,
    container: Option<&ContainerHandle>,
    cfg: &EffectiveConfig,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) -> Option<LoopExit> {
    let artifact_path = workspace_path
        .join(".symphony")
        .join("security_findings.json");
    let raw = match std::fs::read_to_string(&artifact_path) {
        Ok(s) => s,
        Err(e) => {
            return Some(LoopExit::Error(format!(
                "security stage produced no .symphony/security_findings.json artifact: {e}"
            )));
        }
    };
    let mut findings: crate::security::SecurityFindings = match serde_json::from_str(&raw) {
        Ok(f) => f,
        Err(e) => {
            return Some(LoopExit::Error(format!(
                "security_findings artifact is not valid JSON for the expected schema: {e}"
            )));
        }
    };
    if let Err(e) = findings.validate() {
        return Some(LoopExit::Error(format!(
            "security_findings artifact failed validation: {e}"
        )));
    }

    crate::security::run_scanners(
        &cfg.pipeline.security.scanners,
        &mut findings,
        &cfg.workflow_dir,
        workspace_path,
        cfg.hook_timeout_ms,
        container,
    )
    .await;
    findings.recompute_risk();

    let findings_json = serde_json::to_string(&findings).unwrap_or_default();
    let _ = tx.send(OrchMsg::SecurityEvaluated {
        issue_id: issue_id.to_string(),
        stage_id: stage_id.to_string(),
        risk: findings.risk_classification.as_str().to_string(),
        findings_json,
    });

    if !findings.is_blocking(&cfg.pipeline.security.block_on) {
        return None;
    }

    let threshold = cfg
        .pipeline
        .security
        .block_on
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let reason = format!(
        "security findings at or above the blocking threshold ({threshold}); overall risk: {}",
        findings.risk_classification.as_str()
    );

    let db_path = cfg.workflow_dir.join(crate::eventlog::DB_FILENAME);
    let pending = crate::eventlog::pending_override(&db_path, issue_id).unwrap_or(None);
    if let Some(over) = pending {
        let reason = over.message.unwrap_or_default();
        // Written synchronously, not via `tx`/`handle_msg`'s deferred event-log
        // writer: `pending_override` must never see this same override as pending
        // again once this function returns, or a second concurrent (or immediately
        // re-run) evaluation could consume it twice. The `tx.send` below is purely
        // for `/events` visibility -- the one-shot guarantee lives in this write.
        if let Err(e) = crate::eventlog::insert_event(
            &db_path,
            &crate::eventlog::NewEvent {
                issue_id: issue_id.to_string(),
                identifier: issue_id.to_string(),
                title: String::new(),
                session_id: None,
                event_type: "security_override_consumed".to_string(),
                message: Some(reason.clone()),
                input_tokens: None,
                output_tokens: None,
                total_tokens: None,
            },
        ) {
            tracing::warn!(issue_id = %issue_id, error = %e, "failed to record security override consumption");
        }
        let _ = tx.send(OrchMsg::SecurityOverrideConsumed {
            issue_id: issue_id.to_string(),
            reason,
        });
        return None;
    }

    let _ = tx.send(OrchMsg::SecurityBlocked {
        issue_id: issue_id.to_string(),
        reason: reason.clone(),
    });
    Some(LoopExit::Error(reason))
}

/// Returns the turn's outcome, plus two things extracted while forwarding its event
/// stream (so nothing needs a second pass over it): `Some(question)` if this turn
/// called `raise_clarification` with `blocking: true` (AIR-4, lets `run_turn_loop`
/// stop the cycle), and the turn's final text message, if any (AIR-5's approval gate
/// uses a `requires_approval` stage's last words as the plan content shown to a
/// human, `handle_stage_approval`).
async fn run_one_turn(
    session: &mut dyn AgentSession,
    prompt: &str,
    issue_id: &str,
    tx: &mpsc::UnboundedSender<OrchMsg>,
) -> (
    Result<TurnOutcome, crate::agent::AgentError>,
    Option<String>,
    Option<String>,
) {
    let (etx, mut erx) = mpsc::unbounded_channel::<AgentEvent>();
    let fwd_issue_id = issue_id.to_string();
    let fwd_tx = tx.clone();
    let forward = tokio::spawn(async move {
        let mut blocked_on: Option<String> = None;
        let mut last_message: Option<String> = None;
        while let Some(ev) = erx.recv().await {
            if ev.event == "clarification_raised"
                && let Some(msg) = &ev.message
                && let Ok(payload) = serde_json::from_str::<Value>(msg)
                && payload.get("blocking").and_then(|b| b.as_bool()) == Some(true)
            {
                blocked_on = Some(
                    payload
                        .get("question")
                        .and_then(|q| q.as_str())
                        .unwrap_or("(no question given)")
                        .to_string(),
                );
            }
            if let Some(m) = &ev.message {
                last_message = Some(m.clone());
            }
            let _ = fwd_tx.send(OrchMsg::AgentEvent {
                issue_id: fwd_issue_id.clone(),
                event: ev,
            });
        }
        (blocked_on, last_message)
    });
    let result = session.run_turn(prompt, etx).await;
    let (blocked_on, last_message) = forward.await.unwrap_or((None, None));
    (result, blocked_on, last_message)
}

fn render_turn_prompt(
    template_str: &str,
    issue: &Issue,
    attempt: Option<u32>,
    turn_number: u32,
    max_turns: u32,
    // AIR-2's `cycle.*` template namespace, with AIR-3's real per-cycle artifact index
    // already folded into its `"artifacts"` field by `run_pipeline` -- see that
    // function's own doc comment. `None` for the legacy single-stage path (its
    // `WORKFLOW.md` prompt never references `cycle.*`, so omitting the key entirely is
    // safe under `template::render`'s strict mode, which only errors on a *referenced*
    // unknown variable); every built-in pipeline role prompt that references
    // `cycle.artifacts` only ever runs with `Some(cycle)` supplied.
    cycle: Option<&serde_json::Value>,
) -> Result<String, template::TemplateError> {
    if turn_number == 1 {
        let mut ctx = json!({
            "issue": serde_json::to_value(issue).unwrap_or(serde_json::Value::Null),
            "attempt": attempt,
            // AIR-7: always present (like `cycle.artifacts`), not just for the
            // Reviewer role -- the same persona/checklist text `swebot::review` holds
            // its PR reviews to (`crate::review_rubric`), so a project that overrides
            // `roles.reviewer.prompt` can still reference `{{ rubric.* }}` and get the
            // same quality bar rather than having to copy-paste it.
            "rubric": {
                "persona": crate::review_rubric::PERSONA,
                "checklist": crate::review_rubric::CHECKLIST,
            },
        });
        if let Some(cycle) = cycle {
            ctx["cycle"] = cycle.clone();
        }
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
    /// still get its `after_run` hook run before its workspace is touched â€” not just
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
        /// Per-turn-number final message, captured by `run_one_turn`/`run_turn_loop`
        /// as the stage's `last_message` -- AIR-5's approval-gate tests need to control
        /// this (a plan's fenced ```json block) the same way `failures` controls
        /// outcomes. Absent turns just report "turn_completed" with no message, as
        /// every pre-AIR-5 test here already assumed.
        messages: HashMap<u32, String>,
        /// Every prompt run, in order -- AIR-5's "request changes" test asserts the
        /// reviewer's comment actually landed in the resumed stage's first prompt.
        prompts_seen: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptedBackend {
        fn new(calls: Arc<Mutex<u32>>, failures: HashMap<u32, String>) -> Self {
            Self {
                calls,
                failures,
                messages: HashMap::new(),
                prompts_seen: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait::async_trait]
    impl AgentBackend for ScriptedBackend {
        async fn start_session(
            &self,
            _workspace: &Path,
            _issue_id: &str,
            _title: &str,
            _container: Option<&ContainerHandle>,
            _tool_policy: &crate::agent::ToolPolicy,
        ) -> Result<Box<dyn AgentSession>, crate::agent::AgentError> {
            Ok(Box::new(ScriptedSession {
                calls: self.calls.clone(),
                failures: self.failures.clone(),
                messages: self.messages.clone(),
                prompts_seen: self.prompts_seen.clone(),
            }))
        }
    }

    struct ScriptedSession {
        calls: Arc<Mutex<u32>>,
        failures: HashMap<u32, String>,
        messages: HashMap<u32, String>,
        prompts_seen: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl AgentSession for ScriptedSession {
        fn session_id(&self) -> &str {
            "scripted-session"
        }

        async fn run_turn(
            &mut self,
            prompt: &str,
            events: mpsc::UnboundedSender<AgentEvent>,
        ) -> Result<TurnOutcome, crate::agent::AgentError> {
            self.prompts_seen.lock().unwrap().push(prompt.to_string());
            let n = {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                *calls
            };
            let event = match self.messages.get(&n) {
                Some(msg) => AgentEvent::new("turn_completed").with_message(msg.clone()),
                None => AgentEvent::new("turn_completed"),
            };
            let _ = events.send(event);
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
        // `tracker_dir` doubles as `workflow_dir` here (both are throwaway per-test
        // tempdirs already): AIR-3's artifact store and AIR-5's approvals store both
        // derive their db/blob paths from `cfg.workflow_dir`, and using "." like the
        // other tests below would make `run_pipeline` write `symphony.db`/`.symphony/`
        // into this crate's own working directory during `cargo test`.
        let cfg = config::resolve(&cfg_yaml, tracker_dir).unwrap();

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

    /// Like `pipeline_snapshot`, but with a real (tempdir) `workflow_dir` so
    /// `approvals_db_path` resolves to an isolated `symphony.db` instead of the repo's
    /// own working directory -- required for every AIR-5 test below, which all read or
    /// write approval rows. `extra_pipeline_yaml` lets a test add
    /// `awaiting_approval_state:`/`approval:` on top of the stage list.
    fn approval_pipeline_snapshot(
        tracker_dir: &Path,
        workflow_dir: &Path,
        extra_pipeline_yaml: &str,
        stages_yaml: &str,
        backend: ScriptedBackend,
    ) -> DispatchSnapshot {
        let cfg_yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
            "tracker:\n  kind: local\n  active_states: [todo]\n  terminal_states: [done]\n\
             pipeline:\n  enabled: true\n  blocked_state: blocked\n{extra_pipeline_yaml}  stages:\n{stages_yaml}"
        ))
        .unwrap();
        let mut cfg = config::resolve(&cfg_yaml, Path::new(".")).unwrap();
        cfg.workflow_dir = workflow_dir.to_path_buf();

        let provider: serde_yaml::Value =
            serde_yaml::from_str(&format!("dir: {:?}", tracker_dir)).unwrap();
        let tracker_adapter = tracker::build("local", &provider, Path::new(".")).unwrap();

        DispatchSnapshot {
            config: cfg,
            prompt_template: String::new(),
            tracker: Arc::from(tracker_adapter),
            agent_backend: Arc::new(backend),
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
            ScriptedBackend::new(calls.clone(), HashMap::new()),
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-1".to_string()])
            .await
            .unwrap()
            .remove(0);

        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
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

    /// AIR-2: the dashboard shows *which role* is driving a running stage
    /// (`status::RunningRow::stage`, fed by `OrchMsg::StageStarted::role_summary`), not
    /// just the stage id -- this is the human-observability surface for role
    /// resolution. A stage whose role doesn't override `agent.backend` gets a plain
    /// "stage — role" summary; a stage that does gets the backend/model appended too
    /// (covered by `roles::tests::build_backend_for_an_overriding_role_...` for the
    /// backend-selection mechanics themselves).
    #[tokio::test]
    async fn stage_started_role_summary_names_the_resolved_role() {
        let tracker_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-3");
        let snapshot = pipeline_snapshot(
            tracker_dir.path(),
            "  - id: review\n    role: reviewer\n    max_turns: 1\n",
            ScriptedBackend::new(Arc::new(Mutex::new(0)), HashMap::new()),
        );
        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-3".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;

        let msgs = drain(rx).await;
        let role_summary = msgs
            .iter()
            .find_map(|m| match m {
                OrchMsg::StageStarted { role_summary, .. } => Some(role_summary.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(role_summary, "review — reviewer");
    }

    /// End-to-end AIR-6 wiring test: a `pipeline.test` stage actually runs the
    /// configured suite/coverage commands against `workspace_path` (not just the
    /// agent's own turns), emits `test_report`/`coverage` evidence, and -- since the
    /// stage is `blocking: true` and coverage is below `min_line_percent` -- parks the
    /// issue exactly like any other blocking stage failure.
    #[tokio::test]
    async fn test_stage_runs_suites_and_blocks_on_coverage_gate_when_stage_is_blocking() {
        let tracker_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "T-1");
        let workspace_dir = tempdir().unwrap();
        std::fs::write(
            workspace_dir.path().join("coverage.json"),
            r#"{"data":[{"files":[{"filename":"a.rs","summary":{"lines":{"count":10,"covered":1}}}]}]}"#,
        )
        .unwrap();

        let cfg_yaml: serde_yaml::Value = serde_yaml::from_str(
            "tracker:\n  kind: local\n  active_states: [todo]\n  terminal_states: [done]\n\
             pipeline:\n  enabled: true\n  blocked_state: blocked\n  stages:\n    \
             - id: test\n      role: test\n      max_turns: 1\n      blocking: true\n  \
             test:\n    commands:\n      unit: exit 0\n    coverage:\n      command: \"true\"\n      \
             format: llvm-cov\n      min_line_percent: 90\n",
        )
        .unwrap();
        let cfg = config::resolve(&cfg_yaml, Path::new(".")).unwrap();

        let provider: serde_yaml::Value =
            serde_yaml::from_str(&format!("dir: {:?}", tracker_dir.path())).unwrap();
        let tracker_adapter = tracker::build("local", &provider, Path::new(".")).unwrap();

        let snapshot = DispatchSnapshot {
            config: cfg,
            prompt_template: String::new(),
            tracker: Arc::from(tracker_adapter),
            agent_backend: Arc::new(ScriptedBackend::new(
                Arc::new(Mutex::new(0)),
                HashMap::new(),
            )),
            workspace_mgr: Arc::new(WorkspaceManager::new(PathBuf::from("unused"))),
        };

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["T-1".to_string()])
            .await
            .unwrap()
            .remove(0);
        let mut session = snapshot
            .agent_backend
            .start_session(
                Path::new("."),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace_dir.path(),
            None,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));

        let msgs = drain(rx).await;
        let event_types: Vec<String> = msgs
            .iter()
            .filter_map(|m| match m {
                OrchMsg::AgentEvent { event, .. } => Some(event.event.clone()),
                _ => None,
            })
            .collect();
        assert!(
            event_types.contains(&"test_summary".to_string()),
            "{event_types:?}"
        );
        assert!(
            event_types.contains(&"coverage_summary".to_string()),
            "{event_types:?}"
        );
        assert!(
            event_types.contains(&"test_report".to_string()),
            "{event_types:?}"
        );
        assert!(
            event_types.contains(&"coverage".to_string()),
            "{event_types:?}"
        );

        let outcomes: Vec<String> = msgs
            .iter()
            .filter_map(|m| match m {
                OrchMsg::StageFinished { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .collect();
        assert!(
            outcomes.iter().any(|o| o.starts_with("blocked: coverage")),
            "{outcomes:?}"
        );

        let refreshed = snapshot
            .tracker
            .fetch_issues_by_ids(&["T-1".to_string()])
            .await
            .unwrap()
            .remove(0);
        assert_eq!(refreshed.normalized_state(), "blocked");
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
            ScriptedBackend::new(Arc::new(Mutex::new(0)), failures),
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-2".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
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
            ScriptedBackend::new(Arc::new(Mutex::new(0)), failures),
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-3".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
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
            ScriptedBackend::new(calls.clone(), failures),
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-4".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
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
            ScriptedBackend::new(Arc::new(Mutex::new(0)), failures),
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-5".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
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
    // AIR-8: security stage evaluation (`evaluate_security_stage`)
    // -----------------------------------------------------------------------------

    fn write_security_artifact(workspace: &Path, severity: &str) {
        let dir = workspace.join(".symphony");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("security_findings.json"),
            format!(
                r#"{{"schema_version":1,"risk_classification":"low","owasp_checklist":[
                    {{"id":"A01:2021","name":"Broken Access Control","applicable":false,
                     "status":"not_applicable","evidence":"no auth surface touched"}}],
                "findings":[{{"id":"S1","severity":"{severity}","owasp_id":"A03:2021",
                    "cwe":"CWE-89","file":"src/x.rs","line":10,"summary":"sql injection",
                    "exploit_scenario":"...", "remediation":"..."}}],
                "secrets_scan":{{"status":"clean","matches":[]}},
                "dependency_scan":{{"tool":"","status":"not_run","advisories":[]}}}}"#
            ),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn security_stage_blocking_finding_parks_the_issue() {
        let tracker_dir = tempdir().unwrap();
        let workflow_dir = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-SEC-1");
        write_security_artifact(workspace.path(), "critical");

        let mut snapshot = pipeline_snapshot(
            tracker_dir.path(),
            "  - id: security\n    role: security\n    max_turns: 1\n    blocking: true\n",
            ScriptedBackend::new(Arc::new(Mutex::new(0)), HashMap::new()),
        );
        snapshot.config.workflow_dir = workflow_dir.path().to_path_buf();

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-SEC-1".to_string()])
            .await
            .unwrap()
            .remove(0);
        let mut session = snapshot
            .agent_backend
            .start_session(
                Path::new("."),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));

        let refreshed = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-SEC-1".to_string()])
            .await
            .unwrap();
        assert_eq!(refreshed[0].normalized_state(), "blocked");

        let msgs = drain(rx).await;
        assert!(
            msgs.iter()
                .any(|m| matches!(m, OrchMsg::SecurityBlocked { .. }))
        );
        assert!(
            msgs.iter().any(
                |m| matches!(m, OrchMsg::SecurityEvaluated { risk, .. } if risk == "critical")
            )
        );
    }

    #[tokio::test]
    async fn security_stage_pending_override_unblocks_and_is_consumed() {
        let tracker_dir = tempdir().unwrap();
        let workflow_dir = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-SEC-2");
        write_security_artifact(workspace.path(), "critical");

        let db_path = workflow_dir.path().join(crate::eventlog::DB_FILENAME);
        crate::eventlog::insert_event(
            &db_path,
            &crate::eventlog::NewEvent {
                issue_id: "P-SEC-2".to_string(),
                identifier: "P-SEC-2".to_string(),
                title: "Test issue".to_string(),
                session_id: None,
                event_type: "security_override".to_string(),
                message: Some("accepted risk: fix scheduled next sprint".to_string()),
                input_tokens: None,
                output_tokens: None,
                total_tokens: None,
            },
        )
        .unwrap();

        let mut snapshot = pipeline_snapshot(
            tracker_dir.path(),
            "  - id: security\n    role: security\n    max_turns: 1\n    blocking: true\n",
            ScriptedBackend::new(Arc::new(Mutex::new(0)), HashMap::new()),
        );
        snapshot.config.workflow_dir = workflow_dir.path().to_path_buf();

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-SEC-2".to_string()])
            .await
            .unwrap()
            .remove(0);
        let mut session = snapshot
            .agent_backend
            .start_session(
                Path::new("."),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));

        let refreshed = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-SEC-2".to_string()])
            .await
            .unwrap();
        assert_eq!(
            refreshed[0].normalized_state(),
            "todo",
            "an overridden block must not park the issue"
        );

        let msgs = drain(rx).await;
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, OrchMsg::SecurityBlocked { .. }))
        );
        assert!(msgs.iter().any(
            |m| matches!(m, OrchMsg::SecurityOverrideConsumed { reason, .. } if reason.contains("fix scheduled"))
        ));

        // One-shot: a second evaluation with the same pending state must not still be
        // able to use the already-consumed override.
        assert!(
            crate::eventlog::pending_override(&db_path, "P-SEC-2")
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn security_stage_clean_findings_do_not_block() {
        let tracker_dir = tempdir().unwrap();
        let workflow_dir = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-SEC-3");
        write_security_artifact(workspace.path(), "low");

        let mut snapshot = pipeline_snapshot(
            tracker_dir.path(),
            "  - id: security\n    role: security\n    max_turns: 1\n    blocking: true\n",
            ScriptedBackend::new(Arc::new(Mutex::new(0)), HashMap::new()),
        );
        snapshot.config.workflow_dir = workflow_dir.path().to_path_buf();

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-SEC-3".to_string()])
            .await
            .unwrap()
            .remove(0);
        let mut session = snapshot
            .agent_backend
            .start_session(
                Path::new("."),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));

        let refreshed = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-SEC-3".to_string()])
            .await
            .unwrap();
        assert_eq!(refreshed[0].normalized_state(), "todo");
    }

    #[tokio::test]
    async fn security_stage_missing_artifact_is_treated_as_a_stage_failure() {
        let tracker_dir = tempdir().unwrap();
        let workflow_dir = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-SEC-4");
        // Deliberately no `.symphony/security_findings.json` written.

        let mut snapshot = pipeline_snapshot(
            tracker_dir.path(),
            "  - id: security\n    role: security\n    max_turns: 1\n    blocking: true\n",
            ScriptedBackend::new(Arc::new(Mutex::new(0)), HashMap::new()),
        );
        snapshot.config.workflow_dir = workflow_dir.path().to_path_buf();

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-SEC-4".to_string()])
            .await
            .unwrap()
            .remove(0);
        let mut session = snapshot
            .agent_backend
            .start_session(
                Path::new("."),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));

        let refreshed = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-SEC-4".to_string()])
            .await
            .unwrap();
        assert_eq!(refreshed[0].normalized_state(), "blocked");

        let msgs = drain(rx).await;
        let stages = stage_events(&msgs);
        assert!(
            stages
                .iter()
                .any(|(kind, id)| *kind == "finished" && *id == "security")
        );
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
        // `on_tick` now always calls AIR-5's `poll_approval_comments`/
        // `apply_resolved_approvals`, which touch `approvals_db_path(cfg)` -- keep
        // that inside this test's own tempdir rather than the real repo root.
        cfg.workflow_dir = root.path().to_path_buf();

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

    // -----------------------------------------------------------------------------
    // AIR-4: raise_clarification (blocking/non-blocking), optional stage skip, and
    // the requirements stage picking up its builtin role prompt (`roles::resolve`,
    // AIR-3/AIR-2's role machinery -- AIR-4's own now-removed `builtin_prompt` helper
    // did the same lookup before roles were generalized).
    // -----------------------------------------------------------------------------

    /// Like `ScriptedBackend`, but additionally records every prompt it's given (to
    /// prove a stage with a matching `role` actually runs the builtin role prompt
    /// instead of the project's own template -- via `roles::resolve`) and can be
    /// scripted to report a `raise_clarification` call (as the real agent event
    /// stream would: a `clarification_raised` event alongside `turn_completed`,
    /// exactly what `agent/claude.rs::handle_message` emits) on a given turn.
    struct ClarifyingBackend {
        calls: Arc<Mutex<u32>>,
        prompts: Arc<Mutex<Vec<String>>>,
        /// `(turn number, blocking)` -- `None` means never raise a clarification.
        clarify_on_turn: Option<(u32, bool)>,
    }

    #[async_trait::async_trait]
    impl AgentBackend for ClarifyingBackend {
        async fn start_session(
            &self,
            _workspace: &Path,
            _issue_id: &str,
            _title: &str,
            _container: Option<&ContainerHandle>,
            _tool_policy: &crate::agent::ToolPolicy,
        ) -> Result<Box<dyn AgentSession>, crate::agent::AgentError> {
            Ok(Box::new(ClarifyingSession {
                calls: self.calls.clone(),
                prompts: self.prompts.clone(),
                clarify_on_turn: self.clarify_on_turn,
            }))
        }
    }

    struct ClarifyingSession {
        calls: Arc<Mutex<u32>>,
        prompts: Arc<Mutex<Vec<String>>>,
        clarify_on_turn: Option<(u32, bool)>,
    }

    #[async_trait::async_trait]
    impl AgentSession for ClarifyingSession {
        fn session_id(&self) -> &str {
            "clarifying-session"
        }

        async fn run_turn(
            &mut self,
            prompt: &str,
            events: mpsc::UnboundedSender<AgentEvent>,
        ) -> Result<TurnOutcome, crate::agent::AgentError> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            let n = {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                *calls
            };
            if let Some((turn, blocking)) = self.clarify_on_turn
                && turn == n
            {
                let _ = events.send(AgentEvent::new("clarification_raised").with_message(
                    json!({"question": "which auth scheme?", "blocking": blocking}).to_string(),
                ));
            }
            let _ = events.send(AgentEvent::new("turn_completed"));
            Ok(TurnOutcome::Completed { usage: None })
        }

        async fn stop(self: Box<Self>) {}
    }

    fn clarifying_snapshot(
        tracker_dir: &Path,
        stages_yaml: &str,
        backend: ClarifyingBackend,
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
            workspace_mgr: Arc::new(WorkspaceManager::new(PathBuf::from("unused"))),
        }
    }

    #[tokio::test]
    async fn blocking_clarification_stops_the_cycle_and_parks_the_issue() {
        let tracker_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-6");
        let snapshot = clarifying_snapshot(
            tracker_dir.path(),
            "  - id: requirements\n    role: requirements\n    max_turns: 3\n\
             \x20\x20- id: implement\n    role: developer\n    max_turns: 1\n",
            ClarifyingBackend {
                calls: Arc::new(Mutex::new(0)),
                prompts: Arc::new(Mutex::new(Vec::new())),
                clarify_on_turn: Some((1, true)),
            },
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-6".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));

        let refreshed = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-6".to_string()])
            .await
            .unwrap();
        assert_eq!(refreshed[0].state, "blocked");

        let msgs = drain(rx).await;
        let stages = stage_events(&msgs);
        // Stopped right after the first turn of the first stage -- never even
        // finished out that stage's own turn budget (3), let alone started the
        // second stage.
        assert_eq!(
            stages,
            vec![("started", "requirements"), ("finished", "requirements")]
        );
        assert!(msgs.iter().any(
            |m| matches!(m, OrchMsg::StageFinished { outcome, .. } if outcome.contains("blocked: clarification"))
        ));
    }

    #[tokio::test]
    async fn non_blocking_clarification_does_not_stop_the_cycle() {
        let tracker_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-7");
        let calls = Arc::new(Mutex::new(0));
        let snapshot = clarifying_snapshot(
            tracker_dir.path(),
            "  - id: requirements\n    role: requirements\n    max_turns: 1\n\
             \x20\x20- id: implement\n    role: developer\n    max_turns: 1\n",
            ClarifyingBackend {
                calls: calls.clone(),
                prompts: Arc::new(Mutex::new(Vec::new())),
                clarify_on_turn: Some((1, false)),
            },
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-7".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));
        assert_eq!(*calls.lock().unwrap(), 2, "both stages' turns ran");

        let refreshed = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-7".to_string()])
            .await
            .unwrap();
        assert_eq!(
            refreshed[0].state, "todo",
            "non-blocking clarification must not park the issue"
        );

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
    async fn optional_stage_is_skipped_when_issue_carries_its_skip_label() {
        let tracker_dir = tempdir().unwrap();
        std::fs::write(
            tracker_dir.path().join("P-8.md"),
            "---\nidentifier: P-8\ntitle: Test issue\nstate: todo\nlabels: [skip-requirements]\n---\nbody\n",
        )
        .unwrap();
        let calls = Arc::new(Mutex::new(0));
        let snapshot = clarifying_snapshot(
            tracker_dir.path(),
            "  - id: requirements\n    role: requirements\n    max_turns: 1\n    optional: true\n\
             \x20\x20- id: implement\n    role: developer\n    max_turns: 1\n",
            ClarifyingBackend {
                calls: calls.clone(),
                prompts: Arc::new(Mutex::new(Vec::new())),
                clarify_on_turn: None,
            },
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-8".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));
        // Only `implement`'s one turn ran -- `requirements` was skipped entirely.
        assert_eq!(*calls.lock().unwrap(), 1);

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
        assert!(msgs.iter().any(
            |m| matches!(m, OrchMsg::StageFinished { stage_id, outcome, .. } if stage_id == "requirements" && outcome.starts_with("skipped"))
        ));
    }

    #[tokio::test]
    async fn requirements_stage_runs_the_builtin_role_prompt() {
        let tracker_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "P-9");
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let snapshot = clarifying_snapshot(
            tracker_dir.path(),
            "  - id: requirements\n    role: requirements\n    max_turns: 1\n\
             \x20\x20- id: implement\n    role: developer\n    max_turns: 1\n",
            ClarifyingBackend {
                calls: Arc::new(Mutex::new(0)),
                prompts: prompts.clone(),
                clarify_on_turn: None,
            },
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-9".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();

        run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;

        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        // The `requirements` stage got the builtin role prompt (mentions the tools
        // it's meant to call)...
        assert!(prompts[0].contains("record_requirements"));
        assert!(prompts[0].contains("raise_clarification"));
        // ...while `implement` (role "developer") got *its own* built-in role prompt
        // (AIR-2/AIR-3's `roles::resolve` -- "developer" is one of the eight roadmap
        // roles, not a fallback to the project's own `WORKFLOW.md` template).
        assert!(prompts[1].contains("Developer agent"));
        assert_ne!(prompts[0], prompts[1]);
    }

    /// AIR-4 acceptance criterion: "Requirements with `depends_on` blockers list them
    /// under `dependency`." The requirements stage's prompt (`roles::resolve`)
    /// is the thing that has to make the blocker visible to the agent so it can put it
    /// in the `dependency` field it hands to `record_requirements` -- this proves
    /// `issue.blocked_by` (populated by `depends_on` resolution, `tracker::depends_on`)
    /// actually reaches the rendered prompt text.
    #[tokio::test]
    async fn requirements_prompt_surfaces_depends_on_blockers() {
        let tracker_dir = tempdir().unwrap();
        std::fs::write(
            tracker_dir.path().join("BLOCKER.md"),
            "---\nidentifier: BLOCKER\ntitle: Prerequisite\nstate: todo\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            tracker_dir.path().join("P-10.md"),
            "---\nidentifier: P-10\ntitle: Test issue\nstate: todo\ndepends_on: [BLOCKER]\n---\nbody\n",
        )
        .unwrap();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let snapshot = clarifying_snapshot(
            tracker_dir.path(),
            "  - id: requirements\n    role: requirements\n    max_turns: 1\n",
            ClarifyingBackend {
                calls: Arc::new(Mutex::new(0)),
                prompts: prompts.clone(),
                clarify_on_turn: None,
            },
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["P-10".to_string()])
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            issue.blocked_by[0].identifier.as_deref(),
            Some("BLOCKER"),
            "sanity check: depends_on resolution populated blocked_by"
        );
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();

        run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;

        let prompts = prompts.lock().unwrap();
        assert!(prompts[0].contains("BLOCKER"), "{}", prompts[0]);
    }
    // AIR-5: the human approval gate
    // -----------------------------------------------------------------------------

    /// Like `test_shared`, but with a real (tempdir) `workflow_dir` and a pipeline
    /// config -- for the tick-loop halves of the approval gate
    /// (`poll_approval_comments`/`apply_resolved_approvals`), which need a full
    /// `Shared` (tracker + event log), not just a `DispatchSnapshot`.
    fn approval_shared(
        tracker_dir: &Path,
        workflow_dir: &Path,
        extra_pipeline_yaml: &str,
        stages_yaml: &str,
    ) -> Shared {
        let cfg_yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
            "tracker:\n  kind: local\n  active_states: [todo]\n  terminal_states: [done]\n\
             pipeline:\n  enabled: true\n  blocked_state: blocked\n{extra_pipeline_yaml}  stages:\n{stages_yaml}"
        ))
        .unwrap();
        let mut cfg = config::resolve(&cfg_yaml, Path::new(".")).unwrap();
        cfg.workflow_dir = workflow_dir.to_path_buf();

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
            workspace_mgr: Arc::new(WorkspaceManager::new(workflow_dir.to_path_buf())),
            event_tx,
        }
    }

    const TWO_STAGE_PLAN_REQUIRES_APPROVAL: &str = "  - id: plan\n    role: planner\n    max_turns: 1\n    requires_approval: true\n\
         \x20\x20- id: implement\n    role: developer\n    max_turns: 1\n";

    #[tokio::test]
    async fn requires_approval_stage_parks_the_cycle_without_running_the_next_stage() {
        let tracker_dir = tempdir().unwrap();
        let workflow_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "AP-1");
        let calls = Arc::new(Mutex::new(0));
        let snapshot = approval_pipeline_snapshot(
            tracker_dir.path(),
            workflow_dir.path(),
            "",
            TWO_STAGE_PLAN_REQUIRES_APPROVAL,
            ScriptedBackend::new(calls.clone(), HashMap::new()),
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["AP-1".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));
        // Only "plan" ran -- parking must happen *before* "implement" starts, and the
        // worker's own turn budget is spent, i.e. the slot is free to be reclaimed by
        // the caller (`run_agent_attempt`/`handle_msg`'s normal WorkerExit path).
        assert_eq!(*calls.lock().unwrap(), 1);

        let msgs = drain(rx).await;
        let stages = stage_events(&msgs);
        assert_eq!(stages, vec![("started", "plan"), ("finished", "plan")]);
        assert!(msgs.iter().any(
            |m| matches!(m, OrchMsg::ApprovalRequested { stage_id, .. } if stage_id == "plan")
        ));

        let refreshed = snapshot
            .tracker
            .fetch_issues_by_ids(&["AP-1".to_string()])
            .await
            .unwrap();
        assert_eq!(refreshed[0].state, "awaiting approval");

        let db_path = approvals_db_path(&snapshot.config);
        let pending = approvals::list_pending(&db_path).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].stage_id, "plan");
        assert_eq!(pending[0].next_stage_id.as_deref(), Some("implement"));
    }

    #[tokio::test]
    async fn requires_approval_stage_auto_approves_when_policy_matches() {
        let tracker_dir = tempdir().unwrap();
        let workflow_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "AP-2");
        let calls = Arc::new(Mutex::new(0));
        let mut backend = ScriptedBackend::new(calls.clone(), HashMap::new());
        backend.messages.insert(
            1,
            "design done.\n```json\n{\"risk\": \"low\"}\n```".to_string(),
        );
        let snapshot = approval_pipeline_snapshot(
            tracker_dir.path(),
            workflow_dir.path(),
            "  approval:\n    auto_approve_when:\n      risk: low\n",
            TWO_STAGE_PLAN_REQUIRES_APPROVAL,
            backend,
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["AP-2".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();

        let exit = run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));
        // Both stages ran in the same cycle -- auto-approval never parked it.
        assert_eq!(*calls.lock().unwrap(), 2);

        let msgs = drain(rx).await;
        let stages = stage_events(&msgs);
        assert_eq!(
            stages,
            vec![
                ("started", "plan"),
                ("finished", "plan"),
                ("started", "implement"),
                ("finished", "implement"),
            ]
        );
        assert!(msgs.iter().any(
            |m| matches!(m, OrchMsg::ApprovalAutoApproved { stage_id, .. } if stage_id == "plan")
        ));
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, OrchMsg::ApprovalRequested { .. }))
        );

        let db_path = approvals_db_path(&snapshot.config);
        assert!(approvals::list_pending(&db_path).unwrap().is_empty());
        // The issue was never moved out of its active state.
        let refreshed = snapshot
            .tracker
            .fetch_issues_by_ids(&["AP-2".to_string()])
            .await
            .unwrap();
        assert_eq!(refreshed[0].state, "todo");
    }

    #[tokio::test]
    async fn auto_approve_when_never_fires_without_matching_structured_output() {
        let tracker_dir = tempdir().unwrap();
        let workflow_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "AP-3");
        // The stage completes but never emits a ```json block -- auto_approve_when
        // must not fire on missing information, even though it's configured.
        let snapshot = approval_pipeline_snapshot(
            tracker_dir.path(),
            workflow_dir.path(),
            "  approval:\n    auto_approve_when:\n      risk: low\n",
            TWO_STAGE_PLAN_REQUIRES_APPROVAL,
            ScriptedBackend::new(Arc::new(Mutex::new(0)), HashMap::new()),
        );

        let mut issue = snapshot
            .tracker
            .fetch_issues_by_ids(&["AP-3".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();

        run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot,
            workspace.path(),
            None,
            &tx,
        )
        .await;

        let db_path = approvals_db_path(&snapshot.config);
        assert_eq!(approvals::list_pending(&db_path).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn approve_decision_resumes_at_the_next_stage_without_rerunning_the_first() {
        let tracker_dir = tempdir().unwrap();
        let workflow_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "AP-4");

        // First cycle: park at "plan".
        let snapshot1 = approval_pipeline_snapshot(
            tracker_dir.path(),
            workflow_dir.path(),
            "",
            TWO_STAGE_PLAN_REQUIRES_APPROVAL,
            ScriptedBackend::new(Arc::new(Mutex::new(0)), HashMap::new()),
        );
        let mut issue = snapshot1
            .tracker
            .fetch_issues_by_ids(&["AP-4".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot1
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot1,
            workspace.path(),
            None,
            &tx,
        )
        .await;

        let db_path = approvals_db_path(&snapshot1.config);
        let pending = approvals::list_pending(&db_path).unwrap();
        assert_eq!(pending.len(), 1);
        let approval_id = pending[0].id;

        // A human approves via whichever channel -- simulate exactly what
        // `apply_resolved_approvals` does once a decision is resolved.
        assert!(
            approvals::resolve(
                &db_path,
                approval_id,
                approvals::Decision::Approve,
                "alice",
                None
            )
            .unwrap()
        );
        approvals::mark_applied(&db_path, approval_id, pending[0].next_stage_id.as_deref())
            .unwrap();

        // Second cycle, fresh session/backend: "plan" must NOT run again.
        let calls2 = Arc::new(Mutex::new(0));
        let snapshot2 = approval_pipeline_snapshot(
            tracker_dir.path(),
            workflow_dir.path(),
            "",
            TWO_STAGE_PLAN_REQUIRES_APPROVAL,
            ScriptedBackend::new(calls2.clone(), HashMap::new()),
        );
        let mut issue2 = snapshot2
            .tracker
            .fetch_issues_by_ids(&["AP-4".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace2 = tempdir().unwrap();
        let mut session2 = snapshot2
            .agent_backend
            .start_session(
                workspace2.path(),
                &issue2.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx2, rx2) = mpsc::unbounded_channel();
        let exit = run_pipeline(
            &issue2.id.clone(),
            &mut issue2,
            None,
            session2.as_mut(),
            &snapshot2,
            workspace2.path(),
            None,
            &tx2,
        )
        .await;
        assert!(matches!(exit, ExitReason::Normal));
        assert_eq!(*calls2.lock().unwrap(), 1, "only 'implement' should run");

        let msgs2 = drain(rx2).await;
        let stages = stage_events(&msgs2);
        assert_eq!(
            stages,
            vec![("started", "implement"), ("finished", "implement")]
        );

        assert!(
            approvals::take_resume(&db_path, "AP-4").unwrap().is_none(),
            "a resume point must only be consumed once"
        );
    }

    #[tokio::test]
    async fn request_changes_resumes_the_same_stage_with_the_reviewer_comment_injected() {
        let tracker_dir = tempdir().unwrap();
        let workflow_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "AP-5");

        let snapshot1 = approval_pipeline_snapshot(
            tracker_dir.path(),
            workflow_dir.path(),
            "",
            TWO_STAGE_PLAN_REQUIRES_APPROVAL,
            ScriptedBackend::new(Arc::new(Mutex::new(0)), HashMap::new()),
        );
        let mut issue = snapshot1
            .tracker
            .fetch_issues_by_ids(&["AP-5".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace = tempdir().unwrap();
        let mut session = snapshot1
            .agent_backend
            .start_session(
                workspace.path(),
                &issue.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        run_pipeline(
            &issue.id.clone(),
            &mut issue,
            None,
            session.as_mut(),
            &snapshot1,
            workspace.path(),
            None,
            &tx,
        )
        .await;

        let db_path = approvals_db_path(&snapshot1.config);
        let approval_id = approvals::list_pending(&db_path).unwrap()[0].id;
        let reviewer_comment = "please add a rollback plan for the migration";
        assert!(
            approvals::resolve(
                &db_path,
                approval_id,
                approvals::Decision::RequestChanges,
                "alice",
                Some(reviewer_comment),
            )
            .unwrap()
        );
        // "changes" resumes at the *same* stage ("plan"), not `next_stage_id`.
        approvals::mark_applied(&db_path, approval_id, Some("plan")).unwrap();

        let backend2 = ScriptedBackend::new(Arc::new(Mutex::new(0)), HashMap::new());
        let prompts_seen = backend2.prompts_seen.clone();
        let snapshot2 = approval_pipeline_snapshot(
            tracker_dir.path(),
            workflow_dir.path(),
            "",
            TWO_STAGE_PLAN_REQUIRES_APPROVAL,
            backend2,
        );
        let mut issue2 = snapshot2
            .tracker
            .fetch_issues_by_ids(&["AP-5".to_string()])
            .await
            .unwrap()
            .remove(0);
        let workspace2 = tempdir().unwrap();
        let mut session2 = snapshot2
            .agent_backend
            .start_session(
                workspace2.path(),
                &issue2.id,
                "t",
                None,
                &crate::agent::ToolPolicy::default(),
            )
            .await
            .unwrap();
        let (tx2, rx2) = mpsc::unbounded_channel();
        run_pipeline(
            &issue2.id.clone(),
            &mut issue2,
            None,
            session2.as_mut(),
            &snapshot2,
            workspace2.path(),
            None,
            &tx2,
        )
        .await;

        // "plan" ran again (re-parked awaiting another approval, since it still
        // requires one), "implement" never started.
        let msgs2 = drain(rx2).await;
        let stages = stage_events(&msgs2);
        assert_eq!(stages, vec![("started", "plan"), ("finished", "plan")]);

        let prompts = prompts_seen.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].contains(reviewer_comment),
            "resumed stage's first prompt must include the reviewer's comment: {}",
            prompts[0]
        );
    }

    #[tokio::test]
    async fn comment_channel_approve_resolves_and_applies_through_the_tick_helpers() {
        let tracker_dir = tempdir().unwrap();
        let workflow_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "AP-6");
        std::fs::write(tracker_dir.path().join("AP-6.comments.txt"), "/approve\n").unwrap();

        let shared = approval_shared(
            tracker_dir.path(),
            workflow_dir.path(),
            "",
            TWO_STAGE_PLAN_REQUIRES_APPROVAL,
        );
        let db_path = approvals_db_path(&shared.config);
        let approval_id = approvals::create_pending(
            &db_path,
            &approvals::NewApproval {
                issue_id: "AP-6".to_string(),
                identifier: "AP-6".to_string(),
                title: "Test issue".to_string(),
                stage_id: "plan".to_string(),
                next_stage_id: Some("implement".to_string()),
                plan_text: Some("design summary".to_string()),
                plan_json: None,
            },
        )
        .unwrap();
        shared
            .tracker
            .set_issue_state("AP-6", &shared.config.pipeline.awaiting_approval_state)
            .await
            .unwrap();

        poll_approval_comments(&shared).await;
        let row = approvals::get(&db_path, approval_id).unwrap().unwrap();
        assert_eq!(row.decision.as_deref(), Some("approve"));
        assert_eq!(row.actor.as_deref(), Some("issue-comment"));
        assert!(!row.is_pending());

        apply_resolved_approvals(&shared).await;

        let refreshed = shared
            .tracker
            .fetch_issues_by_ids(&["AP-6".to_string()])
            .await
            .unwrap();
        assert_eq!(refreshed[0].state, "todo");

        let resume = approvals::take_resume(&db_path, "AP-6").unwrap().unwrap();
        assert_eq!(resume.stage_id, "implement");
    }

    #[tokio::test]
    async fn comment_channel_reject_parks_in_blocked_state_with_no_resume() {
        let tracker_dir = tempdir().unwrap();
        let workflow_dir = tempdir().unwrap();
        write_pipeline_issue(tracker_dir.path(), "AP-7");
        std::fs::write(
            tracker_dir.path().join("AP-7.comments.txt"),
            "/reject not aligned with the architecture\n",
        )
        .unwrap();

        let shared = approval_shared(
            tracker_dir.path(),
            workflow_dir.path(),
            "",
            TWO_STAGE_PLAN_REQUIRES_APPROVAL,
        );
        let db_path = approvals_db_path(&shared.config);
        approvals::create_pending(
            &db_path,
            &approvals::NewApproval {
                issue_id: "AP-7".to_string(),
                identifier: "AP-7".to_string(),
                title: "Test issue".to_string(),
                stage_id: "plan".to_string(),
                next_stage_id: Some("implement".to_string()),
                plan_text: None,
                plan_json: None,
            },
        )
        .unwrap();

        poll_approval_comments(&shared).await;
        apply_resolved_approvals(&shared).await;

        let refreshed = shared
            .tracker
            .fetch_issues_by_ids(&["AP-7".to_string()])
            .await
            .unwrap();
        assert_eq!(refreshed[0].state, "blocked");
        assert!(approvals::take_resume(&db_path, "AP-7").unwrap().is_none());
    }

    #[test]
    fn parse_approval_command_recognizes_all_three_commands_case_insensitively() {
        assert_eq!(
            parse_approval_command("/approve"),
            Some((approvals::Decision::Approve, None))
        );
        assert_eq!(
            parse_approval_command("/Approve looks great"),
            Some((
                approvals::Decision::Approve,
                Some("looks great".to_string())
            ))
        );
        assert_eq!(
            parse_approval_command("/changes please split the migration"),
            Some((
                approvals::Decision::RequestChanges,
                Some("please split the migration".to_string())
            ))
        );
        assert_eq!(
            parse_approval_command("/REJECT too risky"),
            Some((approvals::Decision::Reject, Some("too risky".to_string())))
        );
        assert_eq!(parse_approval_command("just a regular comment"), None);
        assert_eq!(
            parse_approval_command("I'd like to /approve this once tests are green"),
            None,
            "the command must anchor the comment, not just appear somewhere in it"
        );
    }

    #[test]
    fn evaluate_auto_approve_requires_every_configured_condition() {
        let cond = config::AutoApproveWhen {
            risk: Some("low".to_string()),
            impacted_components_allowlist: Some(vec!["src/foo.rs".to_string()]),
            max_estimate_turns: Some(4),
        };
        let matching = serde_json::json!({
            "risk": "low",
            "impacted_components": ["src/foo.rs"],
            "estimate_turns": 3,
        })
        .to_string();
        assert!(evaluate_auto_approve(Some(&matching), &cond));

        let wrong_risk = serde_json::json!({
            "risk": "high",
            "impacted_components": ["src/foo.rs"],
            "estimate_turns": 3,
        })
        .to_string();
        assert!(!evaluate_auto_approve(Some(&wrong_risk), &cond));

        let disallowed_component = serde_json::json!({
            "risk": "low",
            "impacted_components": ["src/foo.rs", "src/secrets.rs"],
            "estimate_turns": 3,
        })
        .to_string();
        assert!(!evaluate_auto_approve(Some(&disallowed_component), &cond));

        let too_many_turns = serde_json::json!({
            "risk": "low",
            "impacted_components": ["src/foo.rs"],
            "estimate_turns": 10,
        })
        .to_string();
        assert!(!evaluate_auto_approve(Some(&too_many_turns), &cond));

        assert!(
            !evaluate_auto_approve(None, &cond),
            "missing structured output must never satisfy a configured condition"
        );
    }

    #[test]
    fn extract_plan_json_parses_a_fenced_block_and_is_none_without_one() {
        let text = "Here's my plan.\n\n```json\n{\"risk\": \"low\"}\n```\n";
        let json = extract_plan_json(text).unwrap();
        assert!(json.contains("\"risk\""));
        assert!(extract_plan_json("just prose, no JSON here").is_none());
    }

    fn stage(id: &str, role: &str) -> config::StageConfig {
        config::StageConfig {
            id: id.to_string(),
            role: role.to_string(),
            max_turns: 1,
            on_failure: StageFailureAction::Retry,
            blocking: false,
            optional: false,
            requires_approval: false,
        }
    }

    /// AIR-7: the rework loop resends to the *nearest* earlier `role: developer`
    /// stage, not always the pipeline's first one.
    #[test]
    fn developer_stage_index_finds_the_nearest_earlier_developer_stage() {
        let stages = vec![
            stage("implement", "developer"),
            stage("harden", "developer"),
            stage("review", "reviewer"),
        ];
        assert_eq!(developer_stage_index(&stages, 2), Some(1));
    }

    #[test]
    fn developer_stage_index_is_none_when_no_earlier_developer_stage_exists() {
        let stages = vec![stage("review", "reviewer")];
        assert_eq!(developer_stage_index(&stages, 0), None);
    }

    /// AIR-7 acceptance criterion: exceeding `pipeline.review.max_rework_rounds`
    /// escalates instead of looping.
    #[test]
    fn round_exceeds_limit_flips_once_the_next_round_would_pass_the_cap() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        assert!(
            !round_exceeds_limit(&db, "1", 2),
            "round 1 of 2 is within the cap"
        );
        crate::eventlog::record_rework_round(
            &db,
            &crate::eventlog::NewReworkRound {
                issue_id: "1",
                identifier: "AIR-7",
                title: "t",
                stage_id: "review",
                recommendation: "request_changes",
                summary: "round 1",
                escalated: false,
            },
        )
        .unwrap();
        assert!(
            !round_exceeds_limit(&db, "1", 2),
            "round 2 of 2 is still within the cap"
        );
        crate::eventlog::record_rework_round(
            &db,
            &crate::eventlog::NewReworkRound {
                issue_id: "1",
                identifier: "AIR-7",
                title: "t",
                stage_id: "review",
                recommendation: "request_changes",
                summary: "round 2",
                escalated: false,
            },
        )
        .unwrap();
        assert!(
            round_exceeds_limit(&db, "1", 2),
            "round 3 of 2 exceeds the cap"
        );
    }

    /// AIR-7 acceptance criterion: the stage produces a schema-valid `review_findings`
    /// artifact that the rework loop can read a recommendation back out of.
    #[tokio::test]
    async fn latest_review_recommendation_reads_the_recorded_artifact_back() {
        let dir = tempdir().unwrap();
        let workflow_dir = dir.path().to_path_buf();
        let db = workflow_dir.join(crate::eventlog::DB_FILENAME);
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(workspace.join(".symphony")).unwrap();
        std::fs::write(workspace.join(".symphony/current-stage"), "review").unwrap();

        let content = json!({
            "schema_version": 1,
            "recommendation": "request_changes",
            "findings": [],
            "unmet_acceptance_criteria": ["AC3"],
            "over_implementation": []
        })
        .to_string();
        let result = crate::artifacts::execute_tool(
            &db,
            &workflow_dir,
            &workspace,
            "issue-1",
            "issue-1",
            json!({
                "kind": "review_findings",
                "content_type": "application/json",
                "content": content,
                "summary": "requests changes: AC3 unmet"
            }),
        )
        .await;
        assert!(result.success, "{}", result.content);

        let (recommendation, summary) =
            latest_review_recommendation(&workflow_dir, &db, "issue-1", "review").unwrap();
        assert_eq!(recommendation, "request_changes");
        assert_eq!(summary, "requests changes: AC3 unmet");
    }

    // -----------------------------------------------------------------------------
    // AIR-9: release evidence (`finalize_release_evidence`)
    // -----------------------------------------------------------------------------

    fn test_issue(id: &str) -> Issue {
        Issue {
            id: id.to_string(),
            native_ref: None,
            identifier: id.to_string(),
            title: "test issue".to_string(),
            description: Some("- [x] done thing".to_string()),
            priority: None,
            state: "in_progress".to_string(),
            branch_name: None,
            url: None,
            assignee_id: None,
            labels: vec![],
            blocked_by: vec![],
            dispatchable: true,
            created_at: None,
            updated_at: None,
        }
    }

    /// `repo.release_evidence` off (the default) must not touch anything -- no PR
    /// lookup, no artifact upload, no event -- keeping `open_pull_request`'s own
    /// pipeline-off behavior byte-identical to before this feature existed.
    #[tokio::test]
    async fn finalize_release_evidence_is_a_noop_when_the_flag_is_off() {
        let cfg_yaml: serde_yaml::Value =
            serde_yaml::from_str("tracker:\n  kind: local\n").unwrap();
        let cfg = config::resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(cfg.repo.is_none());

        let (tx, mut rx) = mpsc::unbounded_channel();
        finalize_release_evidence("issue-1", &test_issue("AIR-TEST-1"), &cfg, &tx).await;
        drop(tx);
        assert!(
            rx.recv().await.is_none(),
            "no message should be sent when release_evidence is off"
        );
    }

    /// Same no-op guarantee when `repo:` is configured but `release_evidence` itself
    /// wasn't turned on -- the common case of a project only using `pull_request`.
    #[tokio::test]
    async fn finalize_release_evidence_is_a_noop_without_release_evidence_flag() {
        let cfg_yaml: serde_yaml::Value = serde_yaml::from_str(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_ORCH_RELEASE_EVIDENCE_OFF\n  pull_request: true\n",
        )
        .unwrap();
        let cfg = config::resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(!cfg.repo.as_ref().unwrap().release_evidence);

        let (tx, mut rx) = mpsc::unbounded_channel();
        finalize_release_evidence("issue-1", &test_issue("AIR-TEST-2"), &cfg, &tx).await;
        drop(tx);
        assert!(rx.recv().await.is_none());
    }
}
