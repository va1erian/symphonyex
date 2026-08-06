---
identifier: AIR-5
title: Planner/Architecture agent and the human approval gate
state: todo
priority: 1
labels: [phase-1, agent-role, governance]
dispatchable: true
depends_on: [AIR-4]
---
## Context

Roadmap §4: the Planner and Architecture Agent produces the technical design, dependency
analysis, implementation tasks, MR sequence and test strategy — and its required output is
an **engineer-approved** delivery plan. Roadmap §3: "Human approval is required for
architectural, business-critical and high-risk decisions." Symphony has no approval channel
at all today (AGENTS.md, "Trust and safety posture": a run that would need interactive
approval fails rather than stalling). This ticket adds the first real gate.

## Scope

**Planner stage.** Consumes the `requirements` + `acceptance_criteria` artifacts and records
a `plan` artifact (JSON + a rendered Markdown sibling for humans):

```json
{
  "schema_version": 1,
  "design_summary": "...",
  "impacted_components": ["src/orchestrator.rs", "..."],
  "tasks": [{"id": "T1", "title": "...", "requirement_ids": ["R1"], "estimate_turns": 6}],
  "mr_sequence": [{"mr": 1, "task_ids": ["T1","T2"], "depends_on_mr": []}],
  "test_strategy": {"unit": "...", "integration": "...", "e2e": "..."},
  "risks": [{"description": "...", "severity": "high", "mitigation": "..."}],
  "architecture_decisions": [{"decision": "...", "alternatives": ["..."], "rationale": "..."}]
}
```

Delivery guardrails from §4 are part of the prompt rubric: small and reversible changes,
no unmanaged technical debt, stop when unclear.

**Approval gate.** New `pipeline.stages[].requires_approval: true`. When set, the cycle halts
after the stage, the issue moves to `pipeline.awaiting_approval_state` (default
`"awaiting approval"` — non-active, non-terminal, so the dispatcher leaves it alone), and an
approval request is recorded. Approval can arrive through either channel:

1. **Dashboard** — `/approvals` lists pending requests with the rendered plan, and
   Approve / Request changes / Reject buttons. Because this mutates state it must be
   protected by the same admin token `symphony serve` already uses, and must be a POST with
   a CSRF-safe form, not a GET link.
2. **Tracker/repo comment** — a human replying `/approve` or `/changes <reason>` on the
   issue thread, detected by the existing marker-scanning poll (`src/swebot/`), so an
   engineer never has to open the dashboard.

Approve resumes the cycle at the next stage; "request changes" re-runs the planner stage with
the reviewer's comment injected into the prompt; reject terminates the cycle with a recorded
reason. Every decision is written to the event log with who/when/what — this is the
"decision traceability" acceptance criterion of roadmap §4.

**Auto-approval policy.** `pipeline.approval.auto_approve_when:` with conditions
(`risk: low`, `impacted_components` all inside a configured allowlist, `estimate_turns <= N`)
so low-risk tickets don't need a human — the roadmap's autonomy measure is *reduced* human
interventions, not zero governance. Defaults to never auto-approving.

## Implementation notes

- `src/approvals.rs`: pending-approval table in `symphony.db`, resolution API, audit rows.
- `src/status.rs`: `/approvals` GET + POST, admin-token gated, `base_path`-aware.
- The awaiting state must be a first-class pause: verify the orchestrator does not redispatch
  and does not count the issue against `max_concurrent_agents` while parked.
- Approvals must survive a restart (they live in SQLite, not the in-memory status snapshot).

## Acceptance criteria

- [ ] A `requires_approval` stage parks the cycle; nothing is redispatched; the worker slot
      is released.
- [ ] Approving via dashboard **and** via issue comment both resume the cycle at the next stage.
- [ ] "Request changes" re-runs the planner with the human's comment in context.
- [ ] Every approval decision is in the event log with actor, timestamp and outcome.
- [ ] `auto_approve_when` works and defaults to off.
- [ ] Approvals survive a daemon restart.

## Out of scope

- Risk-based routing of *which* stages need approval by policy engine (AIR-19) — here the
  gate is declared per stage.

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
