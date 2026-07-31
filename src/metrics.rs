//! Cumulative usage metrics for the whole process lifetime (agents spawned, turns,
//! tool calls, tokens) plus an HTML report renderer. Unlike `status` (a live snapshot
//! of *current* state), this never resets — it's written to disk after every state
//! change so a report exists even if the process is killed rather than shut down
//! cleanly.

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone)]
pub struct Metrics {
    pub run_started_at: DateTime<Utc>,
    pub agents_spawned: u64,
    pub turns_started: u64,
    pub tool_calls: u64,
    pub tool_call_counts: BTreeMap<String, u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub seconds_running: f64,
    pub issues: BTreeMap<String, IssueMetrics>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            run_started_at: Utc::now(),
            agents_spawned: 0,
            turns_started: 0,
            tool_calls: 0,
            tool_call_counts: BTreeMap::new(),
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            seconds_running: 0.0,
            issues: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Default)]
pub struct IssueMetrics {
    pub title: String,
    pub dispatch_count: u64,
    pub turns: u64,
    pub tool_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub seconds_running: f64,
    pub last_outcome: Option<String>,
}

impl Metrics {
    pub fn issue_entry(&mut self, identifier: &str, title: &str) -> &mut IssueMetrics {
        let entry = self.issues.entry(identifier.to_string()).or_default();
        entry.title = title.to_string();
        entry
    }
}

/// Render and write the HTML usage report to `path`. Best-effort: I/O errors are
/// returned to the caller to log, never panicked on.
pub fn write_report(path: &Path, workflow_path: &Path, metrics: &Metrics) -> std::io::Result<()> {
    let html = render(workflow_path, metrics);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, html)
}

fn render(workflow_path: &Path, m: &Metrics) -> String {
    let now = Utc::now();
    let elapsed = now.signed_duration_since(m.run_started_at);
    let elapsed_str = format_duration(elapsed.num_seconds().max(0) as u64);

    let tool_rows: String = if m.tool_call_counts.is_empty() {
        "<tr><td colspan=\"2\" class=\"empty\">No tool calls recorded.</td></tr>".to_string()
    } else {
        let mut entries: Vec<(&String, &u64)> = m.tool_call_counts.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        entries
            .iter()
            .map(|(name, count)| {
                format!(
                    "<tr><td>{name}</td><td>{count}</td></tr>",
                    name = escape(name),
                    count = count
                )
            })
            .collect()
    };

    let issue_rows: String = if m.issues.is_empty() {
        "<tr><td colspan=\"8\" class=\"empty\">No issues dispatched.</td></tr>".to_string()
    } else {
        m.issues
            .iter()
            .map(|(identifier, im)| {
                format!(
                    "<tr><td>{identifier}</td><td>{title}</td><td>{dispatches}</td><td>{turns}</td>\
                     <td>{tool_calls}</td><td>{tokens}</td><td>{seconds:.1}s</td><td>{outcome}</td></tr>",
                    identifier = escape(identifier),
                    title = escape(&im.title),
                    dispatches = im.dispatch_count,
                    turns = im.turns,
                    tool_calls = im.tool_calls,
                    tokens = im.input_tokens + im.output_tokens,
                    seconds = im.seconds_running,
                    outcome = escape(im.last_outcome.as_deref().unwrap_or("running / pending")),
                )
            })
            .collect()
    };

    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>Symphony usage report</title>
<style>
  body {{ font-family: system-ui, sans-serif; background: #111; color: #eee; margin: 0; padding: 32px; max-width: 1100px; }}
  h1 {{ font-size: 1.3rem; color: #9cf; margin: 0 0 4px; }}
  .meta {{ color: #888; font-size: 0.85rem; margin-bottom: 28px; }}
  .meta code {{ color: #bbb; }}
  .cards {{ display: flex; flex-wrap: wrap; gap: 14px; margin-bottom: 32px; }}
  .card {{ background: #1c1c1c; border: 1px solid #333; border-radius: 8px; padding: 16px 20px; min-width: 140px; }}
  .card .num {{ font-size: 1.8rem; font-weight: 700; color: #fff; }}
  .card .label {{ font-size: 0.78rem; color: #999; text-transform: uppercase; letter-spacing: 0.04em; margin-top: 2px; }}
  section {{ margin-bottom: 36px; }}
  h2 {{ font-size: 0.85rem; text-transform: uppercase; letter-spacing: 0.04em; color: #888; border-bottom: 1px solid #2a2a2a; padding-bottom: 6px; }}
  table {{ border-collapse: collapse; width: 100%; }}
  th, td {{ text-align: left; padding: 7px 10px; font-size: 0.85rem; border-bottom: 1px solid #262626; }}
  th {{ color: #888; font-weight: 500; }}
  tr:hover td {{ background: #191919; }}
  .empty {{ color: #666; font-style: italic; }}
  footer {{ color: #555; font-size: 0.75rem; margin-top: 40px; }}
</style>
</head>
<body>
<h1>Symphony usage report</h1>
<div class="meta">
  workflow <code>{workflow}</code> &middot; started {started} &middot; running for {elapsed}
  &middot; generated {generated}
</div>

<div class="cards">
  <div class="card"><div class="num">{agents}</div><div class="label">Agents spawned</div></div>
  <div class="card"><div class="num">{turns}</div><div class="label">Turns (subprocess launches)</div></div>
  <div class="card"><div class="num">{tool_calls}</div><div class="label">Tool calls</div></div>
  <div class="card"><div class="num">{issue_count}</div><div class="label">Issues touched</div></div>
  <div class="card"><div class="num">{input_tokens}</div><div class="label">Input tokens</div></div>
  <div class="card"><div class="num">{output_tokens}</div><div class="label">Output tokens</div></div>
  <div class="card"><div class="num">{total_tokens}</div><div class="label">Total tokens</div></div>
  <div class="card"><div class="num">{run_seconds:.0}s</div><div class="label">Agent runtime (ended sessions)</div></div>
</div>

<section>
<h2>Tool calls by name</h2>
<table>
<thead><tr><th>Tool</th><th>Calls</th></tr></thead>
<tbody>{tool_rows}</tbody>
</table>
</section>

<section>
<h2>Per-issue breakdown</h2>
<table>
<thead><tr><th>Issue</th><th>Title</th><th>Dispatches</th><th>Turns</th><th>Tool calls</th><th>Tokens</th><th>Runtime</th><th>Last outcome</th></tr></thead>
<tbody>{issue_rows}</tbody>
</table>
</section>

<footer>
  Generated by Symphony's built-in usage tracker. Numbers are cumulative for this
  process's lifetime; they reset on restart. Turns and tool calls are counted as they
  happen; token counts are only recorded when a turn's final <code>result</code> event
  arrives — a turn whose worker gets reclaimed by reconciliation right after it marks
  the issue done (before that event arrives) will show 0 tokens for that dispatch even
  though its turns/tool-calls were counted. This is rare on realistic poll intervals.
</footer>
</body>
</html>
"#,
        workflow = escape(&workflow_path.display().to_string()),
        started = escape(&m.run_started_at.to_rfc3339()),
        elapsed = elapsed_str,
        generated = escape(&now.to_rfc3339()),
        agents = m.agents_spawned,
        turns = m.turns_started,
        tool_calls = m.tool_calls,
        issue_count = m.issues.len(),
        input_tokens = m.input_tokens,
        output_tokens = m.output_tokens,
        total_tokens = m.total_tokens,
        run_seconds = m.seconds_running,
        tool_rows = tool_rows,
        issue_rows = issue_rows,
    )
}

fn format_duration(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn report_contains_key_numbers() {
        let mut m = Metrics {
            agents_spawned: 3,
            turns_started: 7,
            tool_calls: 12,
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            ..Default::default()
        };
        *m.tool_call_counts.entry("Bash".to_string()).or_insert(0) += 5;
        let entry = m.issue_entry("AR-1", "Scaffold");
        entry.dispatch_count = 1;
        entry.turns = 3;
        entry.last_outcome = Some("done (normal)".to_string());

        let html = render(Path::new("WORKFLOW.md"), &m);
        assert!(html.contains(">3<"));
        assert!(html.contains(">7<"));
        assert!(html.contains(">12<"));
        assert!(html.contains("AR-1"));
        assert!(html.contains("Scaffold"));
        assert!(html.contains("Bash"));
        assert!(html.contains("done (normal)"));
    }

    #[test]
    fn write_report_creates_parent_dirs_and_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("report.html");
        let m = Metrics::default();
        write_report(&path, Path::new("WORKFLOW.md"), &m).unwrap();
        assert!(path.exists());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("Symphony usage report"));
    }

    #[test]
    fn escapes_untrusted_content() {
        let mut m = Metrics::default();
        m.issue_entry("<script>", "alpha & <b>bold</b>");
        let html = render(Path::new("WORKFLOW.md"), &m);
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
