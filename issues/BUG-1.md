---
identifier: BUG-1
title: Token usage is always 0 for the claude backend in real runs
state: todo
priority: 1
labels: [bug, metrics, claude-backend]
dispatchable: true
---
## Context

Confirmed live, not theoretical: running the real `claude` backend (Claude Code CLI
2.1.223, headless `-p --output-format stream-json`, `bypassPermissions`, MCP tool
wiring for `update_issue_state`) against real tickets in this repo's own dogfood setup,
`/usage` reports **0 input/output/total tokens after 6 completed turns and 385 tool
calls** across three dispatched issues (AIR-2, AIR-3, AIR-4) that all made real,
verified progress (two reached `done`). The tracing log has **zero** `turn_completed`
or `turn_failed` events for any of those 6 turns — every one of them fell through
`ClaudeSession::run_turn`'s `None if status.success() => Ok(TurnOutcome::Completed {
usage: None })` fallback (`src/agent/claude.rs`), meaning the stdout read loop reached
EOF without ever seeing a `"type":"result"` line that `handle_message` recognized.

This is not the "narrow race" AGENTS.md already documents (reconciliation preempting a
turn right before its `result` event arrives) — this happened on **every single turn**
of a real multi-hour run, including turns that were the *first* turn of a session (no
`--resume` involved), so it isn't only a preemption-timing issue.

A manual sanity check (`claude -p "reply with just OK" --model claude-sonnet-5
--output-format stream-json --verbose`, no `--resume`, no `--mcp-config`) *does* produce
a well-formed final line: `{"is_error":false,...,"usage":{"input_tokens":2,...
"output_tokens":4,...},...,"type":"result",...}`, which `extract_usage`
(`src/agent/claude.rs`) reads correctly in isolation. So the field names and general
shape genuinely are still `usage.input_tokens`/`usage.output_tokens` on the CLI version
installed here — something about a *real* multi-tool-call turn (78 tool calls in one
observed case), possibly combined with `--resume`, `--mcp-config`/`--strict-mcp-config`,
or `--allowedTools mcp__symphony__*`, prevents that same line from ever reaching
`handle_message` as a recognized `"result"` event.

## Scope

Root-cause and fix why the `claude` backend's real turns never register a `result`
event, then get token accounting actually working end to end.

**Investigate** (in this order — cheapest, most informative first):

1. Capture a real turn's raw stdout unfiltered: temporarily tee `claude`'s stdout to a
   file from a manual repro (same flags Symphony actually uses — `--resume`,
   `--mcp-config <file> --strict-mcp-config`, `--allowedTools mcp__symphony__*`,
   `--permission-mode bypassPermissions`, a prompt that does several tool calls and
   finally calls the `update_issue_state` MCP tool) and inspect the last several lines
   by hand. Does a `"type":"result"` line exist at all? If yes, what does its `usage`
   object actually look like — same shape as the manual smoke test, or has the CLI
   changed shape for `--resume`ed / MCP-tool-ending turns specifically?
2. If the line is present but not recognized: check `handle_message`'s dispatch — is
   `msg_type` actually `"result"`, or has it changed to something else (`"type":"result"`
   vs. a nested `subtype`, e.g. the `"system"`/`"post_turn_summary"` message observed in
   the smoke test — is *that* now the real terminal signal for some turns, with the
   classic `"result"` line only appearing for certain turn types)?
3. If the line is genuinely never emitted for a turn that ends by calling an MCP tool:
   check whether `claude`'s own headless mode simply doesn't print a final `result`
   summary when the last action was a tool call it auto-approved via
   `--allowedTools`/`--strict-mcp-config`, as opposed to ending on assistant text.
4. Check whether the stdout `BufReader::lines()` read loop (`src/agent/claude.rs`,
   around `ClaudeSession::run_turn`) could be racing process exit -- e.g. `child.wait()`
   reaping the process while an already-buffered final line hasn't been read yet, on a
   turn producing enough output that stdout backpressure/buffering behaves differently
   than the small manual smoke test.

**Fix**, once the actual cause is known -- likely one of:
- Recognize whatever the real terminal message shape turns out to be (a renamed field,
  a different `type`, or a `usage` object at a different JSON path) in `handle_message`/
  `extract_usage`, with a unit test built from the *actual* captured JSON from step 1,
  not a hand-guessed fixture.
- If the process can exit before the last buffered line is delivered, read remaining
  buffered stdout after `next_line()` returns `Ok(None)` (or before `child.wait()`) rather
  than assuming EOF and process exit happen in a safe order.
- If `claude` genuinely never emits a `result` line for some turn types, use whatever it
  *does* emit as the token-usage source for those turns (e.g. summing incremental usage
  from `assistant` message chunks, if turns_json exposes per-chunk usage) rather than
  reporting 0.

**Do not paper over it** with a silent fallback that keeps reporting 0 -- if a genuine
gap turns out to be unfixable for some turn shape, log a clear warning naming the turn/
session so it's visible on `/events`, instead of a metric that looks fine but is wrong.

## Acceptance criteria

- [ ] A real multi-tool-call turn against the actual `claude` CLI installed in this repo
      produces a non-zero, correct `turn_completed` (or equivalent) event with real
      `input_tokens`/`output_tokens`.
- [ ] `symphony-report.html` and `/usage` show non-zero, plausible token totals after a
      real dispatched ticket completes end to end (not a synthetic test double).
- [ ] A unit test in `src/agent/claude.rs` is built from an actual captured stream-json
      transcript (not hand-written JSON guessing at the shape), covering whatever the
      real terminal event turns out to be.
- [ ] The existing "narrow race" caveat in AGENTS.md (`## Usage report`) is either
      resolved or rewritten to accurately describe what's still a gap and why.
- [ ] No change to `codex`/`opencode` backends' own (separately caveated) usage handling.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Fixing token accounting for the `codex` backend (already documented as not detecting
  tool calls at all) or `opencode`.
- Any of the AI Roadmap 2026 budget/cost features (`AIR-11`, `AIR-22`) that build on top
  of token accounting -- they need this fixed first, but this ticket is just "the number
  is real," not budgets/pricing/tiering.
