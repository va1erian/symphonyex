# Symphony × AI Roadmap 2026 — feature plan

Not a ticket. This is the index/rationale for the `AIR-*` tickets in this directory.
Read it first to understand why each ticket exists and what it is *not* meant to do.

## Where Symphony is today

| Roadmap concept | Symphony today | Gap |
| --- | --- | --- |
| Agent Pool of specialized roles | One coding agent per ticket + SweBot (Q&A / drafting / review) | No role pipeline: a ticket is dispatched to exactly one generalist agent that self-declares "done" |
| Governed flow requirement → production | Poll → workspace → agent → PR | No stages, no gates, no per-stage outputs |
| Evidence & traceability | `symphony.db` event log, `symphony-report.html`, `attach_evidence` (images only) | No structured, per-cycle evidence bundle; no requirement→test→finding traceability |
| Human approval by risk | None (`bypassPermissions`, single-operator trust posture) | No approval gate, no risk classification, no escalation policy |
| Parallel cycle control | `max_concurrent_agents`, per-state limits, `depends_on`, isolated workspaces | No conflict detection before merge, no MR sequencing, no durable/resumable cycle state |
| Application knowledge | `WORKFLOW.md` prompt body only | No context database, no retrieval, no publication gate |
| Provider independence | 3 backends (`claude`, `codex`, `opencode`) chosen statically | No fallback, no per-task model tiering, no caching, no parity measurement |
| Cost control | Token counting in `metrics.rs` / `/usage` | Accounting only — no budgets, no stopping conditions |
| Federation | `symphony serve` multi-repo UI | No machine API (`/api/v1` is explicitly out of scope today), no cross-orchestrator contract |
| Success measures | Turns, tokens, tool calls | No DORA (Deployment Frequency, Lead Time, CFR), no first-pass acceptance / rework metrics |

## Phasing

The three roadmap steps map onto three phases. Each phase is independently useful and
ends in a state Symphony can actually be run in — no phase leaves the daemon half-built.

### Phase 1 — Specialized Agent Workflow (roadmap §4)

Turn the single-agent dispatch into a **governed, staged delivery cycle** with per-stage
outputs, one human approval gate, and a complete evidence bundle. Target acceptance:
*one complete delivery from requirement to production validation, with full traceability
and known token cost.*

`AIR-1` … `AIR-12`

### Phase 2 — Application Orchestrator (roadmap §5)

Make the orchestrator durable, conflict-aware and knowledge-backed so several cycles can
run in parallel against one critical application without regressions.

`AIR-13` … `AIR-20`

### Phase 3 — Federation, provider independence and cost control (roadmap §6–§7)

Expose the orchestrator over a stable contract, make the provider genuinely swappable
with fallback and parity measurement, and support cross-domain coordination.

`AIR-21` … `AIR-26`

## Design constraints that apply to every ticket

Two of these are non-negotiable and are restated verbatim at the bottom of every `AIR-*`
ticket, because a coding agent reads one ticket, not this index:

- **Tiny code, generalized from a small core of abstractions.** Symphony's existing core is
  small on purpose — `TrackerAdapter`, `AgentBackend`/`AgentSession`, `RepoHost`/
  `DiscussionHost`, the hook runner, the workspace manager, the SQLite event log, the
  `status.rs` router. Every feature below must land as a thin layer over those, introducing at
  most **one** new concept and only when unavoidable, general enough that the next ticket
  reuses it. A human must be able to read any new module top to bottom in one sitting. Twenty-six
  tickets each adding "just one small abstraction" is how a legible daemon becomes a framework;
  the guard against that is refusing the second abstraction, every time.
- **Always ship a human-facing UI.** Every capability must be observable *and* actionable from
  the existing dashboard — current state, history, the human controls it implies (approve,
  override, retry, unblock, cancel, clear), and an explanation of *why* the system decided
  what it decided. Engineers stay accountable for outcomes (roadmap §3), which is only possible
  if they can see and steer what the agents did without reading logs or opening SQLite.

And:

- **Additive and opt-in.** Every new capability is off by default and enabled through a
  new `WORKFLOW.md` front-matter key. An existing `WORKFLOW.md` must keep working byte
  for byte, and `cargo test` must stay green without any new config.
- **Tool-agnostic.** Nothing may hardcode Claude/Codex/opencode behaviour outside
  `src/agent/*`. Roadmap §7 is explicit: provider-neutral architecture.
- **Host-mediated writes.** Agents never write tracker state, PR state, evidence or
  knowledge directly — always through an MCP tool executed in the `__mcp_tool_server`
  subprocess, exactly like `update_issue_state` and `open_pull_request` today
  (spec §11.5 boundary).
- **Deterministic, persistent, auditable state.** Anything a cycle depends on to resume
  goes in SQLite, not in memory (roadmap §5, "Parallel delivery controls").
- **Engineers own architecture, security risk and production decisions.** Agents produce
  evidence and recommendations; blocking/merging/deploying stays human or CI/CD.

## Ticket map

| Ticket | Phase | Feature |
| --- | --- | --- |
| [AIR-1](AIR-1.md) | 1 | Stage-pipeline engine (`pipeline:` config) |
| [AIR-2](AIR-2.md) | 1 | Per-role prompts, backends and tool restrictions |
| [AIR-3](AIR-3.md) | 1 | Cycle artifact store + `record_artifact` agent tool |
| [AIR-4](AIR-4.md) | 1 | Requirements agent + acceptance criteria |
| [AIR-5](AIR-5.md) | 1 | Planner/Architecture agent + human approval gate |
| [AIR-6](AIR-6.md) | 1 | Test agent + coverage/regression evidence |
| [AIR-7](AIR-7.md) | 1 | Reviewer agent stage |
| [AIR-8](AIR-8.md) | 1 | Security agent (OWASP) + risk classification |
| [AIR-9](AIR-9.md) | 1 | Release agent + evidence bundle & traceability manifest |
| [AIR-10](AIR-10.md) | 1 | Observability agent + telemetry/SLO evidence |
| [AIR-11](AIR-11.md) | 1 | Token and cost budgets with stopping conditions |
| [AIR-12](AIR-12.md) | 1 | DORA and success-measure metrics |
| [AIR-13](AIR-13.md) | 2 | Durable, resumable cycle state machine |
| [AIR-14](AIR-14.md) | 2 | Agent pool scaling and workload-aware scheduling |
| [AIR-15](AIR-15.md) | 2 | Conflict and code-ownership detection before merge |
| [AIR-16](AIR-16.md) | 2 | MR sequencing / merge-train ordering |
| [AIR-17](AIR-17.md) | 2 | Application knowledge base (context DB) + retrieval |
| [AIR-18](AIR-18.md) | 2 | Knowledge publication gate |
| [AIR-19](AIR-19.md) | 2 | Risk-based human escalation policy |
| [AIR-20](AIR-20.md) | 2 | Technical-debt detection and register |
| [AIR-21](AIR-21.md) | 3 | Provider adapter abstraction + automatic fallback |
| [AIR-22](AIR-22.md) | 3 | Model tiering and result caching |
| [AIR-23](AIR-23.md) | 3 | `/api/v1` orchestration API (requests, status, evidence, approvals) |
| [AIR-24](AIR-24.md) | 3 | Federated orchestrator registry and delegation |
| [AIR-25](AIR-25.md) | 3 | Application cartography and platform impact analysis |
| [AIR-26](AIR-26.md) | 3 | Provider parity and quality benchmark harness |

## Dependency shape

```
AIR-1 ──┬── AIR-2 ── AIR-4 ── AIR-5 ── AIR-6 ── AIR-7 ── AIR-8 ── AIR-9 ── AIR-10
        └── AIR-3 ──┘                                    │
AIR-11 ── AIR-12                                         │
AIR-13 ─┬─ AIR-14 ── AIR-19                              │
        ├─ AIR-15 ── AIR-16 ───────────────────────────── (needs AIR-9 evidence)
        └─ AIR-17 ─┬─ AIR-18
                   └─ AIR-20
AIR-21 ── AIR-22 ── AIR-26
AIR-23 ── AIR-24 ── AIR-25
```
