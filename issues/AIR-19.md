---
identifier: AIR-19
title: Risk-based human escalation policy engine
state: todo
priority: 1
labels: [phase-2, governance, orchestrator]
dispatchable: true
depends_on: [AIR-14, AIR-5]
---
## Context

Roadmap §5: the Orchestrator must "enforce architecture, quality, security and cost controls"
and "trigger human intervention **based on risk**". Roadmap §3: human approval is required for
architectural, business-critical and high-risk decisions. Roadmap §11 measures autonomy as
*reduced* human interventions — so escalation must be selective, not blanket. AIR-5 gives a
per-stage `requires_approval` flag; that is a static switch, not a risk assessment.

## Scope

**Risk scoring.** Compute a risk level per cycle, continuously, from signals Phase 1 and 2
already produce: security classification (AIR-8), impacted components against a configured
criticality map, blast radius (files/lines changed, public API or schema/migration touched),
test coverage delta on changed code (AIR-6), review findings by severity (AIR-7), conflict
overlap (AIR-15), budget overrun (AIR-11), rework rounds, and the change's proximity to known
incident history (AIR-17 knowledge).

**Policy, declared not hardcoded.** A rule set evaluated deterministically:

```yaml
governance:
  criticality:
    "src/payment/**": critical
    "migrations/**": high
  policies:
    - when: {security_risk: [high, critical]}
      then: {action: block, notify: security-owners}
    - when: {touches_criticality: [critical]}
      then: {action: require_approval, stage: plan}
    - when: {coverage_delta_below: -5}
      then: {action: require_approval, stage: release}
    - when: {risk: low, all_checks_pass: true}
      then: {action: auto_proceed}
```

Actions: `auto_proceed`, `require_approval` (at a named stage, via AIR-5's channel),
`block`, `notify`. First matching rule wins, evaluation order is the declared order, and the
matched rule id is recorded on the cycle — an escalation must always be explainable by
pointing at the exact rule that fired.

**Notification.** Reuse existing surfaces only: a comment on the ticket/MR through
`RepoHost`, plus the dashboard. No new notification integrations in this ticket.

**Measurement feedback.** Every escalation records outcome (approved unchanged / changed /
rejected). A rule that fires often and is always approved unchanged is noise; surface a
"rule effectiveness" table on `/insights` (AIR-12) so policies get tuned by evidence rather
than by feeling.

## Implementation notes

- `src/governance.rs`: signal collection, scoring, rule evaluation. Pure functions over a
  signals struct — this must be trivially unit-testable and must not call a model. Risk
  policy is a control, not a judgement call delegated to the thing being controlled.
- Defaults: no policies configured → the AIR-5 static flags govern, unchanged.

## Acceptance criteria

- [ ] Risk signals are collected from all listed sources and exposed on the cycle view.
- [ ] Rule evaluation is deterministic, first-match-wins, and unit-tested across a matrix of
      signal combinations including conflicting rules.
- [ ] Each action produces the specified behaviour, routed through the existing approval and
      blocking machinery.
- [ ] The rule that caused an escalation is recorded and displayed.
- [ ] Rule-effectiveness stats are computed and shown on `/insights`.
- [ ] No policies configured → behaviour unchanged.

## Out of scope

- Cross-domain risk (infrastructure, data) — Phase 3.

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
