---
identifier: AIR-23
title: /api/v1 orchestration API — requests, status, evidence and approvals
state: todo
priority: 1
labels: [phase-3, api, federation]
dispatchable: true
---
## Context

Roadmap §6's operating rules for federation start with *"use common contracts for requests,
status, evidence and approvals"*. AGENTS.md currently lists `/api/v1/*` under "deliberately
out of scope" — correct for a single-operator dashboard, blocking for federation. Nothing can
delegate to an orchestrator that only speaks HTML.

## Scope

A versioned JSON API served by the same router `src/status.rs` builds, mounted under
`/api/v1`, `base_path`-aware so it works both in single-project CLI mode and nested under
`symphony serve`.

**Endpoints** (the four roadmap contracts, plus the minimum to be useful):

| Method | Path | Purpose |
| --- | --- | --- |
| `POST` | `/api/v1/requests` | Submit a delivery request (intent, requirements, constraints, callback URL, correlation id) → creates a ticket via the tracker adapter and returns a request id |
| `GET` | `/api/v1/requests/{id}` | Request status: cycle, stage, state, blockers, ETA signals |
| `GET` | `/api/v1/cycles` / `/{id}` | Cycle list and detail (state, transitions, budget, risk) |
| `GET` | `/api/v1/cycles/{id}/evidence` | The evidence bundle (AIR-9), JSON |
| `GET`/`POST` | `/api/v1/approvals` / `/{id}` | List pending approvals; submit a decision (AIR-5, AIR-18) |
| `GET` | `/api/v1/knowledge` | Published knowledge, filterable by category (AIR-17) |
| `GET` | `/api/v1/health`, `/api/v1/capabilities` | Liveness; declared capabilities/version for federation handshakes |

**Contract discipline.** Publish an OpenAPI 3.1 document at `/api/v1/openapi.json`, generated
from the same types the handlers use so it cannot drift. Everything is additive within `v1`;
a breaking change means `v2`. This document is the contract other orchestrators integrate
against (AIR-24) — its stability is the whole feature.

**Auth and tenancy.** Bearer tokens with **scopes** (`read`, `submit`, `approve`, `admin`),
not the single shared admin token. Each token gets an identity recorded on every mutating
action, so an approval arriving over the API is as auditable as one clicked in the dashboard —
AIR-5's audit requirement must hold on both paths. Tokens are configured as env-var references
(`$VAR`), consistent with `repo.token`. Note explicitly in the docs that this extends
`symphony serve`'s single-operator posture into a *multi-client* one and what that does and
does not guarantee (still no per-repo access control; that gap must be stated, not implied).

**Events.** `GET /api/v1/events?stream=sse` reusing the existing `EventSource` plumbing behind
`/fragment-stream`, so a federated orchestrator can subscribe rather than poll. Polling stays
supported — it is what every other integration in this codebase does.

**Idempotency.** `POST /api/v1/requests` honours an `Idempotency-Key` header; a retried
submission returns the original request rather than creating a duplicate ticket.

## Acceptance criteria

- [ ] All endpoints implemented with typed request/response models and correct status codes.
- [ ] OpenAPI document is generated from the handler types and validates.
- [ ] Scoped tokens enforced per endpoint; every mutation records the caller identity.
- [ ] Approving over the API and in the dashboard produce identical audited outcomes.
- [ ] `Idempotency-Key` prevents duplicate tickets.
- [ ] SSE stream delivers events; polling remains available.
- [ ] API is off unless configured; the existing HTML dashboard is unaffected.
- [ ] AGENTS.md's "no `/api/v1`" out-of-scope entry is replaced with real documentation.

## Out of scope

- Cross-orchestrator delegation semantics (AIR-24).

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
