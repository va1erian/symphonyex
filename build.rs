//! Bakes the git branch/commit this binary was built from into `SYMPHONY_VERSION`
//! (read by `main.rs` via `env!` for `--version`/`-V`). Solves exactly the confusion
//! that prompted this file: two worktrees of the same repo on different branches,
//! each producing a `symphony.exe` with the same Cargo.toml version number but very
//! different behavior -- `--version` alone couldn't tell them apart.
//!
//! Best-effort: outside a git checkout (a packaged release with `.git` stripped, or
//! `git` missing from PATH) every lookup below falls back to "unknown" rather than
//! failing the build -- this is diagnostic sugar, not something worth breaking a
//! release over.

use std::process::Command;

fn git_output(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    let branch =
        git_output(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let sha =
        git_output(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    // Non-empty `git status --porcelain` output means uncommitted changes -- flag
    // that explicitly rather than letting a locally-modified build look identical to
    // a clean one at the same commit.
    let dirty = git_output(&["status", "--porcelain"]).is_some();

    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let version = format!(
        "{pkg_version} ({branch}@{sha}{})",
        if dirty { "-dirty" } else { "" }
    );
    println!("cargo:rustc-env=SYMPHONY_VERSION={version}");

    // Re-run this script (and so refresh the baked-in version) whenever HEAD moves --
    // a branch switch or new commit -- rather than cargo caching the old string
    // because no *source* file changed.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
}
