//! `$VAR_NAME` indirection and `~` path expansion (Section 6.1).

use std::path::{Path, PathBuf};

/// If `value` is exactly `$VAR_NAME`, resolve it from the environment.
/// Returns `None` if the value is `$VAR_NAME`-shaped but the variable is
/// unset or resolves to an empty string (spec: "treat that secret as missing").
/// Otherwise returns the value unchanged.
pub fn resolve_var(value: &str) -> Option<String> {
    if let Some(name) = var_name(value) {
        match std::env::var(name) {
            Ok(v) if !v.is_empty() => Some(v),
            _ => None,
        }
    } else {
        Some(value.to_string())
    }
}

/// If `value` is exactly `$VAR_NAME`-shaped, return the bare name (no `$`). Exposed
/// (not just used internally by `resolve_var`) for callers that need the *name* of an
/// env var rather than its resolved value -- e.g. `config::RepoConfig`'s synthesized
/// git hooks reference the var by name in a generated shell script rather than
/// resolving and embedding the secret value directly.
pub fn var_name_of(value: &str) -> Option<&str> {
    var_name(value)
}

fn var_name(value: &str) -> Option<&str> {
    let rest = value.strip_prefix('$')?;
    if !rest.is_empty()
        && rest
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        Some(rest)
    } else {
        None
    }
}

/// Resolve the host's own Claude Code login file (`~/.claude/.credentials.json`), if
/// one exists -- used for `workspace.docker.mount_claude_credentials` (see
/// `config::DockerConfig`'s doc comment) so a containerized `claude` CLI can reuse the
/// host's existing session instead of needing a separate API key. Checks
/// `SYMPHONY_HOST_CLAUDE_CREDENTIALS` first (set by `daemon::start` when daemonized,
/// since a daemon container has no direct view of the real host filesystem to resolve
/// this itself -- see that function's own doc comment), falling back to resolving
/// directly from `USERPROFILE`/`HOME` (the non-daemonized case, where this process
/// *is* already running on the real host).
pub fn resolve_claude_credentials_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SYMPHONY_HOST_CLAUDE_CREDENTIALS") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let path = PathBuf::from(home)
        .join(".claude")
        .join(".credentials.json");
    path.is_file().then_some(path)
}

/// Recursively walk a parsed WORKFLOW.md config tree and collect every distinct
/// `$VAR_NAME`-shaped string value found anywhere in it (bare names, no `$`, sorted
/// and deduplicated). Used to forward exactly the secrets a config actually
/// references into a container's environment (`docker run -e VAR_NAME`, no `=value` --
/// Docker reads the value from *its own* invoking process's environment, so the
/// secret's value never appears in the `docker run`/`docker exec` command line
/// itself) rather than either guessing at field names ad hoc or forwarding the host's
/// entire environment wholesale.
pub fn collect_var_refs(v: &serde_yaml::Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_var_refs_into(v, &mut out);
    out.sort();
    out.dedup();
    out
}

fn collect_var_refs_into(v: &serde_yaml::Value, out: &mut Vec<String>) {
    match v {
        serde_yaml::Value::String(s) => {
            if let Some(name) = var_name(s) {
                out.push(name.to_string());
            }
        }
        serde_yaml::Value::Sequence(items) => {
            for item in items {
                collect_var_refs_into(item, out);
            }
        }
        serde_yaml::Value::Mapping(m) => {
            for (_, val) in m {
                collect_var_refs_into(val, out);
            }
        }
        _ => {}
    }
}

/// Expand `~` (home directory) and `$VAR` indirection for a path-shaped value,
/// then resolve relative paths against `base_dir` and normalize (without
/// requiring the path to exist).
pub fn resolve_path(value: &str, base_dir: &Path) -> PathBuf {
    let resolved = resolve_var(value).unwrap_or_default();
    let expanded = expand_home(&resolved);
    let p = PathBuf::from(expanded);
    let joined = if p.is_absolute() { p } else { base_dir.join(p) };
    normalize(&joined)
}

fn expand_home(value: &str) -> String {
    if let Some(rest) = value.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
        && let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        let home = home.to_string_lossy().to_string();
        return format!("{home}{rest}");
    }
    value.to_string()
}

/// Lexically normalize a path (collapse `.` and `..`) without touching the filesystem.
pub fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_whole_string_var() {
        unsafe {
            std::env::set_var("SYMPHONY_TEST_VAR", "hello");
        }
        assert_eq!(resolve_var("$SYMPHONY_TEST_VAR").as_deref(), Some("hello"));
        unsafe {
            std::env::remove_var("SYMPHONY_TEST_VAR");
        }
    }

    #[test]
    fn missing_var_is_none() {
        unsafe {
            std::env::remove_var("SYMPHONY_TEST_MISSING");
        }
        assert_eq!(resolve_var("$SYMPHONY_TEST_MISSING"), None);
    }

    #[test]
    fn non_var_value_passes_through() {
        assert_eq!(resolve_var("plain-value").as_deref(), Some("plain-value"));
    }

    #[test]
    fn collects_var_refs_from_nested_config_deduped_and_sorted() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            "tracker:\n  provider:\n    token: $GITHUB_TOKEN\nrepo:\n  token: $GITHUB_TOKEN\n\
             other:\n  - plain-value\n  - $ANOTHER_VAR\n",
        )
        .unwrap();
        assert_eq!(
            collect_var_refs(&yaml),
            vec!["ANOTHER_VAR".to_string(), "GITHUB_TOKEN".to_string()]
        );
    }

    #[test]
    fn resolve_claude_credentials_path_prefers_the_daemon_forwarded_override() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        std::fs::write(&creds, "{}").unwrap();
        unsafe {
            std::env::set_var("SYMPHONY_HOST_CLAUDE_CREDENTIALS", &creds);
        }
        assert_eq!(resolve_claude_credentials_path(), Some(creds));
        unsafe {
            std::env::remove_var("SYMPHONY_HOST_CLAUDE_CREDENTIALS");
        }
    }

    #[test]
    fn resolve_claude_credentials_path_ignores_override_pointing_at_missing_file() {
        unsafe {
            std::env::set_var(
                "SYMPHONY_HOST_CLAUDE_CREDENTIALS",
                "/definitely/does/not/exist.json",
            );
        }
        // Falls through to the real USERPROFILE/HOME lookup rather than returning the
        // dangling override path -- assert only that it isn't the dangling path itself,
        // since whether *this* machine happens to have real credentials is unknowable.
        assert_ne!(
            resolve_claude_credentials_path(),
            Some(PathBuf::from("/definitely/does/not/exist.json"))
        );
        unsafe {
            std::env::remove_var("SYMPHONY_HOST_CLAUDE_CREDENTIALS");
        }
    }

    #[test]
    fn normalizes_parent_dirs() {
        let p = normalize(Path::new("/a/b/../c/./d"));
        assert_eq!(p, PathBuf::from("/a/c/d"));
    }
}
