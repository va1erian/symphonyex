---
identifier: FEAT-5
title: Dashboard-style layout (fixed left nav, full-width content) with manual light/dark toggle and emoji nav icons
state: done
priority: 3
labels: [dashboard, ux]
dispatchable: true
---
## Context

`web::page_shell` (`src/web.rs`) renders every dashboard page as `<h1>{app_title}</h1>`
followed by a flat top nav (`web::nav`/`nav_links`, `<nav>{items}</nav>`, styled as a
wrapping flex row: `nav { display: flex; flex-wrap: wrap; gap: 4px 16px; }`) above the
page body. `NAV_LINKS` currently has 12 entries (Status, Events, Usage, Artifacts,
Requirements, Approvals, Reviews, Security, Evidence, Observability, Insights, Chat) —
already enough to wrap across multiple lines above the content on a normal-width window,
pushing the actual page down before a human sees anything.

Dark/light mode already exists, but only as OS-level auto-detection: `web::STYLE`
defines colors as CSS custom properties on `:root`, overridden under
`@media (prefers-color-scheme: light)` — there is no manual toggle and no persistence;
a human whose OS preference doesn't match what they want while looking at this dashboard
has no way to override it.

## Scope

**Layout**: restructure `page_shell` into a fixed-width left sidebar (nav) + full-width
main content area, replacing the current stacked `<h1>` + wrapping top nav. Standard
dashboard shape: sidebar pinned to the left (app title at the top of it, nav links below,
one per line instead of wrapping), content area fills the remaining width and scrolls
independently of the sidebar. Must stay responsive — collapse to a top bar (or a
toggleable off-canvas drawer) below some reasonable width breakpoint, matching this
project's own artifact-authoring convention of never letting content force horizontal
page scroll.

**Theme**: add a manual light/dark toggle control (in the sidebar) on top of the existing
`prefers-color-scheme` auto-detection, persisted in `localStorage` so it survives
navigation and reloads:
- On load, an inline script (following the existing pattern of `web::SCRIPT` being
  embedded directly in `page_shell`, no separate asset/build step) reads a stored
  preference from `localStorage` and applies it (e.g. a `data-theme="light"`/
  `"dark"` attribute on `<html>`) before first paint, falling back to the OS
  `prefers-color-scheme` result when nothing is stored yet — no flash of the wrong
  theme.
- The CSS custom properties already defined for both themes in `web::STYLE` should be
  reusable via a `[data-theme="light"]`/`[data-theme="dark"]` override on `:root`,
  layered alongside (not replacing) the existing `prefers-color-scheme` media query so a
  human who's never touched the toggle still gets correct auto-detected behavior.
- The toggle itself just needs to flip the stored preference and the `data-theme`
  attribute — no server round-trip, no new route.

**Nav icons**: prefix each sidebar nav entry with an emoji as a cheap icon substitute
(e.g. a status/heartbeat mark for Status, a chat bubble for Chat) — plain Unicode
characters in the label text, no icon font, no SVG sprite sheet, no new dependency.

**Tech stack**: this is a pure HTML/CSS/inline-JS change to `src/web.rs` (and whatever
`status.rs`/`service.rs`/`swebot::chat::web` call sites need updating for the new
`page_shell` shape) — no JS framework, no bundler, no build step, no new crate. Matches
this project's existing "one theme, one escaper, one shell" posture (`web.rs`'s own doc
comment) rather than introducing a second rendering approach alongside it.

## Acceptance criteria

- [x] Every page already using `page_shell` renders with the new sidebar layout without
      per-page changes beyond whatever `page_shell`'s own signature requires.
- [x] The layout is responsive: a narrow viewport collapses the sidebar into a top bar or
      drawer rather than clipping/overflowing content, and no page requires horizontal
      scrolling to read.
- [x] Toggling the theme persists across a page reload and across navigating to a
      different page (real `localStorage`, not just in-memory state).
- [x] A human who has never touched the toggle still gets the correct theme from their OS
      preference (`prefers-color-scheme`) — the manual override doesn't regress the
      existing auto-detect behavior.
- [x] No flash of the wrong theme on load (the stored/detected preference is applied
      before first paint, not after a visible re-render).
- [x] Every `NAV_LINKS` entry has an emoji prefix.
- [x] Existing tests covering `web::nav`/`page_shell`/`nav_links`
      (`src/web.rs`'s own test module, plus any `status.rs`/`swebot::chat::web` tests
      asserting on rendered nav HTML) are updated for the new markup shape rather than
      left broken.
- [x] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Any new JS framework, CSS framework, icon font/SVG library, or build tooling — emoji
  and the existing inline `<style>`/`<script>` approach are deliberate, cheap
  alternatives per the ticket's own instruction, not a stopgap to replace later.
- Changing what pages exist or what `NAV_LINKS` links to — this is a layout/theme change
  only, not a navigation-structure change (see FEAT-3/FEAT-4 for content-level dashboard
  improvements).
