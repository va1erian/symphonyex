//! Docker container lifecycle + exec wrapper (Docker mode, see README.md).
//!
//! Shells out to the `docker` CLI -- no SDK/API client dependency, mirroring how
//! `hooks.rs` shells out to `bash` rather than depending on a shell-scripting crate.
//!
//! This exists to close a real class of bug hit in production use: workspace hooks
//! (`hooks.rs`) run their scripts via WSL's `bash`, while the coding agent's own
//! `Bash` tool -- invoked by `claude` running natively on the host -- resolves paths
//! via Git Bash/MSYS. The two disagree about how to spell a Windows path for the same
//! directory (`/mnt/c/...` vs `/c/...`), and there is no single spelling that
//! satisfies both. Running hooks *and* the agent process inside the same container,
//! bind-mounted once, removes the second environment entirely: inside the container
//! there is exactly one filesystem and one path convention.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Fixed in-container mount point for the whole project directory (`workflow_dir`):
/// the per-ticket workspace, the tracker's `issues/` directory, and a mainline repo
/// like bsky-archiver's `app/` are all subdirectories of the same project root, so one
/// mount covers everything a hook or the agent's own git operations need to reach,
/// with paths that are consistent everywhere they're used.
pub const CONTAINER_PROJECT_ROOT: &str = "/project";

/// Fixed path, inside the Symphony base image, to the cross-compiled Linux `symphony`
/// binary -- used as the MCP tool-server `command` in Docker mode instead of
/// `std::env::current_exe()` (which is the host's `.exe`, wrong OS/ABI to run inside
/// the container). See README.md "Docker mode" for the image build steps.
pub const CONTAINER_SYMPHONY_BIN: &str = "/usr/local/bin/symphony";

/// Best-effort: kill any process matching `process_name` running inside `container`.
/// Used as a cancellation-safe cleanup path when the *host-side* `docker exec` client
/// process is killed (e.g. by `kill_on_drop` on task abort) -- that only terminates
/// the client, not the process it was attached to inside the container, so without
/// this an aborted turn's `claude` process would keep running orphaned. Matches this
/// session's own hard-won lesson about abort correctness (see `orchestrator.rs`'s
/// `abort_and_run_after_run` doc comment for the host-side equivalent incident).
pub async fn kill_process_by_name(container: &str, process_name: &str) {
    let _ = run_docker(
        &["exec", container, "pkill", "-f", process_name],
        "exec pkill",
    )
    .await;
}

/// Deterministic container name for `identifier` within the project at `workflow_dir`
/// -- stable across restarts (so `ensure_running` can find and reuse it) and unique
/// per project (so two different projects' containers never collide even if they
/// happen to use the same ticket identifiers).
pub fn derive_container_name(workflow_dir: &Path, identifier: &str) -> String {
    let workflow_dir_n = crate::envsub::normalize(workflow_dir);
    let dir_str = if cfg!(windows) {
        workflow_dir_n.to_string_lossy().to_lowercase()
    } else {
        workflow_dir_n.to_string_lossy().to_string()
    };
    let hash = Sha256::digest(dir_str.as_bytes());
    let hex: String = hash.iter().take(4).map(|b| format!("{b:02x}")).collect(); // 32 bits
    let key = crate::workspace::derive_workspace_key(identifier);
    format!("symphony-{hex}-{key}")
}

#[derive(Debug, Error)]
pub enum ContainerError {
    #[error("failed to launch docker: {0}")]
    Spawn(String),
    #[error("docker {op} failed: {output}")]
    CommandFailed { op: String, output: String },
    #[error("docker exec timed out after {0}ms")]
    Timeout(u64),
}

/// A running (or startable) named container bind-mounted to a project directory.
#[derive(Debug, Clone)]
pub struct ContainerHandle {
    pub name: String,
    /// Path inside the container that `host_root` (the directory passed to
    /// `ensure_running`) is mounted at, e.g. `/project`.
    pub container_root: PathBuf,
}

impl ContainerHandle {
    /// Map a host path known to be under `host_root` (the directory bind-mounted at
    /// `container_root`) to its in-container equivalent. Falls back to
    /// `container_root` itself if `host_path` isn't actually under `host_root` --
    /// callers are expected to only pass paths derived from the same workflow tree.
    pub fn to_container_path(&self, host_root: &Path, host_path: &Path) -> PathBuf {
        let container_root_str = to_container_path_str(&self.container_root);
        match strip_prefix_lenient(host_path, host_root) {
            Some(rel) if rel.is_empty() => self.container_root.clone(),
            Some(rel) => PathBuf::from(format!(
                "{}/{}",
                container_root_str.trim_end_matches('/'),
                rel
            )),
            None => self.container_root.clone(),
        }
    }
}

/// Windows-tolerant prefix strip: case-insensitive, and comparing normalized forms so
/// mixed `/`/`\` separators don't break the match (mirrors `workspace::validate_containment`).
/// Returns the relative remainder with forward slashes and the *original* casing (only
/// the comparison is case-folded, not the returned value).
fn strip_prefix_lenient(path: &Path, root: &Path) -> Option<String> {
    let path_original = to_container_path_str(&crate::envsub::normalize(path));
    let root_original = to_container_path_str(&crate::envsub::normalize(root));
    let (path_cmp, root_cmp) = if cfg!(windows) {
        (path_original.to_lowercase(), root_original.to_lowercase())
    } else {
        (path_original.clone(), root_original.clone())
    };
    if !path_cmp.starts_with(&root_cmp) {
        return None;
    }
    let rel = path_original[root_original.len()..].trim_start_matches('/');
    Some(rel.to_string())
}

fn to_container_path_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

const MAX_LOGGED_OUTPUT: usize = 4096;

fn truncate(s: &str) -> String {
    if s.len() > MAX_LOGGED_OUTPUT {
        format!("{}... [truncated]", &s[..MAX_LOGGED_OUTPUT])
    } else {
        s.to_string()
    }
}

async fn run_docker(args: &[&str], op: &str) -> Result<String, ContainerError> {
    let output = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ContainerError::Spawn(e.to_string()))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(ContainerError::CommandFailed {
            op: op.to_string(),
            output: truncate(&combined),
        })
    }
}

/// True if a `docker` binary is reachable on `PATH`. Used to gate integration tests
/// that need a real daemon, and can be used by callers to fail fast with a clearer
/// error than "docker: command not found" buried in a hook failure.
pub async fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Idempotently ensure a named container exists, is running, has `host_mount`
/// bind-mounted at `container_root`, and stays alive on its own (`tail -f /dev/null`)
/// so repeated `docker exec` calls can target it across hook/turn invocations --
/// mirrors `WorkspaceManager::create_for_issue`'s create-or-reuse behavior.
pub async fn ensure_running(
    image: &str,
    name: &str,
    host_mount: &Path,
    container_root: &Path,
    network: &str,
    mem_limit: Option<&str>,
    cpus: Option<&str>,
) -> Result<ContainerHandle, ContainerError> {
    let handle = ContainerHandle {
        name: name.to_string(),
        container_root: container_root.to_path_buf(),
    };

    let inspect = run_docker(&["inspect", "-f", "{{.State.Running}}", name], "inspect").await;

    match inspect {
        Ok(running) if running.trim() == "true" => return Ok(handle),
        Ok(_) => {
            // Exists but stopped (e.g. after a host reboot or a crashed symphony run).
            run_docker(&["start", name], "start").await?;
            return Ok(handle);
        }
        Err(_) => {} // Doesn't exist yet; fall through to create it.
    }

    let mount_arg = format!(
        "{}:{}",
        to_container_path_str(host_mount),
        to_container_path_str(container_root)
    );
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        name.into(),
        "-v".into(),
        mount_arg,
        "-w".into(),
        to_container_path_str(container_root),
        "--network".into(),
        network.into(),
    ];
    if let Some(mem) = mem_limit {
        args.push("--memory".into());
        args.push(mem.into());
    }
    if let Some(cpus) = cpus {
        args.push("--cpus".into());
        args.push(cpus.into());
    }
    args.push(image.into());
    args.push("tail".into());
    args.push("-f".into());
    args.push("/dev/null".into());

    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    run_docker(&args_ref, "run").await?;
    Ok(handle)
}

/// Stop and remove a named container (best-effort, like
/// `WorkspaceManager::remove_for_issue`'s workspace directory cleanup: failures are
/// logged, never propagated, since there's nothing meaningful left to do about them).
pub async fn remove(name: &str) {
    if let Err(e) = run_docker(&["stop", name], "stop").await {
        tracing::warn!(container = name, error = %e, "failed to stop container (ignored)");
    }
    if let Err(e) = run_docker(&["rm", "-f", name], "rm").await {
        tracing::warn!(container = name, error = %e, "failed to remove container (ignored)");
    }
}

/// Run `program args...` inside `container`, cwd `cwd_in_container`, with optional
/// script content piped over stdin (used for hook scripts). Mirrors `hooks::run_hook`'s
/// stdin-piping approach for the same reason: no script content or path in argv to be
/// re-tokenized differently by whichever shell is on the other end.
pub async fn exec(
    container: &str,
    cwd_in_container: &Path,
    program: &str,
    args: &[String],
    stdin_script: Option<&str>,
    timeout_ms: u64,
) -> Result<(), ContainerError> {
    let mut cmd = Command::new("docker");
    cmd.arg("exec");
    if stdin_script.is_some() {
        cmd.arg("-i");
    }
    cmd.arg("-w")
        .arg(to_container_path_str(cwd_in_container))
        .arg(container)
        .arg(program)
        .args(args)
        .stdin(if stdin_script.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| ContainerError::Spawn(e.to_string()))?;

    let write_task = if let Some(script) = stdin_script {
        let mut stdin = child.stdin.take().expect("piped stdin");
        let script_owned = script.to_string();
        Some(tokio::spawn(async move {
            let _ = stdin.write_all(script_owned.as_bytes()).await;
            // Drop closes the pipe, signaling EOF.
        }))
    } else {
        None
    };

    let wait = child.wait_with_output();
    let result = tokio::time::timeout(Duration::from_millis(timeout_ms), wait).await;
    if let Some(t) = write_task {
        let _ = t.await;
    }

    match result {
        Ok(Ok(output)) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if output.status.success() {
                Ok(())
            } else {
                Err(ContainerError::CommandFailed {
                    op: format!("exec {program}"),
                    output: truncate(&combined),
                })
            }
        }
        Ok(Err(e)) => Err(ContainerError::Spawn(e.to_string())),
        Err(_) => Err(ContainerError::Timeout(timeout_ms)),
    }
}

/// Run a hook script inside `container` via `bash -l -s`, matching `hooks::run_hook`'s
/// semantics exactly (same stdin-piping approach, same success/failure contract) but
/// targeting a container instead of the host.
pub async fn exec_script(
    container: &ContainerHandle,
    cwd_in_container: &Path,
    script: &str,
    timeout_ms: u64,
) -> Result<(), ContainerError> {
    exec(
        &container.name,
        cwd_in_container,
        "bash",
        &["-l".to_string(), "-s".to_string()],
        Some(script),
        timeout_ms,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_host_path_under_root_to_container_path() {
        let handle = ContainerHandle {
            name: "test".into(),
            container_root: PathBuf::from("/project"),
        };
        let root = PathBuf::from("/home/user/proj");
        let path = PathBuf::from("/home/user/proj/.workspaces/AR-1");
        assert_eq!(
            handle.to_container_path(&root, &path),
            PathBuf::from("/project/.workspaces/AR-1")
        );
    }

    #[test]
    fn maps_root_itself_to_container_root() {
        let handle = ContainerHandle {
            name: "test".into(),
            container_root: PathBuf::from("/project"),
        };
        let root = PathBuf::from("/home/user/proj");
        assert_eq!(
            handle.to_container_path(&root, &root),
            PathBuf::from("/project")
        );
    }

    #[test]
    fn unrelated_path_falls_back_to_container_root() {
        let handle = ContainerHandle {
            name: "test".into(),
            container_root: PathBuf::from("/project"),
        };
        let root = PathBuf::from("/home/user/proj");
        let unrelated = PathBuf::from("/somewhere/else");
        assert_eq!(
            handle.to_container_path(&root, &unrelated),
            PathBuf::from("/project")
        );
    }

    // Integration tests below require a real `docker` daemon; they're `#[ignore]`d by
    // default and meant to be run explicitly (`cargo test -- --ignored`) in an
    // environment that has Docker available, matching the plan's rollout step.

    #[tokio::test]
    #[ignore]
    async fn ensure_running_creates_then_reuses_a_container() {
        assert!(
            docker_available().await,
            "docker must be available for this test"
        );
        let dir = tempfile::tempdir().unwrap();
        let name = "symphony-test-ensure-running";
        remove(name).await; // clean slate

        let h1 = ensure_running(
            "debian:bookworm-slim",
            name,
            dir.path(),
            Path::new("/project"),
            "bridge",
            None,
            None,
        )
        .await
        .unwrap();
        let h2 = ensure_running(
            "debian:bookworm-slim",
            name,
            dir.path(),
            Path::new("/project"),
            "bridge",
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(h1.name, h2.name);

        remove(name).await;
    }

    #[tokio::test]
    #[ignore]
    async fn exec_script_runs_inside_the_container_against_the_bind_mount() {
        assert!(
            docker_available().await,
            "docker must be available for this test"
        );
        let dir = tempfile::tempdir().unwrap();
        let name = "symphony-test-exec-script";
        remove(name).await;

        let handle = ensure_running(
            "debian:bookworm-slim",
            name,
            dir.path(),
            Path::new("/project"),
            "bridge",
            None,
            None,
        )
        .await
        .unwrap();

        exec_script(
            &handle,
            Path::new("/project"),
            "echo hi > marker.txt",
            10_000,
        )
        .await
        .unwrap();

        let contents = std::fs::read_to_string(dir.path().join("marker.txt")).unwrap();
        assert_eq!(contents.trim(), "hi");

        remove(name).await;
    }
}
