---
identifier: BUG-11
title: A dispatched workspace left on detached HEAD silently persists across stages/turns instead of being detected and recovered
state: todo
priority: 1
labels: [bug, orchestrator, workspace, ai-roadmap]
dispatchable: true
---
## Context

Confirmed live, twice, from two genuinely different causes, during real dogfood runs
fixing Symphony's own bugs:

1. **BUG-8** (already filed): running `cargo test` inside a dispatched workspace that
   happens to be a clone of this repo can leave that workspace on a stray branch /
   detached HEAD, because some of this repo's own git-integration tests mutate the
   ambient working tree's git state instead of staying fully isolated in a tempdir.
2. **New, observed on BUG-5's rework round**: the developer stage, cleaning up a review
   finding (two accidentally-committed local files), used `git reset --soft` followed by
   selective re-staging and a fresh commit — a reasonable, safe approach (it deliberately
   avoided `git rebase -i`, which would have hung waiting on an editor in a headless
   session) — but this left the workspace on a detached HEAD pointing at the new, correct
   commit, with the `issue-BUG-5` branch ref still pointing at the old, stale history
   containing the files that needed removing.

In both cases the symptom is identical and the fix was identical: manually run
`git branch -f issue-<id> HEAD && git checkout issue-<id>` to reattach the branch to
wherever HEAD actually ended up, after first verifying the detached commit's content is
the one that should be kept. Neither case was caught automatically — the workspace
silently carried a detached HEAD into the next pipeline stage's turns, which then had to
reason about confusing git state (or, worse, could have had a later stage's own commit
land on the detached HEAD too, invisible to the tracked branch and therefore invisible
to whatever eventually reads `issue-<id>` for review/PR/merge purposes).

## Scope

Rather than chase every individual cause of a workspace ending up on detached HEAD (an
open-ended set — BUG-8's cause and this rework-cleanup cause are unlikely to be the only
two), add a single defense-in-depth check that catches the symptom directly, regardless
of cause:

- Before (or after) each pipeline stage's turns run, check whether the workspace is on
  detached HEAD (`git symbolic-ref -q HEAD` exits non-zero) rather than the expected
  `issue-<id>` branch. The expected branch name convention already exists (synthesized
  hooks use `issue-$name`, `src/config.rs`) — reuse it, don't invent a second naming
  scheme.
- If detached: verify the detached commit is a descendant of (or equal to) the tracked
  branch tip — if so, this is exactly the safe case observed above (a clean
  history-rewrite ahead of the branch), so fast-forward the branch ref to HEAD and check
  out the branch, restoring normal state automatically, the same recovery already done
  manually twice.
- If the detached commit is *not* a descendant of the tracked branch (a genuinely
  divergent/ambiguous state — e.g. BUG-8's cause, where the detachment isn't a clean
  intentional rewrite) — don't guess. Surface this loudly (a warning event visible on
  `/events`, naming the issue/stage) so a human can look, rather than silently forcing a
  branch reset that could discard real work in a less clean case than the two observed
  so far.
- This is a workspace/git-hygiene safeguard, not a role-prompt instruction — it should
  work regardless of what any given agent's git workflow choices happen to be.

## Acceptance criteria

- [ ] A workspace left on detached HEAD via a clean history rewrite (branch tip is an
      ancestor of HEAD) is automatically reattached before the next stage's turns begin,
      with no human intervention.
- [ ] A workspace left on detached HEAD via a genuinely divergent state (branch tip is
      *not* an ancestor of HEAD) is not silently force-reset — it's surfaced as a visible
      warning instead.
- [ ] A workspace already on the correct branch (the common case) sees no behavior
      change and no extra git calls beyond one cheap status check.
- [ ] Unit test(s) covering both the safe-fast-forward-reattach case and the
      divergent-surface-a-warning case, using real temporary git repos (matching this
      project's own existing test conventions for git-dependent code, e.g.
      `config.rs`'s `synthesized_hooks_actually_clone_branch_commit_and_push`).
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Root-causing every individual way a workspace could end up on detached HEAD (BUG-8's
  own root cause stays a separate, still-open investigation) — this ticket is the
  general safety net, not a fix for each specific trigger.
