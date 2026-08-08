---
identifier: BUG-6
title: A fully completed pipeline cycle re-dispatches from scratch forever when repo.pull_request isn't configured
state: todo
priority: 1
labels: [bug, orchestrator, ai-roadmap, cost]
dispatchable: true
---
## Context

Confirmed live during a real Phase 1 end-to-end dogfood run: an 8-stage pipeline
(`requirements -> planner -> implement -> test -> review -> security -> observability ->
release`) ran to completion, cleanly, with a genuinely good outcome — the release stage's
final message was *"Release stage: complete... ready for merge, nothing further for me to
do here"* and it had recorded a full evidence bundle. Every stage along the way that
touched on tracker/issue state explicitly said it was *not* responsible for it: the
requirements stage ("`update_issue_state` is reserved for a later stage and this agent
doesn't touch source files"), the release stage ("I did not merge it — that's a human
action per instructions").

The orchestrator then immediately re-dispatched the *same issue* as a brand new attempt,
starting again from the `requirements` stage — the full pipeline ran a second time,
requiring a second manual planner approval, for an issue that was already fully delivered
and evidenced. Root cause: the issue's tracker state never left `active_states` (it
stayed in `todo` the entire run), because **no stage in the built-in 8-role set, and
nothing in `run_pipeline`'s own completion path, calls `update_issue_state` to move a
successfully-completed cycle's issue out of the dispatcher's active pool.** The normal
mechanism that would do this — `repo.pull_request` opening a real PR, whose synthesized
`after_run` hook (per `AGENTS.md`) transitions the tracker to an "in review" state — was
not configured in this run (deliberately: local-only dogfood, no real GitHub touched),
and nothing else stands in for it.

This means: **any project running the AI Roadmap 2026 pipeline without `repo.pull_request`
configured will loop a fully-completed, fully-evidenced cycle forever**, burning real
tokens/turns/cost on an issue that has nothing left to do, until a human manually
transitions its tracker state by hand. Given `AIR-11`'s budget enforcement exists
specifically to cap runaway spend, and `AIR-9`'s release stage is explicitly scoped to
*"prepare and label," never merge* — this looks like a real gap between "prepare" and
"stop being re-dispatched," not a documented, intentional requirement that
`repo.pull_request` is mandatory for the pipeline to be usable at all.

## Scope

Give a completed pipeline cycle a defined way to stop being re-dispatched, independent of
whether `repo.pull_request` is configured.

- The cleanest fix likely lives in `run_pipeline`'s own completion path
  (`src/orchestrator.rs`): once every stage has completed (the loop falls off the end
  rather than parking/erroring), transition the issue to a configurable
  "delivered"/"ready for review" tracker state — mirroring how `pipeline.blocked_state`
  and `pipeline.awaiting_approval_state` already exist as project-configured *outside
  active_states* states for the other two "stop being re-dispatched but not because of
  failure" cases. A `pipeline.completed_state` (or similar name — check for a less
  confusing one given `terminal_states` already exists at the tracker level) config key,
  defaulting to something sensible, would follow that exact precedent.
- Confirm/decide how this interacts with `repo.pull_request` when it *is* configured:
  either the PR-opening hook's existing state transition should be treated as
  sufeirient (this new transition becomes a fallback for when it's absent), or both
  should coexist without conflicting — needs a decision, not just a fix, since projects
  using `repo.pull_request` already have working behavior today that must not regress.
- Whatever the fix, it must not require every project to configure `repo.pull_request`
  just to avoid an infinite-redispatch cost leak — that's the actual bug being fixed
  here.

## Acceptance criteria

- [ ] A pipeline cycle that completes every stage successfully, with no
      `repo.pull_request` configured, does not get re-dispatched from scratch on the next
      poll tick.
- [ ] A pipeline cycle that completes with `repo.pull_request` configured keeps its
      existing (working, tested) PR-opening/state-transition behavior unchanged.
- [ ] A unit test reproducing the exact scenario from this ticket (full stage list,
      `ScriptedBackend`, no `repo.pull_request`) asserts the issue is not eligible for
      dispatch after the cycle completes.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Redesigning the PR-opening/merge workflow itself (`repo.pull_request`,
  `open_pull_request`) — that path already works; this ticket is about the case where
  it's absent.
