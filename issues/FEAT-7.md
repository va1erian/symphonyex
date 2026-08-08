---
identifier: FEAT-7
title: Track the last notification and last tool call separately on the running card
state: todo
priority: 2
labels: [dashboard, ux, orchestrator]
dispatchable: true
---
## Context

Observed live on the `/` dashboard's Running card (`running_card`, `src/status.rs`): a
card can show `last event: other_message` with the message body `rate_limit_event` —
the actual most recent assistant narrative (what a human watching the dashboard actually
wants to see) is gone, clobbered by a trailing housekeeping event.

Root cause, confirmed by reading the code: `handle_msg`'s `OrchMsg::AgentEvent` handler
(`src/orchestrator.rs`, ~line 1560-1567) tracks exactly one `last_event`/`last_message`
pair per running issue, unconditionally overwritten by *every* event that carries a
message:

```rust
if !is_full_report {
    e.last_event = Some(event.event.clone());
    e.last_event_at = Some(Instant::now());
    if let Some(m) = &event.message {
        e.last_message = Some(m.clone());
    }
}
```

There's already a carve-out here for `test_report`/`coverage` specifically *because*
their full-JSON-blob message would be the wrong thing to show on this card — the same
reasoning applies to `other_message` events (`"system"`, `"rate_limit_event"`, and
similar CLI housekeeping noise), which carry the literal stream-message type as their
`message`, not real content, and to `tool_call` events, whose own message (the tool
name) is genuinely useful but distinct information, not a substitute for the last thing
the agent actually said. Right now both a real notification and a bare tool-call name
compete for the same one-slot summary, and whichever arrives last wins — so a card can
show a tool name when a human wanted the narrative, or vice versa, or (worst case, as
observed) neither, showing raw housekeeping noise instead.

(Related but distinct from BUG-4, which is the same "last event unconditionally
overwrites a summary field" pattern in a different consumer —
`orchestrator::run_one_turn`'s `last_message`, feeding `/approvals` — not this one. Worth
fixing with the same underlying principle, but a separate code path and a separate fix.)

## Scope

Track the last genuine assistant notification and the last tool call as two separate
fields on the running-issue tracking state (`src/orchestrator.rs`'s per-issue `running`
entry) and on `status::RunningRow`, and render both on the card:

- `last_notification: Option<String>` — updated only from `notification`-type events
  (the event `handle_message` in `src/agent/claude.rs` emits specifically for genuine
  assistant/user text), never from `other_message`/`malformed`/bookkeeping events.
- `last_tool_call: Option<String>` — updated only from `tool_call` events (already
  distinguished via `is_tool_call` in the code above), holding the tool name.
- `last_event`/`last_event_at` can stay as a general "most recent event of any kind"
  indicator if still useful elsewhere (check current consumers before removing it), but
  the card itself should read from the two new fields, not from whichever one happened
  to arrive last.
- `running_card`'s markup gains a second row for the tool call alongside the existing
  notification `<div class="msg">`, both always visible when present (not one replacing
  the other).

## Acceptance criteria

- [ ] A running card shows the last real assistant notification even when housekeeping
      events (`other_message`, `rate_limit_event`, etc.) arrive after it.
- [ ] A running card shows the last tool call name even when a notification arrives
      after it (and vice versa) — both are visible at once, not whichever is more
      recent.
- [ ] Unit test(s) extending `running_card`'s/`StatusSnapshot` update coverage in
      `src/orchestrator.rs` and `src/status.rs`, built from a realistic interleaved
      event sequence (notification, tool_call, other_message, in some order) asserting
      both fields survive correctly.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- BUG-4 (`run_one_turn`'s `last_message` feeding `/approvals`) — same underlying
  pattern, different code path; already filed and tracked separately.
