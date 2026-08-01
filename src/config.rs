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
    #[error(
        "invalid_config: workspace.docker.image is required when workspace.docker.enabled is true"
    )]
    MissingDockerImage,
    #[error(
        "invalid_config: repo.token must be a $VAR_NAME reference (naming an env var), not a literal value"
    )]
    InvalidRepoToken,
    #[error(
        "invalid_config: claude.api_key must be a $VAR_NAME reference (naming an env var), not a literal value"
    )]
    InvalidClaudeApiKey,
    #[error("invalid_config: repo.url is required when the repo block is present")]
    MissingRepoUrl,
    #[error(
        "invalid_config: workspace.docker.enabled is not supported with agent.backend=codex yet \
         (Docker mode only runs the Claude backend's turns inside the container -- Codex always \
         runs on the host regardless -- so enabling both silently doesn't do what it looks like)"
    )]
    DockerNotSupportedForCodex,
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
    /// Name of an env var holding an Anthropic API key (no leading `$`), e.g.
    /// `ANTHROPIC_API_KEY`. An alternative to `workspace.docker.mount_claude_credentials`
    /// for authenticating the `claude` CLI in Docker mode: some hosts (Windows in
    /// particular) don't keep a portable session in `~/.claude/.credentials.json` at
    /// all, so there's nothing to mount -- an API key sidesteps that entirely. Named,
    /// not resolved to a value here, for the same reason as `RepoConfig::token_env`:
    /// referencing it by name is enough for `envsub::collect_var_refs` to pick it up
    /// and forward it into containers via `docker run -e`, without Symphony itself
    /// ever needing to hold or embed the actual key value. Not read anywhere beyond
    /// `resolve()`'s own validation (that it's a `$VAR_NAME` reference, not a literal)
    /// -- `collect_var_refs` walks the raw config tree directly, so this field's real
    /// job is documenting and validating the convention, not driving further logic.
    #[allow(dead_code)]
    pub api_key_env: Option<String>,
}

/// Extension: run workspace hooks and the coding agent inside a per-ticket Docker
/// container (bind-mounted to `workflow_dir`) instead of directly on the host. See
/// README.md "Docker mode" -- this exists specifically to eliminate the class of bug
/// where the hook's shell (WSL) and the agent's own shell (Git Bash/MSYS) disagree
/// about how to spell a Windows path for the same directory.
#[derive(Debug, Clone)]
pub struct DockerConfig {
    pub enabled: bool,
    pub image: Option<String>,
    pub network: String,
    pub mem_limit: Option<String>,
    pub cpus: Option<String>,
    /// `docker run --user` value, e.g. `"1000:1000"`. `None` runs as the image's own
    /// default (root, unless the image itself sets `USER`). Needed for the Claude
    /// backend specifically: `claude` refuses `bypassPermissions` when running as
    /// root -- see the Symphony base `Dockerfile`'s own doc comment on this.
    pub user: Option<String>,
    /// Bind-mount the host's own Claude Code login (`~/.claude/.credentials.json`)
    /// read-only into each per-ticket container, so the containerized `claude` CLI
    /// reuses the host's existing session instead of needing its own separate API
    /// key. Off by default: every container that runs gets read access to this file
    /// while it's enabled, which is a real trust concession worth opting into
    /// deliberately, not defaulting on.
    pub mount_claude_credentials: bool,
}

/// Extension: git-repo-as-first-class-input (see README.md "Git repo as first-class
/// input"). When set, and a project hasn't supplied its own `hooks.after_create` /
/// `before_run` / `after_run`, `resolve()` synthesizes the clone/pull/commit-push
/// sequence a project would otherwise have to hand-write (as bsky-archiver's
/// `WORKFLOW.md` originally did) -- see `synthesize_repo_hooks` below. An explicit
/// `hooks.*` entry always wins over the synthesized default.
#[derive(Debug, Clone)]
pub struct RepoConfig {
    pub url: String,
    pub default_branch: String,
    /// Name of an env var holding the git credential (no leading `$`), e.g.
    /// `GITHUB_TOKEN`. Deliberately *not* resolved to its value here: the synthesized
    /// hook script references the var by name in a generated `git config
    /// credential.helper`, so the secret value only ever needs to exist in the hook's
    /// own process environment (which it already inherits from Symphony's), never as
    /// a value Symphony itself holds or embeds in a URL/script body.
    pub token_env: Option<String>,
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
    pub docker: DockerConfig,
    /// Only consumed by hook synthesis (`synthesize_repo_hooks`) so far -- kept on the
    /// resolved config too since daemonized Symphony (Docker-outside-of-Docker mode)
    /// will also need `repo.url` directly to know what to clone into its own
    /// named-volume mount at startup.
    #[allow(dead_code)]
    pub repo: Option<RepoConfig>,

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
    get(v, key)
        .cloned()
        .unwrap_or(Value::Mapping(Default::default()))
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

    let workspace_root_raw = get_str(workspace, "root").unwrap_or_else(default_workspace_root);
    let workspace_root = envsub::resolve_path(&workspace_root_raw, workflow_dir);

    let docker = get(workspace, "docker").unwrap_or(&empty);
    let docker_cfg = DockerConfig {
        enabled: get(docker, "enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        image: get_str(docker, "image"),
        network: get_str(docker, "network").unwrap_or_else(|| "bridge".to_string()),
        mem_limit: get_str(docker, "mem_limit"),
        cpus: get_str(docker, "cpus"),
        user: get_str(docker, "user"),
        mount_claude_credentials: get(docker, "mount_claude_credentials")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    };

    let hook_timeout_ms = get_u64(hooks, "timeout_ms", 60_000);
    if hook_timeout_ms == 0 {
        return Err(ConfigError::InvalidHookTimeout);
    }

    let repo_cfg = get(config, "repo")
        .map(|r| -> Result<RepoConfig, ConfigError> {
            let url = get_str(r, "url").ok_or(ConfigError::MissingRepoUrl)?;
            let default_branch = get_str(r, "default_branch").unwrap_or_else(|| "main".to_string());
            let token_env = get_str(r, "token")
                .map(|t| {
                    envsub::var_name_of(&t)
                        .map(|s| s.to_string())
                        .ok_or(ConfigError::InvalidRepoToken)
                })
                .transpose()?;
            Ok(RepoConfig {
                url,
                default_branch,
                token_env,
            })
        })
        .transpose()?;

    // An explicit `hooks.*` entry always wins, per-hook (not all-or-nothing) -- a
    // project can override just one of the three and still get the synthesized
    // defaults for the others.
    let (synth_after_create, synth_before_run, synth_after_run) = match &repo_cfg {
        Some(repo) => {
            let (c, b, a) = synthesize_repo_hooks(repo);
            (Some(c), Some(b), Some(a))
        }
        None => (None, None, None),
    };
    let hook_after_create = get_str(hooks, "after_create").or(synth_after_create);
    let hook_before_run = get_str(hooks, "before_run").or(synth_before_run);
    let hook_after_run = get_str(hooks, "after_run").or(synth_after_run);

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
                    if val > 0 {
                        Some((key, val as u32))
                    } else {
                        None
                    }
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
        api_key_env: get_str(claude, "api_key")
            .map(|k| {
                envsub::var_name_of(&k)
                    .map(|s| s.to_string())
                    .ok_or(ConfigError::InvalidClaudeApiKey)
            })
            .transpose()?,
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
        docker: docker_cfg,
        repo: repo_cfg,

        hook_after_create,
        hook_before_run,
        hook_after_run,
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

/// Synthesize `(after_create, before_run, after_run)` hook scripts for `repo` --
/// always a per-ticket branch (`issue-$name`, `$name` = the workspace directory name,
/// i.e. the sanitized identifier: see `workspace::derive_workspace_key`), never
/// pushed straight to `default_branch`. A generic daemon can't know in advance
/// whether tickets are safely sequential or genuinely parallel the way a
/// hand-authored WORKFLOW.md can (see bsky-archiver's own hooks, which *did* push
/// directly to `main` for its known-sequential chain) -- always branching is the
/// conservative default that's safe either way; merging back is left to the operator
/// or a future PR-automation feature, not this synthesis.
///
/// Mirrors every hard-won lesson from this session's hand-written hooks: a loud
/// `exit 1` on push failure (never `|| true` on that specific line -- a failed push
/// must not report success), and an `is-inside-work-tree` guard so a silently-failed
/// `after_create` fails loudly on the next hook instead of quietly no-op'ing.
///
/// Credential handling: `-c credential.helper=...` scoped to just the `clone`
/// invocation (no `.git` exists yet to scope a repo-local config value to), then
/// persisted as a repo-local (not `--global`) config value so `before_run`/`after_run`
/// -- separate hook invocations against the same already-cloned workspace -- pick it
/// up automatically. The helper script references the credential by *env var name*,
/// never embeds the resolved secret value in the generated script text.
fn synthesize_repo_hooks(repo: &RepoConfig) -> (String, String, String) {
    let branch = &repo.default_branch;
    let url = &repo.url;

    // Repo-local (not --global) identity for the automatic after_run commit -- without
    // this, `git commit` fails outright with "Author identity unknown". That failure
    // was previously swallowed by the `|| true` on the commit line in after_run, so it
    // looked "harmless" in isolation, but it meant *nothing the safety-net hook does*
    // ever actually got committed, silently, on every single run.
    let identity = "git config user.email \"symphony@local\"\n\
        git config user.name \"Symphony Agent\"\n";

    // In Docker mode this directory can end up owned by a different uid than the one
    // that clones into it (the daemon's own orchestrator process, root when
    // daemonized, creates it; a per-ticket container running as `workspace.docker.user`
    // does the actual clone -- see workspace.rs's `chmod_permissive`). Even once
    // permission bits allow the write, git's own "dubious ownership" protection
    // (2.35.2+) still refuses to operate on a directory it doesn't own unless told
    // it's trusted. This has to run *before* `git clone`, not after: clone itself
    // starts using the freshly-initialized `.git` the moment it creates it.
    let trust_dir = "git config --global --add safe.directory \"$PWD\"\n";

    let after_create = match &repo.token_env {
        Some(var) => format!(
            "name=\"$(basename \"$PWD\")\"\n\
             {trust_dir}\
             cred_helper='!f() {{ echo username=x-access-token; echo \"password=${var}\"; }}; f'\n\
             git -c credential.helper=\"$cred_helper\" clone \"{url}\" .\n\
             git config credential.helper \"$cred_helper\"\n\
             {identity}\
             git checkout -b \"issue-$name\" \"origin/{branch}\"\n"
        ),
        None => format!(
            "name=\"$(basename \"$PWD\")\"\n\
             {trust_dir}\
             git clone \"{url}\" .\n\
             {identity}\
             git checkout -b \"issue-$name\" \"origin/{branch}\"\n"
        ),
    };

    let before_run = "name=\"$(basename \"$PWD\")\"\n\
        if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then\n\
        \x20\x20echo \"FATAL: workspace is not a git repository (after_create must have failed silently)\" >&2\n\
        \x20\x20exit 1\n\
        fi\n\
        git pull --ff-only origin \"issue-$name\" || true\n"
        .to_string();

    let after_run = "name=\"$(basename \"$PWD\")\"\n\
        if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then\n\
        \x20\x20echo \"FATAL: workspace is not a git repository (after_create must have failed silently)\" >&2\n\
        \x20\x20exit 1\n\
        fi\n\
        git add -A\n\
        git commit -m \"symphony: $name\" -q --allow-empty-message || true\n\
        if ! git push origin \"HEAD:refs/heads/issue-$name\" -q; then\n\
        \x20\x20echo \"FATAL: git push failed -- this attempt's work did NOT reach the shared repo\" >&2\n\
        \x20\x20exit 1\n\
        fi\n"
        .to_string();

    (after_create, before_run, after_run)
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
        return Err(ConfigError::UnsupportedTrackerKind(
            cfg.tracker_kind.clone(),
        ));
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
    if cfg.docker.enabled {
        if cfg.docker.image.as_deref().unwrap_or("").trim().is_empty() {
            return Err(ConfigError::MissingDockerImage);
        }
        if cfg.agent_backend == AgentBackendKind::Codex {
            return Err(ConfigError::DockerNotSupportedForCodex);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// `C:\Users\x` -> `/mnt/c/Users/x`, for handing a local path to WSL's `git` in
    /// tests (see `synthesized_hooks_actually_clone_branch_commit_and_push`).
    fn to_wsl_path(p: &Path) -> String {
        let s = p.to_string_lossy().replace('\\', "/");
        if let Some(drive) = s.chars().next()
            && s.as_bytes().get(1) == Some(&b':')
        {
            format!("/mnt/{}{}", drive.to_ascii_lowercase(), &s[2..])
        } else {
            s
        }
    }

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
        assert_eq!(
            cfg.concurrency_limit_for_state("bad"),
            cfg.max_concurrent_agents
        );
        assert_eq!(
            cfg.concurrency_limit_for_state("also_bad"),
            cfg.max_concurrent_agents
        );
    }

    #[test]
    fn repo_absent_by_default_and_hooks_stay_unset() {
        let cfg_yaml = parse_yaml("tracker:\n  kind: local\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(cfg.repo.is_none());
        assert!(cfg.hook_after_create.is_none());
        assert!(cfg.hook_before_run.is_none());
        assert!(cfg.hook_after_run.is_none());
    }

    #[test]
    fn repo_synthesizes_all_three_hooks_with_credential_helper() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             default_branch: main\n  token: $GITHUB_TOKEN\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        let create = cfg.hook_after_create.unwrap();
        assert!(create.contains("clone \"https://github.com/o/r.git\""));
        assert!(create.contains("origin/main"));
        assert!(create.contains("$GITHUB_TOKEN"));
        assert!(create.contains("credential.helper"));
        assert!(create.contains("git config user.email"));
        assert!(create.contains("git config user.name"));
        assert!(create.contains("safe.directory"));
        assert!(create.find("safe.directory").unwrap() < create.find("clone").unwrap());

        let before = cfg.hook_before_run.unwrap();
        assert!(before.contains("is-inside-work-tree"));
        assert!(before.contains("git pull --ff-only origin \"issue-$name\""));

        let after = cfg.hook_after_run.unwrap();
        assert!(after.contains("git push origin \"HEAD:refs/heads/issue-$name\" -q"));
        assert!(after.contains("is-inside-work-tree"));
        // FATAL guard comes before the push, not after.
        assert!(after.find("is-inside-work-tree").unwrap() < after.find("git push").unwrap());
    }

    #[test]
    fn repo_without_token_synthesizes_hooks_with_no_credential_helper() {
        let cfg_yaml =
            parse_yaml("tracker:\n  kind: local\nrepo:\n  url: https://example.com/r.git\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        let create = cfg.hook_after_create.unwrap();
        assert!(!create.contains("credential.helper"));
        assert!(create.contains("origin/main")); // default_branch defaults to "main"
    }

    #[test]
    fn explicit_hook_wins_over_synthesized_default_per_hook() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n\
             hooks:\n  before_run: echo custom\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        // before_run explicitly overridden...
        assert_eq!(cfg.hook_before_run.as_deref(), Some("echo custom"));
        // ...but after_create/after_run still get synthesized since they weren't set.
        assert!(cfg.hook_after_create.unwrap().contains("git clone"));
        assert!(cfg.hook_after_run.unwrap().contains("git push"));
    }

    #[test]
    fn repo_token_must_be_var_reference_not_a_literal() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  token: not-a-var\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::InvalidRepoToken)
        ));
    }

    #[test]
    fn claude_api_key_must_be_var_reference_not_a_literal() {
        let cfg_yaml =
            parse_yaml("tracker:\n  kind: local\nclaude:\n  api_key: sk-not-a-var-reference\n");
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::InvalidClaudeApiKey)
        ));
    }

    #[test]
    fn claude_api_key_var_reference_resolves_and_is_collected_for_passthrough() {
        let cfg_yaml =
            parse_yaml("tracker:\n  kind: local\nclaude:\n  api_key: $ANTHROPIC_API_KEY\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.claude.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(
            envsub::collect_var_refs(&cfg_yaml).contains(&"ANTHROPIC_API_KEY".to_string()),
            "collect_var_refs must pick up claude.api_key so it gets forwarded into \
             Docker-mode containers via env_passthrough"
        );
    }

    /// End-to-end: actually *run* the synthesized hooks (via `hooks::run_hook`, the
    /// same execution path the orchestrator uses) against a real local git repo --
    /// string-content assertions above prove the scripts *look* right, this proves
    /// they *work*: clone, branch, commit+push, pull-into-existing-clone all actually
    /// succeed as real git operations, not just plausible-looking bash.
    #[tokio::test]
    async fn synthesized_hooks_actually_clone_branch_commit_and_push() {
        // Isolate from any real global/system git config (e.g. this machine's own
        // `user.email`/`user.name`, set for this session's own commits) so the
        // synthesized hooks' *own* `git config user.email`/`user.name` lines are what
        // actually get exercised here, not silently masked by ambient config the way
        // they were the first time this test was written -- it passed then too, but
        // only because a real identity happened to already be configured locally; a
        // genuinely fresh container (no ~/.gitconfig at all) had no such fallback and
        // hit "Author identity unknown" for real. `git` respects these two env vars
        // (2.32+) to redirect where it looks, without touching $HOME itself.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        }

        let origin = tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(origin.path())
            .status()
            .unwrap();
        std::fs::write(origin.path().join("README.md"), "hello\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(origin.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "seed",
            ])
            .current_dir(origin.path())
            .status()
            .unwrap();
        // Accept a push into the currently checked-out branch (this "origin" repo is
        // acting as a bare-ish shared remote for the test, mirroring how the real
        // mainline `app/` repo is configured -- see workspace.rs's Docker-mode test
        // for the same pattern).
        std::process::Command::new("git")
            .args(["config", "receive.denyCurrentBranch", "updateInstead"])
            .current_dir(origin.path())
            .status()
            .unwrap();

        // hooks.rs's `bash` resolves to WSL on this machine (see hooks.rs's own module
        // doc comment) -- WSL's git has no concept of Windows drive letters, so the
        // clone source has to be spelled as a `/mnt/c/...` path, not the Windows-style
        // path `tempdir()` returns (see README.md's "ssh: Could not resolve hostname
        // c" note; this is the exact same class of bug, just hit by this test's setup
        // rather than by a real `https://` `repo.url`, which has no such ambiguity).
        let origin_wsl_path = to_wsl_path(origin.path());
        let cfg_yaml = parse_yaml(&format!(
            "tracker:\n  kind: local\nrepo:\n  url: {origin_wsl_path:?}\n  default_branch: main\n"
        ));
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();

        let ws_named = origin.path().parent().unwrap().join("42");
        std::fs::create_dir_all(&ws_named).unwrap();

        crate::hooks::run_hook(
            "after_create",
            cfg.hook_after_create.as_deref().unwrap(),
            &ws_named,
            15_000,
        )
        .await
        .unwrap();
        assert!(ws_named.join("README.md").exists());

        std::fs::write(ws_named.join("new_file.txt"), "work\n").unwrap();

        crate::hooks::run_hook(
            "after_run",
            cfg.hook_after_run.as_deref().unwrap(),
            &ws_named,
            15_000,
        )
        .await
        .unwrap();

        crate::hooks::run_hook(
            "before_run",
            cfg.hook_before_run.as_deref().unwrap(),
            &ws_named,
            15_000,
        )
        .await
        .unwrap();

        // The push in after_run must have actually reached "origin" on the per-ticket
        // branch (not "main" -- updateInstead only refreshes the working tree for
        // pushes to the *currently checked-out* branch, which stays "main" here, so
        // check the pushed branch's own content directly instead).
        let show = std::process::Command::new("git")
            .args(["show", "issue-42:new_file.txt"])
            .current_dir(origin.path())
            .output()
            .unwrap();
        assert!(
            show.status.success(),
            "after_run's push should have created branch issue-42 on origin with new_file.txt: {}",
            String::from_utf8_lossy(&show.stderr)
        );

        let _ = std::fs::remove_dir_all(&ws_named);
        unsafe {
            std::env::remove_var("GIT_CONFIG_GLOBAL");
            std::env::remove_var("GIT_CONFIG_SYSTEM");
        }
    }

    #[test]
    fn repo_missing_url_errors() {
        let cfg_yaml = parse_yaml("tracker:\n  kind: local\nrepo:\n  default_branch: main\n");
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::MissingRepoUrl)
        ));
    }

    #[test]
    fn docker_disabled_by_default() {
        let cfg_yaml = parse_yaml("tracker:\n  kind: local\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(!cfg.docker.enabled);
        assert_eq!(cfg.docker.network, "bridge");
    }

    #[test]
    fn docker_block_parses() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nworkspace:\n  docker:\n    enabled: true\n    image: my-agent:latest\n    network: none\n    mem_limit: 4g\n    cpus: \"2\"\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(cfg.docker.enabled);
        assert_eq!(cfg.docker.image.as_deref(), Some("my-agent:latest"));
        assert_eq!(cfg.docker.network, "none");
        assert_eq!(cfg.docker.mem_limit.as_deref(), Some("4g"));
        assert_eq!(cfg.docker.cpus.as_deref(), Some("2"));
    }

    #[test]
    fn validate_rejects_docker_enabled_without_image() {
        let cfg_yaml =
            parse_yaml("tracker:\n  kind: local\nworkspace:\n  docker:\n    enabled: true\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(matches!(
            validate_for_dispatch(&cfg, &["local"]),
            Err(ConfigError::MissingDockerImage)
        ));
    }

    #[test]
    fn validate_rejects_docker_enabled_with_codex_backend() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nagent:\n  backend: codex\nworkspace:\n  docker:\n    \
             enabled: true\n    image: some-image:latest\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(matches!(
            validate_for_dispatch(&cfg, &["local"]),
            Err(ConfigError::DockerNotSupportedForCodex)
        ));
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
