---
identifier: AIR-6
title: Test agent — generate and execute tests, produce coverage and regression evidence
state: done
priority: 1
labels:
- phase-1
- agent-role
- quality
dispatchable: true
depends_on:
- AIR-5
updated_at: 2026-08-06T23:35:39.372062900+00:00
---
## Context

Roadmap §4: the Test Agent generates and executes unit, component, integration and end-to-end
tests, and its required output is **requirement coverage and regression evidence**. Roadmap
§2 lists Quality Engineering — "increase automated test coverage and requirement validation"
— as a priority. Symphony today has no notion of tests at all: whether the agent ran any is
invisible outside the raw event stream.

## Scope

A dedicated stage running after implementation, with edits allowed (it writes tests) but a
distinct rubric from the Developer role: it must not modify production code to make a test
pass — if a test fails, that is a finding, not something to paper over.

**Configuration** — the project declares how its tests actually run, since Symphony must stay
language-agnostic:

```yaml
pipeline:
  test:
    commands:
      unit: cargo test
      integration: ./scripts/it.sh
    coverage:
      command: cargo llvm-cov --json --output-path coverage.json
      format: llvm-cov          # llvm-cov | lcov | cobertura | jacoco | none
      min_line_percent: 70      # advisory unless `blocking: true` on the stage
```

**Outputs** — two artifacts (AIR-3):

- `test_report`: per suite, command, exit code, duration, pass/fail/skip counts, and the
  tail of the output on failure.
- `coverage`: parsed totals plus per-file lines, normalized across the supported formats so
  the report and later stages don't care which tool produced it.

**Requirement coverage.** The agent must map each `AC*` from AIR-4 to the test(s) that
exercise it, recorded in `test_report.requirement_coverage`. An `AC` with no test is reported
as a gap — that mapping is exactly what makes AIR-9's traceability manifest possible and is
the difference between "tests exist" and "requirements are validated".

**Regression evidence.** Before touching anything, the stage runs the suite once on the merge
base to establish a baseline, so a failure that was already failing on `default_branch` is
reported as pre-existing rather than blamed on this change.

## Implementation notes

- Test execution goes through the existing hook/command plumbing (`src/hooks.rs`,
  `run_hook_maybe_containerized`) so Docker mode works unchanged — do not shell out directly.
- Coverage parsers in `src/quality/coverage.rs`, one small module per format, all producing
  the same normalized struct. Unsupported/absent coverage must degrade gracefully to "not
  measured", never fail the cycle.
- Surface a per-issue test/coverage column in `src/metrics.rs`'s HTML report and on `/usage`.

## Acceptance criteria

- [ ] Configured suites run through the hook plumbing and produce a `test_report` artifact,
      including in Docker mode.
- [ ] Each of the supported coverage formats parses into the normalized struct (fixture-based
      unit tests); `format: none` degrades cleanly.
- [ ] Baseline run distinguishes pre-existing failures from new ones.
- [ ] Every `AC*` is either mapped to a test or listed as a coverage gap.
- [ ] `min_line_percent` violations are advisory by default and blocking when the stage is
      marked `blocking: true`.

## Out of scope

- Deciding whether a gap blocks the merge — that is the Reviewer's call (AIR-7) and the
  human's (AIR-5 / AIR-19).

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
