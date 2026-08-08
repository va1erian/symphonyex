---
identifier: FEAT-4
title: Give /requirements and /reviews a real issue-picker instead of a "pass ?issue= in the URL" hint
state: done
priority: 3
labels: [dashboard, ux]
dispatchable: true
---
## Context

`/requirements` and `/reviews` (`requirements_page`/`reviews_page`, `src/status.rs`) both
require an `?issue=<id>` query parameter and, when it's missing, render nothing but a
text hint:

```html
<p>Pass an issue id, e.g. <code>{base}/requirements?issue=1</code>.
   Find one on the <a href="{base}/events">Events</a> page.</p>
```

(the same pattern, word-for-word except the path, on both pages — confirmed at
`requirements_page` ~line 1683-1687 and `reviews_page` ~line 1918-1922). A human landing
on either page with no query string has no way to act on the page itself — they have to
already know an issue id, or go find one on a *different* page (`/events`) and hand-type
the URL. Every other dashboard page that lists things (`/events`, `/usage`, `/security`,
`/evidence`, `/observability`) shows real data immediately; these two are the only ones
that show nothing until the human already has information the page itself could have
given them.

## Scope

Replace the bare hint with:

1. **A real form to submit an issue id** — reuse this project's existing
   `<form class="filters" method="get">` pattern (already used identically on
   `/events`, `/usage`'s per-issue links, and elsewhere in `status.rs`) with a text input
   for the issue id and a submit button, rather than requiring the URL to be typed by
   hand.
2. **A list of the last 10 active issues to pick from**, each linking straight to
   `?issue=<id>` on the same page. `eventlog::usage_by_issue` (`src/eventlog.rs`) already
   returns exactly this shape — `issue_id`/`identifier`/`title`, ordered by most recent
   activity (`ORDER BY MAX(id) DESC`) — and is already used by `/usage` for the same
   "which issues have been active" purpose; reuse it (take the first 10) rather than
   writing a second query. No new `eventlog` function should be needed.

Apply the same fix to both `requirements_page` and `reviews_page` — they share the exact
same gap and the exact same fix shape; factor the picker into one small shared render
function (e.g. `issue_picker(base: &str, active_path: &str, recent: &[eventlog::IssueUsageRow]) -> String`)
rather than duplicating the markup twice, matching this codebase's existing preference
for one shared helper over copies drifting apart (see `page_shell`, `web::nav_links`,
etc. for the established pattern).

## Acceptance criteria

- [x] Visiting `/requirements` or `/reviews` with no `?issue=` shows a submittable form
      (not just a code-snippet hint) and a list of up to 10 recently-active issues to
      click through to.
- [x] The list reflects real data — an issue with recorded events appears; an
      issue/database with no history yet shows the empty state gracefully (no crash, no
      broken query).
- [x] Submitting the form or clicking a listed issue navigates to that page's own
      `?issue=<id>` exactly as manually typing the URL does today — no change to the
      already-working "issue specified" rendering path on either page.
- [x] Unit tests extending `requirements_page`'s/`reviews_page`'s existing "prompts for
      one" test coverage (`reviews_page_without_an_issue_prompts_for_one` and its
      requirements-page counterpart) to assert the form and list are present instead of
      just the old hint text.
- [x] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Any other dashboard page — this is scoped to the two pages that share this exact gap.
- Search/filtering within the picker (e.g. by title substring) — a plain "last 10" list
  is enough to fix "the page shows nothing usable by default"; a search box can be a
  follow-up if the list itself proves insufficient in practice.
