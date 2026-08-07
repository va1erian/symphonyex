---
identifier: AIR-7
title: Reviewer agent stage — requirement coverage, standards and minimal-implementation review
state: done
priority: 2
labels:
- phase-1
- agent-role
- quality
dispatchable: true
depends_on:
- AIR-6
updated_at: 2026-08-07T03:59:25.638342400+00:00
---
## Context

Roadmap §4: the Reviewer Agent validates requirement coverage, maintainability, coding
standards, technical debt, minimal implementation and acceptance criteria, and outputs
findings plus an approval recommendation. Symphony already has most of this as SweBot's PR/MR
review (`src/swebot/review.rs`, `swebot::PERSONA` rubric) — but it runs as a *separate poll
loop against an already-open PR*, with no access to the cycle's requirements, plan, tests or
coverage. That is the gap: an in-cycle reviewer can check the change against the actual
acceptance criteria, and can send it back before a PR ever exists.

## Scope

An in-cycle Reviewer stage, `allow_edits: false` (AIR-2), that consumes the `requirements`,
`acceptance_criteria`, `plan`, `test_report` and `coverage` artifacts plus the working diff,
and records a `review_findings` artifact:

```json
{
  "schema_version": 1,
  "recommendation": "approve | request_changes | comment",
  "findings": [{
    "id": "F1", "severity": "blocker|major|minor|nit",
    "category": "requirement-coverage|correctness|maintainability|standards|debt|over-implementation",
    "file": "src/x.rs", "line": 42,
    "requirement_id": "R1",
    "summary": "...", "failure_scenario": "..."
  }],
  "unmet_acceptance_criteria": ["AC3"],
  "over_implementation": ["..."]
}
```

**Reuse, don't fork, the rubric.** Extract `swebot::PERSONA` and the review rubric into a
shared module both SweBot and this stage consume, so there is one quality bar in the codebase
and `request_changes` keeps meaning the same thing in both places.

**Feedback loop.** `pipeline.review.max_rework_rounds` (default 2): a `request_changes`
recommendation sends the cycle back to the Developer stage with the findings injected into
the prompt. Exceeding the limit escalates to a human rather than looping — the roadmap's
"rework" is a measured quantity (§11), so every round increments a counter recorded on the
cycle for AIR-12.

**Minimal implementation** is an explicit review dimension: code that goes beyond the
approved plan's tasks is a finding (`over-implementation`), matching the roadmap guardrail
"deliver small and reversible changes" and "do not introduce unmanaged technical debt".

## Implementation notes

- `src/roles/builtin/reviewer.md`, sharing the persona module with `src/swebot/review.rs`.
- The stage must see the diff without write access: compute `git diff <merge-base>..HEAD` in
  the workspace via the hook plumbing and hand it to the agent as context.
- Rework rounds and their outcomes belong in the event log, keyed by cycle.

## Acceptance criteria

- [ ] The stage produces a schema-valid `review_findings` artifact and cannot write files
      (assert the backend-native restriction is applied).
- [ ] Unmet acceptance criteria from AIR-4/AIR-6 are detected and listed.
- [ ] `request_changes` re-runs the Developer stage with findings in context; the round
      counter increments and is queryable.
- [ ] Exceeding `max_rework_rounds` escalates instead of looping.
- [ ] SweBot's existing review behaviour and tests are unchanged after the rubric extraction.

## Out of scope

- Posting the review to the code host — in-cycle review happens before the PR exists; the PR
  gets the consolidated evidence in AIR-9, and SweBot's post-PR review continues as is.

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
