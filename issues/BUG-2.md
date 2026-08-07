---
identifier: BUG-2
title: Dashboard nav always shows "Chat" even when swebot.chat is disabled, linking to a 404
state: todo
priority: 2
labels: [bug, dashboard, swebot-chat]
dispatchable: true
---
## Context

`src/web.rs`'s `NAV_LINKS` (shared by every status-dashboard page: `/`, `/events`,
`/usage`) hardcodes a `"Chat"` entry pointing at `/chat`:

```rust
pub const NAV_LINKS: &[NavLink] = &[
    NavLink { href: "/", label: "Status" },
    NavLink { href: "/events", label: "Events" },
    NavLink { href: "/usage", label: "Usage" },
    NavLink { href: "/chat", label: "Chat" },
];
```

`status.rs`'s `page_shell()` renders this list unconditionally on every dashboard page
(`crate::web::nav(crate::web::NAV_LINKS, active_nav, base)`). But the `/chat` route
itself is only ever mounted when chat is actually on:

- Single-project (`src/orchestrator.rs::run_inner`): `chat_router` is built via
  `chat.as_ref().filter(|handles| handles.web_enabled).map(...)`, and only
  `.nest("/chat", chat_router)`'d onto the dashboard when that's `Some` -- i.e. only
  when `swebot.enabled && swebot.chat.enabled` with the `web` connector active
  (`ChatHandles::web_enabled`, `src/swebot/chat/mod.rs`).
- Multi-project (`src/service.rs::project_proxy`): same condition, `Some(handles) if
  handles.web_enabled => sub_router.nest(...)`.

So on any project that doesn't have `swebot.chat.enabled` (the common case --
`swebot.chat` defaults to off even when `swebot.enabled` itself is on, per
`SwebotChatConfig`'s own default), every dashboard page shows a "Chat" tab that leads
to a 404. Confirmed by inspection of both `status.rs`'s single `page_shell()` call site
and `service.rs`'s nested one -- neither passes any chat-enabled signal into `nav()`
today; `NAV_LINKS` is a `const`, not something either call site can vary per project.

## Scope

Make the nav's "Chat" entry conditional on whether `/chat` is actually mounted for the
page being rendered, in both the single-project and nested multi-project dashboards.

- `src/status.rs`: `AppState` gains a `chat_enabled: bool` (or equivalent), threaded
  through `router(status_rx, workflow_dir, base_path, ...)`'s signature. `page_shell()`
  builds its nav list from `crate::web::NAV_LINKS` filtered down when `!chat_enabled`
  (or a small `nav_links(chat_enabled: bool) -> Vec<NavLink>` helper in `web.rs`, since
  the same filtering is needed in `service.rs`'s nested case too -- one helper, not two
  copies of the same `if` in two files).
- Wire the actual value in from both call sites:
  - `src/orchestrator.rs::run_inner` already computes the same boolean inline (the
    `chat.as_ref().filter(|handles| handles.web_enabled)` expression) -- pass that
    through to `status::router`/`serve_composite` instead of only using it to decide
    whether to `.nest()` the chat router.
  - `src/service.rs::project_proxy` -- same `handles.web_enabled` check already present
    right next to where it nests the chat sub-router; thread the same value into the
    `status::router(...)` call a few lines above it.
- `src/swebot/chat/web.rs`'s own pages (rendered by the chat UI itself) already only
  exist when chat is enabled, so their unconditional `NAV_LINKS` render is not itself
  buggy -- but if the nav becomes a filtered helper, have chat's pages call it with
  `chat_enabled: true` for consistency (their own `/chat` link should stay active, not
  disappear).

Keep this to the nav/link layer only -- no change to `NAV_LINKS` itself needing new
entries, no new route.

## Acceptance criteria

- [ ] With `swebot.chat.enabled` unset/false (or `swebot` unset entirely, the default),
      the dashboard's nav on `/`, `/events`, and `/usage` does **not** show a "Chat" link,
      in both single-project (`symphony ./WORKFLOW.md --port`) and nested multi-project
      (`symphony serve`) modes.
- [ ] With `swebot.chat.enabled: true` and the `web` connector active, the "Chat" link
      still appears and still works exactly as today, in both modes.
- [ ] Chat's own pages (`swebot/chat/web.rs`) still show themselves as the active nav
      item when chat is enabled.
- [ ] Unit tests: `web.rs`'s nav-filtering behavior (chat present/absent) and at least
      one `status.rs` test asserting the rendered page HTML omits/includes the `/chat`
      link based on the new flag (extending the existing `page_shell`/`nav` test
      coverage in `web.rs`'s and `status.rs`'s own test modules).
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Any other disabled-feature-shows-a-dead-link cases -- this ticket is scoped to the
  one confirmed instance (chat). If a similar pattern exists elsewhere, file a separate
  ticket for it rather than folding it in here.
