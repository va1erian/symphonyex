---
identifier: AIR-8
title: Security agent — OWASP-aligned validation, risk classification and blocking findings
state: done
priority: 1
labels:
- phase-1
- agent-role
- security
dispatchable: true
depends_on:
- AIR-7
updated_at: 2026-08-07T04:23:09.290214300+00:00
---
## Context

Roadmap §4: the Security Agent validates application security controls and risks using
OWASP-aligned requirements and outputs security evidence, a risk classification and
**blocking findings**. Roadmap §2 lists Applicative Security as an engineering priority:
"apply security validation throughout delivery, aligned with OWASP standards". Symphony has
no security stage; its own posture section (AGENTS.md §"Trust and safety") is about running
agents safely, not about validating the code they produce.

## Scope

A stage with `allow_edits: false`, running after review and before release, producing a
`security_findings` artifact:

```json
{
  "schema_version": 1,
  "risk_classification": "low|medium|high|critical",
  "owasp_checklist": [{"id": "A01:2021", "name": "Broken Access Control",
                       "applicable": true, "status": "pass|fail|not_applicable",
                       "evidence": "..."}],
  "findings": [{"id": "S1", "severity": "critical|high|medium|low",
                "owasp_id": "A03:2021", "cwe": "CWE-89",
                "file": "src/x.rs", "line": 10,
                "summary": "...", "exploit_scenario": "...", "remediation": "..."}],
  "secrets_scan": {"status": "clean|findings", "matches": []},
  "dependency_scan": {"tool": "cargo audit", "advisories": []}
}
```

**Scope of review is the change, not the world.** The agent reviews the cycle's diff plus the
code paths it touches, against the OWASP Top 10 checklist and the non-functional security
requirements captured in AIR-4. A full-repo audit would blow the token budget on every ticket
and produce findings nobody asked for.

**Blocking semantics.** `pipeline.security.block_on: [critical, high]` (default). A finding
at or above the threshold stops the cycle: the issue parks in `pipeline.blocked_state`, the
findings are posted for a human, and no PR is opened. This is the one place where an agent
verdict genuinely halts delivery — the roadmap is explicit that Security produces "blocking
findings" — but the *override* is human-only, through the AIR-5 approval channel, and the
override is recorded with its justification.

**Deterministic scanners alongside the model.** Configurable commands
(`pipeline.security.scanners: [{name, command, format}]` — e.g. `cargo audit`, `gitleaks`,
`semgrep`) run through the hook plumbing and their output is folded into the artifact. A
model reasoning about a diff and a scanner grepping for known patterns catch different
things; the roadmap asks for "automated quality and security controls", not a chat about
security.

## Implementation notes

- `src/roles/builtin/security.md` carrying the OWASP Top 10 checklist as the rubric.
- `src/security/` for scanner adapters + normalization into the artifact shape; unavailable
  scanners are reported as `not_run`, never a silent pass.
- Secret findings must never be echoed in full into the artifact, the event log or the PR
  body — store a redacted match with file and line only. A security stage that leaks the
  secret it found into a public PR body is worse than no stage.

## Acceptance criteria

- [ ] Schema-valid `security_findings` artifact including the full OWASP checklist with
      explicit `not_applicable` justifications.
- [ ] A `high`/`critical` finding blocks the cycle and no PR is opened.
- [ ] A human override through the approval channel unblocks it and is recorded with reason.
- [ ] Configured scanners run through the hook plumbing (Docker mode included); a missing
      scanner reports `not_run` rather than passing.
- [ ] Secret matches are redacted everywhere they surface (unit test on the redaction).

## Out of scope

- Infrastructure/cloud security posture — that belongs to a Security Orchestrator in
  Phase 3 (AIR-24).

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
