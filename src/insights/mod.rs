//! Roadmap §11 success-measure metrics (Flow, Quality, Autonomy, Economics,
//! Reliability), computed purely from `symphony.db` (`eventlog.rs`) so a restart never
//! loses history and every surface (`/metrics`, `/insights`, `symphony metrics`, the
//! `symphony-report.html` summary block) is backed by the exact same numbers.
//!
//! Most of the roadmap's definitions need data this codebase does not yet collect --
//! merged-MR history from the code host (`RepoHost` has no "list merged MRs" method),
//! MR review rounds (AIR-7), production validation verdicts (AIR-10), human
//! approvals/clarifications (AIR-4/5/8), and per-backend cost. Rather than approximate
//! those from a loosely related signal (e.g. treating "worker exited without error" as
//! "the change was accepted" -- it wasn't, it just means the process didn't crash),
//! every measure that can't be honestly computed from `events` alone reports
//! [`MetricValue::Unknown`] with the specific reason, per the ticket's explicit "do not
//! fabricate a number to fill a cell" instruction. Only two measures are derivable
//! today: [`Dimension`] "autonomy"'s `successful_parallel_cycles` and "reliability"'s
//! `repeated_failures`, both reconstructed from `dispatched`/`worker_exit` event pairs.
//! As the data sources above land, their compute functions replace the corresponding
//! `Unknown` arm -- the surfaces (Prometheus/HTML/JSON) need no changes to pick that up.

use crate::eventlog::{self, EventRow};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum MetricValue {
    Number(f64),
    Unknown(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct Metric {
    /// Stable snake_case id, unique across the whole report -- used verbatim as the
    /// Prometheus metric name suffix (`status.rs`'s `/metrics`) and as the JSON key.
    pub key: &'static str,
    pub label: &'static str,
    /// The exact formula/definition, word-for-word the same text used as the
    /// dashboard's tooltip (`title` attribute) and this field's doc anchor -- see the
    /// module doc comment for why a single source of truth here matters.
    pub formula: &'static str,
    pub value: MetricValue,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dimension {
    pub key: &'static str,
    pub label: &'static str,
    pub metrics: Vec<Metric>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InsightsReport {
    pub since: Option<DateTime<Utc>>,
    pub until: DateTime<Utc>,
    pub dimensions: Vec<Dimension>,
}

#[cfg(test)]
impl InsightsReport {
    fn metric(&self, key: &str) -> Option<&Metric> {
        self.dimensions
            .iter()
            .flat_map(|d| d.metrics.iter())
            .find(|m| m.key == key)
    }
}

fn unknown(reason: &str) -> MetricValue {
    MetricValue::Unknown(reason.to_string())
}

/// One dispatch attempt reconstructed from a `dispatched` event paired with the next
/// `worker_exit` event for the same `issue_id` -- see the module doc comment for why
/// this (not a merge/PR) is the unit of "cycle" everything here can actually observe.
struct Cycle {
    issue_id: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    succeeded: bool,
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Pairs up `dispatched` -> next `worker_exit` per issue, in event order. A
/// `worker_exit` with no pending `dispatched` (its start fell before `since`) is
/// dropped -- its true start is unknown, so it can't be placed on the timeline. A
/// trailing `dispatched` with no `worker_exit` yet (still running, or the process was
/// killed before recording one) is dropped too: it never concluded, so it's neither a
/// successful nor a failed cycle.
fn reconstruct_cycles(events: &[EventRow]) -> Vec<Cycle> {
    let mut pending: BTreeMap<&str, DateTime<Utc>> = BTreeMap::new();
    let mut cycles = Vec::new();
    for ev in events {
        let Some(ts) = parse_ts(&ev.created_at) else {
            continue;
        };
        match ev.event_type.as_str() {
            "dispatched" => {
                pending.insert(&ev.issue_id, ts);
            }
            "worker_exit" => {
                if let Some(start) = pending.remove(ev.issue_id.as_str()) {
                    let succeeded = ev.message.as_deref() == Some("worker exited normally");
                    cycles.push(Cycle {
                        issue_id: ev.issue_id.clone(),
                        start,
                        end: ts,
                        succeeded,
                    });
                }
            }
            _ => {}
        }
    }
    cycles
}

fn intervals_overlap(a: &Cycle, b: &Cycle) -> bool {
    a.start < b.end && b.start < a.end
}

/// Autonomy: "successful parallel cycles" -- dispatch attempts that (a) completed with
/// a normal exit and (b) overlapped in wall-clock time with at least one other issue's
/// dispatch attempt. Approximates the roadmap's "successful parallel cycles" using the
/// one unit of concurrency Symphony's own event log actually records (overlapping
/// `dispatched`/`worker_exit` windows), rather than anything about merge/PR outcome.
fn successful_parallel_cycles(cycles: &[Cycle]) -> u64 {
    cycles
        .iter()
        .filter(|c| {
            c.succeeded
                && cycles
                    .iter()
                    .any(|other| other.issue_id != c.issue_id && intervals_overlap(c, other))
        })
        .count() as u64
}

/// Reliability: "repeated failures on the same issue" -- count of distinct issues with
/// two or more `worker_exit` events in the period whose message indicates an error
/// (`orchestrator.rs`'s `ExitReason::Error` path always records `"error: {err}"`).
fn repeated_failures(events: &[EventRow]) -> u64 {
    let mut error_counts: BTreeMap<&str, u64> = BTreeMap::new();
    for ev in events {
        if ev.event_type == "worker_exit"
            && ev.message.as_deref().is_some_and(|m| m.starts_with("error:"))
        {
            *error_counts.entry(ev.issue_id.as_str()).or_insert(0) += 1;
        }
    }
    error_counts.values().filter(|&&c| c >= 2).count() as u64
}

const NO_REPO_HOST_MERGE_DATA: &str =
    "requires a merged-MR/PR listing from the code host; RepoHost currently only \
     exposes open Symphony-authored PRs, not merge history (AIR-12 scope note)";
const NO_REVIEW_ROUND_DATA: &str =
    "requires MR review-round tracking (AIR-7), not yet recorded anywhere Symphony persists";
const NO_HUMAN_ACTION_DATA: &str =
    "requires approval/override/clarification tracking (AIR-4/5/8), not yet recorded";
const NO_VALIDATION_DATA: &str =
    "requires production validation verdicts (AIR-10) or a linked incident ticket, neither tracked yet";
const NO_COST_DATA: &str =
    "requires per-token pricing and deployment counts; neither is configured or tracked yet";
const NO_BACKEND_PER_EVENT: &str =
    "requires the backend/model used to be recorded per event; only tokens are captured today";
const NO_INCIDENT_SOURCE: &str =
    "requires an external incident/rollback source; no metrics.incident_source hook is configured";

/// Compute every roadmap §11 measure for `[since, now)` (all of history when `since` is
/// `None`) from `db_path`. Read-only, single pass over `eventlog::events_in_period`.
pub fn compute(db_path: &Path, since: Option<DateTime<Utc>>) -> rusqlite::Result<InsightsReport> {
    let events = eventlog::events_in_period(db_path, since.as_ref())?;
    let until = Utc::now();
    let cycles = reconstruct_cycles(&events);

    let flow = Dimension {
        key: "flow",
        label: "Flow",
        metrics: vec![
            Metric {
                key: "deployment_frequency",
                label: "Deployment Frequency",
                formula: "merged MRs per period from the repo host, filtered to \
                    Symphony-authored branches (issue-*)",
                value: unknown(NO_REPO_HOST_MERGE_DATA),
            },
            Metric {
                key: "lead_time_for_change",
                label: "Lead Time for Change",
                formula: "first dispatch of the issue -> merge timestamp",
                value: unknown(NO_REPO_HOST_MERGE_DATA),
            },
            Metric {
                key: "change_failure_rate",
                label: "Change Failure Rate",
                formula: "merges followed by a degraded production validation (AIR-10), \
                    a revert, or a linked incident ticket, over total merges",
                value: unknown(NO_VALIDATION_DATA),
            },
        ],
    };

    let quality = Dimension {
        key: "quality",
        label: "Quality",
        metrics: vec![
            Metric {
                key: "first_pass_acceptance",
                label: "First-pass acceptance",
                formula: "cycles reaching MR with zero request_changes rounds (AIR-7)",
                value: unknown(NO_REVIEW_ROUND_DATA),
            },
            Metric {
                key: "rework",
                label: "Rework",
                formula: "AIR-7 rework rounds per cycle, plus human-requested changes on the MR",
                value: unknown(NO_REVIEW_ROUND_DATA),
            },
            Metric {
                key: "production_fixes",
                label: "Production fixes",
                formula: "merges that required a follow-up fix after a degraded \
                    production validation (AIR-10)",
                value: unknown(NO_VALIDATION_DATA),
            },
        ],
    };

    let autonomy = Dimension {
        key: "autonomy",
        label: "Autonomy",
        metrics: vec![
            Metric {
                key: "orchestrated_share",
                label: "Orchestrated share",
                formula: "Symphony-authored merges over all merges to default_branch",
                value: unknown(NO_REPO_HOST_MERGE_DATA),
            },
            Metric {
                key: "human_interventions",
                label: "Human interventions",
                formula: "approvals, overrides and clarifications answered (AIR-4, AIR-5, AIR-8)",
                value: unknown(NO_HUMAN_ACTION_DATA),
            },
            Metric {
                key: "escalations",
                label: "Escalations",
                formula: "cycles ending in escalate / blocked",
                value: unknown(
                    "no escalate/blocked terminal state is persisted to the event log yet \
                     (the orchestrator calls TrackerAdapter::set_issue_state directly \
                     without recording an event)",
                ),
            },
            Metric {
                key: "successful_parallel_cycles",
                label: "Successful parallel cycles",
                formula: "dispatch attempts (dispatched -> worker_exit) that exited \
                    normally and overlapped in wall-clock time with another issue's \
                    dispatch attempt",
                value: MetricValue::Number(successful_parallel_cycles(&cycles) as f64),
            },
        ],
    };

    let economics = Dimension {
        key: "economics",
        label: "Economics",
        metrics: vec![
            Metric {
                key: "tokens_per_accepted_change",
                label: "Tokens per accepted change",
                formula: "cycle tokens over merged cycles (AIR-11)",
                value: unknown(NO_REPO_HOST_MERGE_DATA),
            },
            Metric {
                key: "ai_cost_per_deployment",
                label: "AI cost per deployment",
                formula: "cycle cost over deployments",
                value: unknown(NO_COST_DATA),
            },
            Metric {
                key: "provider_efficiency",
                label: "Provider efficiency",
                formula: "accepted-change rate and cost per backend/model",
                value: unknown(NO_BACKEND_PER_EVENT),
            },
        ],
    };

    let reliability = Dimension {
        key: "reliability",
        label: "Reliability",
        metrics: vec![
            Metric {
                key: "reverts",
                label: "Reverts",
                formula: "merges later reverted on the default branch",
                value: unknown(NO_REPO_HOST_MERGE_DATA),
            },
            Metric {
                key: "repeated_failures",
                label: "Repeated failures on the same issue",
                formula: "issues with 2 or more worker_exit events carrying an error \
                    message in the period",
                value: MetricValue::Number(repeated_failures(&events) as f64),
            },
            Metric {
                key: "recovery_time",
                label: "Recovery time",
                formula: "time from a degraded production validation (AIR-10) back to healthy",
                value: unknown(NO_INCIDENT_SOURCE),
            },
        ],
    };

    Ok(InsightsReport {
        since,
        until,
        dimensions: vec![flow, quality, autonomy, economics, reliability],
    })
}

/// Render `report` as Prometheus text-exposition format (`/metrics`,
/// `status.rs`). Each known metric gets `HELP`/`TYPE` + a sample line; an `Unknown`
/// metric still gets `HELP`/`TYPE` (so the reason string is visible to a scrape-time
/// reader) but its sample is `NaN` -- a valid Prometheus text-format value meaning "no
/// data" -- never a fabricated number.
pub fn render_prometheus(report: &InsightsReport) -> String {
    let mut out = String::new();
    for dim in &report.dimensions {
        for m in &dim.metrics {
            let name = format!("symphony_{}_{}", dim.key, m.key);
            out.push_str(&format!("# HELP {name} {}\n", m.formula.replace('\n', " ")));
            out.push_str(&format!("# TYPE {name} gauge\n"));
            let value = match &m.value {
                MetricValue::Number(n) => *n,
                MetricValue::Unknown(reason) => {
                    out.push_str(&format!("# unknown: {reason}\n"));
                    f64::NAN
                }
            };
            out.push_str(&format!("{name} {value}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (issue_id, event_type, message, total_tokens, created_at)
    type SeedRow<'a> = (&'a str, &'a str, Option<&'a str>, Option<i64>, &'a str);

    fn seed(db: &Path, rows: &[SeedRow]) {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute_batch(
            "CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_id TEXT NOT NULL,
                identifier TEXT NOT NULL,
                title TEXT NOT NULL,
                session_id TEXT,
                event_type TEXT NOT NULL,
                importance TEXT NOT NULL,
                message TEXT,
                input_tokens INTEGER,
                output_tokens INTEGER,
                total_tokens INTEGER,
                created_at TEXT NOT NULL
            );",
        )
        .unwrap();
        for (issue_id, event_type, message, total_tokens, created_at) in rows {
            conn.execute(
                "INSERT INTO events (issue_id, identifier, title, session_id, event_type, \
                 importance, message, input_tokens, output_tokens, total_tokens, created_at) \
                 VALUES (?1, ?1, ?1, NULL, ?2, 'normal', ?3, NULL, NULL, ?4, ?5)",
                rusqlite::params![issue_id, event_type, message, total_tokens, created_at],
            )
            .unwrap();
        }
    }

    #[test]
    fn everything_unknown_reports_reasons_not_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        seed(&db, &[]);

        let report = compute(&db, None).unwrap();
        let df = report.metric("deployment_frequency").unwrap();
        assert_eq!(
            df.value,
            MetricValue::Unknown(NO_REPO_HOST_MERGE_DATA.to_string())
        );

        let spc = report.metric("successful_parallel_cycles").unwrap();
        assert_eq!(spc.value, MetricValue::Number(0.0));
        let rf = report.metric("repeated_failures").unwrap();
        assert_eq!(rf.value, MetricValue::Number(0.0));
    }

    #[test]
    fn successful_parallel_cycles_counts_overlapping_successful_dispatches() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        seed(
            &db,
            &[
                ("1", "dispatched", None, None, "2026-01-01T00:00:00Z"),
                (
                    "2",
                    "dispatched",
                    None,
                    None,
                    "2026-01-01T00:00:30Z",
                ),
                (
                    "1",
                    "worker_exit",
                    Some("worker exited normally"),
                    None,
                    "2026-01-01T00:01:00Z",
                ),
                (
                    "2",
                    "worker_exit",
                    Some("worker exited normally"),
                    None,
                    "2026-01-01T00:01:30Z",
                ),
                // Issue 3 runs entirely after 1 and 2 finish -- not parallel with anything.
                ("3", "dispatched", None, None, "2026-01-01T01:00:00Z"),
                (
                    "3",
                    "worker_exit",
                    Some("worker exited normally"),
                    None,
                    "2026-01-01T01:01:00Z",
                ),
            ],
        );

        let report = compute(&db, None).unwrap();
        let spc = report.metric("successful_parallel_cycles").unwrap();
        assert_eq!(spc.value, MetricValue::Number(2.0));
    }

    #[test]
    fn repeated_failures_requires_two_or_more_errors_on_the_same_issue() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        seed(
            &db,
            &[
                ("1", "dispatched", None, None, "2026-01-01T00:00:00Z"),
                (
                    "1",
                    "worker_exit",
                    Some("error: boom"),
                    None,
                    "2026-01-01T00:01:00Z",
                ),
                ("1", "dispatched", None, None, "2026-01-01T00:02:00Z"),
                (
                    "1",
                    "worker_exit",
                    Some("error: boom again"),
                    None,
                    "2026-01-01T00:03:00Z",
                ),
                // Issue 2 fails once only -- doesn't count as "repeated".
                ("2", "dispatched", None, None, "2026-01-01T00:00:00Z"),
                (
                    "2",
                    "worker_exit",
                    Some("error: once"),
                    None,
                    "2026-01-01T00:01:00Z",
                ),
            ],
        );

        let report = compute(&db, None).unwrap();
        let rf = report.metric("repeated_failures").unwrap();
        assert_eq!(rf.value, MetricValue::Number(1.0));
    }

    #[test]
    fn since_filter_excludes_events_before_the_period() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        seed(
            &db,
            &[
                (
                    "1",
                    "worker_exit",
                    Some("error: old"),
                    None,
                    "2020-01-01T00:00:00Z",
                ),
                (
                    "2",
                    "dispatched",
                    None,
                    None,
                    "2026-01-01T00:00:00Z",
                ),
            ],
        );

        let since: DateTime<Utc> = "2025-01-01T00:00:00Z".parse().unwrap();
        let report = compute(&db, Some(since)).unwrap();
        assert_eq!(report.since, Some(since));
        // The 2020 error event is excluded by `since`, so no issue has any recorded
        // failures at all in this period.
        let rf = report.metric("repeated_failures").unwrap();
        assert_eq!(rf.value, MetricValue::Number(0.0));
    }

    #[test]
    fn render_prometheus_emits_help_type_and_nan_for_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("symphony.db");
        seed(&db, &[]);
        let report = compute(&db, None).unwrap();
        let text = render_prometheus(&report);

        assert!(text.contains("# HELP symphony_flow_deployment_frequency"));
        assert!(text.contains("# TYPE symphony_flow_deployment_frequency gauge"));
        assert!(text.contains("symphony_flow_deployment_frequency NaN"));
        assert!(text.contains("symphony_autonomy_successful_parallel_cycles 0"));
        assert!(text.contains("symphony_reliability_repeated_failures 0"));
    }
}
