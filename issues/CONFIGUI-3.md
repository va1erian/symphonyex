---
identifier: CONFIGUI-3
title: Config UI — edit core settings and write them back to WORKFLOW.md
state: todo
priority: 2
labels: [dashboard, config, ui]
dispatchable: true
depends_on: [CONFIGUI-2]
---
## Context

CONFIGUI-2 gets every config section on screen, read-only. This ticket adds the first slice
of actual editing: the settings a project is most likely to tune day to day —
`tracker.*`, `polling.*`, `workspace.*`, `hooks.*`, `agent.*`, `claude.*`, `codex.*`,
`opencode.*`, `repo.*` — editable from the `/config` page and written back to
`WORKFLOW.md`'s YAML front matter, with the file's Markdown prompt body left untouched.

The write path is the hard part and sets the pattern the later tickets (CONFIGUI-4, CONFIGUI-5)
reuse:

- **Round-trip safety.** `frontmatter::split` parses the front matter into a
  `serde_yaml::Value` and hands back the body separately; nothing in the codebase today
  writes `WORKFLOW.md` back out. `serde_yaml` does not preserve comments or key
  ordering — re-serializing the whole map naively would silently strip any comments a
  project wrote in `WORKFLOW.md`. Edit only the specific keys the submitted form
  actually changed, in place in the parsed `Value` tree, then re-serialize just that
  tree and reassemble `--- \n<yaml>\n---\n<body>` — and prove in a test that untouched
  keys and their sibling comments' *structure* (i.e. nothing outside the edited keys
  gets reordered or dropped) survives a save. If comment preservation genuinely can't be
  done with `serde_yaml::Value`, say so explicitly and land a documented, tested
  trade-off rather than a silent one.
- **Validate before writing.** Reuse `config::resolve` on the candidate merged config
  before it's ever written to disk — an edit that would make `WORKFLOW.md` fail to load
  next boot must be rejected with the same `ConfigError` message a startup failure would
  show, not saved and discovered later when Symphony won't start.
- **Never write a resolved secret.** Fields that are `$VAR_NAME` references
  (`repo.token`, `claude.api_key`, `codex`/`opencode` equivalents once they exist) must
  only ever accept/emit the `$VAR_NAME` form, validated the same way
  `envsub::var_name_of` already validates it in `config::resolve` — never a literal
  value, matching `ConfigError::InvalidRepoToken`/`InvalidClaudeApiKey` today.
- **Admin-token gated, POST-only.** Match the existing pattern (`security_override`,
  `evidence_override`, `approvals_decide`): a state-changing save is a `POST` guarded by
  `admin_token_ok`, never a GET.
- **Atomic write.** Write to a temp file in the same directory and rename over
  `WORKFLOW.md`, so a crash mid-write can't leave a half-written config file that fails
  to parse on next boot.

## Scope

- `/config/save` POST handler (or one per section — whichever keeps each form small and
  the diff reviewable) covering: `tracker.*` (kind/provider/active_states/
  terminal_states/required_labels), `polling.interval_ms`, `workspace.root` and
  `workspace.docker.*`, `hooks.timeout_ms` and the four `hook_*` script paths,
  `agent.*` (backend/max_concurrent_agents/max_turns/max_retry_backoff_ms and the
  per-state concurrency map), `claude.*`/`codex.*`/`opencode.*`, `repo.*`.
- Each field gets a form control matched to its type: a bounded numeric field for
  `*_ms`/`max_turns`-style settings (with the same lower bound the resolver already
  enforces, e.g. `hooks.timeout_ms` must stay positive per
  `ConfigError::InvalidHookTimeout`), a dropdown for enum-like fields
  (`agent.backend`, `claude.permission_mode`, `repo.provider`), checkboxes for booleans
  (`repo.pull_request`, `repo.evidence`, `docker.enabled`, ...), free text elsewhere.
  Reuse the same inline explanation text CONFIGUI-2 already renders next to each field.
- On save: validate (as above), write, re-render the page from the freshly re-resolved
  config so the user immediately sees the effect, and surface the exact `ConfigError`
  inline (next to the offending field where feasible) on rejection rather than a generic
  failure.
- A visible "restart required" note if changing a field doesn't take effect until
  Symphony's process restarts (confirm case by case which of these actually hot-reload
  vs. only apply at next boot; don't assume — check how/where `EffectiveConfig` is
  currently loaded and cached in `main.rs`/`service.rs`).

## Acceptance criteria

- [ ] Every field listed under Scope is editable from `/config` and persists to
      `WORKFLOW.md` on save.
- [ ] Saving an edit that would fail `config::resolve` is rejected before writing, with
      the same error message a startup failure would show.
- [ ] A save that touches one key leaves every other key, and the Markdown body below
      the front matter, byte-for-byte unchanged (tested).
- [ ] `repo.token`/`claude.api_key` (and codex/opencode equivalents) only ever
      round-trip as `$VAR_NAME`; a literal value is rejected with the existing
      `ConfigError` variant.
- [ ] Save is POST-only and rejected without a valid admin token, mirroring
      `security_override`'s existing test coverage shape.
- [ ] A crash/kill mid-write cannot leave `WORKFLOW.md` unparseable (atomic rename;
      test by asserting the temp-file-then-rename sequence, or by asserting the original
      file is untouched until the new one is fully written).
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- `swebot.*`, `observability.*`, `budgets.*`, `stop_conditions.*`, `pricing.*` (CONFIGUI-4).
- `pipeline.*` including `stages`/`roles` (CONFIGUI-5) — those are structurally different
  (arrays of nested objects) and deserve their own editing UI rather than being squeezed
  into this ticket's flat-field form pattern.
