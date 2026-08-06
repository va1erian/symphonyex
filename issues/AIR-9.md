---
identifier: AIR-9
title: Release agent — evidence bundle, traceability manifest and deployment-ready MR
state: todo
priority: 1
labels: [phase-1, agent-role, evidence, release]
dispatchable: true
depends_on: [AIR-8]
---
## Context

Roadmap §4: the Release Agent consolidates the MR and release evidence before CI/CD
execution, output "traceable MR and deployment readiness". Roadmap §4 acceptance criteria
demand "complete evidence and decision traceability". Symphony today opens a PR whose body is
whatever the coding agent felt like writing, with optional screenshots attached.

## Scope

**Evidence bundle.** At the end of a cycle, assemble every artifact into a single
`evidence_bundle` (JSON + rendered Markdown) containing:

- the requirements and acceptance criteria, each with its verdict,
- the approved plan and who approved it, when, and any auto-approval rule that fired,
- test results, coverage totals and pre-existing-failure baseline,
- review findings and how each was resolved (fixed / accepted with reason),
- security checklist, risk classification and any human override with justification,
- token and cost totals for the cycle (AIR-11),
- the full stage timeline with timestamps.

**Traceability manifest.** A matrix `R* → AC* → test → commit → finding`, rendered into the
MR body as a compact table. Any row missing a link is shown as a gap rather than omitted —
the point of the manifest is to make gaps visible, not to look complete.

**MR consolidation.** Extend the existing `open_pull_request` path (`src/repo_host/*`) so the
PR/MR body is generated from the bundle (agent narrative first, then the evidence sections),
rather than being purely agent-authored. Existing behaviour when the pipeline is off must not
change. Large artifacts are attached via the existing content-hashed
`.symphony/evidence/`-style upload (reuse `attach_evidence`'s plumbing) and linked, not
inlined — an MR body with a 4000-line coverage dump helps nobody.

**Deployment readiness.** A `ready | blocked | ready_with_risk` verdict computed
deterministically from the artifacts (not asked of a model): blocked if any blocking security
finding or unmet blocking acceptance criterion stands; `ready_with_risk` if there are
documented assumptions, accepted findings or coverage gaps. The verdict goes in the MR body
and into the event log.

**CI/CD stays CI/CD.** Per the roadmap guardrail "delegate build, deployment and rollback to
CI/CD", this stage never deploys and never merges. It prepares and labels.

## Implementation notes

- `src/release.rs`: bundle assembly, manifest computation, Markdown rendering (reuse
  `pulldown-cmark` only for HTML views; the MR body is Markdown).
- Persist the bundle as an artifact so it survives even if the MR is later closed.
- Add an `/evidence/<cycle_id>` dashboard view rendering the same bundle.
- Respect AIR-8's redaction rules when rendering — nothing redacted may reappear here.

## Acceptance criteria

- [ ] A complete cycle produces a bundle containing every section above, and the MR body is
      generated from it.
- [ ] The traceability matrix shows gaps explicitly.
- [ ] The readiness verdict is computed deterministically and unit-tested across
      ready/blocked/ready_with_risk inputs.
- [ ] Oversized artifacts are linked, not inlined; the MR body stays under the host's size
      limit (test with a synthetic large coverage artifact).
- [ ] Pipeline-off behaviour of `open_pull_request` is unchanged.
- [ ] Nothing redacted by AIR-8 appears in the bundle or MR body.

## Out of scope

- Ordering merges across multiple parallel MRs (AIR-16).

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
