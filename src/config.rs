//! Typed configuration layer (Section 6): defaults, `$VAR` resolution, validation.

use crate::envsub;
use serde_yaml::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid_tracker_config: tracker.kind is required")]
    MissingTrackerKind,
    #[error("unsupported_tracker_kind: '{0}' is not a supported adapter")]
    UnsupportedTrackerKind(String),
    #[error("invalid_config: agent.max_turns must be a positive integer")]
    InvalidMaxTurns,
    #[error("invalid_config: hooks.timeout_ms must be a positive integer")]
    InvalidHookTimeout,
    #[error("invalid_config: {backend}.command must be present and non-empty")]
    MissingAgentCommand { backend: String },
}

/// Extension: which coding-agent backend implementation to launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentBackendKind {
    Claude,
    Codex,
}

impl AgentBackendKind {
    fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "codex" => AgentBackendKind::Codex,
            _ => AgentBackendKind::Claude,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CodexConfig {
    pub command: String,
    pub approval_policy: Option<String>,
    pub thread_sandbox: Option<String>,
    pub turn_sandbox_policy: Option<String>,
    pub turn_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub stall_timeout_ms: i64,
}

#[derive(Debug, Clone)]
pub struct ClaudeConfig {
    pub command: String,
    pub args: Vec<String>,
    pub model: Option<String>,
    /// Passed as `--permission-mode`. High-trust default: `bypassPermissions` — note
    /// `acceptEdits` only covers file-edit tools (Write/Edit/NotebookEdit), NOT
    /// Bash/PowerShell command execution; in a headless run with nobody to approve
    /// anything interactively, that leaves every shell command auto-denied.
    pub permission_mode: String,
    pub turn_timeout_ms: u64,
    pub stall_timeout_ms: i64,
}

#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub tracker_kind: String,
    pub tracker_provider: Value,
    /// Directory containing the loaded `WORKFLOW.md`, preserved so a backend can
    /// re-resolve tracker-relative paths (e.g. to rebuild the same adapter for the MCP
    /// tool-server subprocess; Section 10.5).
    pub workflow_dir: PathBuf,
    pub required_labels: Vec<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,

    pub poll_interval_ms: u64,

    pub workspace_root: PathBuf,

    pub hook_after_create: Option<String>,
    pub hook_before_run: Option<String>,
    pub hook_after_run: Option<String>,
    pub hook_before_remove: Option<String>,
    pub hook_timeout_ms: u64,

    pub max_concurrent_agents: u32,
    pub max_turns: u32,
    pub max_retry_backoff_ms: u64,
    pub max_concurrent_agents_by_state: HashMap<String, u32>,
    pub agent_backend: AgentBackendKind,

    pub codex: CodexConfig,
    pub claude: ClaudeConfig,
}

impl EffectiveConfig {
    pub fn effective_command(&self) -> &str {
        match self.agent_backend {
            AgentBackendKind::Claude => &self.claude.command,
            AgentBackendKind::Codex => &self.codex.command,
        }
    }

    pub fn effective_stall_timeout_ms(&self) -> i64 {
        match self.agent_backend {
            AgentBackendKind::Claude => self.claude.stall_timeout_ms,
            AgentBackendKind::Codex => self.codex.stall_timeout_ms,
        }
    }

    /// Per-state concurrency limit, falling back to the global limit (Section 8.3).
    pub fn concurrency_limit_for_state(&self, state: &str) -> u32 {
        let key = state.trim().to_lowercase();
        *self
            .max_concurrent_agents_by_state
            .get(&key)
            .unwrap_or(&self.max_concurrent_agents)
    }
}

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key)
}

fn get_str(v: &Value, key: &str) -> Option<String> {
    get(v, key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn get_u64(v: &Value, key: &str, default: u64) -> u64 {
    get(v, key).and_then(|x| x.as_u64()).unwrap_or(default)
}

fn get_i64(v: &Value, key: &str, default: i64) -> i64 {
    get(v, key).and_then(|x| x.as_i64()).unwrap_or(default)
}

fn get_vec_str(v: &Value, key: &str) -> Vec<String> {
    get(v, key)
        .and_then(|x| x.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn get_map(v: &Value, key: &str) -> Value {
    get(v, key).cloned().unwrap_or(Value::Mapping(Default::default()))
}

/// Resolve typed effective config from raw front matter (Section 6.1-6.4).
///
/// `workflow_dir` is the directory containing the loaded `WORKFLOW.md`, used to resolve
/// relative paths (e.g. `workspace.root`).
pub fn resolve(config: &Value, workflow_dir: &Path) -> Result<EffectiveConfig, ConfigError> {
    let empty = Value::Mapping(Default::default());
    let tracker = get(config, "tracker").unwrap_or(&empty);
    let polling = get(config, "polling").unwrap_or(&empty);
    let workspace = get(config, "workspace").unwrap_or(&empty);
    let hooks = get(config, "hooks").unwrap_or(&empty);
    let agent = get(config, "agent").unwrap_or(&empty);
    let codex = get(config, "codex").unwrap_or(&empty);
    let claude = get(config, "claude").unwrap_or(&empty);

    let tracker_kind = get_str(tracker, "kind").ok_or(ConfigError::MissingTrackerKind)?;

    let workspace_root_raw = get_str(workspace, "root")
        .unwrap_or_else(default_workspace_root);
    let workspace_root = envsub::resolve_path(&workspace_root_raw, workflow_dir);

    let hook_timeout_ms = get_u64(hooks, "timeout_ms", 60_000);
    if hook_timeout_ms == 0 {
        return Err(ConfigError::InvalidHookTimeout);
    }

    let max_turns = get_u64(agent, "max_turns", 20);
    if max_turns == 0 {
        return Err(ConfigError::InvalidMaxTurns);
    }

    let max_concurrent_agents_by_state = get(agent, "max_concurrent_agents_by_state")
        .and_then(|v| v.as_mapping())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let key = k.as_str()?.trim().to_lowercase();
                    let val = v.as_i64()?;
                    if val > 0 { Some((key, val as u32)) } else { None }
                })
                .collect()
        })
        .unwrap_or_default();

    let agent_backend = AgentBackendKind::parse(&get_str(agent, "backend").unwrap_or_default());

    let codex_cfg = CodexConfig {
        command: get_str(codex, "command").unwrap_or_else(|| "codex app-server".to_string()),
        approval_policy: get_str(codex, "approval_policy"),
        thread_sandbox: get_str(codex, "thread_sandbox"),
        turn_sandbox_policy: get_str(codex, "turn_sandbox_policy"),
        turn_timeout_ms: get_u64(codex, "turn_timeout_ms", 3_600_000),
        read_timeout_ms: get_u64(codex, "read_timeout_ms", 5_000),
        stall_timeout_ms: get_i64(codex, "stall_timeout_ms", 300_000),
    };

    let claude_cfg = ClaudeConfig {
        command: get_str(claude, "command").unwrap_or_else(|| "claude".to_string()),
        args: get_vec_str(claude, "args"),
        model: get_str(claude, "model"),
        permission_mode: get_str(claude, "permission_mode")
            .unwrap_or_else(|| "bypassPermissions".to_string()),
        turn_timeout_ms: get_u64(claude, "turn_timeout_ms", 3_600_000),
        stall_timeout_ms: get_i64(claude, "stall_timeout_ms", 300_000),
    };

    let cfg = EffectiveConfig {
        tracker_kind,
        tracker_provider: get_map(tracker, "provider"),
        workflow_dir: workflow_dir.to_path_buf(),
        required_labels: get_vec_str(tracker, "required_labels")
            .into_iter()
            .map(|s| s.trim().to_lowercase())
            .collect(),
        active_states: get_vec_str(tracker, "active_states"),
        terminal_states: get_vec_str(tracker, "terminal_states"),

        poll_interval_ms: get_u64(polling, "interval_ms", 30_000),

        workspace_root,

        hook_after_create: get_str(hooks, "after_create"),
        hook_before_run: get_str(hooks, "before_run"),
        hook_after_run: get_str(hooks, "after_run"),
        hook_before_remove: get_str(hooks, "before_remove"),
        hook_timeout_ms,

        max_concurrent_agents: get_u64(agent, "max_concurrent_agents", 10) as u32,
        max_turns: max_turns as u32,
        max_retry_backoff_ms: get_u64(agent, "max_retry_backoff_ms", 300_000),
        max_concurrent_agents_by_state,
        agent_backend,

        codex: codex_cfg,
        claude: claude_cfg,
    };

    Ok(cfg)
}

fn default_workspace_root() -> String {
    std::env::temp_dir()
        .join("symphony_workspaces")
        .to_string_lossy()
        .to_string()
}

/// Dispatch preflight validation (Section 6.3).
pub fn validate_for_dispatch(
    cfg: &EffectiveConfig,
    supported_tracker_kinds: &[&str],
) -> Result<(), ConfigError> {
    if !supported_tracker_kinds.contains(&cfg.tracker_kind.as_str()) {
        return Err(ConfigError::UnsupportedTrackerKind(cfg.tracker_kind.clone()));
    }
    let command = cfg.effective_command();
    if command.trim().is_empty() {
        let backend = match cfg.agent_backend {
            AgentBackendKind::Claude => "claude",
            AgentBackendKind::Codex => "codex",
        };
        return Err(ConfigError::MissingAgentCommand {
            backend: backend.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_yaml(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn defaults_apply_when_missing() {
        let cfg_yaml = parse_yaml("tracker:\n  kind: local\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.poll_interval_ms, 30_000);
        assert_eq!(cfg.max_concurrent_agents, 10);
        assert_eq!(cfg.max_turns, 20);
        assert_eq!(cfg.max_retry_backoff_ms, 300_000);
        assert_eq!(cfg.hook_timeout_ms, 60_000);
        assert_eq!(cfg.agent_backend, AgentBackendKind::Claude);
    }

    #[test]
    fn missing_tracker_kind_errors() {
        let cfg_yaml = parse_yaml("{}");
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::MissingTrackerKind)
        ));
    }

    #[test]
    fn per_state_concurrency_normalizes_and_ignores_invalid() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nagent:\n  max_concurrent_agents_by_state:\n    \" In Progress \": 3\n    bad: -1\n    also_bad: notanumber\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.concurrency_limit_for_state("in progress"), 3);
        assert_eq!(cfg.concurrency_limit_for_state("bad"), cfg.max_concurrent_agents);
        assert_eq!(cfg.concurrency_limit_for_state("also_bad"), cfg.max_concurrent_agents);
    }

    #[test]
    fn validate_rejects_unsupported_tracker_kind() {
        let cfg_yaml = parse_yaml("tracker:\n  kind: jira\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(matches!(
            validate_for_dispatch(&cfg, &["local"]),
            Err(ConfigError::UnsupportedTrackerKind(_))
        ));
    }
}
