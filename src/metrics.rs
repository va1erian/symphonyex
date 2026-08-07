//! Cumulative usage metrics for the whole process lifetime (agents spawned, turns,
//! tool calls, tokens) plus an HTML report renderer. Unlike `status` (a live snapshot
//! of *current* state), this never resets — it's written to disk after every state
//! change so a report exists even if the process is killed rather than shut down
//! cleanly.

use crate::insights::{self, MetricValue};
use crate::web::{self, escape};
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
/// returned to the caller to log, never panicked on. `db_path` is `symphony.db`
/// (`eventlog::DB_FILENAME` joined onto `workflow_dir`) -- the same database
/// `/insights`/`/metrics`/`symphony metrics` read, so the summary block at the top of
/// this report always matches those other surfaces for the same (all-time) period.
pub fn write_report(
    path: &Path,
    workflow_path: &Path,
    db_path: &Path,
    metrics: &Metrics,
) -> std::io::Result<()> {
    let html = render(workflow_path, db_path, metrics);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, html)
}

/// The roadmap SS11 summary block rendered at the top of the report -- one row per
/// dimension's known measures (unknown ones are omitted here; the full breakdown with
/// reasons lives on `/insights`, linked from this block).
fn insights_summary(db_path: &Path) -> String {
    let report = match insights::compute(db_path, None) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    let known: Vec<String> = report
        .dimensions
        .iter()
        .flat_map(|d| d.metrics.iter())
        .filter_map(|m| match &m.value {
            MetricValue::Number(n) => Some(format!(
                "<div class=\"stat\"><div class=\"n\">{n}</div><div class=\"l\">{}</div></div>",
                escape(m.label)
            )),
            MetricValue::Unknown(_) => None,
        })
        .collect();
    if known.is_empty() {
        return String::new();
    }
    format!(
        "<section>\n<h3>Delivery metrics (roadmap &sect;11)</h3>\n<div class=\"stats\">{}</div>\n\
         <p class=\"empty\">Full breakdown, including not-yet-derivable measures and why: /insights</p>\n</section>",
        known.join("")
    )
}

fn render(workflow_path: &Path, db_path: &Path, m: &Metrics) -> String {
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

    let insights_block = insights_summary(db_path);

    let body = format!(
        r#"<div class="meta">
  workflow <code>{workflow}</code> &middot; started {started} &middot; running for {elapsed}
  &middot; generated {generated}
</div>

{insights_block}

<div class="stats">
  <div class="stat"><div class="n">{agents}</div><div class="l">Agents spawned</div></div>
  <div class="stat"><div class="n">{turns}</div><div class="l">Turns (subprocess launches)</div></div>
  <div class="stat"><div class="n">{tool_calls}</div><div class="l">Tool calls</div></div>
  <div class="stat"><div class="n">{issue_count}</div><div class="l">Issues touched</div></div>
  <div class="stat"><div class="n">{input_tokens}</div><div class="l">Input tokens</div></div>
  <div class="stat"><div class="n">{output_tokens}</div><div class="l">Output tokens</div></div>
  <div class="stat"><div class="n">{total_tokens}</div><div class="l">Total tokens</div></div>
  <div class="stat"><div class="n">{run_seconds:.0}s</div><div class="l">Agent runtime (ended sessions)</div></div>
</div>

<section>
<h3>Tool calls by name</h3>
<div class="table-wrap">
<table>
<thead><tr><th data-sort>Tool</th><th data-sort>Calls</th></tr></thead>
<tbody>{tool_rows}</tbody>
</table>
</div>
</section>

<section>
<h3>Per-issue breakdown</h3>
<div class="table-wrap">
<table>
<thead><tr><th data-sort>Issue</th><th data-sort>Title</th><th data-sort>Dispatches</th><th data-sort>Turns</th><th data-sort>Tool calls</th><th data-sort>Tokens</th><th data-sort>Runtime</th><th>Last outcome</th></tr></thead>
<tbody>{issue_rows}</tbody>
</table>
</div>
</section>

<footer style="color: var(--fg-dimmer); font-size: 0.75rem; margin-top: 40px;">
  Generated by Symphony's built-in usage tracker. Numbers are cumulative for this
  process's lifetime; they reset on restart. Turns and tool calls are counted as they
  happen; token counts are only recorded when a turn's final <code>result</code> event
  arrives — a turn whose worker gets reclaimed by reconciliation right after it marks
  the issue done (before that event arrives) will show 0 tokens for that dispatch even
  though its turns/tool-calls were counted. This is rare on realistic poll intervals.
</footer>"#,
        workflow = escape(&workflow_path.display().to_string()),
        started = escape(&m.run_started_at.to_rfc3339()),
        elapsed = elapsed_str,
        generated = escape(&now.to_rfc3339()),
        insights_block = insights_block,
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
    );
    web::page_shell("Symphony", "usage report", "", &body, "")
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn report_contains_key_numbers() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("symphony.db");
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

        let html = render(Path::new("WORKFLOW.md"), &db_path, &m);
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
        let db_path = dir.path().join("symphony.db");
        let m = Metrics::default();
        write_report(&path, Path::new("WORKFLOW.md"), &db_path, &m).unwrap();
        assert!(path.exists());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("Symphony &mdash; usage report"));
    }

    #[test]
    fn escapes_untrusted_content() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("symphony.db");
        let mut m = Metrics::default();
        m.issue_entry("<script>", "alpha & <b>bold</b>");
        let html = render(Path::new("WORKFLOW.md"), &db_path, &m);
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn insights_summary_shows_known_measures_and_links_to_insights() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("symphony.db");
        let m = Metrics::default();
        let html = render(Path::new("WORKFLOW.md"), &db_path, &m);
        assert!(html.contains("Delivery metrics"));
        assert!(html.contains("Successful parallel cycles"));
        assert!(html.contains("/insights"));
    }
}
