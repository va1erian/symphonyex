---
identifier: FEAT-8
title: Let the Planner reproduce an issue by running the app and writing a failing test, then hand that test to the Developer as a starting snippet
state: done
priority: 2
labels: [pipeline, planner, developer, ai-roadmap]
dispatchable: true
---
## Context

Today the Planner stage (`src/roles/builtin/planner.md`) is prompt-restricted to
proposing an approach only — *"You do not write or edit code in this stage. Your output
is the plan the implementation stage will follow."* That's a convention enforced purely
by the prompt: the Planner's `ToolPolicy` is actually the unrestricted default
(`roles::builtin::default_tool_policy` only special-cases `reviewer`/`security` to
`ToolPolicy::SWEBOT`; every other role, including `planner`, keeps full file/edit/exec
access). So the Planner already *can* run the app or a repro script via `Bash` today —
it's just never asked to, and it never writes anything beyond the plan text itself.

Two real, related gaps observed during the Phase 1 dogfood run motivate closing that:

1. A plan is currently pure prose/JSON (`plan` artifact, `KNOWN_KINDS`,
   `src/artifacts.rs`) describing an *intended* approach — it's never actually confirmed
   against the running app before the Developer stage starts implementing against it.
   A Planner that reproduces the bug first (runs the app, observes the actual failure)
   catches "the issue as described doesn't reproduce" or "the real symptom is different
   from the report" before an implementation turn is spent on the wrong fix.
2. This run deliberately paired a smart planner model with a cheaper developer model
   (`roles.developer.model: claude-haiku-4-5`, `roles.planner.model` left on the
   stronger default) specifically to save cost — and a weaker Developer benefits more
   from a concrete, runnable starting point (a failing test it can watch turn green)
   than from prose alone. A test snippet the Planner already proved reproduces the issue
   is a much stronger scaffold for a cheaper model than a written description of one.

## Scope

- Extend `src/roles/builtin/planner.md`'s instructions: after proposing the approach,
  attempt to reproduce the issue by running the app/its existing test suite (via `Bash`,
  already available under the Planner's current unrestricted tool policy — no
  `ToolPolicy` change needed) and, where practical, write a minimal failing test that
  demonstrates the bug or missing behavior. Explicit non-goal, stated in the prompt: this
  is a reproduction/verification aid, not implementation — no production code changes,
  and the test is a starting point the Developer stage is free to revise, not a
  contract it must keep as-is.
- Record that test as an attachment on the `plan` artifact so the Developer stage picks
  it up the same way it already reads the plan (`cycle.artifacts`, `.symphony/artifacts/`
  workspace convention in `src/artifacts.rs`) — either as a `test_snippet` field inside
  the existing `plan` artifact's content (simplest: `plan` isn't in `STRUCTURED_KINDS`,
  so no schema migration needed) or as a small new recognized kind if a separate artifact
  proves cleaner in implementation. Whichever shape is chosen, the Developer stage's
  workspace should end up with the failing test file already written/restorable, not
  just described in text.
- Update `src/roles/builtin/developer.md` to mention the Planner's reproduction test when
  present: run it first, expect it to fail, treat making it pass as one (not necessarily
  the only) acceptance signal, and feel free to adjust it if the Planner's repro attempt
  turns out to be imprecise.
- This is best-effort, not blocking: some issues (e.g. pure refactors, config/doc
  changes) have nothing to reproduce — the Planner should skip the repro step cleanly in
  that case rather than forcing one, and the pipeline should behave exactly as it does
  today when no reproduction test was produced.

## Acceptance criteria

- [ ] For a reproducible bug, the Planner stage runs the app/tests, writes a failing test
      demonstrating the issue, and that test is available to the Developer stage's
      workspace at the start of the implement stage (not just mentioned in prose).
- [ ] The Developer stage's prompt references the Planner's reproduction test when one
      exists and treats it as a starting point, not a rewrite target.
- [ ] An issue with nothing to reproduce (e.g. a docs-only change) produces a plan with
      no reproduction test and no behavior change from today.
- [ ] The Planner still performs no production-code edits — only test code and its own
      plan artifact.
- [ ] Unit/integration test(s) covering: a plan artifact carrying a recorded test
      snippet is correctly surfaced to the next stage's workspace, matching the existing
      artifact-restore test conventions in `src/artifacts.rs`.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Giving the Planner a distinct, more restrictive `ToolPolicy` — it already has the
  access this needs; this ticket is a prompt/artifact-shape change, not a permissions
  change.
- Enforcing that the Developer stage's final implementation must keep the Planner's test
  unchanged — the Developer stage remains free to replace or extend it.
