//! Built-in role prompts (AIR-2's eventual `roles:` config is out of scope here --
//! this is deliberately the smallest slice of it a pipeline stage needs today: a
//! stage whose `role` names a file in `builtin/` runs that prompt instead of the
//! project's own `WORKFLOW.md` template; every other stage is unaffected). Adding the
//! next role (AIR-5's implementer, AIR-7's reviewer, ...) is one more `include_str!`
//! arm here, not a new mechanism.

/// The requirements stage's extraction rubric (AIR-4): digest the issue into
/// validated requirements/acceptance criteria and stop rather than guess at
/// ambiguity. See the file itself for the full instructions.
const REQUIREMENTS: &str = include_str!("builtin/requirements.md");

/// `Some(prompt)` if `role` names a built-in role prompt; `None` means the caller
/// should fall back to the project's own `WORKFLOW.md` prompt template (every
/// pipeline stage's behavior before this existed, and still every stage whose `role`
/// isn't one of the names below).
pub fn builtin_prompt(role: &str) -> Option<&'static str> {
    match role.trim() {
        "requirements" => Some(REQUIREMENTS),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirements_role_resolves_to_the_builtin_prompt() {
        let prompt = builtin_prompt("requirements").unwrap();
        assert!(prompt.contains("record_requirements"));
        assert!(prompt.contains("raise_clarification"));
    }

    #[test]
    fn unknown_role_falls_back_to_none() {
        assert!(builtin_prompt("developer").is_none());
        assert!(builtin_prompt("").is_none());
    }
}
