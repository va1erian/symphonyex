---
identifier: AIR-2
title: Role definitions — per-role prompt, backend, model and tool restrictions
state: done
priority: 1
labels:
- phase-1
- pipeline
- config
dispatchable: true
depends_on:
- AIR-1
updated_at: 2026-08-06T18:38:07.861487400+00:00
---
## Context

Roadmap §4 gives each agent in the pool a distinct responsibility and a distinct required
output. A Reviewer that can edit files is not a reviewer; a Requirements agent that can push
a branch is not a requirements agent. Symphony already proves the mechanism exists: SweBot
sessions run with file-mutating tools denied (`--disallowedTools Edit,Write,NotebookEdit` for
`claude`, `OPENCODE_PERMISSION={"edit":"deny"}` for `opencode`), see `src/swebot/mod.rs`.
That restriction is currently hardcoded for SweBot. Generalize it into reusable **roles**.

## Scope

Add a `roles:` block, referenced by `pipeline.stages[].role` (AIR-1):

```yaml
roles:
  reviewer:
    prompt_file: ./prompts/reviewer.md   # or inline `prompt:`
    backend: opencode                    # defaults to agent.backend
    model: fireworks/<model-id>          # backend-specific, optional
    max_turns: 6
    tools:
      allow_edits: false                 # maps to the backend-native deny mechanism
      allow_commands: true
    exit_criteria:
      required_artifacts: [review_findings]   # see AIR-3
```

Requirements:

- **Prompt rendering** reuses `src/template.rs` strictly (unknown variable/filter = error),
  with the same `issue.*` variables the main prompt has, plus a new `cycle.*` namespace:
  `cycle.id`, `cycle.stage`, `cycle.artifacts` (the artifact index from AIR-3) and
  `cycle.previous_stage_summary`.
- **Backend selection is per role.** A cheap role (Requirements) may run on a small model
  while Developer runs on the strong one — this is the roadmap's "use smaller models for
  simple tasks" (§7) prerequisite, without yet doing automatic tiering (AIR-22).
- **Tool restriction is backend-native, not a hardcoded flag.** Refactor SweBot's existing
  restriction into a shared `agent::ToolPolicy` that both SweBot and roles consume, so
  `claude` and `opencode` each translate the same policy their own way and a third backend
  only has to implement the translation once. `codex` must refuse to start a restricted role
  rather than silently running unrestricted — exactly the posture SweBot already takes.
- **Built-in default roles.** Ship sensible built-in prompts for the eight roadmap roles so
  a project can enable the pipeline with `role: reviewer` and no prompt file at all;
  a project-supplied `prompt_file` overrides the built-in.

## Implementation notes

- `src/config.rs`: `RoleConfig`, validated against `pipeline.stages[].role` at resolve time
  (a stage naming an undefined, non-built-in role is a config error, not a runtime surprise).
- New `src/roles/` with `mod.rs` (registry + resolution) and `builtin/*.md` embedded via
  `include_str!`.
- `src/agent/mod.rs`: add `ToolPolicy` to `AgentBackend::start_session`'s inputs; implement
  translation in `claude.rs` and `opencode.rs`; return a clear `AgentError` from `codex.rs`.
- Update `src/swebot/mod.rs` to build its restriction from `ToolPolicy` instead of its own
  inline flags — one mechanism, not two.

## Acceptance criteria

- [ ] A stage can select a different backend/model from `agent.backend` and it is actually
      used (assert on the constructed command line in a unit test).
- [ ] `allow_edits: false` produces the correct backend-native denial for `claude` and
      `opencode`, and a clean startup error for `codex`.
- [ ] SweBot's behaviour is unchanged after the refactor (its existing tests stay green).
- [ ] A stage referencing an unknown role fails config resolution with a helpful message.
- [ ] Built-in role prompts exist for all eight roadmap roles and render under `template.rs`'s
      strict mode with the documented `cycle.*` variables.
- [ ] AGENTS.md documents `roles:`, the built-ins, and the `ToolPolicy` translation table.

## Out of scope

- Automatic model tiering by task difficulty (AIR-22).
- Provider fallback (AIR-21).

## Global constraints (apply to every AIR ticket)

**1. Tiny code, small core of abstractions.** This feature must be expressible as a thin
layer over the abstractions Symphony already has (`TrackerAdapter`, `AgentBackend`/
`AgentSession`, `RepoHost`/`DiscussionHost`, the hook runner, the workspace manager, the
SQLite event log, the `status.rs` router). Before adding a concept, try to express the
feature with an existing one; if a new concept is unavoidable, introduce **one**, name it
plainly, and make it general enough that the next ticket reuses it rather than adding its
own. A reviewer must be able to read the new module top to bottom in one sitting and hold
it in their head. Concretely: no framework, no code generation, no trait with a single
implementation and no prospect of a second, no config key that only exists to toggle a
branch three call-levels down. If the implementation is growing past a few hundred lines
of genuinely new logic, that is the signal to simplify or split the ticket — say so in the
MR rather than shipping the bulk.

**2. Always ship a human-facing UI.** Every capability here must be observable *and*
actionable by a human without reading logs, tailing SQLite or parsing config. That means,
in the existing dashboard (`src/status.rs`, mounted `base_path`-aware so it works both under
the single-project `--port` mode and nested under `symphony serve`):
- a view showing the feature's current state and history, live-updating through the existing
  SSE `/fragment-stream` mechanism rather than a page-refresh hack;
- the human actions the feature implies (approve, override, retry, unblock, cancel, clear)
  as real controls — POST, admin-token protected, never a state-changing GET link;
- an explanation surface: whatever the feature decided, the UI must show *why* (the rule that
  fired, the inputs that produced a score, the evidence behind a verdict). An automated
  decision a human cannot interrogate is not usable governance.
Naming, layout and interaction should match the existing pages, so the dashboard stays one
coherent tool rather than a pile of feature panels.
