//! Shared HTML rendering primitives used by every hand-rolled web UI in this crate
//! (`status.rs`, `service.rs`, `metrics.rs`, `swebot::chat::web`). Previously each
//! module carried its own near-duplicate copy of the stylesheet, `escape()`, and
//! `urlencode()` -- independent copies that had already drifted (`service.rs`'s admin
//! dashboard had no `.badge`/`.tabs`/form styling, and no persistent nav at all; the
//! chat UI had its own hardcoded dark-only palette). One theme, one escaper, one
//! shell now.
//!
//! Colors are CSS custom properties (`--bg`, `--fg`, `--accent`, ...) rather than
//! hardcoded hex values, set once in `:root` and overridden under
//! `@media (prefers-color-scheme: light)` -- the old stylesheets were dark-only.

pub const STYLE: &str = r#"
  :root {
    --bg: #111318;
    --bg-alt: #1c1f26;
    --fg: #e8e8ec;
    --fg-strong: #ffffff;
    --fg-dim: #9a9fab;
    --fg-dimmer: #7b8090;
    --border: #333844;
    --border-soft: #262a33;
    --accent: #7fb8ff;
    --accent-strong: #a9d1ff;
    --good-bg: #1c3325;
    --good-fg: #7be08c;
    --bad-bg: #3a2020;
    --bad-fg: #ff9a9a;
    --warn-fg: #e0c56b;
    --focus-ring: #7fb8ff;
  }
  @media (prefers-color-scheme: light) {
    :root {
      --bg: #f6f7fb;
      --bg-alt: #ffffff;
      --fg: #1c1f26;
      --fg-strong: #0a0a0a;
      --fg-dim: #5b6070;
      --fg-dimmer: #6b7182;
      --border: #d7dbe3;
      --border-soft: #e6e9ef;
      --accent: #2b6cb0;
      --accent-strong: #1a4d80;
      --good-bg: #e3f6e7;
      --good-fg: #1f7a34;
      --bad-bg: #fbe4e4;
      --bad-fg: #b32828;
      --warn-fg: #8a6d1f;
      --focus-ring: #2b6cb0;
    }
  }
  * { box-sizing: border-box; }
  body { font-family: system-ui, sans-serif; background: var(--bg); color: var(--fg); margin: 0 auto; padding: 24px; max-width: 1180px; }
  h1 { font-size: 1.1rem; font-weight: 600; color: var(--accent); margin: 0 0 4px; }
  h2 { font-size: 1rem; color: var(--fg-strong); margin: 0 0 8px; }
  a { color: var(--accent); }
  .meta { color: var(--fg-dim); font-size: 0.8rem; margin-bottom: 12px; }
  nav { margin-bottom: 20px; display: flex; flex-wrap: wrap; gap: 4px 16px; }
  nav a { color: var(--accent); text-decoration: none; font-size: 0.85rem; }
  nav a:hover { text-decoration: underline; }
  nav a.active { color: var(--fg-strong); font-weight: 600; }
  .grid { display: flex; flex-wrap: wrap; gap: 12px; margin-bottom: 32px; }
  .card { background: var(--bg-alt); border: 1px solid var(--border); border-left: 4px solid var(--good-fg); border-radius: 6px; padding: 12px 14px; flex: 1 1 260px; min-width: 220px; max-width: 100%; }
  .card h2 { font-size: 0.95rem; margin: 0 0 6px; color: var(--fg-strong); }
  .card .row { font-size: 0.8rem; color: var(--fg-dim); margin: 2px 0; }
  .card .row b { color: var(--fg); }
  .card .msg { margin-top: 8px; font-size: 0.78rem; color: var(--fg); background: var(--bg); border-radius: 4px; padding: 6px 8px; max-height: 4.5em; overflow: hidden; cursor: pointer; }
  .card .msg.expanded { max-height: none; }
  .card .msg[data-expandable]::after { content: "\2026 click to expand"; display: block; color: var(--fg-dimmer); font-size: 0.7rem; margin-top: 2px; }
  .card .msg.expanded[data-expandable]::after { content: "click to collapse"; }
  .badge { display: inline-block; background: var(--good-bg); color: var(--good-fg); border-radius: 10px; padding: 1px 8px; font-size: 0.72rem; }
  .badge.closed { background: var(--bad-bg); color: var(--bad-fg); }
  .badge.warn { background: rgba(224,197,107,0.16); color: var(--warn-fg); }
  .table-wrap { overflow-x: auto; max-width: 100%; }
  table { border-collapse: collapse; width: 100%; max-width: 1100px; }
  th, td { text-align: left; padding: 6px 10px; font-size: 0.82rem; border-bottom: 1px solid var(--border-soft); }
  th { color: var(--fg-dim); font-weight: 500; }
  th[data-sort] { cursor: pointer; user-select: none; white-space: nowrap; }
  th[data-sort]:hover { color: var(--fg); }
  th.sorted-asc::after { content: " \2191"; }
  th.sorted-desc::after { content: " \2193"; }
  .msg-cell { max-height: 4.5em; overflow: hidden; cursor: pointer; }
  .msg-cell.expanded { max-height: none; white-space: normal; }
  .empty { color: var(--fg-dimmer); font-style: italic; }
  section { margin-bottom: 32px; }
  section h3 { font-size: 0.85rem; text-transform: uppercase; letter-spacing: 0.04em; color: var(--fg-dim); }
  form.filters { margin: 12px 0; display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
  form.filters input, form.filters select { background: var(--bg-alt); border: 1px solid var(--border); color: var(--fg); padding: 4px 8px; border-radius: 4px; font-size: 0.8rem; }
  form.filters button { background: var(--good-bg); border: 1px solid var(--good-fg); color: var(--good-fg); padding: 4px 12px; border-radius: 4px; font-size: 0.8rem; cursor: pointer; }
  .chips { display: flex; flex-wrap: wrap; gap: 6px; margin: 8px 0; }
  .chip { display: inline-flex; align-items: center; gap: 5px; background: var(--bg-alt); border: 1px solid var(--border); color: var(--fg-dim); border-radius: 12px; padding: 2px 6px 2px 10px; font-size: 0.75rem; }
  .chip a { color: var(--fg-dimmer); text-decoration: none; font-weight: 700; padding: 0 2px; }
  .chip a:hover { color: var(--bad-fg); }
  .pager { margin-top: 12px; font-size: 0.82rem; }
  .pager a { margin-right: 12px; text-decoration: none; }
  .stats { display: flex; gap: 14px; margin-bottom: 28px; flex-wrap: wrap; }
  .stats .stat { background: var(--bg-alt); border: 1px solid var(--border); border-radius: 8px; padding: 12px 18px; min-width: 110px; }
  .stats .stat .n { font-size: 1.5rem; font-weight: 700; color: var(--fg-strong); }
  .stats .stat .l { font-size: 0.72rem; color: var(--fg-dim); text-transform: uppercase; letter-spacing: 0.04em; margin-top: 2px; }
  .tabs { margin-bottom: 16px; display: flex; gap: 16px; flex-wrap: wrap; }
  .tabs a { color: var(--accent); text-decoration: none; font-size: 0.9rem; padding-bottom: 4px; }
  .tabs a.active { color: var(--fg-strong); font-weight: 600; border-bottom: 2px solid var(--accent); }
  .thread-list { border: 1px solid var(--border); border-radius: 6px; overflow: hidden; }
  .thread-row { display: flex; justify-content: space-between; gap: 12px; padding: 10px 14px; border-bottom: 1px solid var(--border-soft); flex-wrap: wrap; }
  .thread-row:last-child { border-bottom: none; }
  .thread-row a { color: var(--fg); text-decoration: none; font-size: 0.92rem; }
  .thread-row a:hover { text-decoration: underline; }
  .thread-row .sub { color: var(--fg-dim); font-size: 0.78rem; margin-top: 2px; }
  .thread-row .count { color: var(--good-fg); background: var(--good-bg); border-radius: 10px; padding: 1px 9px; font-size: 0.75rem; align-self: center; }
  .post { background: var(--bg-alt); border: 1px solid var(--border); border-radius: 6px; padding: 12px 16px; margin-bottom: 12px; }
  .post .byline { color: var(--fg-dim); font-size: 0.78rem; margin-bottom: 8px; }
  .post .byline b { color: var(--accent); }
  .post-body { font-size: 0.9rem; line-height: 1.5; }
  .post-body p:first-child { margin-top: 0; }
  .post-body pre { background: var(--bg); padding: 8px 10px; border-radius: 4px; overflow-x: auto; }
  .post-body code { background: var(--bg); padding: 1px 4px; border-radius: 3px; }
  form.compose { margin-top: 16px; }
  form.compose input[type=text], form.compose textarea, form.compose select { width: 100%; background: var(--bg-alt); border: 1px solid var(--border); color: var(--fg); padding: 8px 10px; border-radius: 4px; font-size: 0.85rem; margin-bottom: 4px; font-family: inherit; }
  form.compose textarea { min-height: 100px; resize: vertical; }
  form.compose button { background: var(--good-bg); border: 1px solid var(--good-fg); color: var(--good-fg); padding: 6px 16px; border-radius: 4px; font-size: 0.85rem; cursor: pointer; margin-top: 8px; }
  form.compose button:hover { filter: brightness(1.15); }
  .char-count { font-size: 0.72rem; color: var(--fg-dimmer); text-align: right; margin-bottom: 8px; }
  label { display: block; font-size: 0.78rem; color: var(--fg-dim); margin-bottom: 3px; margin-top: 10px; }
  label:first-of-type { margin-top: 0; }
  input, select, textarea { font-family: inherit; }
  form.admin input { background: var(--bg-alt); color: var(--fg); border: 1px solid var(--border); padding: 6px 8px; margin-top: 2px; width: 320px; max-width: 100%; border-radius: 4px; }
  form.admin button, .btn { background: var(--bg-alt); color: var(--fg); border: 1px solid var(--border); padding: 6px 14px; cursor: pointer; margin-top: 14px; border-radius: 4px; font-size: 0.85rem; }
  form.admin button:hover, .btn:hover { filter: brightness(1.2); }
  .btn-danger { background: var(--bad-bg); color: var(--bad-fg); border-color: var(--bad-fg); margin-top: 0; }
  .status-running { color: var(--good-fg); }
  .status-starting { color: var(--warn-fg); }
  .error-banner { background: var(--bad-bg); color: var(--bad-fg); border: 1px solid var(--bad-fg); border-radius: 6px; padding: 10px 14px; margin: 12px 0; font-size: 0.88rem; }
  .error-banner a { color: var(--bad-fg); text-decoration: underline; }
  :focus-visible { outline: 2px solid var(--focus-ring); outline-offset: 2px; }

  /* Chat-bubble transcript: the shared markup/style contract between the chat UI
     (`swebot::chat::web`, client-rendered from JSON via matching JS) and the
     issue-filtered `/events` view (`status.rs::render_transcript`, server-rendered
     from the same `.msg`/`.bubble`/`.status` shape) -- one visual language for "a
     conversation," whichever side produced it. */
  .transcript { display: flex; flex-direction: column; gap: 10px; }
  .msg { max-width: 78%; animation: msg-in 0.22s ease-out; }
  .msg.user { align-self: flex-end; }
  .msg.assistant { align-self: flex-start; }
  .msg.system, .msg.tool { align-self: center; max-width: 92%; }
  .bubble { border-radius: 10px; padding: 8px 12px; font-size: 0.9rem; line-height: 1.4; white-space: pre-wrap; word-break: break-word; }
  .bubble :first-child { margin-top: 0; }
  .bubble :last-child { margin-bottom: 0; }
  .bubble code { background: rgba(127,184,255,0.14); padding: 1px 5px; border-radius: 4px; font-size: 0.85em; }
  .bubble pre { background: var(--bg); border: 1px solid var(--border); border-radius: 6px; padding: 8px 10px; overflow-x: auto; white-space: pre; }
  .bubble pre code { background: none; padding: 0; }
  .bubble h1, .bubble h2, .bubble h3 { margin: 10px 0 4px; font-size: 1em; color: var(--accent); }
  .bubble ul, .bubble ol { margin: 4px 0; padding-left: 20px; }
  .bubble a { color: var(--accent); }
  .msg.user .bubble { background: var(--bg-alt); border: 1px solid var(--accent); }
  .msg.assistant .bubble { background: var(--bg-alt); border: 1px solid var(--border); }
  .msg.system .bubble { color: var(--fg-dim); background: var(--bg-alt); font-style: italic; border: 1px dashed var(--border); }
  .msg.tool .bubble { color: var(--fg-dim); background: var(--bg-alt); font-family: ui-monospace, SFMono-Regular, Consolas, monospace; font-size: 0.82rem; border: 1px solid var(--border-soft); }
  .status { font-size: 0.72rem; color: var(--fg-dimmer); margin-top: 3px; }
  .status .read { color: var(--good-fg); }
  .status a { color: var(--fg-dimmer); }
  @keyframes msg-in {
    from { opacity: 0; transform: translateY(6px); }
    to { opacity: 1; transform: translateY(0); }
  }
  @media (prefers-reduced-motion: reduce) {
    .msg { animation: none; }
  }
  @media (max-width: 640px) {
    body { padding: 14px; }
    .card { flex-basis: 100%; }
    form.admin input { width: 100%; }
    .stats { gap: 8px; }
    .stats .stat { min-width: 0; flex: 1 1 40%; }
    .msg { max-width: 94%; }
  }
"#;

/// Delegated click/submit handling shared by every page, so individual handlers don't
/// each need to wire up their own listeners: click-to-expand on truncated `.msg`/
/// `.msg-cell` content, client-side sort on `th[data-sort]` (reorders the rows already
/// on the page -- server-side pagination is unaffected, sorting only the current
/// page), a `confirm()` gate on any `<form data-confirm="...">` before it submits
/// (used for destructive actions like removing a registered repo), and a live
/// character counter under any `[maxlength]` field.
pub const SCRIPT: &str = r#"
document.addEventListener('click', function (e) {
  var msg = e.target.closest('.msg, .msg-cell');
  if (msg) { msg.classList.toggle('expanded'); return; }
  var th = e.target.closest('th[data-sort]');
  if (th) { sortTableByColumn(th); }
});
document.addEventListener('submit', function (e) {
  var msg = e.target.getAttribute('data-confirm');
  if (msg && !window.confirm(msg)) { e.preventDefault(); }
});
document.querySelectorAll('[maxlength]').forEach(function (el) {
  var max = el.getAttribute('maxlength');
  var counter = document.createElement('div');
  counter.className = 'char-count';
  el.insertAdjacentElement('afterend', counter);
  var update = function () { counter.textContent = el.value.length + ' / ' + max; };
  el.addEventListener('input', update);
  update();
});
function sortTableByColumn(th) {
  var table = th.closest('table');
  var tbody = table.querySelector('tbody');
  if (!tbody) return;
  var idx = Array.prototype.indexOf.call(th.parentNode.children, th);
  var asc = th.getAttribute('data-sort-dir') !== 'asc';
  var rows = Array.prototype.slice.call(tbody.querySelectorAll('tr'));
  rows.sort(function (a, b) {
    var av = cellSortValue(a, idx), bv = cellSortValue(b, idx);
    var an = parseFloat(av), bn = parseFloat(bv);
    var cmp = (!isNaN(an) && !isNaN(bn)) ? an - bn : av.localeCompare(bv);
    return asc ? cmp : -cmp;
  });
  rows.forEach(function (r) { tbody.appendChild(r); });
  table.querySelectorAll('th[data-sort]').forEach(function (h) {
    h.removeAttribute('data-sort-dir');
    h.classList.remove('sorted-asc', 'sorted-desc');
  });
  th.setAttribute('data-sort-dir', asc ? 'asc' : 'desc');
  th.classList.add(asc ? 'sorted-asc' : 'sorted-desc');
}
function cellSortValue(row, idx) {
  var cell = row.children[idx];
  if (!cell) return '';
  return cell.getAttribute('data-sort-value') || cell.textContent.trim();
}
"#;

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Minimal, dependency-free percent-encoding: this codebase's identifiers/event types
/// are plain ASCII (issue numbers, snake_case event names, board titles), so covering
/// the handful of characters actually meaningful in a query string (space, &, =, #, %,
/// +) is enough -- not a general-purpose encoder.
///
/// Also encodes `"`, `'`, `<`, `>` even though they're not URL-reserved: every call
/// site embeds the result straight into an `href="{}"` attribute built from
/// user-supplied query parameters (`status.rs`'s `/events` filters -- issue/type come
/// straight from the request's own query string), so a literal `"` or `>` here would
/// close the attribute/tag early and let the rest of the value execute as markup --
/// e.g. `?issue=42"><script>alert(1)</script>` reflected straight back into the page.
pub fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '&' => "%26".to_string(),
            '=' => "%3D".to_string(),
            '#' => "%23".to_string(),
            '%' => "%25".to_string(),
            '+' => "%2B".to_string(),
            '"' => "%22".to_string(),
            '\'' => "%27".to_string(),
            '<' => "%3C".to_string(),
            '>' => "%3E".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// One nav link. `href` is mount-relative (e.g. `"/events"`); `base` is prepended only
/// at render time so callers keep comparing/passing the same unprefixed identifiers
/// regardless of where a router ends up nested (see `status.rs`'s `AppState::base_path`
/// doc comment for why that indirection exists).
pub struct NavLink<'a> {
    pub href: &'a str,
    pub label: &'a str,
}

pub fn nav(links: &[NavLink], active: &str, base: &str) -> String {
    let items: String = links
        .iter()
        .map(|l| {
            let class = if l.href == active {
                " class=\"active\""
            } else {
                ""
            };
            format!(r#"<a href="{base}{}"{class}>{}</a>"#, l.href, l.label)
        })
        .collect();
    format!("<nav>{items}</nav>")
}

/// The one nav every per-project surface shares: `status.rs`'s dashboard/events/usage
/// pages and `swebot::chat::web`'s chat UI, all nested under the same `base_path`
/// (single-project root, or `/projects/<id>` under the multi-project service -- see
/// `status.rs`'s `AppState::base_path` doc comment). Each href below is mount-relative;
/// `nav()` prepends `base` at render time.
pub const NAV_LINKS: &[NavLink] = &[
    NavLink {
        href: "/",
        label: "Status",
    },
    NavLink {
        href: "/events",
        label: "Events",
    },
    NavLink {
        href: "/usage",
        label: "Usage",
    },
    NavLink {
        href: "/evidence",
        label: "Evidence",
    },
    NavLink {
        href: "/chat",
        label: "Chat",
    },
];

/// The one page shell every hand-rolled page in this crate renders through. `nav_html`
/// is pre-rendered (via `nav()`, or `""` for pages that don't want one) rather than
/// built here, since callers' nav shapes differ enough that forcing one nav-building
/// function to cover all of them isn't worth it.
///
/// `app_title`/`page_title` are escaped here, unconditionally -- a caller could pass a
/// persisted, user-supplied title straight through as `page_title`, and `<title>`
/// content is parsed as RCDATA: an unescaped `</title>` in that title would close the
/// element early and let whatever follows (e.g. `<script>...`) execute as real markup,
/// a stored XSS. `body` is deliberately NOT escaped here -- every caller already
/// builds it out of a mix of literal markup and its own already-escaped dynamic
/// pieces.
pub fn page_shell(
    app_title: &str,
    page_title: &str,
    nav_html: &str,
    body: &str,
    extra_head: &str,
) -> String {
    let app_title = escape(app_title);
    let page_title = escape(page_title);
    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{app_title} &mdash; {page_title}</title>
<style>{STYLE}</style>
{extra_head}
</head>
<body>
<h1>{app_title}</h1>
{nav_html}
{body}
<script>{SCRIPT}</script>
</body>
</html>
"#
    )
}

/// Styled replacement for the old bare `<p>{msg}</p>` error rendering -- gives errors a
/// distinct visual treatment (red-tinted banner) instead of looking like ordinary body
/// text. `msg` is escaped by the caller's usual `escape()` before being passed in here
/// in every call site except where the caller wants an embedded link (hence this takes
/// pre-built HTML, not a raw string).
pub fn error_banner(html: &str) -> String {
    format!(r#"<div class="error-banner">{html}</div>"#)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_covers_html_special_chars() {
        assert_eq!(escape("<script>&\"'"), "&lt;script&gt;&amp;&quot;&#39;");
    }

    #[test]
    fn urlencode_handles_query_meaningful_characters() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a&b"), "a%26b");
        assert_eq!(urlencode("plain"), "plain");
    }

    /// Regression test: every caller embeds `urlencode`'s output straight into an
    /// `href="{}"` attribute built from user-supplied input (`status.rs`'s `/events`
    /// filters). A raw `"` or `>` here would break out of the attribute/tag.
    #[test]
    fn urlencode_neutralizes_html_attribute_breakout_characters() {
        let encoded = urlencode(r#"42"><script>alert(1)</script>"#);
        assert!(!encoded.contains('"'));
        assert!(!encoded.contains('<'));
        assert!(!encoded.contains('>'));
    }

    #[test]
    fn nav_marks_active_link() {
        let links = [
            NavLink {
                href: "/",
                label: "Status",
            },
            NavLink {
                href: "/events",
                label: "Events",
            },
        ];
        let html = nav(&links, "/events", "/projects/x");
        assert!(html.contains(r#"<a href="/projects/x/events" class="active">Events</a>"#));
        assert!(html.contains(r#"<a href="/projects/x/">Status</a>"#));
    }

    #[test]
    fn page_shell_includes_title_nav_and_body() {
        let html = page_shell("Symphony", "events", "<nav>NAV</nav>", "<p>BODY</p>", "");
        assert!(html.contains("Symphony &mdash; events"));
        assert!(html.contains("<nav>NAV</nav>"));
        assert!(html.contains("<p>BODY</p>"));
    }

    /// Regression test: a caller could pass a persisted, user-submitted title straight
    /// through as `page_title` -- an unescaped `</title>` would close the element
    /// early and let whatever follows execute as real markup.
    #[test]
    fn page_shell_escapes_page_title_against_title_tag_breakout() {
        let html = page_shell("Symphony", "</title><script>alert(1)</script>", "", "", "");
        assert!(!html.contains("</title><script>"));
        assert!(html.contains("&lt;/title&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
    }
}
