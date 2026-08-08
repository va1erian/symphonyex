---
identifier: CONFIGUI-2
title: Config UI foundation — read-only view of WORKFLOW.md's config
state: todo
priority: 2
labels: [dashboard, config, ui]
dispatchable: true
---
## Context

`WORKFLOW.md`'s `---`-delimited YAML front matter is Symphony's entire runtime
configuration surface (`src/config.rs::resolve` — tracker, polling, workspace, hooks,
agent/claude/codex/opencode, repo, swebot incl. chat, pipeline incl. stages/roles/
security/test, budgets, stop_conditions, observability, pricing). Today the only way to
read or change any of it is to open the file in a text editor and know the schema by
heart — there's no discoverability, no inline explanation of what a setting does or what
values are legal, and no feedback short of restarting Symphony and reading a startup
error.

The existing dashboard (`src/status.rs`, an axum router already mounted both standalone
and nested under `symphony serve`) is the right home for this: it already has a page per
concern (`/security`, `/observability`, `/approvals`, `/evidence`...), each server-rendered
HTML, `base_path`-aware, some with POST-based human actions guarded by
`admin_token_ok`/`admin_token_allows`. A config UI should be one more page in that same
family, not a separate tool.

This ticket is the foundation: get a `/config` page on the dashboard showing every
section of the resolved, effective configuration, human-readably, with a short
explanation next to each field — **before** any editing exists. Editing (CONFIGUI-3/4/5)
builds on this page rather than replacing it.

## Scope

- New `/config` route in `src/status.rs` (registered the same way as the existing routes
  in the `Router::new()...route(...)` chain), linked from the dashboard's nav alongside
  `/events`, `/usage`, etc.
- Render every top-level config section as its own card/panel, in the same visual style
  as the existing pages (reuse whatever shared layout/CSS helper the other pages already
  call — do not hand-roll a second style).
- For each field shown: its current effective value (after defaults and `$VAR`
  resolution status — show *that* a secret-like field resolves from an env var, never
  the resolved value itself, matching how `repo.token`/`claude.api_key`/etc. are already
  treated as `$VAR_NAME` references elsewhere in this codebase) and a one- or two-sentence
  explanation of what it controls and why you'd change it. For nested structs
  (`ClaudeConfig`, `RepoConfig`, `SwebotConfig`, `PipelineConfig`, etc.) source the
  explanation from the doc comment already on the field in `src/config.rs` — don't
  re-derive a second copy of that knowledge. `EffectiveConfig`'s own top-level scalar
  fields (`tracker_kind`, `poll_interval_ms`, `workspace_root`, `hook_*`,
  `max_concurrent_agents`, `max_turns`, `agent_backend`, etc.) mostly have no doc comment
  today — add a short one there as part of this ticket rather than assuming it exists.
- A raw-YAML view of the front matter as actually written in `WORKFLOW.md` (via
  `frontmatter::split`), so a user can see exactly what's on disk vs. what defaults were
  applied to produce the effective config above.
- No write path yet. This ticket is display-only.

## Acceptance criteria

- [ ] `/config` renders all sections `EffectiveConfig` exposes (tracker, polling,
      workspace, hooks, agent/claude/codex/opencode, repo, swebot incl. chat, pipeline
      incl. stages/roles/security/test/review, budgets, stop_conditions, observability,
      pricing) without needing to open `WORKFLOW.md`.
- [ ] Every field has an inline explanation; none are unlabeled raw key/value dumps.
- [ ] Fields backed by an env-var reference (`token_env`, `api_key_env`, etc.) show that
      they're set and which var name they reference, never a resolved secret value.
- [ ] The page works both standalone (`--port` single-project mode) and nested under
      `symphony serve` (`base_path`-aware, matching how every other page already handles
      this).
- [ ] A `status.rs` test asserts the page renders and includes at least one field from
      each major section.

## Out of scope

- Editing anything (CONFIGUI-3, CONFIGUI-4, CONFIGUI-5).
- Changing `config.rs`'s parsing/validation logic — this only reads the already-resolved
  `EffectiveConfig` plus the raw front matter, it doesn't change how either is produced.
