//! Workspace management and safety invariants (Section 9).

use crate::config::DockerConfig;
use crate::container::{self, ContainerHandle, MountSource};
use crate::envsub;
use crate::hooks;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace path escapes workspace root: {0:?}")]
    OutsideRoot(PathBuf),
    #[error("workspace path exists and is not a directory: {0:?}")]
    NotADirectory(PathBuf),
    #[error("failed to create workspace directory {0:?}: {1}")]
    Create(PathBuf, String),
    #[error("after_create hook failed: {0}")]
    HookFailed(#[from] hooks::HookError),
    #[error("failed to start container: {0}")]
    ContainerFailed(#[from] container::ContainerError),
}

/// Docker mode context (see README.md): the project root to bind-mount plus the
/// resolved `workspace.docker` config. Only present when `docker.enabled` is true.
#[derive(Debug, Clone)]
pub struct DockerContext {
    pub workflow_dir: PathBuf,
    pub config: DockerConfig,
    /// Where each ticket container's project-root mount actually comes from.
    /// `MountSource::HostPath(workflow_dir)` in the common case (Symphony running
    /// directly on the host); `MountSource::NamedVolume(_)` when Symphony itself is
    /// daemonized (see README.md "Daemonizing Symphony" and `container::MountSource`'s
    /// own doc comment for why a bind-mount can't be used in that case).
    pub mount: MountSource,
    /// Env var *names* (e.g. `repo.token`'s referenced var) to forward into each
    /// per-ticket container via `docker run -e NAME` -- see
    /// `envsub::collect_var_refs`'s doc comment for why this exists: without it, a
    /// container-mode `repo:` hook's credential helper finds its referenced var unset
    /// inside the container, since Docker doesn't inherit the host environment into a
    /// container automatically the way a plain child process would.
    pub env_passthrough: Vec<String>,
    /// Host path to the host's own Claude Code login (`~/.claude/.credentials.json`),
    /// resolved once at startup via `envsub::resolve_claude_credentials_path` --
    /// `Some` only when `config.mount_claude_credentials` is true AND that file was
    /// actually found. Bind-mounted read-only into each per-ticket container at
    /// `/home/agent/.claude/.credentials.json` so the containerized `claude` CLI
    /// reuses the host's existing session instead of needing its own separate
    /// `ANTHROPIC_API_KEY`.
    pub claude_credentials_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub path: PathBuf,
    /// Part of Section 4.1.4's data model; not currently read outside tests since
    /// `path` already encodes it.
    #[allow(dead_code)]
    pub workspace_key: String,
    #[allow(dead_code)]
    pub created_now: bool,
    /// `Some` when Docker mode is enabled: the container hooks and the coding agent
    /// should run inside for this ticket (Section "Docker mode", README.md).
    pub container: Option<ContainerHandle>,
}

/// Derive a sanitized, collision-resistant workspace directory name from an issue
/// identifier (Section 4.2 / 9.5 Invariant 3).
pub fn derive_workspace_key(identifier: &str) -> String {
    let sanitized: String = identifier
        .chars()
        .map(|c| if is_allowed(c) { c } else { '_' })
        .collect();

    // Guard against sanitized values that are themselves path-traversal-shaped even
    // though every individual character is in the allowed set (e.g. "..").
    let dangerous = sanitized.is_empty()
        || sanitized.chars().all(|c| c == '.')
        || sanitized == "."
        || sanitized == "..";

    if sanitized == identifier && !dangerous {
        return sanitized;
    }

    let hash = Sha256::digest(identifier.as_bytes());
    let hex: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect(); // 64 bits
    let base = if dangerous { "issue" } else { &sanitized };
    format!("{base}_{hex}")
}

fn is_allowed(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'
}

/// Best-effort permissive chmod (world rwx) for a freshly created workspace directory
/// in Docker mode -- see the call site's doc comment. No-op on non-Unix targets:
/// Windows has no POSIX mode bits, and host-mode Docker on Windows doesn't hit this
/// class of ownership mismatch the way a real Linux volume in daemon mode does.
#[cfg(unix)]
async fn chmod_permissive(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)).await {
        tracing::warn!(?path, error = %e, "failed to chmod workspace directory (ignored)");
    }
}

#[cfg(not(unix))]
async fn chmod_permissive(_path: &Path) {}

/// Invariant 2: the workspace path MUST stay inside the workspace root.
fn validate_containment(root: &Path, path: &Path) -> Result<(), WorkspaceError> {
    let root_n = envsub::normalize(root);
    let path_n = envsub::normalize(path);
    let contained = if cfg!(windows) {
        path_n
            .to_string_lossy()
            .to_lowercase()
            .starts_with(&root_n.to_string_lossy().to_lowercase())
    } else {
        path_n.starts_with(&root_n)
    };
    if contained {
        Ok(())
    } else {
        Err(WorkspaceError::OutsideRoot(path.to_path_buf()))
    }
}

/// Marks a workspace directory as having successfully completed `after_create`, so
/// `create_for_issue` can tell "already initialized" apart from "directory exists but
/// was never actually set up" (e.g. left behind by a crash between `create_dir_all`
/// and a completed `after_create`, or stale debris from an earlier run). Directory
/// existence alone isn't a safe signal for this: a real production incident this
/// session (bsky-archiver's `AR-12`) hit exactly this case -- a stale empty directory
/// caused `after_create` to be silently skipped forever, so every subsequent
/// `before_run` failed identically on every retry with no path to recovery short of
/// manual intervention.
const INIT_MARKER: &str = ".symphony-initialized";

pub struct WorkspaceManager {
    root: PathBuf,
    docker: Option<DockerContext>,
}

impl WorkspaceManager {
    pub fn new(root: PathBuf) -> Self {
        Self { root, docker: None }
    }

    /// Enable Docker mode: hooks and the coding agent run inside a per-ticket
    /// container bind-mounting `docker.workflow_dir` instead of directly on the host.
    pub fn with_docker(mut self, docker: Option<DockerContext>) -> Self {
        self.docker = docker;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_for(&self, identifier: &str) -> PathBuf {
        self.root.join(derive_workspace_key(identifier))
    }

    /// Create or reuse the workspace for `identifier` (Section 9.2). Runs `after_create`
    /// only when it hasn't successfully completed before (tracked via a marker file,
    /// not directory existence -- see `INIT_MARKER`'s doc comment for why); hook
    /// failure removes the partially-prepared directory and fails workspace creation.
    /// In Docker mode, also ensures the ticket's container is running (idempotent,
    /// safe to call on every dispatch/retry, not just first creation) before
    /// `after_create` runs inside it.
    pub async fn create_for_issue(
        &self,
        identifier: &str,
        after_create_hook: Option<&str>,
        hook_timeout_ms: u64,
    ) -> Result<Workspace, WorkspaceError> {
        let workspace_key = derive_workspace_key(identifier);
        let path = self.root.join(&workspace_key);
        validate_containment(&self.root, &path)?;

        if !path.exists() {
            tokio::fs::create_dir_all(&path)
                .await
                .map_err(|e| WorkspaceError::Create(path.clone(), e.to_string()))?;
            if self.docker.is_some() {
                // In Docker mode this directory is written to by two different
                // processes with two different uids: the orchestrator itself (root,
                // when daemonized -- see README.md "Daemonizing Symphony") creates
                // it, but the per-ticket sibling container writes into it (git clone,
                // etc.) as `workspace.docker.user`. Best-effort permissive chmod so
                // that mismatch doesn't turn into a permission-denied clone failure --
                // no-op on Windows (host-mode Docker's bind-mount translation doesn't
                // hit this the same way; only seen in practice under daemon mode's
                // real Linux volume).
                chmod_permissive(&path).await;
            }
        } else if !path.is_dir() {
            return Err(WorkspaceError::NotADirectory(path));
        }

        let marker = path.join(INIT_MARKER);
        let created_now = !marker.exists();

        let container = match &self.docker {
            None => None,
            Some(ctx) => {
                let name = container::derive_container_name(&ctx.workflow_dir, identifier);
                let container_root = Path::new(container::CONTAINER_PROJECT_ROOT);
                let extra_mounts: Vec<(PathBuf, String, bool)> = ctx
                    .claude_credentials_path
                    .as_ref()
                    .map(|p| {
                        vec![(
                            p.clone(),
                            "/home/agent/.claude/.credentials.json".to_string(),
                            true,
                        )]
                    })
                    .unwrap_or_default();
                let options = container::RunOptions {
                    network: &ctx.config.network,
                    mem_limit: ctx.config.mem_limit.as_deref(),
                    cpus: ctx.config.cpus.as_deref(),
                    user: ctx.config.user.as_deref(),
                    env_passthrough: &ctx.env_passthrough,
                    extra_mounts: &extra_mounts,
                };
                let handle = container::ensure_running(
                    ctx.config.image.as_deref().unwrap_or_default(),
                    &name,
                    &ctx.mount,
                    container_root,
                    &options,
                )
                .await?;
                Some(handle)
            }
        };

        if created_now {
            if let Some(script) = after_create_hook {
                let result = match &container {
                    None => hooks::run_hook("after_create", script, &path, hook_timeout_ms).await,
                    Some(c) => {
                        let docker_ctx = self
                            .docker
                            .as_ref()
                            .expect("container implies docker context");
                        hooks::run_hook_maybe_containerized(
                            "after_create",
                            script,
                            &docker_ctx.workflow_dir,
                            &path,
                            hook_timeout_ms,
                            Some(c),
                        )
                        .await
                    }
                };
                if let Err(e) = result {
                    let _ = tokio::fs::remove_dir_all(&path).await;
                    return Err(WorkspaceError::HookFailed(e));
                }
            }
            // Only written after a successful after_create (or immediately if there's
            // no hook configured) -- a failed hook removes the directory above instead,
            // so there's nothing to mark. Best-effort: a failed write just means
            // after_create may run again next time, which is safe (if not perfectly
            // idempotent for every possible hook script) and far better than the
            // alternative of silently never running it again.
            if let Err(e) = tokio::fs::write(&marker, b"").await {
                tracing::warn!(?marker, error = %e, "failed to write workspace init marker (ignored)");
            }
        }

        Ok(Workspace {
            path,
            workspace_key,
            created_now,
            container,
        })
    }

    /// Remove the workspace for a now-terminal issue (Section 9.4 `before_remove`,
    /// Section 8.6 startup cleanup, Section 8.5 reconciliation cleanup). Best-effort:
    /// hook and removal failures are logged, never propagated. In Docker mode, the
    /// container is stopped and removed before the host directory is deleted.
    pub async fn remove_for_issue(
        &self,
        identifier: &str,
        before_remove_hook: Option<&str>,
        hook_timeout_ms: u64,
    ) {
        let path = self.path_for(identifier);
        if !path.exists() {
            return;
        }
        if validate_containment(&self.root, &path).is_err() {
            tracing::error!(?path, "refusing to remove workspace outside root");
            return;
        }

        let container_name = self
            .docker
            .as_ref()
            .map(|ctx| container::derive_container_name(&ctx.workflow_dir, identifier));

        if let Some(script) = before_remove_hook {
            let result = match (&container_name, &self.docker) {
                (Some(name), Some(ctx)) => {
                    let handle = ContainerHandle {
                        name: name.clone(),
                        container_root: Path::new(container::CONTAINER_PROJECT_ROOT).to_path_buf(),
                    };
                    // `path` (the real host workspace dir) mapped relative to
                    // `workflow_dir` -- not reconstructed from `identifier` alone --
                    // so this stays correct regardless of where `workspace.root` is
                    // configured relative to the project root (e.g. `.workspaces/`).
                    hooks::run_hook_maybe_containerized(
                        "before_remove",
                        script,
                        &ctx.workflow_dir,
                        &path,
                        hook_timeout_ms,
                        Some(&handle),
                    )
                    .await
                }
                _ => hooks::run_hook("before_remove", script, &path, hook_timeout_ms).await,
            };
            if let Err(e) = result {
                tracing::warn!(?path, error = %e, "before_remove hook failed (ignored)");
            }
        }

        if let Some(name) = &container_name {
            container::remove(name).await;
        }

        if let Err(e) = tokio::fs::remove_dir_all(&path).await {
            tracing::warn!(?path, error = %e, "failed to remove workspace (ignored)");
        }
    }

    /// Invariant 1: the coding agent MUST run only in the validated per-issue workspace path.
    pub fn validate_agent_cwd(&self, path: &Path) -> Result<(), WorkspaceError> {
        validate_containment(&self.root, path)?;
        if !path.is_dir() {
            return Err(WorkspaceError::NotADirectory(path.to_path_buf()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn simple_identifier_is_unchanged() {
        assert_eq!(derive_workspace_key("ABC-123"), "ABC-123");
    }

    #[test]
    fn sanitizes_disallowed_characters_with_hash_suffix() {
        let key = derive_workspace_key("team/ABC 123");
        assert!(key.starts_with("team_ABC_123_"));
        assert_eq!(key.len(), "team_ABC_123_".len() + 16);
    }

    #[test]
    fn distinct_identifiers_that_sanitize_equal_get_distinct_keys() {
        let a = derive_workspace_key("team/ABC 123");
        let b = derive_workspace_key("team ABC/123");
        assert_ne!(a, b);
    }

    #[test]
    fn path_traversal_identifier_is_neutralized() {
        let key = derive_workspace_key("..");
        assert!(!key.contains(".."));
        assert!(key.starts_with("issue_"));
    }

    #[tokio::test]
    async fn create_then_reuse_workspace() {
        let root = tempdir().unwrap();
        let mgr = WorkspaceManager::new(root.path().to_path_buf());
        let ws1 = mgr.create_for_issue("ABC-1", None, 5000).await.unwrap();
        assert!(ws1.created_now);
        let ws2 = mgr.create_for_issue("ABC-1", None, 5000).await.unwrap();
        assert!(!ws2.created_now);
        assert_eq!(ws1.path, ws2.path);
    }

    /// Regression test for a real production incident (bsky-archiver's `AR-12`, this
    /// session): a workspace directory that exists but was never actually initialized
    /// (crash between `create_dir_all` and a completed `after_create`, or -- as
    /// happened -- leftover debris from an earlier run) must still run `after_create`
    /// on the next `create_for_issue` call. The old `created_now = !path.exists()`
    /// check treated "the directory exists" as "it's already initialized", so a
    /// stale/incomplete directory silently skipped `after_create` forever, and every
    /// subsequent `before_run` failed the same way on every retry with no path to
    /// recovery short of manual intervention.
    #[tokio::test]
    async fn pre_existing_uninitialized_directory_still_runs_after_create() {
        let root = tempdir().unwrap();
        let mgr = WorkspaceManager::new(root.path().to_path_buf());

        // Simulate the AR-12 scenario directly: the workspace directory already
        // exists (as it would after a `create_dir_all` that succeeded but whose
        // `after_create` never got the chance to run or complete), but is otherwise
        // empty -- nothing marks it as initialized.
        let key = derive_workspace_key("ABC-3");
        std::fs::create_dir_all(root.path().join(&key)).unwrap();
        assert!(root.path().join(&key).exists());

        let ws = mgr
            .create_for_issue("ABC-3", Some("echo initialized >> marker.txt"), 5000)
            .await
            .unwrap();

        assert!(
            ws.path.join("marker.txt").exists(),
            "after_create should have run against the pre-existing but uninitialized directory"
        );
    }

    /// End-to-end Docker-mode check against a real daemon: `after_create` must run
    /// *inside* the container (proven by writing a marker only reachable via the
    /// container's own filesystem view, which shows up on the host through the bind
    /// mount), and `remove_for_issue` must tear the container down.
    #[tokio::test]
    #[ignore]
    async fn docker_mode_runs_after_create_in_container_and_tears_down_on_remove() {
        assert!(
            container::docker_available().await,
            "docker must be available for this test"
        );
        let workflow_dir = tempdir().unwrap();
        let root = workflow_dir.path().join(".workspaces");
        std::fs::create_dir_all(&root).unwrap();

        let mgr = WorkspaceManager::new(root).with_docker(Some(DockerContext {
            workflow_dir: workflow_dir.path().to_path_buf(),
            mount: MountSource::HostPath(workflow_dir.path().to_path_buf()),
            env_passthrough: Vec::new(),
            claude_credentials_path: None,
            config: crate::config::DockerConfig {
                enabled: true,
                image: Some("debian:bookworm-slim".to_string()),
                network: "bridge".to_string(),
                mem_limit: None,
                cpus: None,
                user: None,
                mount_claude_credentials: false,
            },
        }));

        let identifier = "AR-DOCKER-TEST";
        let ws = mgr
            .create_for_issue(identifier, Some("echo from-container > marker.txt"), 30_000)
            .await
            .unwrap();
        assert!(ws.container.is_some());

        let marker = ws.path.join("marker.txt");
        assert!(
            marker.exists(),
            "after_create should have run inside the container"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().trim(),
            "from-container"
        );

        mgr.remove_for_issue(identifier, None, 30_000).await;
        assert!(!ws.path.exists(), "workspace directory should be removed");
    }

    #[tokio::test]
    async fn after_create_hook_runs_only_once() {
        let root = tempdir().unwrap();
        let mgr = WorkspaceManager::new(root.path().to_path_buf());
        let ws1 = mgr
            .create_for_issue("ABC-2", Some("echo x >> marker.txt"), 5000)
            .await
            .unwrap();
        mgr.create_for_issue("ABC-2", Some("echo x >> marker.txt"), 5000)
            .await
            .unwrap();
        let contents = std::fs::read_to_string(ws1.path.join("marker.txt")).unwrap();
        assert_eq!(contents.lines().count(), 1);
    }
}
