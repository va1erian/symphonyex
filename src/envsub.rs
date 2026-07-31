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

/// Expand `~` (home directory) and `$VAR` indirection for a path-shaped value,
/// then resolve relative paths against `base_dir` and normalize (without
/// requiring the path to exist).
pub fn resolve_path(value: &str, base_dir: &Path) -> PathBuf {
    let resolved = resolve_var(value).unwrap_or_default();
    let expanded = expand_home(&resolved);
    let p = PathBuf::from(expanded);
    let joined = if p.is_absolute() {
        p
    } else {
        base_dir.join(p)
    };
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
    fn normalizes_parent_dirs() {
        let p = normalize(Path::new("/a/b/../c/./d"));
        assert_eq!(p, PathBuf::from("/a/c/d"));
    }
}
