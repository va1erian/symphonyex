mod agent;
mod config;
mod container;
mod daemon;
mod domain;
mod envsub;
mod frontmatter;
mod hooks;
mod mcp;
mod metrics;
mod orchestrator;
mod status;
mod template;
mod tracker;
mod workflow;
mod workspace;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Symphony: orchestrates coding agents against a configured issue tracker.
/// See SPEC.md for the full specification this implements.
#[derive(Parser)]
#[command(name = "symphony")]
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
    },

    /// Run Symphony itself as a long-lived, auto-restarting, single-instance Docker
    /// container (see README.md "Daemonizing Symphony"). Requires
    /// `workspace.docker.enabled: true` in WORKFLOW.md.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
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
        }) => {
            return run_mcp_tool_server(&tracker_kind, &tracker_provider, &workflow_dir, &issue_id)
                .await;
        }
        Some(Command::Daemon { action }) => return run_daemon_command(action).await,
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
    match mcp::run_stdio_server(adapter, issue_id).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "mcp tool server exited with error");
            std::process::ExitCode::FAILURE
        }
    }
}
