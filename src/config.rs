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
    #[error(
        "invalid_config: opencode.api_key must be a $VAR_NAME reference (naming an env var), not a literal value"
    )]
    InvalidOpenCodeApiKey,
    #[error(
        "invalid_config: repo.pull_request requires repo.url to be a github.com URL (owner/name) -- open_pull_request only supports GitHub"
    )]
    PullRequestRequiresGithubRepo,
    #[error(
        "invalid_config: repo.pull_request with repo.provider: gitlab requires repo.url to be a \
         parseable GitLab repo URL (group/subgroup/name, HTTPS or SSH)"
    )]
    PullRequestRequiresGitlabRepo,
    #[error("invalid_config: repo.pull_request requires repo.token to be set")]
    PullRequestRequiresRepoToken,
    #[error(
        "invalid_config: repo.evidence requires repo.pull_request to be true -- evidence is \
         attached to a pull/merge request Symphony itself opens, so there has to be one"
    )]
    EvidenceRequiresPullRequest,
    #[error(
        "invalid_config: swebot.enabled with repo.provider: github (the default) requires repo.url to be a github.com URL (owner/name)"
    )]
    SwebotRequiresGithubRepo,
    #[error(
        "invalid_config: swebot.enabled with repo.provider: gitlab requires repo.url to be a \
         parseable GitLab repo URL (group/subgroup/name, HTTPS or SSH)"
    )]
    SwebotRequiresGitlabRepo,
    #[error(
        "invalid_config: repo.provider '{0}' is not a supported code host (expected 'github' or 'gitlab')"
    )]
    UnsupportedRepoProvider(String),
    #[error("invalid_config: swebot.enabled requires repo.token or swebot.token to be set")]
    SwebotRequiresRepoToken,
    #[error(
        "invalid_config: swebot.token must be a $VAR_NAME reference (naming an env var), not a literal value"
    )]
    InvalidSwebotToken,
    #[error("invalid_config: repo.url is required when the repo block is present")]
    MissingRepoUrl,
    #[error(
        "invalid_config: workspace.docker.enabled is not supported with agent.backend=codex yet \
         (Docker mode only runs the Claude backend's turns inside the container -- Codex always \
         runs on the host regardless -- so enabling both silently doesn't do what it looks like)"
    )]
    DockerNotSupportedForCodex,
    #[error(
        "invalid_config: swebot.chat.enabled requires swebot.enabled -- chat mode is a SweBot capability"
    )]
    ChatRequiresSwebot,
    #[error(
        "invalid_config: swebot.chat.connectors lists '{0}', which is not a known connector (known: {1})"
    )]
    UnknownChatConnector(String, String),
    #[error("invalid_config: pipeline.enabled is true but pipeline.stages is empty")]
    EmptyPipelineStages,
    #[error("invalid_config: pipeline.stages[{0}] is missing a non-empty 'id' or 'role' field")]
    InvalidPipelineStage(usize),
    #[error(
        "invalid_config: pipeline.stages[{0}] references role '{1}', which is neither a \
         built-in role (known: {2}) nor defined under roles.{1}"
    )]
    UnknownStageRole(usize, String, String),
    #[error("invalid_config: roles.{0}.prompt_file '{1}' could not be read: {2}")]
    UnreadableRolePromptFile(String, String, String),
}

/// AIR-5: `pipeline.approval.auto_approve_when` -- every condition set (non-`None`)
/// must hold for a `requires_approval` stage's output to be approved without a human.
/// Absent entirely (the `Default`), nothing ever auto-approves -- the roadmap's
/// autonomy measure is *reduced* human interventions, not zero governance.
#[derive(Debug, Clone, Default)]
pub struct AutoApproveWhen {
    /// Matched case-insensitively against the stage output's `risk` field. `None`
    /// (the plan didn't state a risk, or the stage's output couldn't be parsed as
    /// structured JSON) never satisfies this when set.
    pub risk: Option<String>,
    /// Every entry in the stage output's `impacted_components` must appear in this
    /// list (case-insensitive). An empty/absent `impacted_components` trivially
    /// satisfies this.
    pub impacted_components_allowlist: Option<Vec<String>>,
    /// The stage output's `estimate_turns` must be present and `<=` this.
    pub max_estimate_turns: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct ApprovalConfig {
    pub auto_approve_when: Option<AutoApproveWhen>,
}

/// Extension: which coding-agent backend implementation to launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentBackendKind {
    Claude,
    Codex,
    OpenCode,
}

impl AgentBackendKind {
    fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "codex" => AgentBackendKind::Codex,
            "opencode" => AgentBackendKind::OpenCode,
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

#[derive(Debug, Clone)]
pub struct OpenCodeConfig {
    pub command: String,
    pub args: Vec<String>,
    /// `provider/model` string passed to `--model`, e.g.
    /// `fireworks/accounts/fireworks/models/<model-id>`. `opencode` itself must already
    /// have that provider configured (see README.md "Coding-agent backends") --
    /// Symphony only names it, the same way `ClaudeConfig`/`CodexConfig` don't manage
    /// their own CLI's login either.
    pub model: Option<String>,
    /// Passed as `--auto` when true. Same high-trust rationale as `ClaudeConfig`'s
    /// `permission_mode: bypassPermissions`: a headless run has no human to approve
    /// tool calls interactively.
    pub auto_approve: bool,
    pub turn_timeout_ms: u64,
    pub stall_timeout_ms: i64,
    /// Name of an env var holding the API key for whichever provider `model` names
    /// (e.g. `FIREWORKS_API_KEY`). Mirrors `ClaudeConfig::api_key_env` exactly: named,
    /// not resolved to a value here, purely so `envsub::collect_var_refs` (which walks
    /// the raw config tree directly, not this typed struct) picks it up and forwards
    /// it into Docker-mode containers via `docker run -e`. `opencode` itself must
    /// already have a provider configured whose `apiKey` references this same env var
    /// name (see README.md "Coding-agent backends") -- this field's job is documenting
    /// and validating that convention, not driving further logic.
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
///
/// `Serialize`/`Deserialize`: when `pull_request` is true, `orchestrator::build_shared`
/// serializes this whole struct to pass to the `__mcp_tool_server` subprocess (see
/// `agent::claude::McpToolWiring::repo_pr_json`, mirroring how `tracker_provider_json`
/// already crosses that same boundary) -- the token *name*, never its resolved value,
/// so nothing secret crosses in the argv.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RepoConfig {
    pub url: String,
    /// Which code host `url` points at. Defaults to `Github` (`#[serde(default)]`) so
    /// every existing `repo:` block with no `provider:` key resolves exactly as it did
    /// before this field existed -- the crux of backward compatibility here.
    #[serde(default)]
    pub provider: RepoProvider,
    /// Explicit override for a self-managed GitLab instance's REST API root (must
    /// already include the `/api/v4` suffix -- nothing is appended onto an explicit
    /// value). Only meaningful when `provider: gitlab`; ignored for `Github`. When
    /// unset and `provider: gitlab`, `repo_host::gitlab` derives it from `url`'s own
    /// scheme+host, covering the common self-managed case (API reachable at the same
    /// host as the git remote) with zero extra config.
    #[serde(default)]
    pub api_base_url: Option<String>,
    pub default_branch: String,
    /// Name of an env var holding the git credential (no leading `$`), e.g.
    /// `GITHUB_TOKEN`. Deliberately *not* resolved to its value here: the synthesized
    /// hook script references the var by name in a generated `git config
    /// credential.helper`, so the secret value only ever needs to exist in the hook's
    /// own process environment (which it already inherits from Symphony's), never as
    /// a value Symphony itself holds or embeds in a URL/script body.
    pub token_env: Option<String>,
    /// Opt-in: expose the `open_pull_request` agent tool (`src/repo_host/mod.rs`) instead
    /// of leaving each ticket's pushed branch for a human to notice on their own. Off
    /// by default -- this changes what the agent does at the end of a turn (opens a
    /// real PR on the real repo) and how issues close (on merge, not immediately), so
    /// it's a real behavior change a project opts into deliberately, same posture as
    /// `workspace.docker.mount_claude_credentials`.
    pub pull_request: bool,
    /// Opt-in: expose the `attach_evidence` agent tool (`src/repo_host/mod.rs`), which
    /// uploads a screenshot (or other image) the agent already produced in its
    /// workspace to this repo and hands back a markdown image snippet the agent can
    /// paste into the `open_pull_request` body -- letting a reviewer see the working
    /// app instead of taking the agent's word for it. Requires `pull_request: true`
    /// (evidence is attached to a PR/MR Symphony itself opens; there's nothing to
    /// attach it to otherwise). Off by default, same posture as `pull_request` itself:
    /// this commits a real file to the real repo.
    #[serde(default)]
    pub evidence: bool,
}

/// Which code host `repo.url` points at (`repo.provider`, e.g. `provider: gitlab`).
/// `Default` -> `Github` is what makes an existing `repo:` block with no `provider:`
/// key resolve exactly as it did before this enum existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoProvider {
    #[default]
    Github,
    Gitlab,
}

impl RepoProvider {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "github" => Some(Self::Github),
            "gitlab" => Some(Self::Gitlab),
            _ => None,
        }
    }
}

/// Extension: SweBot (README.md "SweBot") -- answers questions and drafts tickets,
/// and reviews the pull/merge requests Symphony's coding agents open. Keyed off
/// `repo:` -- the same "which code host repo" source of truth `repo.pull_request`
/// already uses -- rather than `tracker.provider.repo`, so it works the same way
/// regardless of `tracker.kind`. Off by default, same deliberate-opt-in posture as
/// `repo.pull_request` and `workspace.docker.mount_claude_credentials`: this posts
/// real comments and reviews on a real repo.
///
/// The Q&A/drafting conversational surface is provider-specific: on GitHub it's a
/// Discussions category (`qa_discussion_category`/`drafting_discussion_category`),
/// since Discussions has no REST equivalent Symphony's other adapters could target.
/// GitLab has no Discussions object at all, so there SweBot instead polls Issues
/// carrying a specific label (`qa_label`/`drafting_label`) -- see
/// `repo_host::gitlab::GitlabRepoHost::list_swebot_threads`. Both pairs of fields
/// always coexist; `swebot::run` picks the right one for the configured
/// `repo.provider`.
#[derive(Debug, Clone)]
pub struct SwebotConfig {
    pub enabled: bool,
    /// Coding-agent backend SweBot's own sessions use. `None` follows `agent.backend`
    /// (the same backend ticket dispatch uses); set it to run SweBot on a different
    /// backend than tickets -- e.g. tickets on `claude`, SweBot on `opencode`
    /// (Fireworks). `codex` is not supported for SweBot yet and is rejected at
    /// SweBot startup rather than silently misbehaving (see `swebot::run`).
    pub backend: Option<AgentBackendKind>,
    /// GitHub: Discussion category name SweBot treats as incoming questions.
    pub qa_discussion_category: String,
    /// GitHub: Discussion category name SweBot treats as ticket ideas to draft.
    pub drafting_discussion_category: String,
    /// GitLab: issue label SweBot treats as incoming questions.
    pub qa_label: String,
    /// GitLab: issue label SweBot treats as ticket ideas to draft.
    pub drafting_label: String,
    /// Whether SweBot reviews the pull requests Symphony's own coding agents open
    /// (branch name matching `issue-<identifier>`, the same convention
    /// `synthesize_repo_hooks` produces). Independent toggle from `enabled` at large
    /// so a project can run Q&A/drafting without also turning on review, or vice
    /// versa, without a second top-level flag.
    pub review_enabled: bool,
    /// Name of an env var holding a *separate* GitHub credential for SweBot's own
    /// posts (Q&A replies, drafted issues, PR reviews), distinct from `repo.token`
    /// (the identity the coding agent pushes branches and opens PRs as -- "codebot"
    /// in the two-identity setup this exists for). Falls back to `repo.token_env`
    /// when unset, matching the old single-identity behavior. Giving SweBot its own
    /// identity matters specifically for `review.enabled`: GitHub's API rejects an
    /// `APPROVE`/`REQUEST_CHANGES` review from the same account that authored the
    /// pull request (422 "Can not approve your own pull request"), so if both roles
    /// share one token, SweBot silently can never approve the coding agent's own PRs.
    pub token_env: Option<String>,
    /// Chat mode: a unified Q&A + ticket-drafting conversation surface for SweBot,
    /// distinct from the polled GitHub Discussions loop (`qa`/`drafting` above).
    /// See README.md "SweBot chat mode".
    pub chat: SwebotChatConfig,
}

/// Known `swebot.chat.connectors` names. Single source of truth: config validation
/// rejects unknown names here, and `swebot::chat::connector` builds the registry by
/// this same list (each name maps to a constructable connector implementation).
pub const KNOWN_CHAT_CONNECTORS: &[&str] = &["web"];

#[derive(Debug, Clone)]
pub struct SwebotChatConfig {
    pub enabled: bool,
    /// Connectors to activate, by name (`KNOWN_CHAT_CONNECTORS`). `web` is the bundled
    /// chat UI served by the status dashboard. Future connectors (e.g. `teams`) plug
    /// in here; see `swebot::chat::connector::ChatConnector` for the contract.
    pub connectors: Vec<String>,
    /// How often the worker looks for pending user messages to answer -- purely
    /// local (a SQLite query), regardless of which connector a message came from.
    /// Independent of `polling.interval_ms` (ticket dispatch's cadence) -- chat is
    /// interactive and wants a snappier turn-around ("responds in a reasonable time,
    /// not instant"). Deliberately *not* the cadence remote connectors poll their own
    /// platform on -- see `remote_poll_interval_ms` for that.
    pub poll_interval_ms: u64,
    /// How often each remote connector's own `ingest`/`deliver` runs against its
    /// platform (e.g. GitHub Discussions' GraphQL API) -- separate from
    /// `poll_interval_ms` specifically so a fast, free-to-poll-often local answering
    /// cadence doesn't force an equally aggressive cadence against a rate-limited
    /// remote API. `web` has no remote platform to poll (`ingest`/`deliver` are
    /// no-ops there), so this only matters once a remote connector (`github` today)
    /// is active. Higher than `poll_interval_ms` by default: GitHub's GraphQL rate
    /// limit is easy to burn through polling every few seconds indefinitely.
    pub remote_poll_interval_ms: u64,
    /// How many pending user messages the worker answers per tick, across all
    /// conversations. 1-2 is plenty: one reply turn can take tens of seconds.
    pub max_concurrent_replies: u32,
    /// When SweBot finishes a draft, create the issue immediately (`true`, recommended
    /// default) rather than asking the user to confirm by replying "create it".
    pub auto_create_issue: bool,
    /// How much of a conversation's history feeds each prompt (newest N messages).
    /// Bounds prompt size -- the whole transcript is re-sent every turn; chat does not
    /// use `--resume` continuity (same reasoning as `drafting.rs`).
    pub max_history_messages: usize,
    /// Latency budget (ms) for the assistant's *first streaming text or tool call* to
    /// arrive before the worker posts a "still working" notice to the conversation
    /// (see `worker.rs`). The must-notify rule: a turn that will take longer than
    /// this must tell the user so immediately rather than leaving them staring at a
    /// silent input box. In practice this is only a fallback for a turn that neither
    /// calls a tool nor emits text right away -- the moment any tool call happens,
    /// its name becomes a live-updating status line regardless of this deadline.
    pub first_text_deadline_ms: u64,
}

/// Extension: the AI Roadmap 2026 delivery pipeline (AIR-1) -- run a ticket through an
/// ordered sequence of stages within one workspace instead of a single undifferentiated
/// agent run. Off by default (`enabled: false`, the zero value `Default` produces):
/// `orchestrator::run_attempt_body` then runs its pre-existing single-stage loop exactly
/// as it always has, byte-identical to before this extension existed.
#[derive(Debug, Clone, Default)]
pub struct PipelineConfig {
    pub enabled: bool,
    pub stages: Vec<StageConfig>,
    /// Tracker state a blocking stage's failure parks the issue in -- deliberately
    /// outside both `active_states` and `terminal_states` (a project's own
    /// responsibility to arrange, same convention `repo.pull_request`'s "in review"
    /// state already documents), so the orchestrator's existing eligibility checks
    /// simply stop selecting it for dispatch rather than needing new logic of their own.
    /// Also where AIR-5's "request changes reviewed" -- a rejected approval reuses
    /// this same parked state rather than adding a second one.
    pub blocked_state: String,
    /// AIR-5: tracker state a `requires_approval` stage's completion parks the issue
    /// in while a human (or `approval.auto_approve_when`) decides. Same "outside
    /// active/terminal states, orchestrator dispatch just stops selecting it"
    /// convention as `blocked_state`.
    pub awaiting_approval_state: String,
    pub approval: ApprovalConfig,
    /// How the project's tests actually run (AIR-6) -- `None` unless `pipeline.test` is
    /// configured, in which case a stage identified as `id: test` runs these suites
    /// through the hook plumbing instead of (in addition to) the agent's own turns.
    pub test: Option<TestConfig>,
}

/// `pipeline.test` (AIR-6): the project declares how its own suites and coverage tool
/// run, since Symphony has no built-in notion of "run the tests" for an arbitrary
/// language/toolchain.
#[derive(Debug, Clone)]
pub struct TestConfig {
    /// Suite name -> shell command, in declaration order (`commands:` is a mapping in
    /// YAML, but order still matters for a stable, readable `test_report`).
    pub commands: Vec<(String, String)>,
    pub coverage: Option<CoverageConfig>,
}

#[derive(Debug, Clone)]
pub struct CoverageConfig {
    pub command: String,
    pub format: crate::quality::CoverageFormat,
    /// Advisory unless the stage itself is `blocking: true` (`StageConfig::blocking`).
    pub min_line_percent: Option<f64>,
    /// Where the coverage command writes its report, relative to the workspace root.
    /// Defaults to a sensible filename per `format` when not given explicitly.
    pub path: String,
}

/// What a stage's failure (a turn erroring out, not a judgement about work quality --
/// no exit-criteria evaluation exists yet, that lands with roles/artifacts in AIR-2/AIR-3)
/// does to the rest of the cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageFailureAction {
    /// Stop the cycle. A `blocking` stage parks the issue in `pipeline.blocked_state`;
    /// a non-blocking one falls back to the orchestrator's existing whole-attempt retry
    /// backoff, the same path any turn failure already takes today.
    Escalate,
    /// Re-run the stage's own turn budget once more before falling back to `Escalate`'s
    /// behavior -- a bounded retry, not unbounded looping.
    Retry,
    /// Record the failure and move on to the next stage anyway.
    Skip,
}

impl StageFailureAction {
    fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "retry" => StageFailureAction::Retry,
            "skip" => StageFailureAction::Skip,
            _ => StageFailureAction::Escalate,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StageConfig {
    pub id: String,
    /// A stage's role identity: a key into `EffectiveConfig::roles` (a project's own
    /// `roles:` override) or, absent one, a built-in role name (`src/roles/builtin`) --
    /// see `roles::resolve`. Validated against both at `resolve()` time (below), so a
    /// stage naming an undefined role is a config error, not a runtime surprise.
    pub role: String,
    pub max_turns: u32,
    pub on_failure: StageFailureAction,
    /// Whether this stage's failure parks the issue in `pipeline.blocked_state` rather
    /// than falling back to the whole-attempt retry backoff.
    pub blocking: bool,
    /// Lets a project skip this stage per-issue by labeling the issue
    /// `skip-<stage id>` (e.g. `skip-requirements`), for tickets already written as
    /// specs (AIR-4) that don't need the requirements stage to re-derive anything.
    /// `false` (the default) means the stage always runs, matching AIR-1's original
    /// pre-`optional` behavior exactly.
    pub optional: bool,
    /// AIR-5: whether this stage's *successful* completion still isn't enough to move
    /// on -- the cycle parks in `pipeline.awaiting_approval_state` and waits for a
    /// human decision (dashboard or issue-comment `/approve`/`/changes`/`/reject`),
    /// unless `pipeline.approval.auto_approve_when` matches the stage's output first.
    pub requires_approval: bool,
}

/// AIR-2: a project's override of one of the eight built-in roadmap roles
/// (`roles.<name>` in `WORKFLOW.md`), or a wholly project-defined one. Every field is
/// optional -- an unset one falls back to the built-in prompt (`src/roles/builtin`) and
/// to `agent.*`/an unrestricted `ToolPolicy`, exactly like having no `roles.<name>`
/// entry at all. See `roles::resolve` for how these combine.
#[derive(Debug, Clone, Default)]
pub struct RoleConfig {
    /// Inline prompt template overriding the built-in one. `prompt_file` (read and
    /// substituted in here at resolve time, relative to `workflow_dir`) takes the same
    /// slot -- exactly one of the two, or neither (built-in), is expected; `prompt_file`
    /// wins if both are set, matching "a project-supplied prompt_file overrides the
    /// built-in" from the ticket without needing a third precedence rule for "both set."
    pub prompt: Option<String>,
    pub backend: Option<AgentBackendKind>,
    /// Backend-specific model id, e.g. `fireworks/<model-id>` for `opencode`.
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub tool_policy: crate::agent::ToolPolicy,
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
    pub swebot: SwebotConfig,
    pub pipeline: PipelineConfig,
    /// AIR-2: project overrides of the built-in roadmap roles (`roles:` in
    /// `WORKFLOW.md`), keyed by role name. A role a stage names but that's absent here
    /// falls back to its built-in default (`src/roles/builtin`) entirely -- see
    /// `roles::resolve`.
    pub roles: HashMap<String, RoleConfig>,

    pub hook_after_create: Option<String>,
    pub hook_before_run: Option<String>,
    pub hook_after_run: Option<String>,
    pub hook_before_remove: Option<String>,
    pub hook_timeout_ms: u64,

    pub max_concurrent_agents: u32,
    pub max_turns: u32,
    pub max_retry_backoff_ms: u64,
    /// Retry delay used instead of the normal exponential backoff when a turn fails
    /// because the account has hit its plan's own usage limit (e.g. Claude Code's "You've
    /// hit your session limit" message) -- see `orchestrator::is_plan_rate_limited`.
    /// Deliberately a fixed, much longer delay than `max_retry_backoff_ms`'s own cap:
    /// the plan's reset window is measured in hours, not minutes, and the message that
    /// reports it names a wall-clock local time in an arbitrary timezone, which isn't
    /// reliably parseable into an exact instant without a timezone database this crate
    /// doesn't otherwise need -- so this waits a fixed interval and re-checks, showing
    /// the CLI's own latest (and most accurate) message each time, rather than trying to
    /// compute the precise reset moment.
    pub rate_limit_pause_ms: u64,
    pub max_concurrent_agents_by_state: HashMap<String, u32>,
    pub agent_backend: AgentBackendKind,

    pub codex: CodexConfig,
    pub claude: ClaudeConfig,
    pub opencode: OpenCodeConfig,
}

impl EffectiveConfig {
    pub fn effective_command(&self) -> &str {
        match self.agent_backend {
            AgentBackendKind::Claude => &self.claude.command,
            AgentBackendKind::Codex => &self.codex.command,
            AgentBackendKind::OpenCode => &self.opencode.command,
        }
    }

    pub fn effective_stall_timeout_ms(&self) -> i64 {
        match self.agent_backend {
            AgentBackendKind::Claude => self.claude.stall_timeout_ms,
            AgentBackendKind::Codex => self.codex.stall_timeout_ms,
            AgentBackendKind::OpenCode => self.opencode.stall_timeout_ms,
        }
    }

    /// The backend SweBot's own sessions run on: `swebot.backend` when set, else the
    /// same `agent.backend` ticket dispatch uses -- see `SwebotConfig::backend`.
    pub fn swebot_backend(&self) -> AgentBackendKind {
        self.swebot.backend.unwrap_or(self.agent_backend)
    }

    /// Per-state concurrency limit, falling back to the global limit (Section 8.3).
    pub fn concurrency_limit_for_state(&self, state: &str) -> u32 {
        let key = state.trim().to_lowercase();
        *self
            .max_concurrent_agents_by_state
            .get(&key)
            .unwrap_or(&self.max_concurrent_agents)
    }

    /// `repo:` as SweBot itself should authenticate to GitHub with: `swebot.token` if
    /// set (a separate identity from the coding agent's own `repo.token`, so SweBot's
    /// PR reviews aren't posted by the same account that opened the PR -- see
    /// `SwebotConfig::token_env`'s doc comment), else `repo.token` unchanged, matching
    /// the old single-identity behavior. `None` iff `self.repo` itself is `None`.
    pub fn swebot_repo_config(&self) -> Option<RepoConfig> {
        let repo = self.repo.as_ref()?;
        Some(RepoConfig {
            token_env: self
                .swebot
                .token_env
                .clone()
                .or_else(|| repo.token_env.clone()),
            ..repo.clone()
        })
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

/// Conventional output filename per coverage format, used when `pipeline.test.coverage`
/// doesn't set an explicit `path:` -- matches what each tool's docs use as its own
/// default (e.g. `cargo llvm-cov --json --output-path coverage.json`'s own example).
fn default_coverage_path(format: crate::quality::CoverageFormat) -> String {
    use crate::quality::CoverageFormat as F;
    match format {
        F::LlvmCov => "coverage.json",
        F::Lcov => "lcov.info",
        F::Cobertura => "cobertura.xml",
        F::Jacoco => "jacoco.xml",
        F::None => "coverage",
    }
    .to_string()
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
    let opencode = get(config, "opencode").unwrap_or(&empty);

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
            let provider = get_str(r, "provider")
                .map(|p| RepoProvider::parse(&p).ok_or(ConfigError::UnsupportedRepoProvider(p)))
                .transpose()?
                .unwrap_or_default();
            let api_base_url = get_str(r, "api_base_url");
            let default_branch = get_str(r, "default_branch").unwrap_or_else(|| "main".to_string());
            let token_env = get_str(r, "token")
                .map(|t| {
                    envsub::var_name_of(&t)
                        .map(|s| s.to_string())
                        .ok_or(ConfigError::InvalidRepoToken)
                })
                .transpose()?;
            let pull_request = get(r, "pull_request")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if pull_request {
                match provider {
                    RepoProvider::Github => {
                        if crate::repo_host::github::parse_github_owner_repo(&url).is_none() {
                            return Err(ConfigError::PullRequestRequiresGithubRepo);
                        }
                    }
                    RepoProvider::Gitlab => {
                        if crate::repo_host::gitlab::parse_gitlab_project_path(&url).is_none() {
                            return Err(ConfigError::PullRequestRequiresGitlabRepo);
                        }
                    }
                }
                if token_env.is_none() {
                    return Err(ConfigError::PullRequestRequiresRepoToken);
                }
            }
            let evidence = get(r, "evidence")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if evidence && !pull_request {
                return Err(ConfigError::EvidenceRequiresPullRequest);
            }
            Ok(RepoConfig {
                url,
                provider,
                api_base_url,
                default_branch,
                token_env,
                pull_request,
                evidence,
            })
        })
        .transpose()?;

    let swebot_raw = get(config, "swebot").unwrap_or(&empty);
    let swebot_enabled = get(swebot_raw, "enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let swebot_qa = get(swebot_raw, "qa").unwrap_or(&empty);
    let swebot_drafting = get(swebot_raw, "drafting").unwrap_or(&empty);
    let swebot_review = get(swebot_raw, "review").unwrap_or(&empty);
    let swebot_chat = get(swebot_raw, "chat").unwrap_or(&empty);
    let swebot_token_env = get_str(swebot_raw, "token")
        .map(|t| {
            envsub::var_name_of(&t)
                .map(|s| s.to_string())
                .ok_or(ConfigError::InvalidSwebotToken)
        })
        .transpose()?;
    let chat_connectors = get_vec_str(swebot_chat, "connectors");
    let swebot_chat_cfg = SwebotChatConfig {
        enabled: get(swebot_chat, "enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        connectors: if chat_connectors.is_empty() {
            vec!["web".to_string()]
        } else {
            chat_connectors
        },
        poll_interval_ms: get_u64(swebot_chat, "poll_interval_ms", 5_000).max(100),
        remote_poll_interval_ms: get_u64(swebot_chat, "remote_poll_interval_ms", 30_000).max(100),
        max_concurrent_replies: get_u64(swebot_chat, "max_concurrent_replies", 2).max(1) as u32,
        auto_create_issue: get(swebot_chat, "auto_create_issue")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        max_history_messages: get_u64(swebot_chat, "max_history_messages", 40).max(1) as usize,
        first_text_deadline_ms: get_u64(swebot_chat, "first_text_deadline_ms", 2_000),
    };
    let swebot_cfg = SwebotConfig {
        enabled: swebot_enabled,
        backend: get_str(swebot_raw, "backend").map(|s| AgentBackendKind::parse(&s)),
        qa_discussion_category: get_str(swebot_qa, "discussion_category")
            .unwrap_or_else(|| "Q&A".to_string()),
        drafting_discussion_category: get_str(swebot_drafting, "discussion_category")
            .unwrap_or_else(|| "Ideas".to_string()),
        qa_label: get_str(swebot_qa, "label").unwrap_or_else(|| "swebot::question".to_string()),
        drafting_label: get_str(swebot_drafting, "label")
            .unwrap_or_else(|| "swebot::idea".to_string()),
        review_enabled: get(swebot_review, "enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(swebot_enabled),
        token_env: swebot_token_env,
        chat: swebot_chat_cfg,
    };
    if swebot_cfg.chat.enabled {
        if !swebot_enabled {
            return Err(ConfigError::ChatRequiresSwebot);
        }
        for name in &swebot_cfg.chat.connectors {
            if !KNOWN_CHAT_CONNECTORS.contains(&name.as_str()) {
                return Err(ConfigError::UnknownChatConnector(
                    name.clone(),
                    KNOWN_CHAT_CONNECTORS.join(", "),
                ));
            }
        }
    }
    if swebot_cfg.enabled {
        let repo = repo_cfg
            .as_ref()
            .ok_or(ConfigError::SwebotRequiresGithubRepo)?;
        match repo.provider {
            RepoProvider::Github => {
                if crate::repo_host::github::parse_github_owner_repo(&repo.url).is_none() {
                    return Err(ConfigError::SwebotRequiresGithubRepo);
                }
            }
            RepoProvider::Gitlab => {
                if crate::repo_host::gitlab::parse_gitlab_project_path(&repo.url).is_none() {
                    return Err(ConfigError::SwebotRequiresGitlabRepo);
                }
            }
        }
        if repo.token_env.is_none() && swebot_cfg.token_env.is_none() {
            return Err(ConfigError::SwebotRequiresRepoToken);
        }
    }

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

    let opencode_cfg = OpenCodeConfig {
        command: get_str(opencode, "command").unwrap_or_else(|| "opencode".to_string()),
        args: get_vec_str(opencode, "args"),
        model: get_str(opencode, "model"),
        auto_approve: get(opencode, "auto_approve")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        turn_timeout_ms: get_u64(opencode, "turn_timeout_ms", 3_600_000),
        stall_timeout_ms: get_i64(opencode, "stall_timeout_ms", 300_000),
        api_key_env: get_str(opencode, "api_key")
            .map(|k| {
                envsub::var_name_of(&k)
                    .map(|s| s.to_string())
                    .ok_or(ConfigError::InvalidOpenCodeApiKey)
            })
            .transpose()?,
    };

    let pipeline_raw = get(config, "pipeline").unwrap_or(&empty);
    let pipeline_enabled = get(pipeline_raw, "enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let pipeline_stages = get(pipeline_raw, "stages")
        .and_then(|v| v.as_sequence())
        .map(|seq| -> Result<Vec<StageConfig>, ConfigError> {
            seq.iter()
                .enumerate()
                .map(|(i, s)| {
                    let id = get_str(s, "id")
                        .filter(|v| !v.trim().is_empty())
                        .ok_or(ConfigError::InvalidPipelineStage(i))?;
                    let role = get_str(s, "role")
                        .filter(|v| !v.trim().is_empty())
                        .ok_or(ConfigError::InvalidPipelineStage(i))?;
                    Ok(StageConfig {
                        id,
                        role,
                        max_turns: (get_u64(s, "max_turns", max_turns) as u32).max(1),
                        on_failure: get_str(s, "on_failure")
                            .map(|v| StageFailureAction::parse(&v))
                            .unwrap_or(StageFailureAction::Escalate),
                        blocking: get(s, "blocking").and_then(|v| v.as_bool()).unwrap_or(false),
                        optional: get(s, "optional").and_then(|v| v.as_bool()).unwrap_or(false),
                        requires_approval: get(s, "requires_approval")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .transpose()?
        .unwrap_or_default();
    if pipeline_enabled && pipeline_stages.is_empty() {
        return Err(ConfigError::EmptyPipelineStages);
    }
    let auto_approve_raw = get(pipeline_raw, "approval").and_then(|a| get(a, "auto_approve_when"));
    let auto_approve_when = auto_approve_raw.map(|a| AutoApproveWhen {
        risk: get_str(a, "risk"),
        impacted_components_allowlist: get(a, "impacted_components")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            }),
        max_estimate_turns: get(a, "estimate_turns_max")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
    });
    let test_raw = get(pipeline_raw, "test");
    let test_cfg = test_raw.map(|t| {
        let commands = get_map(t, "commands")
            .as_mapping()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.as_str()?.to_string(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let coverage_raw = get(t, "coverage");
        let coverage = coverage_raw.and_then(|c| {
            let command = get_str(c, "command")?;
            let format =
                crate::quality::CoverageFormat::parse(&get_str(c, "format").unwrap_or_default());
            let path = get_str(c, "path").unwrap_or_else(|| default_coverage_path(format));
            Some(CoverageConfig {
                command,
                format,
                min_line_percent: get(c, "min_line_percent").and_then(|v| v.as_f64()),
                path,
            })
        });
        TestConfig { commands, coverage }
    });

    let pipeline_cfg = PipelineConfig {
        enabled: pipeline_enabled,
        stages: pipeline_stages,
        blocked_state: get_str(pipeline_raw, "blocked_state")
            .unwrap_or_else(|| "blocked".to_string()),
        awaiting_approval_state: get_str(pipeline_raw, "awaiting_approval_state")
            .unwrap_or_else(|| "awaiting approval".to_string()),
        approval: ApprovalConfig { auto_approve_when },
        test: test_cfg,
    };

    let roles_raw = get(config, "roles").unwrap_or(&empty);
    let mut roles_cfg: HashMap<String, RoleConfig> = HashMap::new();
    if let Some(mapping) = roles_raw.as_mapping() {
        for (k, v) in mapping {
            let Some(name) = k.as_str() else { continue };
            let name = name.trim().to_lowercase();
            // `prompt_file` (relative to `workflow_dir`, same convention
            // `workspace.root` uses) wins over inline `prompt` when both are set --
            // matches the ticket's "a project-supplied prompt_file overrides the
            // built-in" without needing a separate precedence rule for "both set."
            let prompt = match get_str(v, "prompt_file") {
                Some(rel) => {
                    let path = envsub::resolve_path(&rel, workflow_dir);
                    let content = std::fs::read_to_string(&path).map_err(|e| {
                        ConfigError::UnreadableRolePromptFile(name.clone(), rel.clone(), e.to_string())
                    })?;
                    Some(content)
                }
                None => get_str(v, "prompt"),
            };
            let tools = get(v, "tools").unwrap_or(&empty);
            let tool_policy = crate::agent::ToolPolicy {
                allow_edits: get(tools, "allow_edits")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(true),
                allow_commands: get(tools, "allow_commands")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(true),
            };
            roles_cfg.insert(
                name,
                RoleConfig {
                    prompt,
                    backend: get_str(v, "backend").map(|s| AgentBackendKind::parse(&s)),
                    model: get_str(v, "model"),
                    max_turns: get(v, "max_turns")
                        .and_then(|x| x.as_u64())
                        .map(|n| n.max(1) as u32),
                    tool_policy,
                },
            );
        }
    }

    // A stage naming an undefined, non-built-in role is a config error, not a runtime
    // surprise -- checked here (both `roles_cfg` and `pipeline_cfg.stages` exist by
    // this point) rather than left for `roles::resolve` to discover mid-cycle.
    for (i, stage) in pipeline_cfg.stages.iter().enumerate() {
        let role_key = stage.role.trim().to_lowercase();
        if !roles_cfg.contains_key(&role_key) && !crate::roles::builtin::is_known(&role_key) {
            return Err(ConfigError::UnknownStageRole(
                i,
                stage.role.clone(),
                crate::roles::builtin::ROLE_NAMES.join(", "),
            ));
        }
    }

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
        swebot: swebot_cfg,
        pipeline: pipeline_cfg,
        roles: roles_cfg,

        hook_after_create,
        hook_before_run,
        hook_after_run,
        hook_before_remove: get_str(hooks, "before_remove"),
        hook_timeout_ms,

        max_concurrent_agents: get_u64(agent, "max_concurrent_agents", 10) as u32,
        max_turns: max_turns as u32,
        max_retry_backoff_ms: get_u64(agent, "max_retry_backoff_ms", 300_000),
        rate_limit_pause_ms: get_u64(agent, "rate_limit_pause_ms", 1_800_000),
        max_concurrent_agents_by_state,
        agent_backend,

        codex: codex_cfg,
        claude: claude_cfg,
        opencode: opencode_cfg,
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
    // The credential-helper placeholder username a bearer token is exchanged against
    // over HTTPS git operations: GitHub's documented convention is `x-access-token`,
    // GitLab's is `oauth2` -- both accept *any* username when the password is a valid
    // token, but each host's own tooling/docs expect its specific one, so this
    // matches convention rather than relying on that leniency. Deploy tokens (which
    // require their *own* configured username, not `oauth2`) aren't supported by this
    // synthesized default -- same "extend, don't configure around" posture as
    // `repo_host::github::parse_github_owner_repo`'s GitHub-Enterprise note.
    let credential_username = match repo.provider {
        RepoProvider::Github => "x-access-token",
        RepoProvider::Gitlab => "oauth2",
    };

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
             cred_helper='!f() {{ echo username={credential_username}; echo \"password=${var}\"; }}; f'\n\
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

    let before_run = format!(
        "name=\"$(basename \"$PWD\")\"\n\
        if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then\n\
        \x20\x20echo \"FATAL: workspace is not a git repository (after_create must have failed silently)\" >&2\n\
        \x20\x20exit 1\n\
        fi\n\
        git pull --ff-only origin \"issue-$name\" || true\n\
        git fetch origin \"{branch}\" -q || true\n\
        if git rev-parse --verify -q \"origin/{branch}\" >/dev/null 2>&1 \
            && ! git rebase \"origin/{branch}\"; then\n\
        \x20\x20if [ -d .git/rebase-merge ] || [ -d .git/rebase-apply ]; then\n\
        \x20\x20\x20\x20echo \"MERGE CONFLICT: issue-$name is behind {branch} and could not be \
rebased automatically. Resolve the conflicts yourself this turn -- edit each file with conflict \
markers, 'git add' it, then 'git rebase --continue' (repeat until it reports the rebase is done, \
or 'git rebase --abort' if the conflict genuinely can't be resolved) -- before doing anything \
else.\" >&2\n\
        \x20\x20else\n\
        \x20\x20\x20\x20git rebase --abort >/dev/null 2>&1 || true\n\
        \x20\x20\x20\x20echo \"WARNING: rebase onto origin/{branch} failed for a reason other \
than a conflict; aborted it and left issue-$name as it was.\" >&2\n\
        \x20\x20fi\n\
        fi\n"
    );

    let after_run = "name=\"$(basename \"$PWD\")\"\n\
        if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then\n\
        \x20\x20echo \"FATAL: workspace is not a git repository (after_create must have failed silently)\" >&2\n\
        \x20\x20exit 1\n\
        fi\n\
        if [ -d .git/rebase-merge ] || [ -d .git/rebase-apply ]; then\n\
        \x20\x20echo \"FATAL: issue-$name is mid-rebase (unresolved merge conflict) -- resolve \
it (or 'git rebase --abort') before this attempt's work can be committed/pushed\" >&2\n\
        \x20\x20exit 1\n\
        fi\n\
        git add -A\n\
        git commit -m \"symphony: $name\" -q --allow-empty-message || true\n\
        if ! git push --force-with-lease origin \"HEAD:refs/heads/issue-$name\" -q; then\n\
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
            AgentBackendKind::OpenCode => "opencode",
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
        assert_eq!(cfg.rate_limit_pause_ms, 1_800_000);
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
        assert!(before.contains("git fetch origin \"main\""));
        assert!(before.contains("git rebase \"origin/main\""));
        assert!(before.contains("MERGE CONFLICT"));

        let after = cfg.hook_after_run.unwrap();
        assert!(
            after.contains("git push --force-with-lease origin \"HEAD:refs/heads/issue-$name\" -q")
        );
        assert!(after.contains("is-inside-work-tree"));
        // A mid-rebase (unresolved conflict) workspace must not be committed/pushed.
        assert!(after.contains("rebase-merge"));
        // FATAL guard comes before the push, not after.
        assert!(after.find("is-inside-work-tree").unwrap() < after.find("git push").unwrap());
    }

    #[test]
    fn gitlab_repo_synthesizes_hooks_with_the_oauth2_credential_username() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  provider: gitlab\n  \
             url: https://gitlab.example.com/o/r.git\n  \
             default_branch: main\n  token: $GITLAB_TOKEN\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        let create = cfg.hook_after_create.unwrap();
        assert!(create.contains("username=oauth2"));
        assert!(!create.contains("username=x-access-token"));
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
    fn pull_request_requires_a_github_repo_url() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://gitlab.com/o/r.git\n  \
             token: $SYMPHONY_TEST_PR_TOKEN\n  pull_request: true\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::PullRequestRequiresGithubRepo)
        ));
    }

    #[test]
    fn pull_request_requires_a_repo_token() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             pull_request: true\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::PullRequestRequiresRepoToken)
        ));
    }

    #[test]
    fn pull_request_true_with_valid_github_repo_and_token_resolves() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_PR_TOKEN\n  pull_request: true\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(cfg.repo.unwrap().pull_request);
    }

    #[test]
    fn pull_request_true_with_valid_gitlab_repo_and_token_resolves() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  provider: gitlab\n  \
             url: https://gitlab.example.com/group/subgroup/r.git\n  \
             token: $SYMPHONY_TEST_PR_GITLAB_TOKEN\n  pull_request: true\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        let repo = cfg.repo.unwrap();
        assert!(repo.pull_request);
        assert_eq!(repo.provider, RepoProvider::Gitlab);
    }

    #[test]
    fn pull_request_requires_a_gitlab_repo_url_when_provider_is_gitlab() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  provider: gitlab\n  url: not-a-url\n  \
             token: $SYMPHONY_TEST_PR_GITLAB_TOKEN_2\n  pull_request: true\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::PullRequestRequiresGitlabRepo)
        ));
    }

    #[test]
    fn unsupported_repo_provider_errors() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  provider: bitbucket\n  url: https://example.com/r.git\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::UnsupportedRepoProvider(p)) if p == "bitbucket"
        ));
    }

    #[test]
    fn repo_provider_defaults_to_github() {
        let cfg_yaml =
            parse_yaml("tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.repo.unwrap().provider, RepoProvider::Github);
    }

    #[test]
    fn pull_request_defaults_to_false() {
        let cfg_yaml =
            parse_yaml("tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(!cfg.repo.unwrap().pull_request);
    }

    #[test]
    fn evidence_defaults_to_false() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_EVIDENCE_DEFAULT\n  pull_request: true\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(!cfg.repo.unwrap().evidence);
    }

    #[test]
    fn evidence_requires_pull_request_to_be_true() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_EVIDENCE_REQUIRES_PR\n  evidence: true\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::EvidenceRequiresPullRequest)
        ));
    }

    #[test]
    fn evidence_true_with_pull_request_true_resolves() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_EVIDENCE_OK\n  pull_request: true\n  evidence: true\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        let repo = cfg.repo.unwrap();
        assert!(repo.pull_request);
        assert!(repo.evidence);
    }

    #[test]
    fn swebot_enabled_requires_a_repo_block() {
        let cfg_yaml = parse_yaml("tracker:\n  kind: local\nswebot:\n  enabled: true\n");
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::SwebotRequiresGithubRepo)
        ));
    }

    #[test]
    fn swebot_enabled_requires_a_github_repo_url() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://gitlab.com/o/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN\nswebot:\n  enabled: true\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::SwebotRequiresGithubRepo)
        ));
    }

    #[test]
    fn swebot_enabled_requires_a_gitlab_repo_url_when_provider_is_gitlab() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  provider: gitlab\n  url: not-a-url\n  \
             token: $SYMPHONY_TEST_SWEBOT_GITLAB_TOKEN\nswebot:\n  enabled: true\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::SwebotRequiresGitlabRepo)
        ));
    }

    #[test]
    fn swebot_enabled_with_a_valid_gitlab_repo_resolves() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  provider: gitlab\n  \
             url: https://gitlab.example.com/group/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_GITLAB_TOKEN_2\nswebot:\n  enabled: true\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(cfg.swebot.enabled);
    }

    #[test]
    fn swebot_qa_and_drafting_labels_default_and_can_be_overridden() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  provider: gitlab\n  \
             url: https://gitlab.example.com/group/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_GITLAB_TOKEN_3\nswebot:\n  enabled: true\n  \
             qa:\n    label: \"question\"\n  drafting:\n    label: \"idea\"\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.swebot.qa_label, "question");
        assert_eq!(cfg.swebot.drafting_label, "idea");

        let cfg_yaml_defaults = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  provider: gitlab\n  \
             url: https://gitlab.example.com/group/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_GITLAB_TOKEN_4\nswebot:\n  enabled: true\n",
        );
        let cfg_defaults = resolve(&cfg_yaml_defaults, Path::new(".")).unwrap();
        assert_eq!(cfg_defaults.swebot.qa_label, "swebot::question");
        assert_eq!(cfg_defaults.swebot.drafting_label, "swebot::idea");
    }

    #[test]
    fn swebot_enabled_requires_a_repo_token() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n\
             swebot:\n  enabled: true\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::SwebotRequiresRepoToken)
        ));
    }

    #[test]
    fn swebot_disabled_by_default_and_needs_no_repo() {
        let cfg_yaml = parse_yaml("tracker:\n  kind: local\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(!cfg.swebot.enabled);
    }

    #[test]
    fn swebot_review_enabled_defaults_to_the_top_level_flag_but_can_be_overridden() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN_2\nswebot:\n  enabled: true\n  \
             review:\n    enabled: false\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(cfg.swebot.enabled);
        assert!(!cfg.swebot.review_enabled);
    }

    #[test]
    fn swebot_discussion_categories_default_and_can_be_overridden() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN_3\nswebot:\n  enabled: true\n  \
             drafting:\n    discussion_category: Feature Requests\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.swebot.qa_discussion_category, "Q&A");
        assert_eq!(cfg.swebot.drafting_discussion_category, "Feature Requests");
    }

    #[test]
    fn swebot_token_defaults_to_none_and_swebot_repo_config_falls_back_to_repo_token() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN_4\nswebot:\n  enabled: true\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(cfg.swebot.token_env.is_none());
        assert_eq!(
            cfg.swebot_repo_config().unwrap().token_env.as_deref(),
            Some("SYMPHONY_TEST_SWEBOT_TOKEN_4")
        );
    }

    #[test]
    fn swebot_token_gives_swebot_its_own_identity_distinct_from_repo_token() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_CODEBOT_TOKEN\nswebot:\n  enabled: true\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN_5\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(
            cfg.swebot.token_env.as_deref(),
            Some("SYMPHONY_TEST_SWEBOT_TOKEN_5")
        );
        assert_eq!(
            cfg.repo.as_ref().unwrap().token_env.as_deref(),
            Some("SYMPHONY_TEST_CODEBOT_TOKEN")
        );
        assert_eq!(
            cfg.swebot_repo_config().unwrap().token_env.as_deref(),
            Some("SYMPHONY_TEST_SWEBOT_TOKEN_5")
        );
    }

    #[test]
    fn swebot_token_must_be_var_reference_not_a_literal() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN_6\nswebot:\n  enabled: true\n  \
             token: not-a-var\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::InvalidSwebotToken)
        ));
    }

    #[test]
    fn chat_disabled_by_default_and_needs_no_connectors() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN_8\nswebot:\n  enabled: true\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(!cfg.swebot.chat.enabled);
    }

    #[test]
    fn chat_enabled_requires_swebot_enabled() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN_9\nswebot:\n  chat:\n    enabled: true\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::ChatRequiresSwebot)
        ));
    }

    #[test]
    fn chat_resolves_defaults_and_overrides() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN_10\nswebot:\n  enabled: true\n  \
             chat:\n    enabled: true\n    connectors: [web]\n    poll_interval_ms: 1500\n    \
             remote_poll_interval_ms: 45000\n    \
             max_concurrent_replies: 3\n    auto_create_issue: false\n    max_history_messages: 10\n    \
             first_text_deadline_ms: 2500\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        let chat = &cfg.swebot.chat;
        assert!(chat.enabled);
        assert_eq!(chat.connectors, vec!["web".to_string()]);
        assert_eq!(chat.poll_interval_ms, 1500);
        assert_eq!(chat.remote_poll_interval_ms, 45_000);
        assert_eq!(chat.max_concurrent_replies, 3);
        assert!(!chat.auto_create_issue);
        assert_eq!(chat.max_history_messages, 10);
        assert_eq!(chat.first_text_deadline_ms, 2500);
    }

    #[test]
    fn chat_remote_poll_interval_defaults_slower_than_poll_interval() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN_14\nswebot:\n  enabled: true\n  \
             chat:\n    enabled: true\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.swebot.chat.poll_interval_ms, 5_000);
        assert_eq!(cfg.swebot.chat.remote_poll_interval_ms, 30_000);
    }

    #[test]
    fn chat_connectors_default_to_web_when_unset() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN_11\nswebot:\n  enabled: true\n  \
             chat:\n    enabled: true\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.swebot.chat.connectors, vec!["web".to_string()]);
    }

    #[test]
    fn chat_rejects_an_unknown_connector() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN_12\nswebot:\n  enabled: true\n  \
             chat:\n    enabled: true\n    connectors: [slack]\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::UnknownChatConnector(_, _))
        ));
    }

    #[test]
    fn chat_bounds_eager_settings() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n  \
             token: $SYMPHONY_TEST_SWEBOT_TOKEN_13\nswebot:\n  enabled: true\n  \
             chat:\n    enabled: true\n    poll_interval_ms: 0\n    remote_poll_interval_ms: 0\n    \
             max_concurrent_replies: 0\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.swebot.chat.poll_interval_ms, 100);
        assert_eq!(cfg.swebot.chat.remote_poll_interval_ms, 100);
        assert_eq!(cfg.swebot.chat.max_concurrent_replies, 1);
    }

    #[test]
    fn swebot_enabled_accepts_swebot_token_alone_with_no_repo_token() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nrepo:\n  url: https://github.com/o/r.git\n\
             swebot:\n  enabled: true\n  token: $SYMPHONY_TEST_SWEBOT_TOKEN_7\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(cfg.repo.as_ref().unwrap().token_env.is_none());
        assert_eq!(
            cfg.swebot_repo_config().unwrap().token_env.as_deref(),
            Some("SYMPHONY_TEST_SWEBOT_TOKEN_7")
        );
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

    /// Real end-to-end exercise of the merge-conflict path added to `before_run`:
    /// a ticket branch pushed once, then `main` on origin advances with a
    /// conflicting change to the same line before the ticket's next turn.
    /// `before_run` must detect the conflict and leave the workspace mid-rebase
    /// (not fail the hook outright) so an agent's own turn gets a chance to resolve
    /// it; `after_run` must then refuse to commit/push while still mid-rebase, and
    /// (once the conflict actually is resolved, simulating what an agent turn would
    /// do) `--force-with-lease` the rewritten history through successfully.
    #[tokio::test]
    async fn before_run_detects_a_real_conflict_and_after_run_pushes_it_once_resolved() {
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        }

        let origin = tempdir().unwrap();
        let git_id = ["-c", "user.email=t@t", "-c", "user.name=t"];
        std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(origin.path())
            .status()
            .unwrap();
        std::fs::write(origin.path().join("shared.txt"), "line1\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(origin.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(git_id)
            .args(["commit", "-m", "seed"])
            .current_dir(origin.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "receive.denyCurrentBranch", "updateInstead"])
            .current_dir(origin.path())
            .status()
            .unwrap();

        let origin_wsl_path = to_wsl_path(origin.path());
        let cfg_yaml = parse_yaml(&format!(
            "tracker:\n  kind: local\nrepo:\n  url: {origin_wsl_path:?}\n  default_branch: main\n"
        ));
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();

        let ws_named = origin.path().parent().unwrap().join("99");
        std::fs::create_dir_all(&ws_named).unwrap();

        crate::hooks::run_hook(
            "after_create",
            cfg.hook_after_create.as_deref().unwrap(),
            &ws_named,
            15_000,
        )
        .await
        .unwrap();

        // The "agent's" own change on the ticket branch, first push -- a normal,
        // conflict-free push (fast-forward from origin's perspective).
        std::fs::write(ws_named.join("shared.txt"), "line1\nagent-change\n").unwrap();
        crate::hooks::run_hook(
            "after_run",
            cfg.hook_after_run.as_deref().unwrap(),
            &ws_named,
            15_000,
        )
        .await
        .unwrap();

        // `main` advances on origin with a conflicting edit to the exact same line
        // -- origin is a normal (non-bare) checkout still on `main`, so this is just
        // a local commit there, no push machinery needed.
        std::fs::write(origin.path().join("shared.txt"), "line1\nmain-change\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(origin.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(git_id)
            .args(["commit", "-m", "main advances"])
            .current_dir(origin.path())
            .status()
            .unwrap();

        // before_run must detect the now-unrebaseable branch, leave it mid-rebase,
        // and still succeed (Ok(())) -- resolving a real conflict is the agent
        // turn's job, not this hook's.
        crate::hooks::run_hook(
            "before_run",
            cfg.hook_before_run.as_deref().unwrap(),
            &ws_named,
            15_000,
        )
        .await
        .unwrap();
        assert!(
            ws_named.join(".git/rebase-merge").exists()
                || ws_named.join(".git/rebase-apply").exists(),
            "before_run should have left the workspace mid-rebase on a real conflict"
        );
        let conflicted = std::fs::read_to_string(ws_named.join("shared.txt")).unwrap();
        assert!(
            conflicted.contains("<<<<<<<"),
            "shared.txt should carry real conflict markers, got: {conflicted}"
        );

        // after_run must refuse to touch a mid-rebase workspace.
        let blocked = crate::hooks::run_hook(
            "after_run",
            cfg.hook_after_run.as_deref().unwrap(),
            &ws_named,
            15_000,
        )
        .await;
        assert!(
            blocked.is_err(),
            "after_run must not commit/push while the workspace is mid-rebase"
        );

        // The agent's own turn resolves it -- pick "main"'s side plus keep the
        // agent's own change, exactly what a real conflict resolution looks like.
        std::fs::write(
            ws_named.join("shared.txt"),
            "line1\nmain-change\nagent-change\n",
        )
        .unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&ws_named)
            .status()
            .unwrap();
        let continue_status = std::process::Command::new("git")
            .args(git_id)
            .args(["rebase", "--continue"])
            .env("GIT_EDITOR", "true")
            .current_dir(&ws_named)
            .status()
            .unwrap();
        assert!(
            continue_status.success(),
            "git rebase --continue should have finished cleanly"
        );
        assert!(!ws_named.join(".git/rebase-merge").exists());

        // Now after_run should push the rewritten history through with
        // --force-with-lease (a plain push would be rejected as non-fast-forward,
        // since the rebase changed issue-99's commit history on top of main).
        crate::hooks::run_hook(
            "after_run",
            cfg.hook_after_run.as_deref().unwrap(),
            &ws_named,
            15_000,
        )
        .await
        .unwrap();

        let show = std::process::Command::new("git")
            .args(["show", "issue-99:shared.txt"])
            .current_dir(origin.path())
            .output()
            .unwrap();
        assert!(show.status.success());
        let content = String::from_utf8_lossy(&show.stdout).to_string();
        assert!(content.contains("main-change") && content.contains("agent-change"));

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
    fn validate_allows_docker_enabled_with_opencode_backend() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nagent:\n  backend: opencode\nworkspace:\n  docker:\n    \
             enabled: true\n    image: some-image:latest\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(validate_for_dispatch(&cfg, &["local"]).is_ok());
    }

    #[test]
    fn opencode_api_key_must_be_var_reference_not_a_literal() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nagent:\n  backend: opencode\nopencode:\n  api_key: not-a-var\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::InvalidOpenCodeApiKey)
        ));
    }

    #[test]
    fn opencode_api_key_var_reference_resolves_to_env_var_name() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nagent:\n  backend: opencode\nopencode:\n  \
             api_key: $FIREWORKS_API_KEY\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(
            cfg.opencode.api_key_env.as_deref(),
            Some("FIREWORKS_API_KEY")
        );
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

    #[test]
    fn opencode_backend_parses_with_defaults() {
        let cfg_yaml = parse_yaml("tracker:\n  kind: local\nagent:\n  backend: opencode\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.agent_backend, AgentBackendKind::OpenCode);
        assert_eq!(cfg.opencode.command, "opencode");
        assert!(cfg.opencode.model.is_none());
        assert!(cfg.opencode.auto_approve);
        assert_eq!(cfg.opencode.turn_timeout_ms, 3_600_000);
        assert_eq!(cfg.opencode.stall_timeout_ms, 300_000);
        assert!(cfg.opencode.api_key_env.is_none());
    }

    #[test]
    fn opencode_block_overrides_parse() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nagent:\n  backend: opencode\nopencode:\n  \
             command: /usr/local/bin/opencode\n  model: fireworks/accounts/fireworks/models/kimi-k2\n  \
             auto_approve: false\n  turn_timeout_ms: 60000\n  stall_timeout_ms: 5000\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.opencode.command, "/usr/local/bin/opencode");
        assert_eq!(
            cfg.opencode.model.as_deref(),
            Some("fireworks/accounts/fireworks/models/kimi-k2")
        );
        assert!(!cfg.opencode.auto_approve);
        assert_eq!(cfg.opencode.turn_timeout_ms, 60_000);
        assert_eq!(cfg.opencode.stall_timeout_ms, 5_000);
        assert_eq!(cfg.effective_command(), "/usr/local/bin/opencode");
        assert_eq!(cfg.effective_stall_timeout_ms(), 5_000);
    }

    #[test]
    fn validate_rejects_missing_opencode_command() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nagent:\n  backend: opencode\nopencode:\n  command: \"\"\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(matches!(
            validate_for_dispatch(&cfg, &["local"]),
            Err(ConfigError::MissingAgentCommand { backend }) if backend == "opencode"
        ));
    }

    #[test]
    fn pipeline_absent_resolves_disabled_with_default_blocked_state() {
        let cfg_yaml = parse_yaml("tracker:\n  kind: local\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(!cfg.pipeline.enabled);
        assert!(cfg.pipeline.stages.is_empty());
        assert_eq!(cfg.pipeline.blocked_state, "blocked");
    }

    #[test]
    fn pipeline_enabled_with_no_stages_errors() {
        let cfg_yaml = parse_yaml("tracker:\n  kind: local\npipeline:\n  enabled: true\n");
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::EmptyPipelineStages)
        ));
    }

    #[test]
    fn pipeline_stage_missing_id_or_role_errors() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\npipeline:\n  enabled: true\n  stages:\n    \
             - role: developer\n      max_turns: 5\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::InvalidPipelineStage(0))
        ));
    }

    #[test]
    fn pipeline_stage_parses_fields_and_defaults() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nagent:\n  max_turns: 8\npipeline:\n  enabled: true\n  \
             blocked_state: parked\n  stages:\n    \
             - id: requirements\n      role: requirements\n    \
             - id: review\n      role: reviewer\n      max_turns: 3\n      \
             on_failure: skip\n      blocking: true\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(cfg.pipeline.enabled);
        assert_eq!(cfg.pipeline.blocked_state, "parked");
        assert_eq!(cfg.pipeline.stages.len(), 2);

        let first = &cfg.pipeline.stages[0];
        assert_eq!(first.id, "requirements");
        // Falls back to agent.max_turns (8) when a stage doesn't set its own.
        assert_eq!(first.max_turns, 8);
        assert_eq!(first.on_failure, StageFailureAction::Escalate);
        assert!(!first.blocking);

        let second = &cfg.pipeline.stages[1];
        assert_eq!(second.max_turns, 3);
        assert_eq!(second.on_failure, StageFailureAction::Skip);
        assert!(second.blocking);
        assert!(!first.optional);
        assert!(!second.optional);
    }

    #[test]
    fn pipeline_stage_optional_flag_parses_and_defaults_false() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\npipeline:\n  enabled: true\n  stages:\n    \
             - id: requirements\n      role: requirements\n      optional: true\n    \
             - id: implement\n      role: developer\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(cfg.pipeline.stages[0].optional);
        assert!(!cfg.pipeline.stages[1].optional);
    }

    /// AIR-2 acceptance criterion: "A stage referencing an unknown role fails config
    /// resolution with a helpful message."
    #[test]
    fn pipeline_stage_referencing_an_unknown_role_fails_resolution() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\npipeline:\n  enabled: true\n  stages:\n    \
             - id: made-up\n      role: not-a-real-role\n",
        );
        let err = resolve(&cfg_yaml, Path::new(".")).unwrap_err();
        match &err {
            ConfigError::UnknownStageRole(idx, role, known) => {
                assert_eq!(*idx, 0);
                assert_eq!(role, "not-a-real-role");
                assert!(known.contains("reviewer"));
            }
            other => panic!("expected UnknownStageRole, got {other:?}"),
        }
        assert!(err.to_string().contains("not-a-real-role"));
    }

    /// A stage naming a role only defined under `roles:` (not one of the eight
    /// built-ins) resolves fine -- `roles:` isn't limited to overriding built-ins.
    #[test]
    fn pipeline_stage_referencing_a_wholly_custom_role_resolves() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\npipeline:\n  enabled: true\n  stages:\n    \
             - id: custom\n      role: my-custom-role\n\
             roles:\n  my-custom-role:\n    prompt: \"do the custom thing\"\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.pipeline.stages[0].role, "my-custom-role");
        assert_eq!(
            cfg.roles.get("my-custom-role").unwrap().prompt.as_deref(),
            Some("do the custom thing")
        );
    }

    #[test]
    fn roles_block_parses_backend_model_and_tool_policy_overrides() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nroles:\n  reviewer:\n    backend: opencode\n    \
             model: fireworks/kimi\n    max_turns: 3\n    tools:\n      \
             allow_edits: false\n      allow_commands: false\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        let role = cfg.roles.get("reviewer").unwrap();
        assert_eq!(role.backend, Some(AgentBackendKind::OpenCode));
        assert_eq!(role.model.as_deref(), Some("fireworks/kimi"));
        assert_eq!(role.max_turns, Some(3));
        assert!(!role.tool_policy.allow_edits);
        assert!(!role.tool_policy.allow_commands);
    }

    #[test]
    fn roles_block_defaults_tool_policy_to_unrestricted() {
        let cfg_yaml = parse_yaml("tracker:\n  kind: local\nroles:\n  reviewer:\n    model: x\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        let role = cfg.roles.get("reviewer").unwrap();
        assert!(role.tool_policy.allow_edits);
        assert!(role.tool_policy.allow_commands);
    }

    #[test]
    fn roles_block_prompt_file_is_read_relative_to_workflow_dir_and_wins_over_inline_prompt() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("reviewer.md"), "custom file prompt").unwrap();
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nroles:\n  reviewer:\n    prompt: \"inline, should lose\"\n    \
             prompt_file: ./reviewer.md\n",
        );
        let cfg = resolve(&cfg_yaml, dir.path()).unwrap();
        assert_eq!(
            cfg.roles.get("reviewer").unwrap().prompt.as_deref(),
            Some("custom file prompt")
        );
    }

    #[test]
    fn roles_block_unreadable_prompt_file_is_a_clear_config_error() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\nroles:\n  reviewer:\n    prompt_file: ./does-not-exist.md\n",
        );
        assert!(matches!(
            resolve(&cfg_yaml, Path::new(".")),
            Err(ConfigError::UnreadableRolePromptFile(..))
        ));
    }

    #[test]
    fn pipeline_awaiting_approval_state_defaults_and_stage_requires_approval_parses() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\npipeline:\n  enabled: true\n  stages:\n    \
             - id: plan\n      role: planner\n      requires_approval: true\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.pipeline.awaiting_approval_state, "awaiting approval");
        assert!(cfg.pipeline.stages[0].requires_approval);
        assert!(cfg.pipeline.approval.auto_approve_when.is_none());
    }

    #[test]
    fn pipeline_auto_approve_when_parses_all_conditions() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\npipeline:\n  enabled: true\n  \
             awaiting_approval_state: pending review\n  \
             approval:\n    auto_approve_when:\n      risk: low\n      \
             impacted_components: [src/foo.rs, src/bar.rs]\n      estimate_turns_max: 4\n  \
             stages:\n    - id: plan\n      role: planner\n      requires_approval: true\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert_eq!(cfg.pipeline.awaiting_approval_state, "pending review");
        let auto = cfg.pipeline.approval.auto_approve_when.as_ref().unwrap();
        assert_eq!(auto.risk.as_deref(), Some("low"));
        assert_eq!(
            auto.impacted_components_allowlist.as_deref(),
            Some(&["src/foo.rs".to_string(), "src/bar.rs".to_string()][..])
        );
        assert_eq!(auto.max_estimate_turns, Some(4));
    }

    #[test]
    fn pipeline_test_absent_resolves_to_none() {
        let cfg_yaml = parse_yaml("tracker:\n  kind: local\n");
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        assert!(cfg.pipeline.test.is_none());
    }

    #[test]
    fn pipeline_test_parses_commands_and_coverage() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\npipeline:\n  test:\n    \
             commands:\n      unit: cargo test\n      integration: ./scripts/it.sh\n    \
             coverage:\n      command: cargo llvm-cov --json --output-path coverage.json\n      \
             format: llvm-cov\n      min_line_percent: 70\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        let test = cfg.pipeline.test.expect("pipeline.test should be Some");
        assert_eq!(test.commands.len(), 2);
        assert!(
            test.commands
                .contains(&("unit".to_string(), "cargo test".to_string()))
        );
        assert!(
            test.commands
                .contains(&("integration".to_string(), "./scripts/it.sh".to_string()))
        );
        let coverage = test.coverage.expect("coverage should be Some");
        assert_eq!(
            coverage.command,
            "cargo llvm-cov --json --output-path coverage.json"
        );
        assert_eq!(coverage.format, crate::quality::CoverageFormat::LlvmCov);
        assert_eq!(coverage.min_line_percent, Some(70.0));
        // Default path derived from format when `path:` isn't given.
        assert_eq!(coverage.path, "coverage.json");
    }

    #[test]
    fn pipeline_test_coverage_format_none_degrades_without_min_percent() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\npipeline:\n  test:\n    \
             commands:\n      unit: cargo test\n    \
             coverage:\n      command: echo skip\n      format: none\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        let coverage = cfg.pipeline.test.unwrap().coverage.unwrap();
        assert_eq!(coverage.format, crate::quality::CoverageFormat::None);
        assert_eq!(coverage.min_line_percent, None);
    }

    #[test]
    fn pipeline_test_without_coverage_block_is_none() {
        let cfg_yaml = parse_yaml(
            "tracker:\n  kind: local\npipeline:\n  test:\n    commands:\n      unit: cargo test\n",
        );
        let cfg = resolve(&cfg_yaml, Path::new(".")).unwrap();
        let test = cfg.pipeline.test.unwrap();
        assert_eq!(
            test.commands,
            vec![("unit".to_string(), "cargo test".to_string())]
        );
        assert!(test.coverage.is_none());
    }
}
