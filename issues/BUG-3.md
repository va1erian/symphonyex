---
identifier: BUG-3
title: Symphony has no graceful handling for the claude backend's plan usage limit (5-hour window)
state: done
priority: 1
labels: [bug, claude-backend, resilience]
dispatchable: true
---
## Context

Investigated live, in the same dogfood run tracked in BUG-1/BUG-2: the `claude` CLI
already emits periodic messages Symphony currently throws away completely. Confirmed in
`dogfood-run.log` -- 8 occurrences across three concurrent sessions (AIR-2, AIR-3,
AIR-4) of a top-level message whose `"type"` field is literally `"rate_limit_event"`:

```
event=other_message ... message=Some("rate_limit_event")
```

`ClaudeSession::handle_message` (`src/agent/claude.rs`) only recognizes `"system"`
(subtype `init`), `"assistant"`, `"user"`, and `"result"` explicitly; everything else
(including this one) falls into the catch-all `other => events.send(AgentEvent::new(
"other_message").with_message(other.to_string()))` -- which keeps only the bare string
`"rate_limit_event"` as the whole payload. Whatever this message actually carries
(current usage vs. the plan's cap, the utilization percentage, a reset timestamp -- the
CLI has to know this to warn a human in its own interactive UI, so the data exists
somewhere in this payload) is discarded before it ever reaches anything that could act
on it.

**Why this matters right now, concretely**: Claude Code/claude.ai subscription plans
enforce a rolling usage window (commonly discussed as "the 5-hour limit"). When it's
hit, every subsequent turn will fail until the window resets -- for hours, not the
minutes `agent.max_retry_backoff_ms` assumes. Symphony's only existing failure-handling
path (`orchestrator::backoff_delay_ms`/`schedule_retry`, `src/orchestrator.rs`) treats
every turn failure identically: exponential backoff *capped* at
`max_retry_backoff_ms` (this repo's own `WORKFLOW.md`/`WORKFLOW.dogfood.md` both set
`300_000` = 5 minutes). If the account is actually rate-limited for the plan's full
reset window, Symphony will keep relaunching the `claude` subprocess every ~5 minutes,
each attempt failing immediately, for the entire remainder of the window -- burning
subprocess launches, filling `/events` and the retry queue with noise, and giving an
operator watching the dashboard no indication of *why* nothing is progressing or *when*
it will resume on its own.

**This also isn't a per-issue problem** -- this dogfood run has two/three tickets
dispatched concurrently (`agent.max_concurrent_agents: 2`+), all authenticating as the
*same* Claude Code login. A plan-wide usage limit is account-wide, not per-ticket: all
concurrently running issues will hit it at effectively the same moment, and each would
independently keep retrying on its own schedule under today's behavior -- N uncoordinated
retry loops all failing for the same underlying reason.

## Scope

**1. Capture the real signal.** Before designing the fix, get the actual shape of both
messages this needs to react to:
- The periodic `rate_limit_event` message -- capture one raw (a debug-level dump of the
  full JSON for any message type `handle_message` doesn't structurally recognize would
  have caught this already and is worth adding on its own: today an unrecognized type
  silently loses its entire payload down to one string, the same blind spot that let
  this one go unnoticed).
- The terminal message when the limit is actually *exhausted* (a `result`/`turn_failed`
  event, most likely, whose `message`/`result` text plausibly says something like "rate
  limit" / "usage limit" / "try again after" -- capture the real text once observed;
  do not guess this into a fixture).

**2. Classify it distinctly.** Add a way for `AgentError` (`src/agent/mod.rs`) to
represent "rate-limited, known or estimated resume time `T`" as its own case, separate
from generic `ResponseError`/`ProcessExit` -- something like
`AgentError::RateLimited { retry_after: Option<DateTime<Utc>> }`. Parse it out of
whichever of the two messages above actually carries the reset time (prefer the
periodic `rate_limit_event`'s own data if it reports one, since it may arrive before a
turn even fails outright).

**3. Handle it as a coordinated pause, not per-issue exponential backoff.** When any
running turn reports rate-limiting:
- Every other currently-running turn on the same backend is going to hit the same wall
  imminently -- pause dispatch for the whole `claude` backend (not just the one issue)
  until the known/estimated reset time, rather than letting each issue independently
  keep retrying on its own schedule.
- If a reset time is known, schedule resumption at (a little after) exactly that time
  instead of blind exponential backoff -- no point retrying every 5 minutes for 4 of the
  5 hours.
- If no reset time is reported, fall back to a distinctly longer, clearly-labeled backoff
  (not the same curve as a transient network error) -- and say in the fix why that
  fallback duration was chosen.
- In-flight turns that were already running are allowed to finish naturally; only *new*
  dispatch is paused.

**4. Make the pause visible.** This is exactly the kind of state a human needs to see at
a glance, not infer from a truncated error string in the retry table. Add a distinct
banner/section to the dashboard (`src/status.rs`) -- e.g. "Paused: `claude` plan rate
limit, resuming ~HH:MM" -- rather than folding it into the existing per-issue retry rows,
which today show one truncated error message per issue with no shared context that
they're all blocked on the same account-wide cause.

## Acceptance criteria

- [ ] The real raw JSON for both the periodic usage-telemetry message and (once/if
      observed) an actual exhausted-limit failure are captured and used to build the
      parsing logic and its tests -- not hand-guessed fixtures.
- [ ] `AgentError` distinguishes rate-limiting from other turn failures, carrying a
      resume time when the CLI reports one.
- [ ] Hitting the limit pauses *new* dispatch for the whole backend (not per-issue
      independent backoff loops), and resumes automatically at the known/estimated time
      without operator intervention.
- [ ] In-flight turns are not killed when the pause begins; they're allowed to finish.
- [ ] The dashboard clearly shows the paused state and expected resume time, distinct
      from the existing per-issue retry queue.
- [ ] An unrecognized message type's full payload is at least debug-logged rather than
      collapsed to a bare string, so the next unrecognized-but-important message isn't
      silently lost the same way this one was.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Token/cost budget enforcement in general (`AIR-11`) and provider fallback to a
  different backend when rate-limited (`AIR-21`) -- this ticket is specifically about
  the `claude` backend recognizing and gracefully surviving its own plan's usage limit,
  not switching away from it.
- `codex`/`opencode` backends' own rate-limiting, if any -- out of scope unless the fix
  naturally generalizes at the `AgentError` level (in which case, note it, but don't
  build backend-specific detection for them here).
