//! Workspace management and safety invariants (Section 9).

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

pub struct WorkspaceManager {
    root: PathBuf,
}

impl WorkspaceManager {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_for(&self, identifier: &str) -> PathBuf {
        self.root.join(derive_workspace_key(identifier))
    }

    /// Create or reuse the workspace for `identifier` (Section 9.2). Runs `after_create`
    /// only when the directory did not already exist; hook failure removes the
    /// partially-prepared directory and fails workspace creation.
    pub async fn create_for_issue(
        &self,
        identifier: &str,
        after_create_hook: Option<&str>,
        hook_timeout_ms: u64,
    ) -> Result<Workspace, WorkspaceError> {
        let workspace_key = derive_workspace_key(identifier);
        let path = self.root.join(&workspace_key);
        validate_containment(&self.root, &path)?;

        let created_now = !path.exists();
        if created_now {
            tokio::fs::create_dir_all(&path)
                .await
                .map_err(|e| WorkspaceError::Create(path.clone(), e.to_string()))?;
        } else if !path.is_dir() {
            return Err(WorkspaceError::NotADirectory(path));
        }

        if created_now
            && let Some(script) = after_create_hook
                && let Err(e) = hooks::run_hook("after_create", script, &path, hook_timeout_ms).await
                {
                    let _ = tokio::fs::remove_dir_all(&path).await;
                    return Err(WorkspaceError::HookFailed(e));
                }

        Ok(Workspace {
            path,
            workspace_key,
            created_now,
        })
    }

    /// Remove the workspace for a now-terminal issue (Section 9.4 `before_remove`,
    /// Section 8.6 startup cleanup, Section 8.5 reconciliation cleanup). Best-effort:
    /// hook and removal failures are logged, never propagated.
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
        if let Some(script) = before_remove_hook
            && let Err(e) = hooks::run_hook("before_remove", script, &path, hook_timeout_ms).await
        {
            tracing::warn!(?path, error = %e, "before_remove hook failed (ignored)");
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
