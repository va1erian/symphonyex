//! `symphony daemon` subcommands (see README.md "Daemonizing Symphony"): ergonomic
//! wrappers around the `docker run`/`stop`/`ps`/`logs` invocations needed to run
//! Symphony itself as a long-lived, auto-restarting, single-instance container --
//! directly replacing the `nohup`/PID-file/background-task juggling this project did
//! by hand before this existed.
//!
//! Requires `workspace.docker.enabled: true` in the target `WORKFLOW.md`: the daemon
//! container reaches the host's Docker daemon through a mounted socket
//! (Docker-outside-of-Docker) to spawn per-ticket sibling containers, and those need
//! Docker mode's container image/network config to exist in the first place.

use crate::config::{self, EffectiveConfig};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

fn daemon_container_name(workflow_dir: &Path) -> String {
    format!(
        "symphony-daemon-{}",
        crate::container::hash_workflow_dir(workflow_dir)
    )
}

fn daemon_volume_name(workflow_dir: &Path) -> String {
    format!(
        "symphony-daemon-{}-data",
        crate::container::hash_workflow_dir(workflow_dir)
    )
}

fn to_docker_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Resolve `workflow_path` the same way `orchestrator::build_shared` does (absolute,
/// normalized directory), then load its config -- daemon subcommands need
/// `docker.image`/`docker.enabled` and the project directory to derive names from.
/// Also returns every `$VAR`-shaped secret reference in the raw config (see
/// `envsub::collect_var_refs`): the daemon container needs these forwarded into
/// *itself* too, since the orchestrator running inside it (e.g. the GitHub tracker
/// adapter resolving `tracker.provider.token`) is subject to the exact same
/// Docker-doesn't-inherit-the-host-environment gap as per-ticket containers are.
fn load(
    workflow_path: &Path,
) -> anyhow::Result<(std::path::PathBuf, EffectiveConfig, Vec<String>)> {
    let wf = crate::workflow::load(workflow_path)?;
    let env_passthrough = crate::envsub::collect_var_refs(&wf.config);
    let workflow_dir_raw = workflow_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let workflow_dir = crate::envsub::normalize(&std::env::current_dir()?.join(workflow_dir_raw));
    let cfg = config::resolve(&wf.config, &workflow_dir)?;
    Ok((workflow_dir, cfg, env_passthrough))
}

async fn is_running(name: &str) -> bool {
    Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", name])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

async fn exists(name_or_volume: &[&str]) -> bool {
    Command::new("docker")
        .args(name_or_volume)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a `docker` command with output passed through to this process's own
/// stdout/stderr (unlike `container.rs`'s helpers, which capture output for the
/// orchestrator's own logging -- these subcommands are interactive CLI tools, so the
/// operator should see `docker`'s own output directly, including a `logs --follow`
/// stream running indefinitely until interrupted).
async fn run_visible(args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("docker").args(args).status().await?;
    if !status.success() {
        anyhow::bail!("docker {} failed: {status}", args.join(" "));
    }
    Ok(())
}

/// First `daemon start` for a project: the named volume starts empty, so seed it with
/// whatever's currently on the host at `workflow_dir` (the same directory a
/// non-daemonized run would use directly) via a one-shot helper container. Later
/// starts see the volume already exists and leave it alone -- the whole point of a
/// persistent named volume is that it survives across daemon restarts, so silently
/// overwriting it with the host's current state every time would throw away whatever
/// the daemon itself has since committed/pushed inside the container.
async fn ensure_volume_seeded(
    volume: &str,
    workflow_dir: &Path,
    image: &str,
) -> anyhow::Result<()> {
    if exists(&["volume", "inspect", volume]).await {
        tracing::info!(
            volume,
            "daemon volume already exists, leaving its contents as-is"
        );
        return Ok(());
    }
    tracing::info!(
        volume,
        ?workflow_dir,
        "seeding new daemon volume from host directory"
    );
    run_visible(&["volume", "create", volume]).await?;
    let host_mount = format!("{}:/src:ro", to_docker_path(workflow_dir));
    let volume_mount = format!("{volume}:/project");
    run_visible(&[
        "run",
        "--rm",
        "-v",
        &host_mount,
        "-v",
        &volume_mount,
        image,
        "bash",
        "-lc",
        "cp -a /src/. /project/",
    ])
    .await
}

pub async fn start(workflow_path: &Path, port: Option<u16>) -> anyhow::Result<()> {
    let (workflow_dir, cfg, env_passthrough) = load(workflow_path)?;
    if !cfg.docker.enabled {
        anyhow::bail!(
            "daemon mode requires workspace.docker.enabled: true in WORKFLOW.md -- \
             the daemon container spawns per-ticket sibling containers via Docker mode"
        );
    }
    let image = cfg
        .docker
        .image
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("workspace.docker.image is required for daemon mode"))?;

    if !crate::container::docker_available().await {
        anyhow::bail!("docker isn't reachable -- is Docker Desktop running?");
    }

    let name = daemon_container_name(&workflow_dir);
    if is_running(&name).await {
        anyhow::bail!(
            "a daemon is already running for this project (container '{name}') -- \
             stop it first with `symphony daemon stop`"
        );
    }
    // A container can exist-but-not-be-running after a previous `daemon stop` (which
    // only stops it, deliberately leaving it -- and its named volume -- intact rather
    // than removing it). `docker run --name` would fail with a name conflict against
    // that leftover container, so resume it directly instead of trying to create a
    // new one. This does mean a stopped daemon resumes with whatever port/env it was
    // originally started with, not any new flags passed to this `start` call -- to
    // pick up a changed image or port, remove the old container first (`docker rm
    // <name>`) rather than just `daemon start` again.
    if exists(&["inspect", &name]).await {
        run_visible(&["start", &name]).await?;
        println!("Resumed existing daemon '{name}' (was stopped, not removed).");
        return Ok(());
    }

    let volume = daemon_volume_name(&workflow_dir);
    ensure_volume_seeded(&volume, &workflow_dir, image).await?;

    // The workflow file's path *inside* the volume, mirroring its position relative
    // to workflow_dir on the host -- almost always just "WORKFLOW.md" at the project
    // root, but this stays correct if it's nested.
    let workflow_rel = workflow_path
        .canonicalize()
        .unwrap_or_else(|_| workflow_path.to_path_buf())
        .strip_prefix(&workflow_dir)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| "WORKFLOW.md".to_string());
    let workflow_in_container = format!("/project/{workflow_rel}");

    let volume_mount = format!("{volume}:/project");
    let daemon_env = format!("SYMPHONY_DAEMON_VOLUME={volume}");
    let port_str = port.map(|p| p.to_string());

    let mut args: Vec<&str> = vec![
        "run",
        "-d",
        "--name",
        &name,
        "--restart",
        "unless-stopped",
        "-v",
        &volume_mount,
        "-v",
        "/var/run/docker.sock:/var/run/docker.sock",
        "-e",
        &daemon_env,
    ];
    // Same secrets a per-ticket container would need (see
    // orchestrator::build_shared's identical use of collect_var_refs) -- the
    // orchestrator running *inside* this daemon container needs them too, e.g. to
    // resolve tracker.provider.token for the GitHub adapter.
    for var_name in &env_passthrough {
        args.push("-e");
        args.push(var_name);
    }
    let port_arg;
    if let Some(p) = &port_str {
        port_arg = format!("{p}:{p}");
        args.push("-p");
        args.push(&port_arg);
    }
    args.push(image);
    args.push("symphony");
    args.push(&workflow_in_container);
    if let Some(p) = &port_str {
        args.push("--port");
        args.push(p);
    }
    args.push("--report");
    args.push("/project/symphony-report.html");

    run_visible(&args).await?;
    println!("Started daemon '{name}' (restart policy: unless-stopped).");
    if let Some(p) = port {
        println!("Status dashboard: http://127.0.0.1:{p}");
    }
    println!("Logs: symphony daemon logs {}", workflow_path.display());
    Ok(())
}

pub async fn stop(workflow_path: &Path) -> anyhow::Result<()> {
    let (workflow_dir, _cfg, _env) = load(workflow_path)?;
    let name = daemon_container_name(&workflow_dir);
    if !exists(&["inspect", &name]).await {
        println!("No daemon found for this project (container '{name}' doesn't exist).");
        return Ok(());
    }
    run_visible(&["stop", &name]).await?;
    println!("Stopped daemon '{name}'.");
    Ok(())
}

pub async fn status(workflow_path: &Path) -> anyhow::Result<()> {
    let (workflow_dir, _cfg, _env) = load(workflow_path)?;
    let name = daemon_container_name(&workflow_dir);
    if !exists(&["inspect", &name]).await {
        println!("No daemon found for this project (container '{name}' doesn't exist).");
        return Ok(());
    }
    run_visible(&[
        "ps",
        "-a",
        "--filter",
        &format!("name={name}"),
        "--format",
        "table {{.Names}}\t{{.Status}}\t{{.RunningFor}}",
    ])
    .await
}

pub async fn logs(workflow_path: &Path, follow: bool) -> anyhow::Result<()> {
    let (workflow_dir, _cfg, _env) = load(workflow_path)?;
    let name = daemon_container_name(&workflow_dir);
    if !exists(&["inspect", &name]).await {
        anyhow::bail!("no daemon found for this project (container '{name}' doesn't exist)");
    }
    if follow {
        run_visible(&["logs", "-f", &name]).await
    } else {
        run_visible(&["logs", "--tail", "200", &name]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_and_volume_names_are_deterministic_and_distinct() {
        let dir = Path::new("/some/project");
        let n1 = daemon_container_name(dir);
        let n2 = daemon_container_name(dir);
        assert_eq!(n1, n2);
        assert_ne!(n1, daemon_volume_name(dir));
        assert!(n1.starts_with("symphony-daemon-"));
        assert!(daemon_volume_name(dir).ends_with("-data"));
    }

    #[test]
    fn different_projects_get_different_names() {
        let a = daemon_container_name(Path::new("/project/a"));
        let b = daemon_container_name(Path::new("/project/b"));
        assert_ne!(a, b);
    }
}
