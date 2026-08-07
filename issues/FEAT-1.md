---
identifier: FEAT-1
title: Show run duration in the per-issue usage table
state: done
priority: 3
labels: [dashboard, metrics]
dispatchable: true
---

## Resolution

- `eventlog::IssueUsageRow` gained `duration_secs: f64` and `duration_open: bool`.
  `eventlog::issue_durations` folds each issue's `"dispatched"`/`"worker_exit"` events
  (ordered by `id`) into closed spans in one pass, summing them; an unmatched trailing
  `"dispatched"` becomes an open span reported up to "now" and flagged `duration_open`.
  Computed in Rust rather than SQL -- pairing rows across an ordered sequence is far
  more direct this way than a self-join/window-function query.
- `status.rs`'s `/usage` page gained a Duration column. `eventlog` only knows whether a
  span is *open*; `status.rs` is what already has the live `StatusSnapshot`, so it
  cross-references the open issue's id against `state.status_rx`'s `running` list to
  decide how to render it: plain while genuinely running (expected to keep advancing),
  `~`-prefixed with an explanatory tooltip when open but not running (a process killed
  mid-attempt, not a precise number).
- Reused `metrics::format_duration` (made `pub`) for the `12m 4s`-style rendering
  instead of a second formatter.
- Tests: `eventlog`'s `usage_by_issue_sums_multiple_closed_dispatch_spans`,
  `usage_by_issue_reports_an_open_span_for_an_unmatched_dispatch`,
  `usage_by_issue_sums_closed_spans_plus_a_trailing_open_one` (via a new `insert_at`
  test helper for controllable timestamps); `status`'s
  `issue_usage_row_shows_duration_states` covers all three render states.

Verified: `cargo build`, `cargo test` (446 passed), `cargo clippy --all-targets`, `cargo fmt` clean.

## Context

`/usage` (`src/status.rs::usage_page`, backed by `eventlog::usage_by_issue`) already
shows, per issue: dispatches, turns, tool calls, input/output/total tokens, and the
last event type/time. It has no notion of wall-clock time spent, even though the data
to compute it already exists in the same `events` table every other column on this page
already reads from -- each dispatch attempt is bracketed by a `"dispatched"` event
(`orchestrator::dispatch_issue`) and, when that attempt ends, a `"worker_exit"` event
(`orchestrator::handle_msg`'s `OrchMsg::WorkerExit` arm), both already persisted with
`created_at` timestamps by `eventlog::spawn_writer`.

A separate, *in-memory-only* notion of this already exists --
`metrics::IssueMetrics::seconds_running`, accumulated by
`orchestrator::finalize_issue_runtime` and shown in the always-on
`symphony-report.html` file -- but that resets on every restart and isn't the source
`/usage` reads from (that page is entirely `eventlog`-driven, which is exactly why it
survives restarts unlike the live dashboard). Adding duration to `/usage` should be
computed the same way the rest of that page already is: from the persisted `events`
table, not by wiring in the separate in-memory `Metrics` struct.

## Scope

Add a **Duration** column to the per-issue table on `/usage`, computed from paired
`"dispatched"` → `"worker_exit"` events per `issue_id`:

- `eventlog::usage_by_issue` (or a small new function next to it, whichever reads more
  clearly) fetches each issue's `"dispatched"`/`"worker_exit"` rows ordered by `id`, and
  folds them into spans: each `"dispatched"` opens a span, the next `"worker_exit"` for
  that same `issue_id` closes it. Sum closed spans' durations per issue.
- A `"dispatched"` with no following `"worker_exit"` yet (the issue is currently running,
  or was mid-run when the process last stopped) is an **open** span -- report its
  duration up to "now" for a running issue, and mark it distinctly (e.g. a `~` prefix or
  a footnote) when the issue **isn't** currently running (so a stale open span from a
  process that was killed rather than stopped cleanly doesn't silently read as a precise
  number).
- Render as a human-readable duration (e.g. `12m 4s`, matching how `running for {:.1}s`
  already reads elsewhere, or coarser if that reads better at these magnitudes) rather
  than raw seconds.
- This is purely additive to the existing query/row struct
  (`eventlog::IssueUsageRow` gains a `duration_secs: f64` and an `is_partial: bool` or
  similar) and the existing `<table>` in `usage_page`/`issue_usage_row`
  (`src/status.rs`) gains one column -- no new page, no new route.

## Acceptance criteria

- [ ] `/usage`'s per-issue table shows a Duration column, summing all of that issue's
      dispatch attempts (not just the most recent one).
- [ ] A currently-running issue's duration keeps advancing (reflects up to "now"), and
      is visually distinguishable from a closed, exact duration.
- [ ] An issue that was killed mid-run (no `worker_exit` ever recorded for its last
      dispatch, and it's *not* currently running) shows its open span clearly marked as
      partial/uncertain rather than silently presented as exact.
- [ ] Unit tests in `eventlog.rs` (extending its existing `usage_by_issue` test
      coverage) cover: multiple closed spans summed, one open span for a running issue,
      and one open-but-stale span for a non-running issue.
- [ ] A `status.rs` test asserts the rendered column appears with the right formatting
      for a fixture row.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Changing `symphony-report.html`/`metrics.rs`'s own separate duration tracking -- that
  stays as is; this ticket only adds the persisted, restart-surviving version to
  `/usage`.
- DORA-style lead-time metrics (`AIR-12`) -- this is a simple per-issue duration column,
  not a flow metric.
