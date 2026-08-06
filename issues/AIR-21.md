---
identifier: AIR-21
title: Provider adapter abstraction with automatic fallback to an approved provider
state: todo
priority: 1
labels: [phase-3, provider, resilience]
dispatchable: true
---
## Context

Roadmap §7 is unambiguous: *"The workflow must remain independent from OpenAI, Claude or
another provider"* — standard agent inputs, outputs, tools and exit criteria; provider
adapters to enable changing model or provider; **automatic fallback to an approved provider**.
The organization's approved tooling (Cursor, Codex, JetBrains AI) differs from its
experimental tooling (Claude Code), so provider portability is an operational requirement,
not a hedge.

Symphony is already partway there: `AgentBackend`/`AgentSession` (`src/agent/mod.rs`) is a
real abstraction with three implementations. What is missing is (a) capability parity being
explicit rather than assumed, and (b) any fallback at all — a backend failure fails the run.

## Scope

**Capability declaration.** Extend `AgentBackend` with a `capabilities()` descriptor:
MCP tool support, tool-restriction mechanism, session resume, streaming usage reporting,
model selection, container support. Today these differences are documented in prose in
AGENTS.md (`codex` has no MCP tool wiring, no SweBot support; `opencode` needs
`OPENCODE_CONFIG_CONTENT`; only `claude` supports Docker mode). Make them data, so the
orchestrator can refuse an impossible configuration at startup with a precise message
instead of failing halfway through a cycle.

**Fallback chain.**

```yaml
agent:
  backend: claude
  fallback: [opencode, codex]        # ordered, all must be "approved" providers
  fallback_on: [startup_failure, rate_limit, provider_error, timeout]
  fallback_max_switches: 2
```

Rules that make fallback safe rather than chaotic:

- Fallback happens at a **stage boundary or a turn boundary**, never mid-turn — a half-written
  turn is not portable across providers.
- A fallback target lacking a capability the current stage requires (e.g. tool restriction for
  a Reviewer role, AIR-2) is skipped, and the skip is recorded.
- Session continuity does not survive a switch. The new provider starts fresh with the cycle's
  artifacts (AIR-3) and knowledge (AIR-17) as its context — which is precisely why those
  exist as durable, provider-neutral state.
- Every switch is an event carrying the trigger, the from/to providers and the cycle's
  position, and counts toward `fallback_max_switches`; exhausting it escalates.

**Classify errors properly.** `AgentError` needs to distinguish transient provider problems
(rate limit, 5xx, timeout) from deterministic ones (bad config, denied tool, non-existent
model). Falling over to a second provider because the *prompt* is broken just burns a second
budget.

**Provider-neutral outputs.** Audit the codebase for places a provider's shape leaks into
stored state — event names, tool-call extraction (currently Claude-only, per AGENTS.md), usage
fields. Normalize them at the adapter boundary so metrics and artifacts read identically
whichever provider produced them; that normalization is what makes AIR-26's parity comparison
meaningful.

## Acceptance criteria

- [ ] `capabilities()` implemented for all three backends; an impossible config is rejected at
      startup with a message naming the missing capability.
- [ ] Fallback triggers on each configured condition, only at safe boundaries, skipping
      capability-incompatible targets, and is fully recorded.
- [ ] Deterministic errors never trigger fallback (unit-tested classification).
- [ ] Tool-call and usage extraction is normalized across backends (`codex`/`opencode` gaps
      either closed or explicitly reported as unsupported, never silently 0).
- [ ] `fallback:` unset → today's behaviour exactly.

## Out of scope

- Choosing a provider for cost reasons (AIR-22); measuring parity (AIR-26).

## Global constraints (apply to every AIR ticket)

**1. Tiny code, small core of abstractions.** This feature must be expressible as a thin
layer over the abstractions Symphony already has (`TrackerAdapter`, `AgentBackend`/
`AgentSession`, `RepoHost`/`DiscussionHost`, the hook runner, the workspace manager, the
SQLite event log, the `status.rs` router). Before adding a concept, try to express the
feature with an existing one; if a new concept is unavoidable, introduce **one**, name it
plainly, and make it general enough that the next ticket reuses it rather than adding its
own. A reviewer must be able to read the new module top to bottom in one sitting and hold
it in their head. Concretely: no framework, no code generation, no trait with a single
implementation and no prospect of a second, no config key that only exists to toggle a
branch three call-levels down. If the implementation is growing past a few hundred lines
of genuinely new logic, that is the signal to simplify or split the ticket — say so in the
MR rather than shipping the bulk.

**2. Always ship a human-facing UI.** Every capability here must be observable *and*
actionable by a human without reading logs, tailing SQLite or parsing config. That means,
in the existing dashboard (`src/status.rs`, mounted `base_path`-aware so it works both under
the single-project `--port` mode and nested under `symphony serve`):
- a view showing the feature's current state and history, live-updating through the existing
  SSE `/fragment-stream` mechanism rather than a page-refresh hack;
- the human actions the feature implies (approve, override, retry, unblock, cancel, clear)
  as real controls — POST, admin-token protected, never a state-changing GET link;
- an explanation surface: whatever the feature decided, the UI must show *why* (the rule that
  fired, the inputs that produced a score, the evidence behind a verdict). An automated
  decision a human cannot interrogate is not usable governance.
Naming, layout and interaction should match the existing pages, so the dashboard stays one
coherent tool rather than a pile of feature panels.
