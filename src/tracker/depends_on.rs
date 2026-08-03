//! Shared cross-issue `depends_on` resolution: tracker-agnostic once you have the raw
//! dependency identifiers and the full fetched issue set. Originally lived only in
//! `local.rs`; extracted so `github.rs` (which sources `depends_on` from a
//! `Depends-On: #N` line in the issue body instead of a YAML field) gets the exact
//! same "AND dispatchable with every dependency being done, populate blocked_by"
//! semantics rather than a second, possibly-diverging implementation.
//!
//! `parse_depends_on` below (the `Depends-On:` line scanner itself) is likewise shared
//! -- `github.rs` and `gitlab.rs` both source `depends_on` from an issue body line
//! rather than a native field, and the `#12, #45` syntax is identical on both hosts.

use crate::domain::{BlockerRef, Issue};
use std::collections::HashMap;

/// One successfully parsed issue, before cross-issue `depends_on` resolution.
pub struct RawIssue {
    pub issue: Issue,
    pub depends_on: Vec<String>,
}

/// Extract `#12`/`#45`-style issue numbers from a case-insensitive `Depends-On:` line
/// in an issue body. Shared by `github.rs` and `gitlab.rs` -- neither host has a
/// native blocking-dependency field reachable via plain REST, so both source it from
/// this same body-text convention.
pub fn parse_depends_on(body: &str) -> Vec<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.to_lowercase().strip_prefix("depends-on:").map(|_| {
            trimmed[trimmed.find(':').map(|i| i + 1).unwrap_or(trimmed.len())..].to_string()
        }) else {
            continue;
        };
        return rest
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter_map(|tok| tok.trim().strip_prefix('#'))
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
    }
    Vec::new()
}

/// `(some caller-chosen key, parsed issue or an error describing why it didn't parse)`.
/// The key is opaque to this module -- `local.rs` uses a filename stem (so a later
/// error can be re-associated with the file that produced it); `github.rs` can just
/// use the issue number, or an adapter with no per-item failure mode can pass no `Err`
/// entries at all.
pub type ParsedEntry<K> = (K, Result<RawIssue, String>);

/// Second pass: AND `dispatchable` with "every `depends_on` target is at state
/// `done`" (case-insensitive; any other terminal state, e.g. cancelled, does NOT
/// satisfy a dependency), and populate `blocked_by` for whichever dependencies aren't
/// satisfied yet. A `depends_on` entry naming an identifier that isn't present in
/// `raw` at all (typo, or that entry failed to parse) is treated as permanently
/// unsatisfied.
pub fn resolve_dependencies<K>(raw: Vec<ParsedEntry<K>>) -> Vec<(K, Result<Issue, String>)> {
    let state_by_identifier: HashMap<String, String> = raw
        .iter()
        .filter_map(|(_, r)| r.as_ref().ok())
        .map(|r| (r.issue.identifier.clone(), r.issue.normalized_state()))
        .collect();

    raw.into_iter()
        .map(|(key, r)| {
            let r = r.map(|mut raw_issue| {
                if raw_issue.depends_on.is_empty() {
                    return raw_issue.issue;
                }
                let mut blockers = Vec::new();
                let mut all_satisfied = true;
                for dep in &raw_issue.depends_on {
                    let dep_state = state_by_identifier.get(dep.as_str()).cloned();
                    let satisfied = dep_state.as_deref() == Some("done");
                    if !satisfied {
                        all_satisfied = false;
                        blockers.push(BlockerRef {
                            id: None,
                            identifier: Some(dep.clone()),
                            state: dep_state,
                        });
                    }
                }
                raw_issue.issue.blocked_by = blockers;
                raw_issue.issue.dispatchable = raw_issue.issue.dispatchable && all_satisfied;
                raw_issue.issue
            });
            (key, r)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Issue;

    fn issue(identifier: &str, state: &str) -> Issue {
        Issue {
            id: identifier.to_string(),
            native_ref: None,
            identifier: identifier.to_string(),
            title: identifier.to_string(),
            description: None,
            priority: None,
            state: state.to_string(),
            branch_name: None,
            url: None,
            assignee_id: None,
            labels: Vec::new(),
            blocked_by: Vec::new(),
            dispatchable: true,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn blocks_until_dependency_is_done() {
        let raw = vec![
            (
                "up",
                Ok(RawIssue {
                    issue: issue("UP-1", "todo"),
                    depends_on: vec![],
                }),
            ),
            (
                "down",
                Ok(RawIssue {
                    issue: issue("DOWN-1", "todo"),
                    depends_on: vec!["UP-1".to_string()],
                }),
            ),
        ];
        let resolved = resolve_dependencies(raw);
        let down = resolved
            .into_iter()
            .find(|(k, _)| *k == "down")
            .unwrap()
            .1
            .unwrap();
        assert!(!down.dispatchable);
        assert_eq!(down.blocked_by[0].identifier.as_deref(), Some("UP-1"));
    }

    #[test]
    fn unblocks_once_dependency_is_done() {
        let raw = vec![
            (
                "up",
                Ok(RawIssue {
                    issue: issue("UP-1", "done"),
                    depends_on: vec![],
                }),
            ),
            (
                "down",
                Ok(RawIssue {
                    issue: issue("DOWN-1", "todo"),
                    depends_on: vec!["UP-1".to_string()],
                }),
            ),
        ];
        let resolved = resolve_dependencies(raw);
        let down = resolved
            .into_iter()
            .find(|(k, _)| *k == "down")
            .unwrap()
            .1
            .unwrap();
        assert!(down.dispatchable);
        assert!(down.blocked_by.is_empty());
    }

    #[test]
    fn parses_depends_on_line() {
        assert_eq!(
            parse_depends_on("Some text\nDepends-On: #12, #45\nmore text"),
            vec!["12".to_string(), "45".to_string()]
        );
        assert_eq!(
            parse_depends_on("no dependency line here"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn missing_dependency_target_stays_blocked() {
        let raw = vec![(
            "down",
            Ok(RawIssue {
                issue: issue("DOWN-1", "todo"),
                depends_on: vec!["NOPE".to_string()],
            }),
        )];
        let resolved = resolve_dependencies(raw);
        let down = resolved.into_iter().next().unwrap().1.unwrap();
        assert!(!down.dispatchable);
        assert_eq!(down.blocked_by[0].state, None);
    }
}
