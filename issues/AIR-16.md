---
identifier: AIR-16
title: Merge-request sequencing — ordered approval and integration without regression
state: todo
priority: 1
labels: [phase-2, orchestrator, release]
dispatchable: true
depends_on: [AIR-15, AIR-9]
---
## Context

Roadmap §5 poses this as an open question, verbatim: *"how to orchestrate a set of Merge
Requests and the order of approval to ensure no regression."* Symphony opens one MR per
ticket and stops there; ordering is whatever humans happen to click first. With several
parallel cycles landing against one application, arbitrary merge order is exactly how a green
MR becomes a red `default_branch`.

## Scope

**Merge train.** A per-project ordered queue of MRs ready to integrate. Order is derived,
not manual:

1. Ticket dependencies (`depends_on`, already in `src/tracker/depends_on.rs`) — a dependent
   ticket's MR never precedes its dependency's.
2. The approved plan's `mr_sequence` (AIR-5) when several MRs come from one plan.
3. Conflict graph (AIR-15) — overlapping MRs are strictly ordered, never adjacent-parallel.
4. Risk (AIR-8 classification) and size — small, low-risk changes first, so a big risky one
   never sits behind a queue it will invalidate anyway.

**Pre-merge verification of the *combination*.** The core anti-regression mechanism: before an
MR is declared integration-ready, rebase it onto the projected post-merge state of everything
ahead of it in the train, and run the project's verification command
(`merge_train.verify_command`, typically the same suite as AIR-6) on that combination. A
failure ejects the MR from the train, records the reason, and hands it back to its cycle as
rework rather than to a human as a broken main branch.

**Symphony never merges.** Per the existing posture (SweBot "never merges — a human always
does that") and the roadmap's human-ownership principle, the train produces an *ordered,
verified, ready-to-merge list* with a clear "next to merge" and posts that status on each MR.
A human (or the project's own CI automerge) does the merging. Optionally, when
`merge_train.request_merge: true`, Symphony may set the host's native merge-when-pipeline-
succeeds flag — still the host merging under its own rules, not Symphony bypassing review.

**Ejection and re-entry.** An MR ejected by a failed combination test, a new conflict, or a
human's "request changes" re-enters the train only after its cycle produces a new verified
head. Every enqueue/eject/promote is an event with a reason.

## Implementation notes

- `src/merge_train.rs` + a `merge_train` table on the cycle store (AIR-13); the train must
  survive restart and rebuild its projection from the host's current state on boot rather than
  trusting stale rows.
- Combination verification runs in a throwaway workspace via the existing workspace manager,
  never in a cycle's live workspace.
- Dashboard: `/train` showing order, position, verification status and ejection history.

## Acceptance criteria

- [ ] Order respects dependencies, plan sequence, conflict ordering and risk/size, and is
      deterministic and unit-tested against fixture MR sets.
- [ ] Combination verification runs against the projected state and ejects on failure with a
      recorded reason; the cycle receives it as rework.
- [ ] Symphony never performs a merge itself; `request_merge` only sets the host's own flag.
- [ ] The train rebuilds correctly after a restart and after an out-of-band human merge.
- [ ] `/train` shows the queue and history; every transition is an event.
- [ ] Feature off by default.

## Out of scope

- Cross-repository merge coordination (AIR-24).

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
