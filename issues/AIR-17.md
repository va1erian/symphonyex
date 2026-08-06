---
identifier: AIR-17
title: Application knowledge base — context database enriched by every cycle
state: todo
priority: 1
labels: [phase-2, knowledge, context]
dispatchable: true
depends_on: [AIR-13]
---
## Context

Roadmap §5, "Application Knowledge and Documentation", is the most load-bearing part of
Step 2: the Orchestrator controls the knowledge its Agent Pool uses — business rules,
architecture decisions and approved patterns, coding standards and repository structure,
APIs/dependencies/ownership, data models, review checklist and debt policy, testing strategy
and critical paths, security rules, technical debt and known limitations, SLOs/incidents/
runbooks, and production/release history — with the explicit call for *"creation of a context
database which could be enriched in every single workflow execution."*

Symphony's entire context today is the `WORKFLOW.md` prompt body, re-sent verbatim every
turn. Nothing an agent learns in one cycle is available to the next.

## Scope

**Store.** A `knowledge` table in `symphony.db`: `id`, `category` (the roadmap's list above as
an enum), `title`, `body` (Markdown), `source` (`seed | cycle:<id> | human`), `status`
(`draft | published | deprecated`), `owner`, `confidence`, `created_at`, `updated_at`,
`supersedes`. Bodies are Markdown so a human can read and edit them; the schema is what makes
them retrievable.

**Seeding.** `knowledge.seed_paths: [docs/**/*.md, ARCHITECTURE.md]` ingests existing repo
documentation at startup (and on change), chunked per heading, categorized by a cheap model
pass, marked `source: seed`. A project with good docs gets a useful knowledge base on day one
instead of waiting for cycles to produce one.

**Retrieval.** Before each stage, select relevant entries and inject them into the prompt
through the `cycle.knowledge` template namespace (AIR-2). Retrieval is category-filtered per
role (Security gets security rules and past security findings; Developer gets coding
standards, repo structure and approved patterns) and then relevance-ranked. Start with
deterministic lexical ranking (BM25-style over SQLite FTS5 — `rusqlite` is already a
dependency); design the trait so an embedding backend can be added later without touching
callers. **Hard token cap per stage** (`knowledge.max_tokens`, default modest): retrieval must
never quietly become the biggest line in the budget (AIR-11).

**Enrichment.** At cycle end, a knowledge-extraction pass proposes new or updated entries from
the cycle's artifacts: architecture decisions from the plan, new patterns from the diff,
limitations and debt from review/security findings, incident and release history from AIR-10.
Proposals land as `status: draft` — never auto-published (that gate is AIR-18).

**Conflict and staleness.** A proposal contradicting a published entry must link it via
`supersedes` and surface the contradiction for the reviewer rather than silently overwriting.
Entries whose cited files no longer exist are flagged stale on a periodic sweep.

## Implementation notes

- `src/knowledge/`: store, seeding, retrieval trait + FTS implementation, extraction pass.
- Retrieval must be fast enough to run per stage; index maintenance goes in the same
  migration pattern the rest of `symphony.db` uses.
- Dashboard `/knowledge`: browse, filter by category/status, view drafts and their source
  cycle.

## Acceptance criteria

- [ ] Seeding ingests configured paths, chunks per heading and categorizes; re-running is
      idempotent.
- [ ] Retrieval returns category-appropriate entries per role, is deterministic for a given
      corpus and query, and never exceeds `knowledge.max_tokens`.
- [ ] A completed cycle proposes draft entries traceable to their source artifacts.
- [ ] A contradicting proposal links `supersedes` and is surfaced, not silently applied.
- [ ] Stale-entry detection flags entries citing deleted files.
- [ ] Knowledge disabled by default; enabled, it measurably changes prompt content in a test.

## Out of scope

- Publication approval (AIR-18); cross-application knowledge (AIR-25).

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
