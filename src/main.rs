mod agent;
mod budget;
mod config;
mod container;
mod daemon;
mod domain;
mod envsub;
mod eventlog;
mod frontmatter;
mod hooks;
mod mcp;
mod metrics;
mod orchestrator;
mod registry;
mod repo_host;
mod service;
mod status;
mod swebot;
mod template;
mod tracker;
mod web;
mod workflow;
mod workspace;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Symphony: orchestrates coding agents against a configured issue tracker.
/// See SPEC.md for the full specification this implements.
///
/// `version`: `build.rs` bakes in `<Cargo.toml version> (<branch>@<short-sha>)` when it
/// runs -- plain `CARGO_PKG_VERSION` alone can't tell two worktrees of this repo on
/// different branches apart, which is exactly the confusion `--version`/`-V` exists to
/// resolve. `option_env!` (not `env!`): a build whose context never copies `build.rs`
/// in the first place (e.g. this repo's own Dockerfile, which only `COPY`s
/// `Cargo.toml`/`Cargo.lock`/`src` for the in-container `symphony` binary) never runs
/// any build script at all, so the var is simply never set -- falling back to
/// `CARGO_PKG_VERSION` (always present, set by Cargo itself, no build script needed)
/// keeps that a normal build instead of a hard compile error.
#[derive(Parser)]
#[command(
    name = "symphony",
    version = option_env!("SYMPHONY_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
)]
struct Cli {
    /// Path to WORKFLOW.md. Defaults to ./WORKFLOW.md (Section 5.1).
    workflow_path: Option<PathBuf>,

    /// Serve a live, auto-refreshing status dashboard on this loopback port
    /// (Section 13.7 extension, minimal subset: no JS, no JSON API).
    #[arg(long)]
    port: Option<u16>,

    /// Where to write the cumulative HTML usage report (agents spawned, turns, tool
    /// calls, tokens). Always on; defaults to `symphony-report.html` next to
    /// `WORKFLOW.md`. Rewritten after every state change, so it stays current even if
    /// the process is killed rather than shut down cleanly.
    #[arg(long)]
    report: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Internal: run as an MCP stdio tool server for one coding-agent turn (Section
    /// 10.5). Spawned by the `claude` CLI itself via a generated `--mcp-config`; not
    /// meant to be invoked directly.
    #[command(hide = true, name = "__mcp_tool_server")]
    McpToolServer {
        #[arg(long)]
        tracker_kind: String,
        /// Tracker provider config as a JSON string (parsed as YAML, so JSON syntax works).
        #[arg(long)]
        tracker_provider: String,
        #[arg(long)]
        workflow_dir: PathBuf,
        #[arg(long)]
        issue_id: String,
        /// `repo:` config as a JSON string, present only when `repo.pull_request` is
        /// enabled -- see `agent::claude::McpToolWiring::repo_pr_json`.
        #[arg(long)]
        repo_pr: Option<String>,
        /// This turn's workspace path exactly as seen by this process itself (the
        /// in-container path in Docker mode, the host path otherwise) -- needed by
        /// `attach_evidence` (`repo.evidence`) to resolve the agent-supplied image
        /// path. Unused by every other tool.
        #[arg(long)]
        workspace_dir: PathBuf,
    },

    /// Run Symphony itself as a long-lived, auto-restarting, single-instance Docker
    /// container (see README.md "Daemonizing Symphony"). Requires
    /// `workspace.docker.enabled: true` in WORKFLOW.md.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Run the long-running, multi-repo web service (see AGENTS.md "Long-running
    /// multi-repo service"): register GitHub repos through a browser instead of
    /// pointing one process at one local WORKFLOW.md, and Symphony fetches each
    /// repo's config and polls its tracker/PRs/discussions continuously. Requires
    /// `SYMPHONY_ADMIN_TOKEN` to be set.
    Serve {
        /// Port to serve the web UI on.
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Where registered projects' fetched WORKFLOW.md files and SQLite registry
        /// are persisted. Defaults to `./symphony-data`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon (idempotent-ish: refuses to start a second one for the same
    /// project; a pre-existing named volume from an earlier start is reused as-is).
    Start {
        /// Path to WORKFLOW.md. Defaults to ./WORKFLOW.md.
        workflow_path: Option<PathBuf>,
        /// Serve the live status dashboard on this loopback port inside the daemon
        /// container, published to the same port on the host.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Stop the daemon (the named volume and its data are left alone).
    Stop { workflow_path: Option<PathBuf> },
    /// Show whether the daemon is running.
    Status { workflow_path: Option<PathBuf> },
    /// Show the daemon's logs.
    Logs {
        workflow_path: Option<PathBuf>,
        /// Stream new log lines as they happen instead of just the recent tail.
        #[arg(long)]
        follow: bool,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    // stdout is reserved for the MCP protocol stream when running as the tool server;
    // logs always go to stderr regardless of subcommand.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    match cli.command {
        Some(Command::McpToolServer {
            tracker_kind,
            tracker_provider,
            workflow_dir,
            issue_id,
            repo_pr,
            workspace_dir,
        }) => {
            return run_mcp_tool_server(
                &tracker_kind,
                &tracker_provider,
                &workflow_dir,
                &issue_id,
                repo_pr.as_deref(),
                &workspace_dir,
            )
            .await;
        }
        Some(Command::Daemon { action }) => return run_daemon_command(action).await,
        Some(Command::Serve { port, data_dir }) => {
            let data_dir = data_dir.unwrap_or_else(|| PathBuf::from("symphony-data"));
            return match service::run(port, data_dir).await {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!(error = %e, "symphony serve exited with error");
                    std::process::ExitCode::FAILURE
                }
            };
        }
        None => {}
    }

    let path = workflow::resolve_workflow_path(cli.workflow_path.as_deref());
    if !path.exists() {
        tracing::error!(path = %path.display(), "missing_workflow_file");
        return std::process::ExitCode::FAILURE;
    }

    tokio::select! {
        result = orchestrator::run(path, cli.port, cli.report) => {
            match result {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!(error = %e, "symphony exited with error");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received shutdown signal");
            std::process::ExitCode::SUCCESS
        }
    }
}

async fn run_daemon_command(action: DaemonAction) -> std::process::ExitCode {
    let result = match action {
        DaemonAction::Start {
            workflow_path,
            port,
        } => {
            let path = workflow::resolve_workflow_path(workflow_path.as_deref());
            daemon::start(&path, port).await
        }
        DaemonAction::Stop { workflow_path } => {
            let path = workflow::resolve_workflow_path(workflow_path.as_deref());
            daemon::stop(&path).await
        }
        DaemonAction::Status { workflow_path } => {
            let path = workflow::resolve_workflow_path(workflow_path.as_deref());
            daemon::status(&path).await
        }
        DaemonAction::Logs {
            workflow_path,
            follow,
        } => {
            let path = workflow::resolve_workflow_path(workflow_path.as_deref());
            daemon::logs(&path, follow).await
        }
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "symphony daemon command failed");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run_mcp_tool_server(
    tracker_kind: &str,
    tracker_provider_json: &str,
    workflow_dir: &std::path::Path,
    issue_id: &str,
    repo_pr_json: Option<&str>,
    workspace_dir: &std::path::Path,
) -> std::process::ExitCode {
    let provider: serde_yaml::Value = match serde_yaml::from_str(tracker_provider_json) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "mcp tool server: invalid tracker-provider argument");
            return std::process::ExitCode::FAILURE;
        }
    };
    let adapter = match tracker::build(tracker_kind, &provider, workflow_dir) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "mcp tool server: failed to build tracker adapter");
            return std::process::ExitCode::FAILURE;
        }
    };
    let repo_host = match repo_pr_json {
        Some(json) => match serde_json::from_str::<config::RepoConfig>(json)
            .map_err(|e| e.to_string())
            .and_then(|cfg| repo_host::build(&cfg))
        {
            Ok(host) => Some(host),
            Err(e) => {
                tracing::error!(error = %e, "mcp tool server: failed to build repo host");
                return std::process::ExitCode::FAILURE;
            }
        },
        None => None,
    };
    match mcp::run_stdio_server(adapter, repo_host, issue_id, workspace_dir).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "mcp tool server exited with error");
            std::process::ExitCode::FAILURE
        }
    }
}
