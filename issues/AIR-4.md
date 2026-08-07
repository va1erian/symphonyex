---
identifier: AIR-4
title: Requirements agent — validated requirements, acceptance criteria and ambiguity stop
state: done
priority: 1
labels:
- phase-1
- agent-role
- requirements
dispatchable: true
depends_on:
- AIR-2
- AIR-3
updated_at: 2026-08-06T19:04:30.552504500+00:00
---
## Context

Roadmap §4: the Requirements Agent digests PRDs and technical requirements, identifies
ambiguity, dependencies, constraints and non-functional requirements, and outputs validated
requirements with acceptance criteria. Roadmap guardrail: **"Stop when requirements,
dependencies or risks are unclear."** Symphony currently feeds the raw issue description
straight to a coding agent, which then guesses.

## Scope

The first pipeline stage. Reads the issue (title, description, labels, `depends_on`
blockers) plus any linked context the project configures, and records two artifacts:

- `requirements` — JSON: `[{id: "R1", statement, source, type: functional|non_functional,
  constraint?, dependency?}]`
- `acceptance_criteria` — JSON: `[{id: "AC1", requirement_ids: ["R1"], given, when, then}]`

Stable `R*`/`AC*` ids matter: every later stage (tests in AIR-6, review in AIR-7, the
traceability manifest in AIR-9) keys off them.

**Ambiguity handling.** When the agent finds a requirement it cannot resolve, it must not
invent one. It calls a new tool `raise_clarification({question, blocking, requirement_id?})`:

- `blocking: true` → the cycle stops, the issue moves to `pipeline.blocked_state`, and the
  question is posted back to the human. Posting reuses the existing repo host: a comment on
  the tracker issue where the adapter supports it, otherwise recorded as an artifact and
  surfaced on the dashboard. Requires an explicit human reply before the cycle resumes.
- `blocking: false` → recorded as a documented assumption in the `requirements` artifact
  (`assumption: true`) and carried into the PR body by AIR-9, so a reviewer sees exactly
  what the agent decided on its own.

**Non-functional requirements** are first-class: performance, security, observability and
operability constraints get `type: non_functional` and are what AIR-8 and AIR-10 validate
against.

## Implementation notes

- Built-in role prompt at `src/roles/builtin/requirements.md` (AIR-2), holding the extraction
  rubric and the explicit instruction to stop rather than guess.
- `raise_clarification` in `src/mcp.rs`, gated on `pipeline.enabled`; blocking answers are
  read back on the next poll tick using host-native markers, the same
  `<!-- swebot:answered:<id> -->` dedupe trick `src/swebot/` already relies on — no new
  webhook receiver.
- The stage must be skippable (`pipeline.stages[].optional: true`) for projects whose tickets
  are already written as specs.

## Acceptance criteria

- [ ] Running the stage against `issues/DEMO-1.md` produces both artifacts with stable ids.
- [ ] A blocking clarification stops the cycle, is visible on the dashboard, and the cycle
      resumes (from this stage) once a human answers.
- [ ] A non-blocking clarification appears as a documented assumption in the artifact.
- [ ] Requirements with `depends_on` blockers list them under `dependency`.
- [ ] Unit tests cover artifact schema conformance and the blocking/non-blocking split.

## Out of scope

- Pulling requirements from an external PRD system (Confluence/Jira) — the issue body and
  configured context files are the only sources here.

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
