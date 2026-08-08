---
identifier: FEAT-3
title: Group events by pipeline stage in the /events conversation view
state: todo
priority: 3
labels: [dashboard, ux, ai-roadmap]
dispatchable: true
---
## Context

`/events?issue=<id>` (`render_transcript`/`transcript_bubble` in `src/status.rs`) renders
every event for an issue as one flat, chronological list of bubbles — dispatch
bookkeeping, every turn's tool calls, and assistant narrative all interleaved with no
visual separation between pipeline stages. On a real multi-stage cycle (confirmed live:
`requirements -> planner -> implement -> test -> review -> security -> observability ->
release`), a human trying to find "what did the review stage actually say" has to scroll
past every other stage's turns and bubbles first, reading `stage_started`/`stage_finished`
bubbles (rendered like any other bookkeeping event, easy to miss) as the only signal that
a new stage began.

Confirmed by reading the code: the pipeline already emits exactly the boundary markers
needed for this — `OrchMsg::StageStarted`/`StageFinished` (`src/orchestrator.rs`,
~1711-1759) are persisted to `symphony.db` as `event_type: "stage_started"` /
`"stage_finished"` rows, with the stage id embedded in the `message` field (`stage_id`
for `stage_started`, `"{stage_id}: {outcome}"` for `stage_finished`) — there is no
separate `stage_id` column on `eventlog::EventRow` itself, and ordinary per-turn events
(`turn_started`, `tool_call`, `notification`, etc.) carry no stage association at all.
Grouping by stage therefore has to be reconstructed from the ordered event stream (every
event between one `stage_started` and the next `stage_started`/cycle end belongs to that
stage), not read off an existing field.

## Scope

In the conversation view, group consecutive events into per-stage sections:

- Walk `rows` (already ordered) and bucket every event between a `stage_started` and the
  next `stage_started` (or the end of the list) as belonging to the stage named in that
  `stage_started` event's message.
- Events before the first `stage_started` (or when the issue's pipeline isn't enabled at
  all, so no `stage_started`/`stage_finished` events exist) fall back to today's flat
  rendering — this feature is purely additive for pipeline-enabled projects, not a
  requirement everywhere.
- Render each stage as a collapsible/labeled section (stage id, and its outcome once the
  matching `stage_finished` is reached, e.g. "review — completed" or "security —
  blocked: 1 critical finding") wrapping that stage's bubbles — reuse this project's
  existing `<section><h3>...</h3>...</section>` pattern already used elsewhere in
  `status.rs` rather than inventing new markup conventions.
- If FEAT-2 (collapsing same-second events) lands first or alongside this, the two should
  compose cleanly — same-second collapsing happens within a stage's bucket, not across
  stage boundaries.

## Acceptance criteria

- [ ] A transcript for a multi-stage cycle renders each stage's events under its own
      labeled section, in stage order.
- [ ] A transcript for an issue with no `stage_started`/`stage_finished` events (pipeline
      not enabled, or no dispatch has happened yet) renders exactly as it does today —
      no regression for non-pipeline projects.
- [ ] A stage's section shows its outcome once available (from the matching
      `stage_finished` event) without duplicating that event as its own bubble inside the
      section.
- [ ] Unit test(s) in `src/status.rs` extending `render_transcript`'s existing coverage,
      built from a realistic multi-stage event sequence.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Changing the plain table view (`?view=table`) or how events are persisted to
  `symphony.db` — this is a rendering-only change over already-available data.
- Adding a `stage_id` column to `eventlog::EventRow`/the `events` table — reconstructing
  the grouping from the existing `stage_started`/`stage_finished` markers is sufficient
  and avoids a schema change for a display-only feature.
