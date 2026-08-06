---
identifier: AIR-3
title: Cycle artifact store and the record_artifact agent tool
state: todo
priority: 1
labels: [phase-1, evidence, mcp]
dispatchable: true
depends_on: [AIR-1]
---
## Context

Every roadmap role has a **required output** ("validated requirements and acceptance
criteria", "engineer-approved delivery plan", "security evidence, risk classification and
blocking findings"). Symphony can currently attach *images* to a PR (`attach_evidence`) and
nothing else. There is no structured, machine-readable place for a stage's output — so no
downstream stage can consume it, and no evidence bundle can be assembled (AIR-9).

## Scope

A per-cycle artifact store plus one new host-mediated agent tool.

**Storage.** Artifacts live in SQLite (`symphony.db`, a new `artifacts` table) with the blob
body on disk under `<workflow_dir>/.symphony/cycles/<cycle_id>/<artifact_id>.<ext>`. SQLite
holds: `cycle_id`, `issue_identifier`, `stage_id`, `kind`, `content_type`, `path`, `sha256`,
`created_at`, `summary`. Content-hashed like `attach_evidence` already does, so a retry never
collides.

**Tool.** `record_artifact({kind, content_type, path | content, summary})`, exposed the same
way `update_issue_state` and `open_pull_request` are — merged into the tool list by
`mcp::run_stdio_server`, executed in the `__mcp_tool_server` subprocess with the
container-aware `--workspace-dir` mapping (see AGENTS.md "Resolving the image path"; reuse
that plumbing, do not invent a second one). Agents never write to `symphony.db` directly.

**Known kinds** (validated; unknown kinds are accepted but flagged in the report):
`requirements`, `acceptance_criteria`, `plan`, `test_report`, `coverage`, `review_findings`,
`security_findings`, `telemetry_evidence`, `debt_findings`, `screenshot`.

**Reading.** Artifacts recorded by earlier stages are exposed to later stages through the
`cycle.artifacts` template namespace (AIR-2) as an index of `{kind, stage, summary, path}`,
and the *content* is readable by the agent because the file also lands in the workspace at
`.symphony/artifacts/<kind>.<ext>` — same workspace across stages (AIR-1).

**Schema for structured kinds.** `requirements`, `acceptance_criteria`, `review_findings`,
`security_findings` and `debt_findings` are JSON with a versioned schema (`schema_version`
field) validated on record; a schema violation returns a `ToolResult::error` telling the
agent exactly what was wrong so it can retry, rather than silently storing junk.

## Implementation notes

- `src/artifacts.rs`: store, schema validation, retrieval helpers.
- `src/eventlog.rs`: new table + migration; keep the existing "open, migrate, keep going"
  pattern so an old `symphony.db` upgrades in place.
- `src/mcp.rs` + `src/tracker/mod.rs`'s `ToolSpec`/`ToolResult`: register the tool
  independently of tracker kind (like `open_pull_request` — it belongs to the *cycle*, not
  the tracker), gated on `pipeline.enabled`.
- `src/status.rs`: an `/artifacts` view listing artifacts by issue and cycle, and a
  per-artifact raw view. Reuse the existing `base_path` prefixing so it nests correctly
  under `symphony serve`.

## Acceptance criteria

- [ ] `record_artifact` appears in the agent's tool list only when the pipeline is enabled,
      and works for both `claude` and `opencode`, in Docker mode and out of it.
- [ ] Recording a JSON artifact with an invalid schema returns an actionable tool error and
      stores nothing.
- [ ] A second stage can read a first stage's artifact both via `cycle.artifacts` in its
      prompt and via the file in the workspace.
- [ ] Artifacts survive a restart and are browsable at `/artifacts`.
- [ ] Unit tests: store round-trip, hash collision-avoidance on retry, schema rejection,
      migration from a pre-existing `symphony.db`.

## Out of scope

- Assembling the artifacts into a PR-facing evidence bundle (AIR-9).
- Promoting artifacts into long-lived application knowledge (AIR-17).

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
