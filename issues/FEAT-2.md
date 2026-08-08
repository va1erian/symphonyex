---
identifier: FEAT-2
title: Collapse same-second events onto one line in the /events conversation view
state: todo
priority: 3
labels: [dashboard, ux]
dispatchable: true
---
## Context

`/events?issue=<id>` (the default "Conversation view", `render_transcript`/
`transcript_bubble` in `src/status.rs`) renders one full-width bubble per event, stacked
top to bottom. Observed live on a real dispatched cycle: a single turn's bookkeeping
alone (`turn_started`, `session_started`, and every individual `tool_call`) produces one
bubble each, and several of these routinely land within the same second —
e.g. `2026-08-08T18:19:19.024610700+00:00` and `2026-08-08T18:19:19.045408800+00:00` are
two separate `turn_started` bubbles 21ms apart, each taking a full row. A single busy
turn (a handful of `Grep`/`Read`/`Edit` tool calls, which is a light turn by this
project's own standards — real dispatches routinely run 20-30+ tool calls) already
produces dozens of near-instant, same-second bubbles before a human ever reaches the
actual assistant narrative they're there to read. The page becomes very long very fast
for exactly the events a human cares about least (`transcript_role`'s own doc comment
already calls dispatch bookkeeping "present for completeness but not the point of
reading this transcript").

## Scope

In `render_transcript`, group consecutive events that occurred within the same second
(truncate `EventRow::created_at`, an RFC3339 string, to whole-second precision and
compare) into a single compact line instead of one bubble each — visually collapsing
bursts like `turn_started` + `session_started`, or a run of back-to-back tool calls, down
to one row.

- **Assistant narrative bubbles (`transcript_role == "assistant"`) should never be
  collapsed into a group with other events**, even if they share a second with a
  `tool`/`system` event — they're the actual content this view exists to show; only
  `tool`/`system`-role events collapse with their same-second neighbors.
- A reasonable rendering for a collapsed group: one line listing the event
  types/names (e.g. `tool_call: Grep, Read · turn_started, session_started`) with the
  shared timestamp once, rather than the current one-`<div class="msg">`-per-event
  structure — still linking through to `/events?issue=...&type=...` the way individual
  bubbles do today, since that's how a human drills into "what exactly happened here."
- This is scoped to the transcript/conversation view only — the plain table view
  (`event_row`, the `?view=table` toggle) already shows one row per event with a visible
  timestamp column and is a reasonable place to keep full granularity; leave it as is.

## Acceptance criteria

- [ ] A transcript with several `tool_call`/bookkeeping events sharing one second renders
      as one compact line, not one bubble per event.
- [ ] An assistant narrative bubble is never merged into a same-second group with
      surrounding tool/system events, even when timestamps coincide.
- [ ] Events in different seconds are never merged, even if adjacent.
- [ ] Unit test(s) in `src/status.rs` extending the existing `render_transcript`/
      `transcript_role` test coverage, built from a realistic same-second event burst.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Changing the plain table view (`?view=table`) or `/events`'s pagination/filtering.
- Any change to what gets persisted to `symphony.db` — this is a rendering-only change.
