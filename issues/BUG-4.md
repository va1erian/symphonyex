---
identifier: BUG-4
title: A pending approval's displayed plan text can be the literal string "system" instead of the real plan
state: todo
priority: 1
labels: [bug, approval-gate, orchestrator]
dispatchable: true
---
## Context

Confirmed live during a real Phase 1 end-to-end dogfood run (all 8 pipeline stages, real
`claude` CLI, against this repo's own codebase): a `requires_approval` planner stage
produced a genuinely good plan (recorded via `record_artifact`, and also narrated in the
turn's final assistant message: *"As the Planner/Architecture agent, my deliverable for
this stage is the plan artifact — already recorded in turn 1 as `plan-8a17240b8792c154`,
covering files touched, handler design..."*). But the pending-approval card rendered on
`/approvals` showed only:

```html
<pre class="msg">system</pre>
```

The literal four-character string `"system"` — not the plan, not a truncation of the
plan, just the word "system". A human deciding whether to approve this stage has nothing
to go on.

**Root cause**, confirmed by reading the code: `run_one_turn`'s event-forwarding task
(`src/orchestrator.rs`, roughly lines 3432-3461) does this for every event coming off the
turn:

```rust
if let Some(m) = &ev.message {
    last_message = Some(m.clone());
}
```

This runs for *every* `AgentEvent` that carries a `message`, not just genuine assistant
text. `handle_message`'s `other => { events.send(AgentEvent::new("other_message")
.with_message(other.to_string())) }` branch (`src/agent/claude.rs`) fires for every
unrecognized top-level stream-json message type (`"system"`, `"rate_limit_event"`, and
others), and sets `message` to the raw `msg_type` string itself — not real content. The
real `claude` CLI reliably emits one or more of these *after* the final assistant text
block within a turn (confirmed live: `notification` with the real plan text, followed by
`other_message("rate_limit_event")`, followed by `other_message("system")`, then
`turn_completed`). Since `last_message` is simply overwritten by whatever arrives last,
it ends up holding `"system"` instead of the real narrative almost every time a turn ends
this way — which, empirically, is most of the time.

This same corrupted `last_message` also feeds `handle_stage_approval`'s
`extract_plan_json` (see BUG-5, a related but distinct bug) and is passed as `plan_text`
into the `NewApproval` row (`approvals::create_pending`), so the corruption isn't
cosmetic to one page — it's the value multiple consumers treat as "the stage's final
output text."

## Scope

Only genuine assistant-authored text should ever update `last_message` — housekeeping
stream messages (`other_message`, `malformed`, and anything else that isn't real
narrative) must not overwrite it.

- In `run_one_turn`'s forwarding loop, only update `last_message` for events whose
  content is actual assistant text (the existing `"notification"` event from
  `handle_message`'s `"assistant" | "user"` branch is exactly this — reuse that
  distinction rather than inventing a new one).
- Do not change what gets forwarded to `tx`/`/events` — every event should keep showing
  up there exactly as it does today. This is scoped to what `last_message` (the tuple
  `run_one_turn` returns) captures, nothing else.

## Acceptance criteria

- [ ] A turn whose real final assistant text is followed by one or more `other_message`
      events (`"system"`, `"rate_limit_event"`, etc.) before `turn_completed` still
      reports the real text as `last_message`, not the trailing event's raw type string.
- [ ] Unit test in `src/orchestrator.rs` covering exactly this event ordering (assistant
      notification, then `other_message`, then completion) built from the real captured
      event sequence, asserting `last_message` is the assistant text.
- [ ] The `/approvals` pending-card display and `extract_plan_json`'s input are both
      fixed by this change (no separate fix needed in `status.rs`/`handle_stage_approval`
      beyond what already reads `last_message`).
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- BUG-5 (`auto_approve_when` never matching a `record_artifact`-based plan) — a related
  finding from the same dogfood run, but a distinct root cause; fix separately.
