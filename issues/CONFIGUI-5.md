---
identifier: CONFIGUI-5
title: Config UI — edit the pipeline (stages, roles, security, test, review, approval)
state: todo
priority: 3
labels: [dashboard, config, ui]
dispatchable: true
depends_on: [CONFIGUI-3]
---
## Context

`pipeline.*` is the structurally hardest part of the config: `pipeline.stages` is an
ordered array of objects (`StageConfig`: id/role/max_turns/on_failure/blocking/
optional/requires_approval/budget) where order matters (it's the execution sequence),
each stage's `role` must resolve to either a built-in role or a `roles.<name>` entry
(validated today via `ConfigError::UnknownStageRole`), and `roles.*` is itself a map of
project-defined overrides (`RoleConfig`: prompt/prompt_file/backend/model/max_turns/
tool_policy). This doesn't fit CONFIGUI-3/CONFIGUI-4's flat-field form pattern and deserves its
own UI: add/remove/reorder stages, and add/edit/remove named roles, with the
cross-references between them kept valid at every step.

This depends on CONFIGUI-3 (the write path) but not on CONFIGUI-4 — it can land in parallel with
it once CONFIGUI-3 is done.

## Scope

- `pipeline.enabled`, `pipeline.blocked_state`, `pipeline.awaiting_approval_state`.
- `pipeline.stages[]`: an ordered list editor — add a stage, remove a stage, reorder
  (drag or up/down controls), edit each stage's fields inline. The `role` field is a
  dropdown populated from the built-in role names (`src/roles/builtin`) plus whatever
  `roles.*` entries currently exist in the config being edited — never free text, so an
  unknown-role save (`ConfigError::UnknownStageRole`) becomes unreachable through the UI
  rather than merely rejected after the fact.
- `roles.<name>`: add/edit/remove named role overrides — `prompt` (textarea) vs.
  `prompt_file` (path, validated readable per `ConfigError::UnreadableRolePromptFile`)
  as mutually-exclusive alternatives in the same UI slot, `backend`, `model`,
  `max_turns`, and `tool_policy` (whatever editing surface fits `agent::ToolPolicy`'s
  actual shape — inspect it before designing this control, don't assume it's a flat
  struct).
- `pipeline.approval.auto_approve_when`: `risk`, `impacted_components_allowlist`
  (editable string list), `max_estimate_turns`.
- `pipeline.review.max_rework_rounds`.
- `pipeline.security.*`: `block_on` (multi-select over `Severity`), `scanners[]`
  (add/remove name+command rows).
- `pipeline.test.*`: `commands` (ordered name->command pairs, add/remove/reorder same as
  stages), `coverage.*` (command/format/min_line_percent/path).
- Deleting a role that a stage still references, or a stage whose `id` something else
  depends on, must be caught client-side where practical and always re-validated
  server-side via `config::resolve` before writing (reusing CONFIGUI-3's validate-then-write
  path) — the same "reject before persisting an unloadable config" guarantee as every
  other ticket in this series.

## Acceptance criteria

- [ ] A stage can be added, removed, reordered, and edited entirely through `/config`,
      and the resulting `pipeline.stages` YAML array reflects the new order exactly.
- [ ] A stage's `role` can only be set to a value that resolves (built-in or
      `roles.<name>`) — the UI cannot produce an `UnknownStageRole` save.
- [ ] A `roles.<name>` entry can be added, edited (including switching between
      `prompt` and `prompt_file`) and removed.
- [ ] `pipeline.security.scanners[]` and `pipeline.test.commands` support add/remove/
      reorder, matching the stages list's interaction pattern (one consistent list-editor
      component, not three bespoke ones).
- [ ] Removing a role still referenced by a stage is rejected (client-side hint plus
      server-side `config::resolve` rejection as the authoritative check) rather than
      silently producing a config that fails to load.
- [ ] Same save/validate/atomic-write/admin-token guarantees as CONFIGUI-3.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- `swebot.*`, `observability.*`, `budgets.*`, `stop_conditions.*`, `pricing.*` (CONFIGUI-4).
- Changing pipeline *execution* semantics (`orchestrator.rs`) — this is purely an
  authoring UI for the existing `pipeline.*` schema.
