---
identifier: AIR-26
title: Provider parity and quality benchmark harness
state: todo
priority: 2
labels: [phase-3, provider, metrics]
dispatchable: true
depends_on: [AIR-22, AIR-12]
---
## Context

Roadmap §7 defines the bar precisely: *"Provider parity means the same accepted outcome and
quality level, not identical generated output"*, and requires measuring quality, latency,
tokens and cost by provider, agent and delivery cycle. Without a harness, "provider-neutral"
is an architectural claim nobody has tested, and the automatic fallback built in AIR-21 is a
switch to an unmeasured alternative.

## Scope

**Benchmark suite.** A set of reproducible tasks — a fixture repository plus tickets with
known-good outcomes and machine-checkable acceptance criteria, covering each pipeline role
(requirements extraction, planning, implementation, test generation, review, security
analysis). Ship a small suite in-repo; let projects add their own from real closed cycles
(`symphony bench import --cycle <id>` turns a completed, human-accepted cycle into a case —
the best benchmark data is work the team already accepted).

**Runner.** `symphony bench run --providers claude,opencode --repeat 3` executes every case
against every provider in isolated workspaces and records, per case × provider × run:
outcome (pass/fail against the case's criteria), rubric score for the roles whose output is
prose, wall-clock latency, turns, tokens, cost (AIR-11 pricing) and cache hit rate.

**Judging.** Deterministic checks first (tests pass, schema-valid artifacts, required
requirement ids covered). Prose quality uses a rubric-scored model pass whose judge model is
configurable and **explicitly recorded** with each score, with a fixed rubric shared with the
review rubric (AIR-7). Never let a provider judge its own output — enforce it in code and
say so in the report. Report variance across repeats, not just a mean: a provider that passes
two runs in three is not equivalent to one that passes three.

**Parity report.** Per role and overall: accepted-outcome rate, quality score, latency, tokens,
cost, and a derived cost-per-accepted-outcome — the roadmap's "provider efficiency". Output as
JSON and as an HTML page reachable from the dashboard, and feed the same numbers into
`/insights` so live production data and benchmark data sit side by side.

**Fallback validation.** A benchmark mode that forces AIR-21 fallback mid-suite and verifies
the fallback provider still meets the parity bar for the roles it would take over. A fallback
that has never been measured is an untested failover path, which is the same as no failover.

## Acceptance criteria

- [ ] The bundled suite runs end to end against at least two providers on a fixture repo.
- [ ] `bench import` converts a real completed cycle into a reusable case.
- [ ] Deterministic checks and rubric scoring both run; the judge model is recorded and can
      never be the provider under test (enforced, with a test).
- [ ] Variance across repeats is reported alongside means.
- [ ] The parity report renders as JSON and HTML and matches `/insights` for shared measures.
- [ ] Forced-fallback mode validates the fallback provider against the same bar.
- [ ] The harness is a separate subcommand; it never runs as part of normal operation.

## Out of scope

- Automatically switching the default provider based on benchmark results — that is a human
  decision, informed by this report.

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
