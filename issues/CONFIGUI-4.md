---
identifier: CONFIGUI-4
title: Config UI — edit SweBot, observability, budgets and stop-condition settings
state: todo
priority: 3
labels: [dashboard, config, ui]
dispatchable: true
depends_on: [CONFIGUI-3]
---
## Context

CONFIGUI-3 lands the write path (validate-then-atomic-write-then-re-resolve, admin-token
gated, `$VAR_NAME`-only secrets) for the core settings. This ticket reuses that exact
pattern for the remaining flat-ish sections: `swebot.*` (including `swebot.chat.*`),
`observability.*` (including `observability.validation.checks[]`), `budgets.*` (all
four scopes plus `stop_conditions.*`), and `pricing.*`. No new write-path machinery
should be needed here — if it turns out one is, that's a sign CONFIGUI-3's abstraction
wasn't general enough and should be revisited rather than duplicated.

These sections have more cross-field validation than CONFIGUI-3's than the core settings
did (e.g. `swebot.chat.enabled` requires `swebot.enabled`; `swebot.enabled` requires a
GitHub- or GitLab-shaped `repo.url` plus a token; `observability.validation.checks[]`
entries are a small repeated struct, not scalars), so the explanations matter more here
— a user turning on `swebot.enabled` needs to see *before* saving that it also needs
`repo.pull_request`-shaped repo config and a token, not discover it from a rejected save.

## Scope

- `swebot.*`: `enabled`, `backend`, the four discussion-category/label fields (paired
  correctly per `repo.provider` — GitHub shows the two `discussion_category` fields,
  GitLab shows the two `label` fields, per `SwebotConfig`'s own doc comment on why both
  pairs coexist), `review.enabled`, `token` (as `$VAR_NAME`), and `swebot.chat.*`
  (`enabled`, `connectors` — a checklist against `KNOWN_CHAT_CONNECTORS`, the four
  numeric tunables, `auto_create_issue`).
- `observability.*`: `backend` (dropdown over `ObservabilityBackendKind`), `query_url`,
  `token` (as `$VAR_NAME`), `definitions_dir`, and `validation.*` including an editable
  list of `checks[]` (name/query/max triples — add/remove rows, not just edit existing
  ones).
- `budgets.*`: `currency`, `window`, `on_exceeded`, and the four `BudgetLimits` scopes
  (platform/application/cycle/stage), each optional tokens+cost pair.
- `stop_conditions.*`: `no_progress_turns`, `repeated_error`.
- `pricing.*`: the `<backend>/<model>` -> price-per-million-tokens override table
  (`crate::budget::PricingTable`) as an editable list of rows, on top of (not replacing)
  the built-in default table.
- Every cross-field precondition already enforced in `config::resolve`
  (`ChatRequiresSwebot`, `SwebotRequiresGithubRepo`/`SwebotRequiresGitlabRepo`,
  `SwebotRequiresRepoToken`, `UnknownChatConnector`, `UnsupportedObservabilityBackend`,
  `InvalidObservabilityToken`) must produce the same inline, field-attributed rejection
  on save that CONFIGUI-3 established, not a generic error.

## Acceptance criteria

- [ ] Every field listed under Scope is editable from `/config` and persists correctly.
- [ ] Enabling `swebot.enabled` (or `.chat.enabled`) without its required preconditions
      is rejected inline with the specific missing precondition named, before writing.
- [ ] `observability.validation.checks[]` and `pricing.*` support adding and removing
      rows, not just editing pre-existing ones.
- [ ] `swebot.token`/`observability.token` only ever round-trip as `$VAR_NAME`.
- [ ] Same save/validate/atomic-write/admin-token guarantees as CONFIGUI-3 (reused, not
      reimplemented).
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- `pipeline.*` (CONFIGUI-5).
- Any new write-path infrastructure — this should be a straightforward reuse of what
  CONFIGUI-3 built.
