---
identifier: AIR-22
title: Model tiering and result caching for cost control
state: todo
priority: 2
labels: [phase-3, provider, cost]
dispatchable: true
depends_on: [AIR-21, AIR-11]
---
## Context

Roadmap §7's token and cost controls: *"use smaller models for simple tasks"* and *"cache
repetitive results"*, alongside the budgets already built in AIR-11. Roadmap §11 measures
Economics as tokens per accepted change, AI cost per deployment and provider efficiency.
AIR-2 lets a role pin a model statically; this ticket makes the choice adaptive and stops
Symphony paying twice for the same answer.

## Scope

**Tiering.** Declare tiers once, per provider, and let stages resolve into them:

```yaml
tiers:
  small:  {claude: <small-model>, opencode: <provider/small-model>}
  large:  {claude: <large-model>, opencode: <provider/large-model>}
routing:
  default_tier: large
  rules:
    - when: {role: [requirements, reviewer]}         then: {tier: small}
    - when: {diff_lines_below: 50, risk: low}        then: {tier: small}
    - when: {rework_round_above: 1}                  then: {tier: large}   # escalate on failure
    - when: {budget_remaining_below_percent: 20}     then: {tier: small}
```

Deterministic, first-match-wins, same evaluation shape as AIR-19's policies — reuse that
evaluator rather than writing a second rule engine. **Escalation on failure is mandatory**: a
stage that fails or produces an unusable output on `small` retries once on `large` before
being called a failure, otherwise tiering trades cost for rework and wins nothing.

**Caching.** Three layers, all keyed on content hashes and all optional:

1. *Prompt-prefix caching* — where a provider supports it natively (system prompt, knowledge
   block, repository context), mark the stable prefix so the provider's own cache applies.
   This is the cheapest win and requires no storage of our own.
2. *Stage-result cache* — key: (role, prompt hash, workspace tree hash, artifact-input hashes).
   A re-run with identical inputs returns the previous artifacts instead of re-invoking a
   model. This is exactly what happens on a resume (AIR-13) or a retried scan, and it must be
   opt-in per role: cacheable for read-only analysis roles (Reviewer, Security, Requirements),
   never for roles that mutate the workspace.
3. *Deterministic-tool-output cache* — scanner and test outputs keyed on tree hash + command.

Every cache hit is an event with the key, and the report/`/insights` shows hit rate and
estimated tokens saved. A cache whose effect nobody can measure is a cache nobody should trust.

**Invalidation.** Any change to the role prompt, tier, provider, workspace tree or input
artifacts invalidates. TTL (`cache.ttl_hours`) bounds staleness; `symphony cache clear` and a
dashboard button exist because sooner or later someone will need them.

## Acceptance criteria

- [ ] Routing rules resolve stages to tiers deterministically and are unit-tested, including
      budget-pressure and rework-escalation rules.
- [ ] A `small`-tier failure retries once at `large` and the escalation is recorded.
- [ ] Stage-result caching returns identical artifacts for identical inputs, never fires for
      mutating roles, and invalidates on each listed input change.
- [ ] Cache hit rate and estimated savings appear on `/insights` and in the HTML report.
- [ ] Per-tier cost is attributed correctly in AIR-11's accounting.
- [ ] Tiering and caching both default to off; unchanged behaviour when absent.

## Out of scope

- Provider quality comparison (AIR-26).

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
