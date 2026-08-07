---
identifier: AIR-1
title: Stage-pipeline engine — run a ticket through an ordered pool of specialized agents
state: done
priority: 1
labels: [phase-1, pipeline, orchestrator]
dispatchable: true
---
## Context

Today a ticket is dispatched to exactly one generalist agent (`orchestrator::dispatch_issue`
→ `run_agent_attempt` → `run_one_turn`), which loops up to `agent.max_turns` and then calls
`update_issue_state`. The AI Roadmap (§4) requires an **Agent Pool**: Requirements → Planner
→ Developer → Test → Reviewer → Security → Release → Observability, each with its own
responsibility, its own required output and its own exit criteria.

This ticket builds the engine only. The individual roles land in AIR-4 … AIR-10; without
them the engine must still behave exactly like today when no pipeline is configured.

## Scope

Add an optional `pipeline:` block to `WORKFLOW.md` front matter:

```yaml
pipeline:
  enabled: true
  stages:
    - id: requirements
      role: requirements       # role definition comes from AIR-2
      max_turns: 4
      on_failure: escalate     # escalate | retry | skip
    - id: implement
      role: developer
      max_turns: 20
    - id: review
      role: reviewer
      max_turns: 6
      blocking: true           # a failed exit criterion stops the cycle
```

A **delivery cycle** is one pass of one issue through the configured stages. The engine:

1. Creates the cycle when an issue is first dispatched, and keeps the *same* workspace
   across all stages (a stage boundary is not a workspace boundary — the Developer stage
   must see what the Planner produced).
2. Runs stages strictly in order, one agent session per stage, each with its own prompt,
   `max_turns` and (AIR-2) tool restrictions.
3. Records a `stage_started` / `stage_finished` event into `src/eventlog.rs` with the
   stage id, outcome, turns and token usage, so `/events` and `/usage` slice by stage.
4. Advances to the next stage only when the current stage reports success; `blocking: true`
   stages stop the cycle on failure and leave the issue in a non-active, non-terminal
   state (`pipeline.blocked_state`, default `"blocked"`) rather than redispatching forever
   — the same convention `repo.pull_request` already documents in AGENTS.md.
5. Applies `on_failure` per stage: `retry` re-runs the stage within `agent.max_retry_backoff_ms`
   budget, `skip` moves on and records the skip as evidence, `escalate` stops the cycle.

## Implementation notes

- Config: extend `src/config.rs` with `PipelineConfig` / `StageConfig`, resolved in
  `resolve()` and carried on `EffectiveConfig`. Unknown keys must still error the way the
  rest of `resolve()` does; absent `pipeline:` must resolve to `enabled: false`.
- Engine: a new `src/pipeline.rs` owning the stage loop, called from
  `orchestrator::run_attempt_body`. When `pipeline.enabled` is false, `run_attempt_body`
  keeps its current single-role behaviour untouched.
- Cycle identity: `cycle_id` = `<workspace_key>-<attempt_seq>`; thread it through
  `AgentEvent` / `NewEvent` so every event is attributable to a cycle *and* a stage.
- Reconciliation (`orchestrator::reconcile`, `reconcile_stalled`) must remain correct:
  a stall timeout kills the current stage, not silently the whole pipeline, and
  `after_run` must still run exactly once when the cycle ends (see the AGENTS.md note on
  the reconciliation-preempts-mid-turn race — do not regress it).

## Acceptance criteria

- [ ] `pipeline:` absent → behaviour, events and report output are byte-identical to today.
- [ ] `pipeline.enabled: true` with two trivial stages runs both, in order, in one workspace,
      and records per-stage events.
- [ ] A `blocking: true` stage failing leaves the issue in `pipeline.blocked_state` and the
      orchestrator does not redispatch it on the next tick.
- [ ] Stall detection kills only the running stage and the cycle is resumable/abortable
      cleanly; `after_run` still runs exactly once.
- [ ] Unit tests in `src/pipeline.rs` cover: ordering, blocking failure, `on_failure` modes,
      and the disabled-by-default path. `cargo test` and `cargo clippy --all-targets` clean.
- [ ] AGENTS.md gains a `## Delivery pipeline` section documenting the block and the
      "no pipeline configured = old behaviour" guarantee.

## Out of scope

- The role definitions themselves (AIR-2) and any specific agent (AIR-4 … AIR-10).
- Durable/resumable cycle state across a daemon restart (AIR-13).

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
