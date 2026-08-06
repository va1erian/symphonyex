//! Workspace lifecycle hook execution (Section 9.4).
//!
//! Hooks are trusted shell scripts from `WORKFLOW.md`, run as a login shell with the
//! workspace directory as `cwd`. Requires `bash` on `PATH` (Git for Windows / WSL /
//! any POSIX host satisfy this).
//!
//! The script is piped to the child's stdin (`bash -l -s`) rather than passed as a
//! `bash -lc <script>` argument or as a file path argument. Both alternatives were
//! tried and both are unreliable on Windows: `bash.exe` (whichever one `PATH`
//! resolves to — MSYS/Git-for-Windows and WSL's launcher behave differently, and
//! either can be what's installed) re-tokenizes the process command line using its
//! own convention, which doesn't agree with the escaping `std::process::Command`
//! applies when building that command line, so a multi-line script with nested quotes
//! can arrive corrupted; and there is no single absolute-path spelling (`C:\...`,
//! `C:/...`, `/c/...`, `/mnt/c/...`) that both MSYS bash and WSL bash accept, so a
//! temp-file-path argument can fail with "no such file" depending on which bash is
//! actually installed. Piping the script over stdin sidesteps both problems: no
//! script content in argv, and no absolute path to get wrong.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const MAX_LOGGED_OUTPUT: usize = 4096;

#[derive(Debug, Error)]
pub enum HookError {
    #[error("hook timed out after {0}ms")]
    Timeout(u64),
    #[error("hook exited with status {status}: {output}")]
    NonZeroExit { status: String, output: String },
    #[error("failed to launch hook: {0}")]
    Spawn(String),
}

/// Run `script` as `bash -l -s` (script piped over stdin) in `cwd`, enforcing
/// `timeout_ms`. stdout/stderr are captured and truncated before logging (Section
/// 15.4).
pub async fn run_hook(
    name: &str,
    script: &str,
    cwd: &Path,
    timeout_ms: u64,
) -> Result<(), HookError> {
    tracing::debug!(hook = name, %timeout_ms, "starting workspace hook");

    let mut cmd = Command::new("bash");
    cmd.arg("-l")
        .arg("-s")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Without this, a hook that times out (or whose future is otherwise dropped
        // before completion) leaves its `bash` process running orphaned and
        // undetected -- `tokio::time::timeout` racing `wait_with_output()` only stops
        // *waiting* on the child, it doesn't touch the child itself. `kill_on_drop`
        // makes dropping the `Child` (which happens on this function's every early
        // return) actually terminate it. Note this still can't reach a *grandchild*
        // the script backgrounded with `&`, and on this machine `bash` may resolve to
        // WSL's launcher (see this module's own doc comment) -- killing the Windows-
        // side launcher process doesn't necessarily terminate what's running inside
        // the WSL VM. A real, if partial, improvement over killing nothing at all.
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| HookError::Spawn(e.to_string()))?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let script_owned = script.to_string();
    let write_task = tokio::spawn(async move {
        let _ = stdin.write_all(script_owned.as_bytes()).await;
        // Drop closes the pipe, signaling EOF so the shell knows the script is complete.
    });

    let wait = child.wait_with_output();
    let result = tokio::time::timeout(Duration::from_millis(timeout_ms), wait).await;
    let _ = write_task.await;

    match result {
        Ok(Ok(output)) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let truncated = truncate(&combined);
            if output.status.success() {
                tracing::debug!(hook = name, "hook completed");
                Ok(())
            } else {
                tracing::warn!(hook = name, status = %output.status, output = %truncated, "hook failed");
                Err(HookError::NonZeroExit {
                    status: output.status.to_string(),
                    output: truncated,
                })
            }
        }
        Ok(Err(e)) => Err(HookError::Spawn(e.to_string())),
        Err(_) => {
            tracing::warn!(hook = name, %timeout_ms, "hook timed out");
            Err(HookError::Timeout(timeout_ms))
        }
    }
}

/// Run a hook either on the host (`container` is `None`, today's behavior, unchanged)
/// or inside a Docker container (Docker mode, see README.md and `crate::container`).
/// `host_cwd` is still required in the container case: it's used to map to the
/// container-side working directory via `ContainerHandle::to_container_path`, keeping
/// every call site working in host `PathBuf`s as it does today and converting only
/// here, at the exec boundary.
pub async fn run_hook_maybe_containerized(
    name: &str,
    script: &str,
    host_root: &std::path::Path,
    host_cwd: &Path,
    timeout_ms: u64,
    container: Option<&crate::container::ContainerHandle>,
) -> Result<(), HookError> {
    match container {
        None => run_hook(name, script, host_cwd, timeout_ms).await,
        Some(c) => {
            let container_cwd = c.to_container_path(host_root, host_cwd);
            crate::container::exec_script(c, &container_cwd, script, timeout_ms)
                .await
                .map_err(|e| HookError::NonZeroExit {
                    status: "docker exec".to_string(),
                    output: e.to_string(),
                })
        }
    }
}

/// Full output of a captured command, regardless of exit status -- used by test
/// execution (AIR-6), which needs pass/fail counts and output text as *data*, not just
/// a success/failure signal.
#[derive(Debug, Clone)]
pub struct HookOutput {
    /// `None` only if the process was killed by a signal rather than exiting.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Like `run_hook`, but a non-zero exit is returned as data (`HookOutput::exit_code`)
/// rather than `HookError::NonZeroExit`. Still errors on a genuine spawn/timeout
/// failure -- those really do mean "couldn't get an answer at all".
pub async fn run_hook_capture(
    name: &str,
    script: &str,
    cwd: &Path,
    timeout_ms: u64,
) -> Result<HookOutput, HookError> {
    tracing::debug!(hook = name, %timeout_ms, "starting captured workspace command");

    let mut cmd = Command::new("bash");
    cmd.arg("-l")
        .arg("-s")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| HookError::Spawn(e.to_string()))?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let script_owned = script.to_string();
    let write_task = tokio::spawn(async move {
        let _ = stdin.write_all(script_owned.as_bytes()).await;
    });

    let wait = child.wait_with_output();
    let result = tokio::time::timeout(Duration::from_millis(timeout_ms), wait).await;
    let _ = write_task.await;

    match result {
        Ok(Ok(output)) => Ok(HookOutput {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }),
        Ok(Err(e)) => Err(HookError::Spawn(e.to_string())),
        Err(_) => {
            tracing::warn!(hook = name, %timeout_ms, "captured command timed out");
            Err(HookError::Timeout(timeout_ms))
        }
    }
}

/// Container-aware counterpart to `run_hook_capture`, mirroring
/// `run_hook_maybe_containerized`'s host/Docker split.
pub async fn run_hook_capture_maybe_containerized(
    name: &str,
    script: &str,
    host_root: &std::path::Path,
    host_cwd: &Path,
    timeout_ms: u64,
    container: Option<&crate::container::ContainerHandle>,
) -> Result<HookOutput, HookError> {
    match container {
        None => run_hook_capture(name, script, host_cwd, timeout_ms).await,
        Some(c) => {
            let container_cwd = c.to_container_path(host_root, host_cwd);
            crate::container::exec_capture_script(c, &container_cwd, script, timeout_ms)
                .await
                .map(|o| HookOutput {
                    exit_code: o.exit_code,
                    stdout: o.stdout,
                    stderr: o.stderr,
                })
                .map_err(|e| match e {
                    crate::container::ContainerError::Timeout(ms) => HookError::Timeout(ms),
                    other => HookError::Spawn(other.to_string()),
                })
        }
    }
}

fn truncate(s: &str) -> String {
    if s.len() > MAX_LOGGED_OUTPUT {
        format!("{}... [truncated]", &s[..MAX_LOGGED_OUTPUT])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn successful_hook_runs() {
        let dir = tempdir().unwrap();
        let res = run_hook("test", "echo hi > out.txt", dir.path(), 5000).await;
        assert!(res.is_ok());
        assert!(dir.path().join("out.txt").exists());
    }

    #[tokio::test]
    async fn failing_hook_errors() {
        let dir = tempdir().unwrap();
        let res = run_hook("test", "exit 1", dir.path(), 5000).await;
        assert!(matches!(res, Err(HookError::NonZeroExit { .. })));
    }

    #[tokio::test]
    async fn timeout_is_enforced() {
        let dir = tempdir().unwrap();
        let res = run_hook("test", "sleep 5", dir.path(), 100).await;
        assert!(matches!(res, Err(HookError::Timeout(_))));
    }

    #[tokio::test]
    async fn run_hook_capture_returns_exit_code_and_output_without_erroring() {
        let dir = tempdir().unwrap();
        let res = run_hook_capture("test", "echo out; echo err 1>&2; exit 3", dir.path(), 5000)
            .await
            .unwrap();
        assert_eq!(res.exit_code, Some(3));
        assert!(res.stdout.contains("out"));
        assert!(res.stderr.contains("err"));
    }

    #[tokio::test]
    async fn run_hook_capture_still_errors_on_timeout() {
        let dir = tempdir().unwrap();
        let res = run_hook_capture("test", "sleep 5", dir.path(), 100).await;
        assert!(matches!(res, Err(HookError::Timeout(_))));
    }

    #[tokio::test]
    async fn multiline_script_with_nested_quotes_and_case_statement() {
        // Regression test: this exact shape (nested double quotes inside a command
        // substitution, next to a case/esac block) previously arrived corrupted at
        // the shell when passed as a single `bash -lc <script>` argument on Windows.
        let dir = tempdir().unwrap();
        let script = r#"
name="$(basename "$PWD")"
case "$name" in
  nonexistent-branch) echo wrong >> out.txt ;;
  *) echo "matched: $name" >> out.txt ;;
esac
"#;
        let res = run_hook("test", script, dir.path(), 5000).await;
        assert!(res.is_ok(), "{res:?}");
        let contents = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
        assert!(
            contents.starts_with("matched:"),
            "unexpected contents: {contents:?}"
        );
    }
}
