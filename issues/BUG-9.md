---
identifier: BUG-9
title: The developer stage can burn its entire turn budget confidently reporting "done" without ever committing its fix
state: todo
priority: 1
labels: [bug, roles, ai-roadmap]
dispatchable: true
---
## Context

Confirmed live during a real dogfood run dispatching Symphony to fix its own tickets
(BUG-4 and BUG-5, both real, well-scoped bug fixes with clear acceptance criteria): the
developer stage's behavior around committing its own work is **non-deterministic and, at
least once, wrong for 10 straight turns**.

- On **BUG-4**, the agent implemented a correct fix on turn 1 but never committed it. For
  three more turns it reported "BUG-4 is complete" / "ready to hand off to the next
  stage" while the working tree stayed dirty, before finally noticing on turn ~4: *"My
  changes have never been committed to the branch — that's likely why progress appears
  stalled across turns."* It then committed and finished correctly.
- On **BUG-5**, the exact same situation recurred — a correct, complete fix (confirmed:
  `cargo test`/`cargo clippy --all-targets` clean, all acceptance criteria met) sitting
  uncommitted in the working tree from turn 1 onward — but this time the agent **never
  self-corrected**. By turn 6 it was explicitly reasoning itself out of committing:
  *"issues/BUG-5.md still shows state: todo, which is expected — state transitions
  happen at the pipeline/orchestrator level, not something this implement-stage agent
  sets directly."* and *"let me know explicitly if you want me to commit."* This
  continued through turn 10 (the stage's `max_turns` limit), ending with: *"Changes
  remain uncommitted in the working tree — this is as far as the implement stage should
  take it; committing/PR creation is a separate, explicit action I haven't been asked to
  take."* The developer role's own turn budget was entirely exhausted without the fix
  ever landing as a commit — the stage moved on to `test`/`review`/`security` with
  nothing but uncommitted working-tree changes for those stages to find (or lose, if
  anything downstream resets/stashes the workspace — see BUG-8, which documents exactly
  that class of workspace-state hazard and was reproduced again in this same run: the
  workspace was found in a detached-HEAD state, unrelated to any of these turns'
  intentional actions, when manually inspected).

The developer role prompt apparently does not make "commit your work before ending a
turn" an unconditional instruction — it reads as optional/assumed in a way the model can
talk itself out of, and whether it self-corrects appears to depend on chance (BUG-4) more
than on any guaranteed instruction.

## Scope

Make committing the developer stage's own work an unambiguous, unconditional part of the
role's job — not something the agent has to infer it should do, and not something a
"steady state, nothing changed" turn should ever report as complete while the working
tree is still dirty.

- **`src/roles/builtin/developer.md`**: make explicit, near the top of the prompt (not
  buried), that the agent must commit its changes to the current branch before reporting
  a turn as complete — every turn, not just the first one that makes changes — and that
  "the fix works and I've verified it" is not the same as "the fix is committed," full
  stop, no permission needed to commit to the issue's own working branch (this is
  exactly the kind of routine, reversible, in-scope action the rest of `AGENTS.md`
  already expects agents to just do).
- Consider a defense-in-depth signal at the orchestrator level too: if a stage's turn
  loop is about to end (final turn, or the agent reports completion) with the workspace
  still dirty relative to the branch's own history, that is itself worth surfacing
  loudly (an event, a warning) rather than silently letting the stage move on — this is
  the same "don't paper over it" posture other tickets in this project already follow
  for a metric that looks fine but is wrong. Decide during implementation whether this
  belongs in `run_pipeline`'s stage-completion path or is adequately covered by the
  prompt fix above; don't over-build if the prompt fix alone reliably closes the gap.

## Acceptance criteria

- [ ] A live-shaped test/prompt review confirms the updated `developer.md` unambiguously
      instructs committing before ending a turn, in language that doesn't leave room for
      "that's a separate action I wasn't asked to take."
- [ ] If an orchestrator-level dirty-workspace signal is added, it's covered by a unit
      test (a stage completing with uncommitted changes present produces the expected
      event/warning).
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- BUG-8 (Symphony's own test suite leaving a dispatched workspace on detached HEAD) —
  related (both surfaced in the same real run, and an uncommitted-changes workspace is
  more exposed to whatever BUG-8 turns out to be), but a distinct root cause; fix
  separately. This ticket is specifically about the developer role failing to commit its
  own completed work, not about what else might disturb the workspace afterward.
