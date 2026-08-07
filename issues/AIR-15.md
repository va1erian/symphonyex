---
identifier: AIR-15
title: Conflict and code-ownership detection across parallel cycles
state: todo
priority: 1
labels: [phase-2, orchestrator, safety]
dispatchable: true
depends_on: [AIR-13]
---
## Context

Roadmap §5 requires, for each parallel cycle: "dependency and code-ownership controls" and
"conflict detection **before merge**". Symphony isolates workspaces and rebases each ticket
branch onto `default_branch` every `before_run` (AGENTS.md, "Keeping a long-running ticket
rebased"), which catches drift against *merged* work — but two cycles running in parallel
against the same files discover each other only when the second one rebases after the first
merges, i.e. after the conflict already exists.

## Scope

**Change-surface tracking.** For every running cycle, maintain the set of files (and, where
cheaply derivable, symbols) it has touched, refreshed after each turn from
`git diff --name-only <merge-base>..HEAD` in the workspace. Store it on the cycle (AIR-13).

**Overlap detection.** After every turn and before any MR is opened, compute pairwise overlap
between running cycles. Three configurable responses:

```yaml
conflicts:
  detection: on              # off | warn | on
  on_overlap: serialize      # warn | serialize | block
  ownership_file: CODEOWNERS # optional; overlap on an owned path escalates
  hot_paths: ["src/orchestrator.rs", "migrations/**"]   # always serialize
```

- `warn` — record and surface, keep going.
- `serialize` — the later-started cycle is parked until the earlier one merges or ends, then
  rebased and resumed. This is the useful default: it costs latency, not correctness.
- `block` — park and escalate to a human.

**Pre-dispatch avoidance.** The scheduler (AIR-14) consults predicted change surface — the
approved plan's `impacted_components` (AIR-5) — before dispatching a new cycle, so two
overlapping tickets are never started concurrently in the first place. Prediction is a hint,
not a guarantee; actual detection above is what enforces.

**Semantic conflict, not just textual.** A textual non-overlap can still break: cycle A
changes a function's signature, cycle B adds a caller in a different file. Detect the cheap,
high-value case — a symbol modified by A that appears in B's diff — and report it as a
`potential_semantic_conflict` finding for the reviewer/human rather than trying to be a
compiler. Be explicit in the docs that this is best-effort.

**Ownership.** Where a `CODEOWNERS`-style file exists, overlap on an owned path adds the owner
to the escalation, so the right human is asked.

## Implementation notes

- `src/conflicts.rs`: surface tracking, overlap computation, policy application. Use the hook
  plumbing for git commands so Docker mode works.
- Serialized cycles must release their worker slot while parked (they are not running), and
  resume through the AIR-13 checkpoint machinery.
- Watch for the known reconciliation-preempts-mid-turn behaviour: parking must go through the
  same abort-and-await-`after_run` path so nothing is lost.

## Acceptance criteria

- [ ] Two cycles touching the same file are detected before either opens an MR.
- [ ] Each `on_overlap` mode behaves as specified; `serialize` parks, releases the slot,
      resumes and rebases cleanly.
- [ ] `hot_paths` always serialize regardless of overlap size.
- [ ] The symbol-level heuristic produces a finding on a constructed signature-change case and
      does not fire on unrelated diffs.
- [ ] CODEOWNERS owners appear in the escalation.
- [ ] `detection: off` (default) leaves behaviour unchanged.

## Out of scope

- Deciding merge order once conflicts are resolved (AIR-16).

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
