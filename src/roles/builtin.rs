//! Built-in default prompts for the eight roadmap roles (AIR-2), embedded at compile
//! time so a project can enable the pipeline with `role: reviewer` and no prompt file
//! at all. Each renders under `template.rs`'s strict mode against the same context
//! `orchestrator::render_turn_prompt` builds for a pipeline stage (`issue.*` plus
//! `cycle.*`) -- see the `mod tests` at the bottom of `src/roles/mod.rs`.

pub const ROLE_NAMES: &[&str] = &[
    "requirements",
    "planner",
    "developer",
    "test",
    "reviewer",
    "security",
    "release",
    "observability",
];

const REQUIREMENTS: &str = include_str!("builtin/requirements.md");
const PLANNER: &str = include_str!("builtin/planner.md");
const DEVELOPER: &str = include_str!("builtin/developer.md");
const TEST: &str = include_str!("builtin/test.md");
const REVIEWER: &str = include_str!("builtin/reviewer.md");
const SECURITY: &str = include_str!("builtin/security.md");
const RELEASE: &str = include_str!("builtin/release.md");
const OBSERVABILITY: &str = include_str!("builtin/observability.md");

/// The built-in prompt template for `role_name`, or `None` if it isn't one of the
/// eight roadmap roles (`ROLE_NAMES`). Case/whitespace already normalized by the
/// caller (`config::resolve`'s validation, `roles::resolve`).
pub fn prompt_for(role_name: &str) -> Option<&'static str> {
    match role_name {
        "requirements" => Some(REQUIREMENTS),
        "planner" => Some(PLANNER),
        "developer" => Some(DEVELOPER),
        "test" => Some(TEST),
        "reviewer" => Some(REVIEWER),
        "security" => Some(SECURITY),
        "release" => Some(RELEASE),
        "observability" => Some(OBSERVABILITY),
        _ => None,
    }
}

pub fn is_known(role_name: &str) -> bool {
    ROLE_NAMES.contains(&role_name)
}

/// A built-in role's own default `ToolPolicy`, used by `roles::resolve` when a project
/// hasn't set `roles.<name>.tools` itself. Every role but Reviewer and Security keeps
/// the unrestricted default every backend already starts with; both get
/// `ToolPolicy::SWEBOT` (AIR-7/AIR-8: file-mutating tools denied, same restriction
/// SweBot's own PR review already runs under) -- a review or security stage that can
/// edit files isn't review-only, and a project shouldn't have to remember to
/// configure that by hand (AIR-8's own acceptance criterion: `allow_edits: false`).
pub fn default_tool_policy(role_name: &str) -> crate::agent::ToolPolicy {
    match role_name {
        "reviewer" | "security" => crate::agent::ToolPolicy::SWEBOT,
        _ => crate::agent::ToolPolicy::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_name_has_a_built_in_prompt() {
        for name in ROLE_NAMES {
            assert!(
                prompt_for(name).is_some(),
                "role '{name}' listed in ROLE_NAMES has no built-in prompt"
            );
        }
    }

    #[test]
    fn unknown_role_has_no_built_in_prompt() {
        assert_eq!(prompt_for("not-a-real-role"), None);
        assert!(!is_known("not-a-real-role"));
    }
}
