//! Minimal git helpers for SweBot's own read-only workspaces -- a shared clone for
//! Q&A/drafting context, and a one-shot per-PR checkout for review. Deliberately not
//! reusing `WorkspaceManager`/`hooks::run_hook` (built for per-ticket branches and
//! Docker-mode hook execution): these are simpler, host-side-only operations with no
//! branch of Symphony's own to manage, and never run inside a per-ticket container.

use crate::config::RepoConfig;
use std::path::Path;
use tokio::process::Command;

async fn run_git(args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().await.map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "git {} -> {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// `(env var name, resolved token)` for `repo.token`, if configured and set --
/// mirrors `config::synthesize_repo_hooks`'s own resolution, kept separate since that
/// one runs inside a generated shell script rather than a directly-spawned process.
fn credential(repo: &RepoConfig) -> Option<(String, String)> {
    let var = repo.token_env.as_ref()?;
    let token = std::env::var(var).ok().filter(|v| !v.is_empty())?;
    Some((var.clone(), token))
}

async fn clone_into(repo: &RepoConfig, dest: &Path, branch: Option<&str>) -> Result<(), String> {
    let cred = credential(repo);
    // Same credential-helper trick `synthesize_repo_hooks` uses (see that function's
    // own comment on `credential_username` for why this differs per provider): the
    // token never appears in argv or the remote URL, only referenced by env var
    // *name* in a helper script -- the actual value only ever lives in this child
    // process's env.
    let credential_username = match repo.provider {
        crate::config::RepoProvider::Github => "x-access-token",
        crate::config::RepoProvider::Gitlab => "oauth2",
    };
    let helper_arg = cred.as_ref().map(|(var, _)| {
        format!(
            "credential.helper=!f() {{ echo username={credential_username}; echo password=${var}; }}; f"
        )
    });

    let mut args: Vec<&str> = Vec::new();
    if let Some(h) = &helper_arg {
        args.push("-c");
        args.push(h);
    }
    args.push("clone");
    if let Some(b) = branch {
        args.push("--branch");
        args.push(b);
    }
    args.push(&repo.url);
    args.push(".");

    let env: Vec<(&str, &str)> = cred
        .as_ref()
        .map(|(var, token)| vec![(var.as_str(), token.as_str())])
        .unwrap_or_default();
    run_git(&args, dest, &env).await
}

/// Clone `repo` into `dest` if it doesn't already exist there, else `git pull`
/// (fast-forward only -- if that fails, e.g. a force-push happened upstream, the next
/// poll cycle just retries against whatever's there; SweBot's own reads don't need
/// perfect freshness on every single cycle).
pub async fn ensure_shared_clone(repo: &RepoConfig, dest: &Path) -> Result<(), String> {
    if dest.join(".git").is_dir() {
        return run_git(&["pull", "--ff-only"], dest, &[]).await;
    }
    tokio::fs::create_dir_all(dest)
        .await
        .map_err(|e| e.to_string())?;
    clone_into(repo, dest, None).await
}

/// One-shot clone of `repo` at `branch` into `dest`, which must not already exist --
/// the caller is expected to pass a fresh scratch directory per review.
pub async fn clone_branch(repo: &RepoConfig, dest: &Path, branch: &str) -> Result<(), String> {
    tokio::fs::create_dir_all(dest)
        .await
        .map_err(|e| e.to_string())?;
    clone_into(repo, dest, Some(branch)).await
}

/// `git diff origin/<default_branch>...HEAD` inside an already-checked-out review
/// clone. `origin/<default_branch>`, not a bare `<default_branch>`: `clone_branch`
/// clones with `--branch <ticket-branch>`, which only creates a *local* branch for
/// the one actually checked out -- every other branch (including the default one)
/// exists only as a remote-tracking `origin/*` ref, never a local `<default_branch>`
/// one, so diffing against the bare name fails with "unknown revision".
pub async fn diff_against(cwd: &Path, default_branch: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(["diff", &format!("origin/{default_branch}...HEAD")])
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "git diff -> {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
