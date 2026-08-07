---
identifier: AIR-25
title: Application cartography and platform impact analysis
state: todo
priority: 2
labels: [phase-3, federation, architecture]
dispatchable: true
depends_on: [AIR-24, AIR-17]
---
## Context

Roadmap §1, Stage 3 promises "cross-domain coordination, platform impact analysis,
application cartography and technical-debt tracking" as the concrete result of federation.
Each Symphony instance already accumulates, per application, what it touches, what it depends
on, what debt it carries and how it behaves in production (AIR-15, AIR-17, AIR-20, AIR-10).
Cartography is that data, joined across peers.

## Scope

**Application model.** One record per application, built from data already collected — not a
new manual inventory nobody maintains: repositories and their structure, exposed APIs and
consumed dependencies (from the knowledge base's API/dependency categories), owners, data
models touched, criticality (AIR-19's map), SLOs and incident history (AIR-10), debt score
(AIR-20), and DORA metrics (AIR-12).

**Cartography.** Peers exchange their application models over the AIR-23 contract
(`GET /api/v1/cartography`, added here). Every instance builds the same platform-level graph:
applications as nodes; API calls, shared data models and delegation history as edges. The
graph is derived and rebuildable — never hand-edited, so it cannot rot into fiction.

**Impact analysis.** Given a proposed change (the Planner's `impacted_components`, AIR-5),
answer: which other applications consume what is changing, which owners must be informed,
which SLOs and critical paths are downstream, and whether this has caused incidents before.
The answer is injected into the Planner and Reviewer stages as context and attached to the
evidence bundle — an impact analysis nobody reads at the right moment is decoration.

**Platform views.** Aggregate debt, DORA and risk across applications, with drill-down to the
owning instance. This is the platform-level picture the roadmap's Stage 3 is for.

**Honesty about coverage.** The graph only knows what participating instances report. Any
view must state its coverage (N of M known applications reporting, last refresh per peer) and
mark unknown regions as unknown. A cartography that silently omits half the platform is worse
than none, because people will trust it.

## Acceptance criteria

- [ ] Application models are derived automatically from existing data, with no manual inventory.
- [ ] Peers exchange models over the versioned contract; a stale or unreachable peer is shown
      as stale, and its data is never presented as current.
- [ ] Impact analysis returns consumers, owners, downstream SLOs and prior incidents for a
      proposed change, and is injected into Planner/Reviewer context and the evidence bundle.
- [ ] Platform aggregates for debt, DORA and risk are consistent with each instance's own
      `/insights` numbers for the same period.
- [ ] Every view states its coverage and refresh recency.
- [ ] Feature off by default; single-instance operation is unaffected.

## Out of scope

- Automated remediation of anything cartography reveals — it informs humans and agents,
  it does not act.

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
