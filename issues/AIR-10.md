---
identifier: AIR-10
title: Observability agent — telemetry, dashboards, SLOs and production validation evidence
state: done
priority: 2
labels:
- phase-1
- agent-role
- observability
dispatchable: true
depends_on:
- AIR-9
updated_at: 2026-08-07T09:21:25.093840600+00:00
---
## Context

Roadmap §4: the Observability Agent validates logs, metrics, dashboards, SLOs and production
behaviour, and supports migration to the Open Observability Platform; output is "telemetry
evidence and production validation". Roadmap §2 names the Datadog → Open Observability
Platform migration explicitly. Roadmap §4's headline acceptance criterion is *one complete
delivery from requirement to **production validation***, which is exactly this stage.

## Scope

Two distinct jobs — keep them as two stages, since one runs pre-merge and one post-deploy.

**1. `observability` (pre-merge).** Validates that the change is observable *before* it
ships: required log lines exist at the right level with structured fields, metrics/spans are
emitted for new code paths, no secret or PII lands in telemetry, and the non-functional
observability requirements from AIR-4 are satisfied. Produces a `telemetry_evidence`
artifact listing each signal, where it is emitted, and how it was verified. Where the project
declares a dashboard/SLO definition directory (`observability.definitions_dir`), the stage may
also propose dashboard or SLO changes as ordinary code in the same MR — reviewed by humans,
never applied through a live API.

**2. `production_validation` (post-deploy, opt-in).** After a merge is deployed, query the
observability backend for the deployed change's health over a configured window and record a
verdict. Backends are pluggable behind a small trait, because provider-neutrality is a
roadmap principle:

```yaml
observability:
  backend: otlp            # otlp | prometheus | datadog | none
  query_url: https://...
  token: $OBS_TOKEN        # env var name, same convention as repo.token
  validation:
    after_deploy: true
    window_minutes: 30
    checks:
      - {name: error_rate, query: "...", max: 0.01}
      - {name: p95_latency_ms, query: "...", max: 400}
```

Outcome (`healthy | degraded | unknown`) is recorded as a `production_validation` artifact,
posted back on the ticket, and feeds the Change Failure Rate metric in AIR-12. `unknown`
(backend unreachable, no data) is never reported as healthy.

**Trigger.** Post-deploy validation needs a deploy signal. Support two, both poll-based, no
webhook receiver: the code host's deployment/pipeline status API (`src/repo_host/*` already
speaks to both hosts) and a configurable command. Anything else is out of scope.

## Implementation notes

- `src/observability/` with the backend trait and one adapter per backend; `none` disables
  cleanly. Datadog and OTLP/Prometheus adapters share the check-evaluation logic so the
  migration is a config change, not a rewrite — that is the point.
- Reuse the `$VAR_NAME` env-var-reference convention for tokens; the value must never be
  embedded in config or logged, and must be included in the Docker-mode env forwarding scan.

## Acceptance criteria

- [ ] Pre-merge stage produces `telemetry_evidence` mapping each observability requirement to
      a verified signal, and flags secrets/PII in telemetry as findings.
- [ ] Post-deploy validation runs against a `wiremock`-faked backend for OTLP/Prometheus and
      Datadog, evaluating checks identically through the shared logic.
- [ ] `unknown` is distinguishable from `healthy` everywhere it surfaces.
- [ ] Deploy detection works from both the host status API and a configured command.
- [ ] `backend: none` (default) leaves the daemon's behaviour unchanged.

## Out of scope

- Rollback decisions — CI/CD and humans own those (roadmap guardrail).

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
