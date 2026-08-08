---
identifier: BUG-5
title: pipeline.approval.auto_approve_when can never match because the planner records its plan via record_artifact, not inline JSON
state: todo
priority: 1
labels: [bug, approval-gate, ai-roadmap]
dispatchable: true
---
## Context

Confirmed live during a real Phase 1 end-to-end dogfood run: a `planner` stage with
`requires_approval: true` and `pipeline.approval.auto_approve_when: {risk: low,
estimate_turns_max: 20}` configured never auto-approved, even though the plan it produced
was genuinely low-risk, single-file, and well within the turn estimate — it required a
manual approval every time.

**Root cause**, confirmed by reading the code: `handle_stage_approval`
(`src/orchestrator.rs`) does this:

```rust
let plan_json = last_message.as_deref().and_then(extract_plan_json);
```

`extract_plan_json` looks for a fenced ` ```json ` code block in the stage's *last turn
text message* (`crate::swebot::extract_json_block`). But `src/roles/builtin/planner.md`
— the actual instructions the planner agent follows — tells it to record its plan via
the `mcp__symphony__record_artifact` MCP tool, not by embedding a JSON block in its chat
reply. Confirmed live: the planner called `record_artifact` (`"Plan recorded as artifact
plan-8a17240b8792c154"`), and its final text message was ordinary prose narrating that
fact, with no fenced JSON anywhere in it. `evaluate_auto_approve` correctly treats a
missing/unparseable `plan_json` as "never satisfies a set condition" (the safe default),
so with `auto_approve_when` configured but the agent never emitting JSON where
`extract_plan_json` looks, **`auto_approve_when` is unreachable in practice for the
built-in planner role** — every `requires_approval: true` planner stage requires a human,
regardless of configuration, defeating the point of configuring it at all.

(Related but distinct from BUG-4: even if `last_message` weren't corrupted by BUG-4,
this bug means the fenced-JSON extraction still wouldn't find anything, because the real
content isn't in the chat text at all — it's in the recorded artifact.)

## Scope

`auto_approve_when` needs to actually see the plan's structured summary. Two viable
approaches — pick whichever keeps the "one small core of abstractions" posture:

1. **Read the recorded artifact instead of/in addition to the chat text.** The planner
   already calls `record_artifact` with a `kind` (e.g. `"plan"`) — `handle_stage_approval`
   could look up that stage's recorded artifact for the issue's current cycle
   (`crate::artifacts`, same store AIR-3 already provides) and parse `PlanSummary` from
   its content, falling back to the existing `extract_plan_json(last_message)` path for
   roles that don't record an artifact. This keeps `evaluate_auto_approve`'s matching
   logic unchanged and only widens where `plan_json` comes from.
2. **Update `src/roles/builtin/planner.md`** to also emit the same structured summary
   as a fenced ` ```json ` block in its final reply, duplicating what it already sent to
   `record_artifact`. Simpler, but asks every stage's final text to double as both a
   human-readable report and a machine-readable structured payload — worth weighing
   against option 1's small `artifacts` lookup.

Whichever is chosen, document the actual contract in `AutoApproveWhen`'s and
`handle_stage_approval`'s doc comments (currently describe only the fenced-JSON path),
so a future stage/role author knows what's actually being matched against.

## Acceptance criteria

- [ ] A live-shaped test (real recorded-artifact content matching what the planner role
      actually produces, or a real captured fenced-JSON reply if approach 2 is chosen)
      demonstrates `auto_approve_when: {risk: low, estimate_turns_max: N}` matching a
      genuinely low-risk plan and skipping the human approval step.
- [ ] A plan that fails to satisfy a configured condition (high risk, or missing/
      unparseable structured output) still requires human approval — the safe-default
      behavior must not regress.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- BUG-4 (the display corruption) — fix separately; this ticket is about the matching
  logic actually having real data to match against, not about what gets rendered on
  `/approvals`.
