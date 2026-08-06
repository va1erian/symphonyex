---
identifier: AIR-12
title: DORA and success-measure metrics (flow, quality, autonomy, economics, reliability)
state: todo
priority: 1
labels: [phase-1, metrics, observability]
dispatchable: true
depends_on: [AIR-11]
---
## Context

Roadmap §11 defines the five dimensions the whole programme is judged on: **Flow**
(Deployment Frequency, Lead Time for Change, Change Failure Rate), **Quality** (CFR,
first-pass acceptance, rework, production fixes), **Autonomy** (% orchestrated changes, human
interventions, escalations, successful parallel cycles), **Economics** (tokens per accepted
change, AI cost per deployment, provider efficiency), **Reliability** (rollbacks, incidents,
recovery time, repeated failures). Symphony reports turns, tool calls and tokens. None of the
above exists, and without it there is no way to prove Step 1's "measurable delivery-time
improvement" acceptance criterion.

## Scope

A metrics layer computing every roadmap measure from data Symphony already has or gains in
Phase 1, plus an export surface.

**Definitions to implement** (document each precisely — a metric nobody can reproduce is
worse than none):

| Measure | Source |
| --- | --- |
| Deployment Frequency | merged MRs per period from the repo host, filtered to Symphony-authored branches (`issue-*`) |
| Lead Time for Change | first dispatch of the issue → merge timestamp |
| Change Failure Rate | merges followed by a `degraded` production validation (AIR-10), a revert, or a linked incident ticket, over total merges |
| First-pass acceptance | cycles reaching MR with zero `request_changes` rounds (AIR-7) |
| Rework | AIR-7 rework rounds per cycle; also human-requested changes on the MR |
| Human interventions | approvals, overrides and clarifications answered (AIR-4, AIR-5, AIR-8) |
| Escalations | cycles ending in `escalate` / blocked |
| Orchestrated share | Symphony-authored merges over all merges to `default_branch` |
| Tokens per accepted change | cycle tokens over merged cycles (AIR-11) |
| AI cost per deployment | cycle cost over deployments |
| Provider efficiency | accepted-change rate and cost per backend/model |
| Reliability | reverts, repeated failures on the same issue, recovery time from `degraded` to `healthy` |

**Surfaces:**

1. `/metrics` in Prometheus text format on the existing status router (no new dependency —
   render it directly), so the Open Observability Platform can scrape Symphony itself.
2. A `/insights` dashboard page grouping the five dimensions, with a period selector.
3. The existing `symphony-report.html` gains a summary block at the top.
4. `symphony metrics --format json --since <date>` for offline reporting.

**Data gaps.** Anything not derivable (incidents, rollbacks not visible to the code host) is
reported as `unknown` with the reason, and the config gains an optional
`metrics.incident_source` hook command a project can supply. Do not fabricate a number to
fill a cell.

## Implementation notes

- `src/insights/` computing from `symphony.db` only, so a restart never loses history and the
  same computation backs every surface.
- Merge/deploy data needs a small poll against the code host; reuse `RepoHost` and cache it —
  do not re-query per page render.
- Every metric gets a doc comment with its exact formula and the same text on the dashboard
  as a tooltip.

## Acceptance criteria

- [ ] All measures in the table compute from a seeded test database with known expected values.
- [ ] `/metrics` output parses as valid Prometheus text and carries `HELP`/`TYPE` lines.
- [ ] `/insights` renders the five dimensions and a period selector, nested correctly under
      `symphony serve`'s `base_path`.
- [ ] `symphony metrics --format json` matches the dashboard numbers for the same period.
- [ ] Non-derivable measures report `unknown` with a reason; nothing is fabricated.

## Out of scope

- Cross-application aggregation across orchestrators (AIR-25).

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
