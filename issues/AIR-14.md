---
identifier: AIR-14
title: Agent pool scaling and workload-aware scheduling
state: todo
priority: 2
labels: [phase-2, orchestrator, scheduling]
dispatchable: true
depends_on: [AIR-13]
---
## Context

Roadmap §5: the Application Orchestrator must "create and assign work to agent instances",
"sequence or parallelize agent activities", "scale Agent Pools according to workload" and
"monitor handoffs, failures and blocked states". Symphony has a flat
`agent.max_concurrent_agents` plus per-state limits (`max_concurrent_agents_by_state`) and
sorted candidate selection — a fixed ceiling, no awareness of what a stage actually costs or
of host resources.

## Scope

**Pool model.** Concurrency limits become per-role, not just global: a Reviewer stage is cheap
and read-only while a Developer stage may hold a container and a full build. Config:

```yaml
pools:
  developer:   {max_concurrent: 3, weight: 4}
  reviewer:    {max_concurrent: 6, weight: 1}
  security:    {max_concurrent: 2, weight: 2}
  total_weight: 12          # global admission control across all pools
```

Admission uses the weight sum, so three developers and six reviewers can coexist while twelve
developers cannot. The existing `max_concurrent_agents` remains the fallback when `pools:` is
absent.

**Workload-aware autoscaling.** `pools.autoscale: true` adjusts effective per-pool limits
between `min`/`max` from observed signals: queue depth per role, host load
(CPU/memory/available disk for workspaces), running-container count in Docker mode, and
budget headroom (AIR-11 — scaling up into an exhausted budget is pointless). Scale decisions
are recorded as events with the inputs that produced them, so a surprising scale-down is
explainable rather than mysterious.

**Scheduling.** Replace pure `created_at` ordering with a scoring function over: issue
priority, blocked-dependency readiness (`depends_on` already exists in
`src/tracker/depends_on.rs`), age, cycle stage (finishing an in-flight cycle beats starting a
new one — WIP limits are how throughput actually improves), and rework rounds. The function
must be deterministic and unit-testable; ties break on `created_at` for stability.

**Handoff and stuck-state monitoring.** A watchdog reports cycles that have sat in one state
past a configurable threshold (`pools.stuck_after_ms`), distinguishing *waiting on a human*
(approval, clarification — expected, not an alarm) from *stuck on nothing*, and surfaces the
latter on the dashboard and as a metric for AIR-12.

## Implementation notes

- `src/scheduler.rs`: admission control + scoring, consumed by `orchestrator::on_tick`'s
  candidate selection. Keep the orchestrator's single-authority property — the scheduler
  advises, the orchestrator dispatches.
- Host signals behind a small trait with a no-op implementation, so autoscaling degrades to
  static limits where signals are unavailable rather than misbehaving.

## Acceptance criteria

- [ ] Per-pool and total-weight admission are enforced; `pools:` absent → today's behaviour.
- [ ] The scoring function is deterministic and unit-tested against a fixture backlog,
      including WIP-preference and dependency-readiness cases.
- [ ] Autoscaling moves limits within bounds from simulated signals and records each decision
      with its inputs; exhausted budget prevents scale-up.
- [ ] The watchdog distinguishes waiting-on-human from genuinely stuck and both are visible.
- [ ] Concurrency invariants hold under a stress test (no pool ever exceeds its limit).

## Out of scope

- Remote/SSH workers.

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
