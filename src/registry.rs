//! Persisted registry of repos registered with the multi-project service
//! (`symphony serve`, `src/service.rs`): which repo, which branch, where its
//! `WORKFLOW.md` lives in that repo, and (optionally) which env var names a GitHub
//! token for it. Never stores a token *value* -- only a var name, matching
//! `config::RepoConfig::token_env`'s convention everywhere else in this codebase: a
//! literal secret pasted into the registration form would have to live somewhere
//! Symphony holds it at rest, which is a real change to the trust model this project
//! has deliberately avoided so far (see README.md "Trust and safety posture").
//!
//! Deliberately synchronous, connection-opened-per-call (unlike `eventlog.rs`'s
//! dedicated writer task fed over a channel): registering/removing a project is an
//! operator clicking a button, not a hot dispatch path -- no need for a background
//! writer here.

use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub const DB_FILENAME: &str = "registry.db";

#[derive(Debug, Clone)]
pub struct ProjectRow {
    pub id: String,
    pub repo_url: String,
    pub default_branch: String,
    pub workflow_path: String,
    pub token_env: Option<String>,
    pub status: String,
    pub added_at: String,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    repo_url TEXT NOT NULL,
    default_branch TEXT NOT NULL,
    workflow_path TEXT NOT NULL,
    token_env TEXT,
    status TEXT NOT NULL,
    added_at TEXT NOT NULL
);
";

pub fn open(data_dir: &Path) -> anyhow::Result<Connection> {
    std::fs::create_dir_all(data_dir)?;
    let conn = Connection::open(data_dir.join(DB_FILENAME))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// Derive a filesystem/URL-safe id from `owner/repo`, e.g. `octocat-hello-world` --
/// used both as the SQLite primary key and as the on-disk directory name under
/// `<data-dir>/projects/<id>/` and the URL path segment `/projects/<id>/...`.
pub fn slug(owner: &str, repo: &str) -> String {
    format!("{owner}-{repo}")
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Where a registered project's materialized `WORKFLOW.md` (and everything
/// `config::resolve` derives relative to it -- default workspace root, etc.) lives on
/// disk, keyed by `slug`.
pub fn project_dir(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join("projects").join(id)
}

/// Insert a new project row, or update an existing one with the same id (re-adding a
/// previously removed repo resurrects it rather than erroring).
pub fn insert(conn: &Connection, row: &ProjectRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO projects (id, repo_url, default_branch, workflow_path, token_env, status, added_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(id) DO UPDATE SET \
           repo_url = excluded.repo_url, \
           default_branch = excluded.default_branch, \
           workflow_path = excluded.workflow_path, \
           token_env = excluded.token_env, \
           status = excluded.status",
        rusqlite::params![
            row.id,
            row.repo_url,
            row.default_branch,
            row.workflow_path,
            row.token_env,
            row.status,
            row.added_at,
        ],
    )?;
    Ok(())
}

pub fn list_active(conn: &Connection) -> rusqlite::Result<Vec<ProjectRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, repo_url, default_branch, workflow_path, token_env, status, added_at \
         FROM projects WHERE status = 'active' ORDER BY added_at",
    )?;
    stmt.query_map([], |r| {
        Ok(ProjectRow {
            id: r.get(0)?,
            repo_url: r.get(1)?,
            default_branch: r.get(2)?,
            workflow_path: r.get(3)?,
            token_env: r.get(4)?,
            status: r.get(5)?,
            added_at: r.get(6)?,
        })
    })?
    .collect()
}

pub fn mark_removed(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute("UPDATE projects SET status = 'removed' WHERE id = ?1", [id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn row(id: &str) -> ProjectRow {
        ProjectRow {
            id: id.to_string(),
            repo_url: format!("https://github.com/o/{id}.git"),
            default_branch: "main".to_string(),
            workflow_path: "WORKFLOW.md".to_string(),
            token_env: Some("GITHUB_TOKEN".to_string()),
            status: "active".to_string(),
            added_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn slug_is_lowercase_and_safe() {
        assert_eq!(slug("Octo-Cat", "Hello World"), "octo-cat-hello-world");
    }

    #[test]
    fn insert_then_list_active_roundtrips() {
        let dir = tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        insert(&conn, &row("a")).unwrap();
        insert(&conn, &row("b")).unwrap();
        let rows = list_active(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "a");
        assert_eq!(rows[0].token_env.as_deref(), Some("GITHUB_TOKEN"));
    }

    #[test]
    fn mark_removed_excludes_from_active_list() {
        let dir = tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        insert(&conn, &row("a")).unwrap();
        mark_removed(&conn, "a").unwrap();
        assert!(list_active(&conn).unwrap().is_empty());
    }

    #[test]
    fn re_inserting_same_id_updates_in_place() {
        let dir = tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        insert(&conn, &row("a")).unwrap();
        mark_removed(&conn, "a").unwrap();
        let mut resurrected = row("a");
        resurrected.default_branch = "develop".to_string();
        insert(&conn, &resurrected).unwrap();
        let rows = list_active(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].default_branch, "develop");
    }
}
