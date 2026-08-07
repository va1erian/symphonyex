---
identifier: AIR-20
title: Technical-debt detection, register and prioritization
state: todo
priority: 2
labels: [phase-2, quality, knowledge]
dispatchable: true
depends_on: [AIR-17]
---
## Context

Roadmap §2 names Technical Debt as an engineering priority: "detect, document, measure and
prioritize debt by application and risk". Roadmap §4's guardrails open with "do not introduce
unmanaged technical debt", and §5 lists the review checklist and debt policy among the
knowledge the Orchestrator owns (the roadmap links Voyage Privé's own *Technical Debt Content
& Assessment Guidelines*). Symphony has no concept of debt: an agent that takes a shortcut
leaves no trace beyond a code comment nobody aggregates.

## Scope

**Detection, from two sources.**

1. *Introduced debt* — the Developer and Reviewer stages must declare it rather than hide it.
   A `debt_findings` artifact (AIR-3) is recorded whenever a cycle knowingly ships a shortcut:
   what was done, what should have been done, why, and the estimated cost of fixing it later.
   "Unmanaged" is the word the roadmap uses — declaring debt is allowed, hiding it is not.
2. *Existing debt* — a periodic scan (`debt.scan: weekly`) over the repository: TODO/FIXME/
   HACK markers with age from `git blame`, deprecated API usage, files with high churn ×
   complexity, dependency staleness/advisories (reuse AIR-8's scanners), untested modules
   (AIR-6 coverage), and duplicated patterns. Deterministic signals first; a model pass only
   to summarize and classify what the signals surfaced.

**Register.** A `debt` table (in the knowledge store, category `technical_debt`, so it is
retrievable by agents like everything else): `id`, `title`, `location`, `origin`
(`introduced:<cycle_id>` | `detected`), `category`, `impact`, `effort`, `risk`, `status`
(`open | accepted | scheduled | resolved`), `owner`, `first_seen`, `last_seen`, `evidence`.

**Prioritization.** A deterministic score from impact × risk ÷ effort, weighted by the
criticality map (AIR-19) — debt in `src/payment/**` outranks the same debt in a demo module.
The project can override weights; the formula must be documented and reproducible.

**Policy enforcement.** `debt.policy.max_new_per_cycle` and
`debt.policy.forbid_categories: [security, data-integrity]` — a cycle exceeding either
escalates through AIR-19 instead of merging. This is the mechanism that makes "do not
introduce unmanaged technical debt" a control rather than a slogan.

**Feedback into the backlog.** High-priority debt items can be promoted into real tickets via
the tracker adapter — reuse SweBot's existing drafting path (`src/swebot/drafting.rs`), which
already turns a rough idea into a properly scoped issue. Promotion is human-approved, never
automatic: a backlog silently filled by a scanner is a backlog nobody trusts.

## Acceptance criteria

- [ ] Cycles record introduced debt as an artifact and into the register with `origin` set.
- [ ] The periodic scan populates the register from all listed signals and is idempotent
      (`last_seen` updates, no duplicates).
- [ ] Prioritization is deterministic, documented and unit-tested, including criticality
      weighting.
- [ ] `max_new_per_cycle` / `forbid_categories` escalate through AIR-19.
- [ ] Debt entries are retrievable as knowledge by later cycles.
- [ ] Promotion to a ticket reuses the drafting path and requires human approval.
- [ ] A `/debt` dashboard view lists the register sorted by score, filterable by status.

## Out of scope

- Cross-application debt aggregation (AIR-25).

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
