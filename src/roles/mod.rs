//! Roles (AIR-2): per-`pipeline.stages[].role` prompt, backend, model and tool-policy
//! resolution. Generalizes what was previously SweBot's own hardcoded restricted-backend
//! construction (`swebot::build_restricted_backend`) into something a pipeline stage
//! can reuse -- a project's `roles.<name>` override (`config::RoleConfig`), or, absent
//! one, a built-in default prompt for each of the eight roadmap roles (`builtin`).
//!
//! Two entry points, both used from `orchestrator::run_pipeline`:
//! - [`resolve`]: turn a stage's `role` name into a fully-resolved prompt/backend/
//!   model/tool policy, applying `agent.*` defaults for anything a role doesn't
//!   override.
//! - [`build_backend`]: construct a standalone `AgentBackend` for a role that overrides
//!   `agent.backend`/model -- mirrors `orchestrator::build_shared`'s own backend
//!   construction, minus MCP tool wiring (a stage-scoped role session doesn't get
//!   provider-native tracker tools; that stays whole-cycle, on the shared session).

pub mod builtin;

use crate::agent::claude::ClaudeBackend;
use crate::agent::codex::CodexBackend;
use crate::agent::opencode::OpenCodeBackend;
use crate::agent::{AgentBackend, ToolPolicy};
use crate::config::{AgentBackendKind, EffectiveConfig};

/// A role's identity fully resolved against `EffectiveConfig` defaults.
pub struct ResolvedRole {
    pub prompt: String,
    pub backend: AgentBackendKind,
    pub model: Option<String>,
    #[allow(dead_code)]
    pub max_turns: Option<u32>,
    pub tool_policy: ToolPolicy,
    /// Whether `roles.<name>.backend` or `.model` was actually set, vs just inherited
    /// from `agent.*`/`<backend>.model`. `orchestrator::run_pipeline` uses this to
    /// decide whether a stage needs its own freshly-started session
    /// ([`build_backend`]) or can keep running on the cycle's shared one.
    pub overrides_backend: bool,
}

/// Resolve `role_name` (already validated against `cfg.roles`/`builtin::is_known` by
/// `config::resolve`) into a [`ResolvedRole`]. Returns `Err` only if a project's
/// `roles.<name>` names a role that's neither configured nor built-in -- defense in
/// depth; `config::resolve` should already have rejected that stage.
pub fn resolve(role_name: &str, cfg: &EffectiveConfig) -> Result<ResolvedRole, String> {
    let key = role_name.trim().to_lowercase();
    let project = cfg.roles.get(&key);

    let prompt = match project.and_then(|r| r.prompt.clone()) {
        Some(p) => p,
        None => builtin::prompt_for(&key)
            .ok_or_else(|| {
                format!("role '{role_name}' is not a built-in role and has no roles.{key} entry")
            })?
            .to_string(),
    };

    let backend_override = project.and_then(|r| r.backend);
    let model_override = project.and_then(|r| r.model.clone());
    let backend = backend_override.unwrap_or(cfg.agent_backend);
    let model = model_override.clone().or_else(|| default_model_for(backend, cfg));
    // AIR-7: a project's `roles.<name>.tools` always wins when set; absent one, a
    // built-in role gets its own default posture (the Reviewer's is `ToolPolicy::SWEBOT`
    // -- "a Reviewer that can edit files is not a reviewer", roadmap §4 -- same as
    // SweBot's own restricted sessions) rather than blanket-unrestricted.
    let tool_policy = project
        .map(|r| r.tool_policy)
        .unwrap_or_else(|| builtin::default_tool_policy(&key));
    let max_turns = project.and_then(|r| r.max_turns);

    Ok(ResolvedRole {
        prompt,
        backend,
        model,
        max_turns,
        tool_policy,
        overrides_backend: backend_override.is_some() || model_override.is_some(),
    })
}

fn default_model_for(backend: AgentBackendKind, cfg: &EffectiveConfig) -> Option<String> {
    match backend {
        AgentBackendKind::Claude => cfg.claude.model.clone(),
        AgentBackendKind::OpenCode => cfg.opencode.model.clone(),
        AgentBackendKind::Codex => None,
    }
}

/// Build a standalone backend for a role whose `overrides_backend` is true. Same
/// command/timeout/args as `agent.*`'s own backend of that kind, but `role.model`
/// instead of `<backend>.model`, and no MCP tool wiring (see module doc).
pub fn build_backend(role: &ResolvedRole, cfg: &EffectiveConfig) -> Box<dyn AgentBackend> {
    match role.backend {
        AgentBackendKind::Claude => Box::new(ClaudeBackend {
            command: cfg.claude.command.clone(),
            extra_args: cfg.claude.args.clone(),
            model: role.model.clone(),
            permission_mode: cfg.claude.permission_mode.clone(),
            turn_timeout_ms: cfg.claude.turn_timeout_ms,
            mcp_wiring: None,
            workflow_dir: cfg.workflow_dir.clone(),
        }),
        AgentBackendKind::OpenCode => Box::new(OpenCodeBackend {
            command: cfg.opencode.command.clone(),
            model: role.model.clone(),
            extra_args: cfg.opencode.args.clone(),
            auto_approve: cfg.opencode.auto_approve,
            turn_timeout_ms: cfg.opencode.turn_timeout_ms,
            mcp_wiring: None,
            workflow_dir: cfg.workflow_dir.clone(),
        }),
        AgentBackendKind::Codex => Box::new(CodexBackend {
            command: cfg.codex.command.clone(),
            approval_policy: cfg.codex.approval_policy.clone(),
            thread_sandbox: cfg.codex.thread_sandbox.clone(),
            turn_sandbox_policy: cfg.codex.turn_sandbox_policy.clone(),
            turn_timeout_ms: cfg.codex.turn_timeout_ms,
            read_timeout_ms: cfg.codex.read_timeout_ms,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Issue;
    use serde_json::json;

    fn test_cfg(yaml_extra: &str) -> EffectiveConfig {
        let yaml: serde_yaml::Value = serde_yaml::from_str(&format!(
            "tracker:\n  kind: local\n{yaml_extra}"
        ))
        .unwrap();
        crate::config::resolve(&yaml, std::path::Path::new(".")).unwrap()
    }

    #[test]
    fn built_in_role_resolves_with_no_roles_config_at_all() {
        let cfg = test_cfg("");
        let role = resolve("reviewer", &cfg).unwrap();
        assert!(role.prompt.contains("Reviewer agent"));
        assert_eq!(role.backend, AgentBackendKind::Claude); // cfg.agent_backend default
        assert!(!role.overrides_backend);
        // AIR-7: the built-in Reviewer role defaults to a restricted tool policy (no
        // `roles.reviewer` override needed) -- "a Reviewer that can edit files is not
        // a reviewer."
        assert_eq!(role.tool_policy, ToolPolicy::SWEBOT);
    }

    #[test]
    fn built_in_developer_role_still_defaults_to_an_unrestricted_tool_policy() {
        let cfg = test_cfg("");
        let role = resolve("developer", &cfg).unwrap();
        assert_eq!(role.tool_policy, ToolPolicy::default());
    }

    #[test]
    fn project_role_prompt_overrides_the_built_in_one() {
        let cfg = test_cfg(
            "roles:\n  reviewer:\n    prompt: \"Custom reviewer prompt for {{ issue.identifier }}\"\n",
        );
        let role = resolve("reviewer", &cfg).unwrap();
        assert_eq!(role.prompt, "Custom reviewer prompt for {{ issue.identifier }}");
    }

    #[test]
    fn project_role_backend_and_model_override_agent_defaults() {
        let cfg = test_cfg(
            "agent:\n  backend: claude\nroles:\n  reviewer:\n    backend: opencode\n    \
             model: fireworks/some-model\n",
        );
        let role = resolve("reviewer", &cfg).unwrap();
        assert_eq!(role.backend, AgentBackendKind::OpenCode);
        assert_eq!(role.model.as_deref(), Some("fireworks/some-model"));
        assert!(role.overrides_backend);
    }

    #[test]
    fn tools_allow_edits_false_produces_a_restricted_tool_policy() {
        let cfg = test_cfg("roles:\n  reviewer:\n    tools:\n      allow_edits: false\n");
        let role = resolve("reviewer", &cfg).unwrap();
        assert!(!role.tool_policy.allow_edits);
        assert!(role.tool_policy.allow_commands);
    }

    #[test]
    fn unknown_non_built_in_role_is_an_error() {
        // Bypasses `config::resolve`'s own stage-role validation by calling `resolve`
        // directly with a name no `roles:` entry defines and no built-in matches.
        let cfg = test_cfg("");
        assert!(resolve("not-a-real-role", &cfg).is_err());
    }

    /// AIR-2 acceptance criterion: "A stage can select a different backend/model from
    /// `agent.backend` and it is actually used" -- asserted here on the constructed
    /// backend's own fields, the same way `swebot`'s tests assert on
    /// `build_restricted_backend`'s output (those fields are what `start_session`
    /// turns into the real command line -- see `agent::opencode`'s own `run_args`
    /// tests for that translation).
    #[test]
    fn build_backend_for_an_overriding_role_uses_the_roles_own_backend_and_model() {
        let cfg = test_cfg(
            "agent:\n  backend: claude\nclaude:\n  model: sonnet\nroles:\n  reviewer:\n    \
             backend: opencode\n    model: fireworks/kimi\n",
        );
        let role = resolve("reviewer", &cfg).unwrap();
        assert!(role.overrides_backend);
        let backend = build_backend(&role, &cfg);
        let oc = backend
            .as_any()
            .and_then(|a| a.downcast_ref::<OpenCodeBackend>())
            .expect("role overriding backend to opencode should build an OpenCodeBackend");
        assert_eq!(oc.model.as_deref(), Some("fireworks/kimi"));
        assert_eq!(oc.command, cfg.opencode.command);
    }

    #[test]
    fn build_backend_for_a_non_overriding_role_still_matches_agent_backend() {
        let cfg = test_cfg("agent:\n  backend: claude\nclaude:\n  model: sonnet\n");
        let role = resolve("developer", &cfg).unwrap();
        let backend = build_backend(&role, &cfg);
        let cb = backend
            .as_any()
            .and_then(|a| a.downcast_ref::<ClaudeBackend>())
            .expect("default agent.backend is claude");
        assert_eq!(cb.model.as_deref(), Some("sonnet"));
    }

    /// AIR-2 acceptance criterion: every built-in role prompt renders under
    /// `template.rs`'s strict mode with the documented `cycle.*` variables (id, stage,
    /// artifacts, previous_stage_summary) alongside the same `issue.*` context the
    /// main prompt already gets.
    #[test]
    fn every_built_in_role_prompt_renders_with_issue_and_cycle_context() {
        let issue = Issue {
            id: "1".to_string(),
            native_ref: None,
            identifier: "AIR-2".to_string(),
            title: "Role definitions".to_string(),
            description: Some("do the thing".to_string()),
            priority: Some(1),
            state: "todo".to_string(),
            branch_name: None,
            url: None,
            assignee_id: None,
            labels: vec!["config".to_string()],
            blocked_by: Vec::new(),
            dispatchable: true,
            created_at: None,
            updated_at: None,
        };
        let ctx = json!({
            "issue": serde_json::to_value(&issue).unwrap(),
            "attempt": Some(1),
            "cycle": {
                "id": "AIR-2-1",
                "stage": "review",
                "artifacts": {},
                "previous_stage_summary": "implement: completed",
            },
        });
        for name in builtin::ROLE_NAMES {
            let template = builtin::prompt_for(name).unwrap();
            let rendered = crate::template::render(template, &ctx)
                .unwrap_or_else(|e| panic!("built-in role '{name}' failed to render: {e}"));
            assert!(!rendered.trim().is_empty());
        }
    }
}
