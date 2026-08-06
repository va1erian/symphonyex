---
identifier: AIR-18
title: Knowledge publication gate — orchestrator and technical-owner validation
state: todo
priority: 2
labels: [phase-2, knowledge, governance]
dispatchable: true
depends_on: [AIR-17]
---
## Context

Roadmap §5: *"Concurrent agents can read shared knowledge. Publication requires validation by
the Orchestrator and the responsible technical owner."* Without this, AIR-17's enrichment loop
is a machine for laundering one agent's guess into every future agent's context — a wrong
"approved pattern" written once gets cited forever.

## Scope

**Two-stage validation** for any `draft` entry:

1. **Orchestrator validation (automatic).** Deterministic checks before a human ever looks:
   the entry cites at least one concrete source (artifact, commit, file, incident); it does
   not contradict a published entry without an explicit `supersedes`; it is not a near-duplicate
   of an existing entry (reuse the FTS index for similarity); its category is one a cycle is
   allowed to propose (a cycle must never propose new *security rules* or *debt policy* on its
   own — those are owned upstream). Failures are rejected with a reason, not queued to a human.

2. **Technical-owner approval (human).** Surviving drafts route to an owner resolved from
   `knowledge.owners:` (category → owner) and CODEOWNERS for cited files. Approval flows
   through the same channel as AIR-5: `/knowledge/pending` in the dashboard (admin-token,
   POST) or a `/approve-knowledge <id>` comment on the source ticket. Approve publishes,
   edit-and-approve publishes the edited body, reject records the reason and keeps the draft
   for context.

**Batching, so this isn't a new bottleneck.** Drafts are digested per period
(`knowledge.review_digest: weekly`) into one review item, not one interrupt per cycle. If
nobody reviews within `knowledge.draft_ttl_days`, drafts expire rather than accumulating —
an unreviewed backlog of 400 drafts helps no one.

**Auditability.** Publication, edit, rejection and deprecation are append-only events with
actor, timestamp and diff of the body. A published entry always answers "who approved this,
when, based on what evidence" — which is what makes it safe for agents to cite.

**Read path unchanged.** Retrieval (AIR-17) only ever serves `published` entries to agents;
drafts are visible to humans and to the extraction pass, never injected into prompts.

## Implementation notes

- `src/knowledge/publication.rs`, reusing `src/approvals.rs` (AIR-5) rather than building a
  second approval mechanism — one approval inbox for plans, overrides and knowledge.
- Owner resolution must degrade gracefully: no owner configured → route to the project's
  default approver, and say so in the UI.

## Acceptance criteria

- [ ] Uncited, contradicting, duplicate and forbidden-category drafts are auto-rejected with
      specific reasons.
- [ ] Surviving drafts route to the correct owner via config and CODEOWNERS.
- [ ] Approve / edit-and-approve / reject all work from both dashboard and comment, and are
      fully audited with body diffs.
- [ ] Agents never receive `draft` entries (assert on rendered prompt content).
- [ ] Digest batching and TTL expiry work as configured.

## Out of scope

- Federated knowledge sharing between applications (AIR-25).

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
