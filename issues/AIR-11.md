---
identifier: AIR-11
title: Token and cost budgets with enforced stopping conditions
state: todo
priority: 1
labels: [phase-1, cost, orchestrator]
dispatchable: true
depends_on: [AIR-1]
---
## Context

Roadmap §7: "apply token budgets, retry limits and stopping conditions", and §4's acceptance
criteria require **known token consumption and AI cost** before scaling. Symphony *measures*
tokens (`src/metrics.rs`, `/usage`, `src/eventlog.rs`) but enforces nothing: a stuck agent
burns budget until `agent.max_turns` or a stall timeout, and cost in currency is never
computed at all.

## Scope

**Budgets** at the four context levels the roadmap names (platform, application, agent,
cycle):

```yaml
budgets:
  currency: EUR
  platform:    {tokens: 500000000, cost: 20000}   # symphony serve, across all projects
  application: {tokens: 20000000,  cost: 800}     # this WORKFLOW.md, rolling window
  window: monthly                                  # daily | weekly | monthly
  cycle:       {tokens: 2000000,   cost: 40}
  stage:       {tokens: 400000}                    # default for every stage
  on_exceeded: stop                                # stop | escalate | warn
```

Per-stage overrides live on `pipeline.stages[].budget`.

**Pricing.** A `pricing:` table mapping `<backend>/<model>` → input/output/cache price per
million tokens, so cost is real currency, not a token proxy. Ship a table that a project can
override, and treat an unpriced model as cost `unknown` (surfaced as such) rather than 0.
Never hardcode prices deep in backend modules — one table, resolved at config time.

**Enforcement.** Checked after every turn, using the usage already flowing through
`AgentEvent`:

- `warn` → event + dashboard banner.
- `escalate` → park the cycle in the approval channel (AIR-5) asking a human to extend.
- `stop` → end the cycle cleanly: current turn finishes, `after_run` runs (never skip it —
  the work must be persisted), issue parks in `pipeline.blocked_state` with the reason.

**Stopping conditions beyond budget** — the roadmap groups these together, so implement them
here: `stop_conditions.no_progress_turns` (N consecutive turns with no workspace diff and no
tool calls that change state), `stop_conditions.repeated_error` (same error signature N times
in a row). Both currently manifest as an agent looping until `max_turns` while producing
nothing, which is the most common way real budget gets wasted.

## Implementation notes

- `src/budget.rs`: accounting against the SQLite event log (which already survives restarts),
  window rollup, enforcement decisions. Reuse `eventlog::usage_summary` rather than a second
  counter, so there is one source of truth.
- Respect the known accuracy caveat documented in AGENTS.md (tokens land only on a turn's
  final `result` event; a preempted turn reports 0) — budget checks must not assume every
  turn reports usage, and the caveat should be repeated in the new docs section.
- Dashboard: budget consumption bars per project/cycle on `/usage`.

## Acceptance criteria

- [ ] Cost is computed from the pricing table for every backend/model; unpriced models show
      `unknown`, never 0.
- [ ] Each of `warn` / `escalate` / `stop` behaves as specified; on `stop`, `after_run` still
      runs exactly once and the workspace is not lost.
- [ ] Budgets are enforced at stage, cycle and application level, with per-stage overrides.
- [ ] `no_progress_turns` and `repeated_error` stop a looping agent in a unit test.
- [ ] Consumption survives a restart and rolls up correctly over the configured window.
- [ ] No budgets configured → no behaviour change.

## Out of scope

- Choosing a cheaper model to stay in budget (AIR-22).

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
