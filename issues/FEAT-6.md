---
identifier: FEAT-6
title: Render artifact content by content_type on /artifacts/{id} instead of a raw dump
state: done
priority: 3
labels: [dashboard, ux]
dispatchable: true
---
## Context

Observed live on `/artifacts/{id}` (`artifact_raw_page`, `src/status.rs`, ~line 1618):
every artifact's content is rendered identically regardless of its actual
`content_type` — read as bytes, lossily converted to a string, HTML-escaped, and dumped
into a single `<pre class="table-wrap">{content}</pre>`. Confirmed on a real recorded
`plan` artifact (`content_type: application/json`): the JSON is unformatted, wrapped
mid-object at the container width with no line breaks a human placed, making a small,
genuinely useful structured summary (`schema_version`, `summary`, `risk`,
`impacted_components`, `estimate_turns`) hard to actually read.

This is a real gap relative to what the codebase already does elsewhere:
`content_type` is already a meaningful, dispatched-on field —
`artifacts::ext_for` (`src/artifacts.rs`, ~line 112) already switches on it
(`contains("json")` / `contains("markdown")` / `starts_with("text/")` / image types) to
pick a file extension when persisting an artifact to disk. `/evidence`'s
`render_evidence_content` (`src/status.rs`, ~line 2899) already renders Markdown to HTML
via `pulldown-cmark` (already a dependency — "reuse pulldown-cmark only for HTML views"
per `release.rs`'s own doc comment) for exactly this readability reason. `/artifacts/{id}`
is the one artifact-content view in this codebase that doesn't do any of that.

## Scope

In `artifact_raw_page`, render `content` based on `row.content_type` instead of always
using a raw `<pre>` dump:

- **`application/json`** (or anything containing `"json"`, matching `ext_for`'s own
  matching convention): pretty-print via `serde_json` (`to_string_pretty`) before
  escaping/rendering, so structure is actually visible. If parsing fails (malformed
  JSON despite the declared content type), fall back to the raw dump rather than
  erroring the whole page.
- **`text/markdown`** (or anything containing `"markdown"`): render to HTML via
  `pulldown-cmark`, reusing `render_evidence_content`'s exact approach
  (`Options::ENABLE_TABLES`, `pulldown_cmark::html::push_html`) rather than a second
  markdown-rendering implementation — factor the parser setup into a small shared helper
  both call, if that's cleaner than duplicating the two lines.
- **Everything else** (plain text, image types, anything unrecognized): keep today's
  `<pre>` behavior — this ticket is additive for the two content types that clearly
  benefit, not a rewrite of the whole page.
- A toggle to view the raw (unrendered) content alongside the rendered view is worth
  including if it's cheap (e.g. a `?raw=1` query param or a small inline expandable
  section) — a human debugging a malformed artifact still needs to see the exact bytes
  sometimes.

## Acceptance criteria

- [x] A `content_type: application/json` artifact renders pretty-printed, readable JSON
      instead of an unformatted single-line/wrapped dump.
- [x] A `content_type: text/markdown` artifact renders as formatted HTML (headings,
      lists, tables, code blocks), matching `/evidence`'s existing rendering quality.
- [x] A non-JSON, non-Markdown artifact (or a JSON artifact with genuinely malformed
      content) still renders exactly as it does today — no regression, no page error.
- [x] Unit tests extending `artifact_raw_page`'s existing coverage (or a new test module
      if none exists yet) for both the JSON and Markdown rendering paths, plus the
      malformed-JSON fallback.
- [x] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Changing `/artifacts`'s listing page (`artifacts_page`) — this is scoped to the
  single-artifact detail view.
- Rendering for content types beyond JSON/Markdown (images already have their own
  extension handling in `ext_for`; this ticket doesn't add inline image display).
