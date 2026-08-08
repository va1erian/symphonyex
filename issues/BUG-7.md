---
identifier: BUG-7
title: "/insights and /metrics report \"First-pass acceptance\"/\"Rework\" as unknown even though AIR-7 already persists that data"
state: todo
priority: 2
labels: [bug, metrics, ai-roadmap]
dispatchable: true
---
## Context

Confirmed live during a real Phase 1 end-to-end dogfood run: `/metrics` (Prometheus text
output) reports, with an explanation attached:

```
# HELP symphony_quality_first_pass_acceptance cycles reaching MR with zero request_changes rounds (AIR-7)
# TYPE symphony_quality_first_pass_acceptance gauge
# unknown: requires MR review-round tracking (AIR-7), not yet recorded anywhere Symphony persists
symphony_quality_first_pass_acceptance NaN
```

and the same `NO_REVIEW_ROUND_DATA` reason for `rework`. But this is factually wrong:
AIR-7's reviewer stage rework loop already persists exactly this data, and has since AIR-7
was merged (before AIR-12 in the same Phase 1 chain) — `eventlog.rs`:

```rust
/// AIR-7: one row per Reviewer-stage `request_changes` rework round, reusing the
/// `events` table (`event_type = "rework_round"`) rather than a new table...
pub fn record_rework_round(db_path: &Path, round: &NewReworkRound) -> rusqlite::Result<i64>
...
pub fn rework_rounds_for_issue(db_path: &Path, issue_id: &str) -> rusqlite::Result<Vec<EventRow>>
```

`src/status.rs`'s `/reviews` page already queries and renders this data correctly (round
number, recommendation, escalated flag) for the exact same dogfood run this ticket is
based on. `src/insights/mod.rs` (~lines 211-226), however, hardcodes:

```rust
Metric {
    key: "first_pass_acceptance",
    ...
    value: unknown(NO_REVIEW_ROUND_DATA),
},
Metric {
    key: "rework",
    ...
    value: unknown(NO_REVIEW_ROUND_DATA),
},
```

with no query against `eventlog` at all for either metric. This looks like AIR-12 was
developed without integrating against AIR-7's actual persistence mechanism (the two
tickets were dispatched as sibling branches with no visibility into each other's real
code, per this repo's own Phase 1 integration notes) and the gap survived the merge since
it's a semantic omission, not a compile error.

## Scope

Wire `first_pass_acceptance` and `rework` in `src/insights/mod.rs` to the `events` table's
`rework_round` rows (via `eventlog`, following the same "read from `symphony.db` only, one
computation backs every surface" posture the rest of `insights::compute` already uses):

- **Rework**: sum of `rework_round` events per cycle over the requested period — the
  formula the metric's own doc comment already states ("AIR-7 rework rounds per cycle").
  A straightforward count/aggregate query, no new table needed.
- **First-pass acceptance**: cycles that reached the MR/release stage with *zero*
  `rework_round` events recorded, over total cycles that reached that stage in the
  period. This needs "did this cycle reach MR" as a denominator, which may itself need a
  signal this repo doesn't cleanly have yet (e.g. a `release`/evidence-bundle event) —
  if so, use whatever real completion signal exists (e.g. AIR-9's evidence bundle
  recording, or a `StageFinished`/`released` event) rather than fabricating one; if truly
  no such signal exists cleanly, that's a legitimate reason to keep `first_pass_acceptance`
  `unknown` for now, but `rework` alone has no such blocker and should stop reporting
  `unknown`.
- Human-requested changes on the MR (the other half of "Rework"'s stated formula) can
  stay out of scope if no such signal is tracked yet — partial improvement (rework
  rounds alone) is still strictly better than the current always-`unknown` state, as
  long as the formula/doc comment is updated to reflect exactly what is and isn't
  counted.

## Acceptance criteria

- [ ] A seeded test database with known `rework_round` events for one or more issues
      produces the correct non-`unknown` `rework` count via `insights::compute`.
- [ ] `/metrics`'s `symphony_quality_rework` line reflects real seeded data instead of
      `NaN`, with an accurate `HELP` line describing exactly what's counted.
- [ ] If `first_pass_acceptance` stays `unknown` for lack of a completion signal, its
      reason string is updated to reflect the *actual* blocker (not "review-round
      tracking," since that part now exists) — or it's also wired up if a suitable
      completion signal is identified during implementation.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Any other `unknown` metric on `/insights`/`/metrics` genuinely blocked on missing
  RepoHost merge-history/production-validation data (deployment frequency, lead time,
  change failure rate, etc.) — those are correctly `unknown` today and out of scope here.
