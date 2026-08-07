---
identifier: AIR-13
title: Durable, versioned, resumable delivery-cycle state machine
state: todo
priority: 1
labels: [phase-2, orchestrator, durability]
dispatchable: true
depends_on: [AIR-1]
---
## Context

Roadmap §5: each parallel cycle requires "a versioned workflow state", "a complete activity
and decision history", and the Orchestrator must "retry failed activities **from the correct
checkpoint**". It states flatly: *critical workflow state and policies must be deterministic,
persistent and auditable.*

Symphony's live state is in memory (`orchestrator.rs`'s running-worker map + the `watch`
channel `status.rs` reads). The event log persists what *happened*, but not enough to resume:
after a restart the dashboard correctly shows nothing running, and a cycle that was three
stages in starts over from stage one. With Phase 1's multi-stage cycles, restarting from
scratch throws away real work and real tokens.

## Scope

Promote the cycle to a first-class persisted entity with an explicit state machine.

**Schema** (`symphony.db`): a `cycles` table (`cycle_id`, `issue_identifier`, `workspace_key`,
`current_stage`, `status`, `state_version`, `attempt`, `created_at`, `updated_at`,
`blocked_reason`) and a `cycle_transitions` append-only table recording every transition with
actor (agent | orchestrator | human), reason and timestamp — the audit trail the roadmap asks
for.

**States:** `pending → running(stage) → awaiting_approval | blocked | failed | completed`,
with legal transitions enforced in code (an illegal transition is a bug and must panic in
debug / log-and-refuse in release, not silently corrupt the cycle).

**Optimistic concurrency:** every write carries the read `state_version` and bumps it; a
mismatch means another writer (a second orchestrator process, or `symphony serve` handling an
approval) touched it, and the caller re-reads and retries. Without this, the AIR-5 approval
endpoint and the poll loop can race on the same cycle.

**Resume on startup:** on boot, load non-terminal cycles and reconcile them against reality —
workspace present? branch present? agent process obviously gone (it is: the process died with
us). Resume at the last completed checkpoint, i.e. re-run the interrupted stage from its
start, not the whole cycle. A stage is the checkpoint granularity because a partially
completed stage's artifacts are not trustworthy. Interaction with the existing
`startup_terminal_cleanup` must be explicit: cleanup applies to terminal issues, resume to
non-terminal cycles, and they must not fight over the same workspace.

**Idempotency:** resuming must not double-post PR comments, double-open MRs or double-record
artifacts. Artifacts are content-hashed (AIR-3) and `open_pull_request` already updates in
place; verify and test both under resume.

## Implementation notes

- `src/cycle.rs` owning the state machine and its storage; `orchestrator.rs` becomes a
  consumer of it rather than the keeper of truth.
- Keep the in-memory `watch` snapshot as a *derived* projection for the dashboard — do not
  build a second source of truth.
- Migration must upgrade an existing `symphony.db` in place, as `eventlog.rs` already does.

## Acceptance criteria

- [ ] Killing the daemon mid-cycle and restarting resumes at the interrupted stage, with
      earlier stages' artifacts intact and no repeated side effects.
- [ ] Every transition is recorded with actor and reason and is visible in a
      `/cycles/<id>` dashboard view.
- [ ] Illegal transitions are rejected; unit tests cover the full legal transition table.
- [ ] Concurrent writers (poll loop + approval endpoint) produce no lost updates
      (test with two writers on one cycle).
- [ ] A pre-existing database migrates in place without data loss.
- [ ] With the pipeline disabled, behaviour is unchanged.

## Out of scope

- Distributing cycles across multiple hosts — workers stay local (Appendix A remains
  out of scope).

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
