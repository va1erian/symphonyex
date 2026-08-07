---
identifier: AIR-24
title: Federated orchestrator registry and cross-domain delegation
state: todo
priority: 2
labels: [phase-3, federation]
dispatchable: true
depends_on: [AIR-23]
---
## Context

Roadmap §6: Application Orchestrator Agents connect with other application orchestrators and
with Data, Infrastructure, Support and Security Orchestrators — using common contracts,
executing independent domain activities in parallel, preserving domain ownership, coordinating
dependencies and priorities, returning evidence and risks to the initiating Orchestrator, and
maintaining traceability across the complete workflow.

AIR-23 gives Symphony a contract to speak. This ticket makes it speak to peers.

## Scope

**Registry.** Configured peers, each with the AIR-23 contract or an adapter to a foreign one:

```yaml
federation:
  identity: {name: platform-v2-checkout, domain: application}
  peers:
    - {name: data-platform, domain: data, url: https://..., token: $DATA_ORCH_TOKEN}
    - {name: infra, domain: infrastructure, url: https://..., token: $INFRA_TOKEN}
    - {name: search-app, domain: application, url: https://..., token: $SEARCH_TOKEN}
```

On registration, handshake via `/api/v1/capabilities` and cache what each peer can actually
do; a peer that fails the handshake is marked degraded rather than silently used.

**Delegation.** A cycle can emit a `delegate_request` when work falls outside its domain — a
schema change owned by the data platform, a new queue owned by infrastructure. The delegating
cycle:

- posts a request to the peer over `/api/v1/requests` with its correlation id,
- parks in a `waiting_on_peer` state (AIR-13) that releases its worker slot and is exempt from
  stall timeouts but subject to its own `federation.peer_timeout`,
- resumes when the peer reports completion (SSE subscription, polling fallback), pulling the
  peer's evidence bundle into its own bundle (AIR-9) so the MR shows the whole chain.

**Domain ownership is absolute.** Symphony never executes work in a peer's domain, even if it
technically could — it asks, waits, and records. That is the roadmap's "preserve domain
ownership", and it is also the only way the evidence trail stays meaningful.

**Cross-orchestrator dependency and priority coordination.** Delegations join the same
dependency graph the merge train (AIR-16) and scheduler (AIR-14) already read, so an MR
blocked on a peer's schema change never gets promoted ahead of it. Priority is exchanged in
the request and honoured on a best-effort basis — say so plainly rather than implying a
guarantee across administrative boundaries.

**Failure semantics.** Peer unreachable, request rejected, or timed out → escalate to a human
with full context (AIR-19). Never proceed as if a delegated dependency were satisfied. Retries
are bounded and idempotent (`Idempotency-Key`, AIR-23).

**Traceability.** A correlation id propagates through every request, event and artifact on
both sides, so a delivery spanning three orchestrators can be reconstructed end to end from
either end.

## Acceptance criteria

- [ ] Peer registration, capability handshake and degraded-peer marking work against a
      `wiremock`-faked peer.
- [ ] A delegating cycle parks, releases its slot, resumes on peer completion and incorporates
      the peer's evidence into its bundle.
- [ ] Delegations participate in scheduling and merge-train ordering.
- [ ] All failure modes escalate with context; retries are bounded and idempotent.
- [ ] Correlation ids appear on every related event and artifact on both sides.
- [ ] Federation off by default; no peers configured → no behaviour change.

## Out of scope

- Platform-wide aggregate analysis across peers (AIR-25).

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
