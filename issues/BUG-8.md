---
identifier: BUG-8
title: Running `cargo test` inside a Symphony-dispatched clone of this repo can leave the workspace on a stray branch/detached HEAD
state: todo
priority: 3
labels: [bug, tests, git]
dispatchable: true
---
## Context

Observed live during a real Phase 1 end-to-end dogfood run, where the dispatched
workspace (a real `git clone` of this repo, created by Symphony to work on ticket
HEALTH-1) happened to be a checkout of this same codebase — a meta/self-referential
scenario ("Symphony dispatching a change to Symphony"), not a normal target project.

During the `implement`/`test` pipeline stages, the coding agent ran `cargo test` (both as
part of its own verification and as the deterministic test stage's configured command)
directly in that cloned workspace. Twice, the agent then found the workspace's git state
had changed unexpectedly — once in detached HEAD, once switched onto a branch named
`issue-99` (a name this repo's own `config.rs` tests use, e.g.
`synthesized_hooks_actually_clone_branch_commit_and_push` and
`before_run_detects_a_real_conflict_and_after_run_pushes_it_once_resolved`) — with a
commit like `symphony: 99` / `main advances` present. The agent correctly diagnosed this
as a side effect of the test suite's own git-integration tests rather than a real defect
in its change, and safely fast-forwarded back onto `issue-HEALTH-1` each time (not a
reset) before re-verifying.

**This was not fully root-caused.** A quick check of the *investigating* agent's own dev
worktree (a separate, non-dispatched checkout, where `cargo test` is also run constantly
throughout this repo's own development) showed no such stray branches/commits leaking
into that repo's real branch list — so whatever is happening is either specific to
running inside a workspace that is *itself* a plain `git clone` (as opposed to a linked
worktree or the original repo), or specific to some interaction between two of this
repo's own tests running concurrently, or something else not yet identified. `cargo
test`'s default parallel test execution is a likely contributing factor (two git-mutating
tests targeting overlapping state at once) but this is a guess, not a confirmed cause.

## Scope

Root-cause and fix (or confirm harmless and document) why `cargo test`, run from within a
plain `git clone` of this repository, can leave that clone's branch/HEAD state changed by
tests that are supposed to operate on their own isolated tempdirs.

**Investigate first**, in roughly this order:
1. Find which test(s) actually touch branches named `issue-99`/`issue-42`, or commit
   messages `symphony: 99`/`main advances` (grep the test suite for these literals —
   several likely candidates are already known from this investigation:
   `config.rs`'s `synthesized_hooks_actually_clone_branch_commit_and_push` and
   `before_run_detects_a_real_conflict_and_after_run_pushes_it_once_resolved`).
2. For each, confirm whether its git operations are fully scoped to a `tempfile::tempdir()`
   (as its own doc comments claim) or whether any code path resolves a path/repo
   relative to the test process's actual current working directory instead — e.g. a
   missing `.current_dir(&tempdir)` on a spawned `git` `Command`, or a `git worktree add`
   call that (correctly) shares refs with the invoking repository by design but is being
   used somewhere it shouldn't leak visibly.
3. Reproduce directly: `cd` into a fresh `git clone` of this repo (not this worktree) and
   run `cargo test` there, watching `git branch`/`git status` in that clone before and
   after, to confirm the effect and narrow down which specific test triggers it.

**Fix**, once found — most likely either adding a missing `.current_dir()` on a git
subprocess call, or confirming the mechanism is inherent to how git worktrees share refs
and adjusting the test to use a fully separate temporary repository (`git init` in a
tempdir, not a worktree of the ambient one) instead.

## Acceptance criteria

- [ ] The specific test(s) responsible are identified with a minimal reproduction.
- [ ] Running `cargo test` from within a plain clone of this repository no longer changes
      that clone's checked-out branch or `HEAD` state as an unintended side effect.
- [ ] If the mechanism turns out to be intentional/unavoidable given how git worktrees
      work, this is documented clearly (e.g. in `AGENTS.md`) as a known caveat for anyone
      dispatching Symphony changes to work on Symphony itself, rather than left as a
      silent surprise.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Any change to how normal (non-Symphony-codebase) target projects are dispatched to —
  this is specific to the self-referential "Symphony working on Symphony" scenario.
