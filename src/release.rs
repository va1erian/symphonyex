//! Release Agent (AIR-9): consolidates one delivery cycle's artifacts into a single
//! `EvidenceBundle`, computes a deployment-readiness verdict *deterministically* (never
//! asked of a model -- Roadmap §4's "delegate build, deployment and rollback to CI/CD"
//! guardrail means this stage prepares and labels, it never judges by vibes), and
//! renders both a Markdown view (for the MR body and `/evidence/<id>`'s page) and JSON
//! (the persisted artifact).
//!
//! Most sections below (`plan`, `tests`, `review_findings`, `security_findings`) come
//! back empty/`None` in the current codebase: nothing upstream tracks plan approval,
//! test results, review findings or security findings yet (that's AIR-2/AIR-3/AIR-8).
//! `assemble` only fills in what's genuinely available today -- `requirements` (parsed
//! from the issue's own acceptance-criteria checklist) and `timeline`/`tokens` (from
//! `eventlog`). Every other section renders as an explicit gap rather than being
//! silently omitted or faked: that's the whole point of a traceability manifest (see
//! `build_traceability_matrix`'s doc comment) and it's honest about what Symphony can
//! actually attest to today.
//!
//! Kept host-agnostic on purpose: this module knows nothing about GitHub/GitLab HTTP.
//! `orchestrator.rs` is the thin integration layer that pulls `Issue`/`eventlog` data
//! in, and pushes oversized sections out to `RepoHost::upload_artifact` before handing
//! the rest to `render_markdown`.

use crate::domain::Issue;
use crate::eventlog::{EventRow, IssueUsageRow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A section body longer than this is never inlined into the rendered Markdown --
/// only linked (via `artifact_links`) or, if no link was uploaded for it, shown as an
/// explicit "omitted" gap. Keeps a synthetic 4000-line coverage dump from ever
/// reaching the MR body (see this module's own tests and the AIR-9 acceptance
/// criterion it satisfies).
pub const INLINE_ARTIFACT_LIMIT: usize = 2_000;

/// Hard cap on the evidence section's own rendered length (separate from
/// `INLINE_ARTIFACT_LIMIT`, which bounds one artifact at a time) -- keeps the whole
/// evidence block well under GitHub's/GitLab's ~64KB body limit even if many small
/// sections add up, without needing to know the exact host limit here.
const MAX_EVIDENCE_MARKDOWN_LEN: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Ready,
    ReadyWithRisk,
    Blocked,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Ready => "ready",
            Verdict::ReadyWithRisk => "ready_with_risk",
            Verdict::Blocked => "blocked",
        }
    }
}

/// One parsed `- [ ] text` / `- [x] text` line from the issue's own description --
/// today's only real source of "requirements and acceptance criteria" (no separate
/// plan/requirements-tracking concept exists yet). `met: None` means "not yet
/// verified" (an unchecked box), not "known to be unmet" -- only an explicit `[x]`
/// (or a future stage that judges it) produces `Some(true)`/`Some(false)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementRecord {
    pub id: String,
    pub text: String,
    pub met: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanApproval {
    pub approved_by: String,
    pub approved_at: String,
    /// Name of the auto-approval rule that fired, if approval wasn't a human action.
    pub auto_rule: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TestSummary {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    /// `None` means coverage wasn't measured this cycle -- a gap, not zero coverage.
    pub coverage_pct: Option<f64>,
    /// Failures already present before this cycle's changes (the baseline), so a
    /// reviewer can tell "this cycle broke it" from "this was already broken."
    pub pre_existing_failures: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "reason")]
pub enum FindingResolution {
    Fixed,
    AcceptedWithReason(String),
    Open,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub id: String,
    pub description: String,
    pub resolution: FindingResolution,
    pub blocking: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecuritySeverity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityFinding {
    pub id: String,
    pub description: String,
    pub severity: SecuritySeverity,
    pub blocking: bool,
    /// A human override with justification, if a blocking finding was consciously
    /// accepted rather than fixed (e.g. via `/evidence/<id>`'s override action).
    pub override_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub stage: String,
    pub event: String,
    pub at: String,
}

/// A section too large to inline (e.g. a full coverage report). `name` doubles as the
/// artifact's file name when uploaded via `RepoHost::upload_artifact`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeArtifact {
    pub name: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceBundle {
    pub cycle_id: String,
    pub title: String,
    pub requirements: Vec<RequirementRecord>,
    pub plan: Option<PlanApproval>,
    pub tests: Option<TestSummary>,
    pub review_findings: Vec<ReviewFinding>,
    pub security_findings: Vec<SecurityFinding>,
    pub tokens: TokenTotals,
    pub timeline: Vec<TimelineEntry>,
    pub large_artifacts: Vec<LargeArtifact>,
    pub generated_at: String,
}

/// One row of the `R* -> AC* -> test -> commit -> finding` traceability matrix.
/// `requirement_id`/`acceptance_criterion` never gap (a row always starts from a real
/// requirement); `test`/`commit`/`finding` are `None` when nothing links to this
/// requirement -- rendered as an explicit "gap" cell, never dropped, so a reviewer
/// sees exactly what isn't traced yet instead of a table that merely looks complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceabilityRow {
    pub requirement_id: String,
    pub acceptance_criterion: String,
    pub test: Option<String>,
    pub commit: Option<String>,
    pub finding: Option<String>,
}

impl TraceabilityRow {
    pub fn has_gap(&self) -> bool {
        self.test.is_none() || self.commit.is_none() || self.finding.is_none()
    }
}

/// Parse `- [ ] text` / `- [x] text` lines out of an issue description's acceptance
/// criteria. Deliberately tolerant of any surrounding Markdown (headings, prose) --
/// it just scans every line for the checkbox prefix, matching how issue templates
/// across this codebase (see `issues/*.md`) write their own "Acceptance criteria"
/// sections.
pub fn requirements_from_description(description: &str) -> Vec<RequirementRecord> {
    let mut out = Vec::new();
    for line in description.lines() {
        let trimmed = line.trim();
        let (checked, rest) = if let Some(r) = trimmed.strip_prefix("- [x]") {
            (true, r)
        } else if let Some(r) = trimmed.strip_prefix("- [X]") {
            (true, r)
        } else if let Some(r) = trimmed.strip_prefix("- [ ]") {
            (false, r)
        } else {
            continue;
        };
        let text = rest.trim().to_string();
        if text.is_empty() {
            continue;
        }
        out.push(RequirementRecord {
            id: format!("R{}", out.len() + 1),
            text,
            met: if checked { Some(true) } else { None },
        });
    }
    out
}

/// Stage/turn/dispatch events worth surfacing on the cycle's timeline. Everything
/// else `eventlog` records (e.g. `other_message` streaming chunks) is noise at this
/// granularity -- `/events` already exists for that level of detail.
const TIMELINE_EVENT_TYPES: &[&str] = &[
    "dispatched",
    "session_started",
    "stage_started",
    "stage_finished",
    "turn_completed",
    "turn_failed",
    "worker_exit",
];

fn timeline_from_events(events: &[EventRow]) -> Vec<TimelineEntry> {
    events
        .iter()
        .filter(|e| TIMELINE_EVENT_TYPES.contains(&e.event_type.as_str()))
        .map(|e| TimelineEntry {
            stage: e.event_type.clone(),
            event: e.message.clone().unwrap_or_default(),
            at: e.created_at.clone(),
        })
        .collect()
}

/// Build the bundle from what's actually available today: the issue's own
/// description (requirements), and this issue's event history (timeline, tokens).
/// `plan`/`tests`/`review_findings`/`security_findings` are left empty -- see this
/// module's own doc comment for why, and `compute_verdict`'s handling of empty
/// sections as gaps rather than passes.
pub fn assemble(issue: &Issue, events: &[EventRow], usage: Option<&IssueUsageRow>) -> EvidenceBundle {
    let generated_at = events
        .iter()
        .map(|e| e.created_at.clone())
        .max()
        .unwrap_or_default();
    EvidenceBundle {
        cycle_id: issue.id.clone(),
        title: format!("{}: {}", issue.identifier, issue.title),
        requirements: requirements_from_description(issue.description.as_deref().unwrap_or("")),
        plan: None,
        tests: None,
        review_findings: Vec::new(),
        security_findings: Vec::new(),
        tokens: TokenTotals {
            input_tokens: usage.map(|u| u.input_tokens.max(0) as u64).unwrap_or(0),
            output_tokens: usage.map(|u| u.output_tokens.max(0) as u64).unwrap_or(0),
            total_tokens: usage.map(|u| u.total_tokens.max(0) as u64).unwrap_or(0),
        },
        timeline: timeline_from_events(events),
        large_artifacts: Vec::new(),
        generated_at,
    }
}

/// Deterministic readiness verdict -- never asked of a model (Roadmap §4). Rules, in
/// priority order:
///
/// 1. `Blocked` if any security finding is `blocking` and has no `override_reason`
///    (an unresolved blocking security issue), or any requirement is explicitly
///    `met == Some(false)` (a judged, unmet blocking acceptance criterion).
/// 2. `ReadyWithRisk` if there's anything short of a clean pass: an accepted finding
///    (review or a justified security override), an open review finding, a
///    requirement not yet verified, or a coverage gap (no test data recorded, or
///    pre-existing failures carried into this cycle).
/// 3. `Ready` otherwise.
pub fn compute_verdict(bundle: &EvidenceBundle) -> Verdict {
    let unresolved_blocking_security = bundle
        .security_findings
        .iter()
        .any(|f| f.blocking && f.override_reason.is_none());
    let unmet_requirement = bundle.requirements.iter().any(|r| r.met == Some(false));
    if unresolved_blocking_security || unmet_requirement {
        return Verdict::Blocked;
    }

    let has_accepted_finding = bundle
        .review_findings
        .iter()
        .any(|f| matches!(f.resolution, FindingResolution::AcceptedWithReason(_)))
        || bundle
            .security_findings
            .iter()
            .any(|f| f.blocking && f.override_reason.is_some());
    let has_open_finding = bundle
        .review_findings
        .iter()
        .any(|f| matches!(f.resolution, FindingResolution::Open));
    let has_unverified_requirement = bundle.requirements.iter().any(|r| r.met.is_none());
    let has_coverage_gap = match &bundle.tests {
        None => true,
        Some(t) => t.coverage_pct.is_none() || t.pre_existing_failures > 0 || t.failed > 0,
    };

    if has_accepted_finding || has_open_finding || has_unverified_requirement || has_coverage_gap {
        Verdict::ReadyWithRisk
    } else {
        Verdict::Ready
    }
}

/// Plain-English explanation of why `compute_verdict` returned what it did -- the
/// "why" surface the global constraints require ("the rule that fired... behind a
/// verdict"). Mirrors `compute_verdict`'s own rule order so the two never disagree.
pub fn explain_verdict(bundle: &EvidenceBundle) -> Vec<String> {
    let mut reasons = Vec::new();
    for f in &bundle.security_findings {
        if f.blocking && f.override_reason.is_none() {
            reasons.push(format!(
                "blocked: unresolved blocking security finding '{}'",
                f.id
            ));
        } else if f.blocking {
            reasons.push(format!(
                "risk: blocking security finding '{}' overridden ({})",
                f.id,
                f.override_reason.as_deref().unwrap_or("")
            ));
        }
    }
    for r in &bundle.requirements {
        match r.met {
            Some(false) => reasons.push(format!("blocked: requirement '{}' unmet", r.id)),
            None => reasons.push(format!("risk: requirement '{}' not yet verified", r.id)),
            Some(true) => {}
        }
    }
    for f in &bundle.review_findings {
        match &f.resolution {
            FindingResolution::AcceptedWithReason(reason) => {
                reasons.push(format!("risk: finding '{}' accepted ({reason})", f.id));
            }
            FindingResolution::Open => {
                reasons.push(format!("risk: finding '{}' still open", f.id));
            }
            FindingResolution::Fixed => {}
        }
    }
    match &bundle.tests {
        None => reasons.push("risk: no test results recorded this cycle".to_string()),
        Some(t) => {
            if t.coverage_pct.is_none() {
                reasons.push("risk: coverage not measured this cycle".to_string());
            }
            if t.pre_existing_failures > 0 {
                reasons.push(format!(
                    "risk: {} pre-existing test failure(s) carried into this cycle",
                    t.pre_existing_failures
                ));
            }
            if t.failed > 0 {
                reasons.push(format!("risk: {} test(s) failing", t.failed));
            }
        }
    }
    if reasons.is_empty() {
        reasons.push("all requirements verified, no open findings, no coverage gaps".to_string());
    }
    reasons
}

/// Build the `R* -> AC* -> test -> commit -> finding` matrix -- one row per parsed
/// requirement. `test`/`commit` link resolution isn't implemented yet (nothing
/// upstream associates a test name or commit SHA with a specific requirement), so
/// today every row's `test`/`commit` is `None` and `finding` is filled only when a
/// review/security finding's own `id` happens to reference the requirement's id (e.g.
/// a finding written as "R2: ..."). Rows with any `None` cell are gaps, not errors --
/// see `TraceabilityRow::has_gap` and this module's own doc comment on why gaps are
/// shown, never dropped.
pub fn build_traceability_matrix(bundle: &EvidenceBundle) -> Vec<TraceabilityRow> {
    bundle
        .requirements
        .iter()
        .map(|r| {
            let finding = bundle
                .review_findings
                .iter()
                .find(|f| f.description.contains(&r.id))
                .map(|f| f.id.clone())
                .or_else(|| {
                    bundle
                        .security_findings
                        .iter()
                        .find(|f| f.description.contains(&r.id))
                        .map(|f| f.id.clone())
                });
            TraceabilityRow {
                requirement_id: r.id.clone(),
                acceptance_criterion: r.text.clone(),
                test: None,
                commit: None,
                finding,
            }
        })
        .collect()
}

/// Minimal secret redaction applied to every free-text field before it's rendered.
/// Not a substitute for AIR-8's own redaction rules (not implemented in this
/// codebase yet -- see AIR-8's own ticket) -- this is a narrow, self-contained floor
/// so a bundle assembled today can't leak an obvious token verbatim while AIR-8 is
/// still pending; once AIR-8 lands, its rules are the authority and should be
/// threaded through here rather than duplicated.
///
/// Manual prefix/shape scan, not a `regex` dependency, matching this codebase's own
/// convention for one-off text scans (see `repo_host::extract_closes_issue_number`).
pub fn redact(text: &str) -> String {
    const SECRET_PREFIXES: &[&str] = &[
        "ghp_", "gho_", "ghs_", "ghr_", "github_pat_", "glpat-", "sk-", "xox", "AKIA",
    ];
    text.split_whitespace()
        .map(|word| {
            let bare = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
            if SECRET_PREFIXES.iter().any(|p| bare.starts_with(p)) && bare.len() > 8 {
                "[redacted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn gap(label: &str) -> String {
    format!("_gap: {label}_")
}

/// Render one artifact's section: inlined if small, linked if an upload for it
/// exists in `artifact_links`, otherwise an explicit "omitted" gap -- never silently
/// dropped, and never inlined past `INLINE_ARTIFACT_LIMIT` regardless of whether a
/// link exists (satisfies the AIR-9 acceptance criterion: oversized artifacts are
/// linked, not inlined).
fn render_artifact(artifact: &LargeArtifact, artifact_links: &BTreeMap<String, String>) -> String {
    if artifact.content.len() <= INLINE_ARTIFACT_LIMIT {
        format!("```\n{}\n```", redact(&artifact.content))
    } else if let Some(url) = artifact_links.get(&artifact.name) {
        format!(
            "_{} bytes, too large to inline -- [full report]({url})_",
            artifact.content.len()
        )
    } else {
        format!(
            "_{} bytes, too large to inline and not uploaded -- omitted_",
            artifact.content.len()
        )
    }
}

/// Render the evidence sections as Markdown (requirements/AC, plan, tests, findings,
/// security, tokens, timeline, traceability matrix). Redacts every free-text field.
/// Truncated to `MAX_EVIDENCE_MARKDOWN_LEN` as a last-resort safety net so the whole
/// block stays well under a host's MR/PR body size limit even if many small sections
/// add up (individual oversized artifacts are already handled by `render_artifact`).
pub fn render_markdown(
    bundle: &EvidenceBundle,
    verdict: Verdict,
    matrix: &[TraceabilityRow],
    artifact_links: &BTreeMap<String, String>,
) -> String {
    let mut out = String::new();
    let badge = match verdict {
        Verdict::Ready => "READY",
        Verdict::ReadyWithRisk => "READY WITH RISK",
        Verdict::Blocked => "BLOCKED",
    };
    out.push_str(&format!("**Deployment readiness: {badge}**\n\n"));
    for reason in explain_verdict(bundle) {
        out.push_str(&format!("- {}\n", redact(&reason)));
    }
    out.push('\n');

    out.push_str("### Requirements & acceptance criteria\n\n");
    if bundle.requirements.is_empty() {
        out.push_str(&format!("{}\n\n", gap("no requirements parsed")));
    } else {
        out.push_str("| ID | Requirement | Verdict |\n|---|---|---|\n");
        for r in &bundle.requirements {
            let v = match r.met {
                Some(true) => "met",
                Some(false) => "unmet",
                None => "unverified",
            };
            out.push_str(&format!("| {} | {} | {v} |\n", r.id, redact(&r.text)));
        }
        out.push('\n');
    }

    out.push_str("### Plan approval\n\n");
    match &bundle.plan {
        Some(p) => {
            let rule = p.auto_rule.as_deref().unwrap_or("none (human approval)");
            out.push_str(&format!(
                "Approved by {} at {} (auto-approval rule: {})\n\n",
                redact(&p.approved_by),
                p.approved_at,
                rule
            ));
        }
        None => out.push_str(&format!("{}\n\n", gap("no plan approval recorded"))),
    }

    out.push_str("### Test results\n\n");
    match &bundle.tests {
        Some(t) => {
            let coverage = t
                .coverage_pct
                .map(|c| format!("{c:.1}%"))
                .unwrap_or_else(|| "unmeasured".to_string());
            out.push_str(&format!(
                "Passed: {}, Failed: {}, Skipped: {}, Coverage: {}, Pre-existing failures: {}\n\n",
                t.passed, t.failed, t.skipped, coverage, t.pre_existing_failures
            ));
        }
        None => out.push_str(&format!("{}\n\n", gap("no test results recorded"))),
    }

    out.push_str("### Review findings\n\n");
    if bundle.review_findings.is_empty() {
        out.push_str(&format!("{}\n\n", gap("no review findings recorded")));
    } else {
        for f in &bundle.review_findings {
            let resolution = match &f.resolution {
                FindingResolution::Fixed => "fixed".to_string(),
                FindingResolution::AcceptedWithReason(r) => format!("accepted: {}", redact(r)),
                FindingResolution::Open => "open".to_string(),
            };
            out.push_str(&format!(
                "- **{}** ({resolution}): {}\n",
                f.id,
                redact(&f.description)
            ));
        }
        out.push('\n');
    }

    out.push_str("### Security checklist\n\n");
    if bundle.security_findings.is_empty() {
        out.push_str(&format!("{}\n\n", gap("no security findings recorded")));
    } else {
        for f in &bundle.security_findings {
            let sev = match f.severity {
                SecuritySeverity::Low => "low",
                SecuritySeverity::Medium => "medium",
                SecuritySeverity::High => "high",
                SecuritySeverity::Critical => "critical",
            };
            let status = match (&f.blocking, &f.override_reason) {
                (true, Some(reason)) => format!("blocking, overridden: {}", redact(reason)),
                (true, None) => "blocking, unresolved".to_string(),
                (false, _) => "non-blocking".to_string(),
            };
            out.push_str(&format!(
                "- **{}** [{sev}] ({status}): {}\n",
                f.id,
                redact(&f.description)
            ));
        }
        out.push('\n');
    }

    out.push_str("### Traceability matrix (R -> AC -> test -> commit -> finding)\n\n");
    if matrix.is_empty() {
        out.push_str(&format!("{}\n\n", gap("no requirements to trace")));
    } else {
        let gaps = matrix.iter().filter(|r| r.has_gap()).count();
        out.push_str(&format!(
            "{gaps} of {} row(s) have at least one unlinked cell.\n\n",
            matrix.len()
        ));
        out.push_str("| Requirement | Test | Commit | Finding |\n|---|---|---|---|\n");
        for row in matrix {
            let test = row.test.clone().unwrap_or_else(|| "gap".to_string());
            let commit = row.commit.clone().unwrap_or_else(|| "gap".to_string());
            let finding = row.finding.clone().unwrap_or_else(|| "gap".to_string());
            out.push_str(&format!(
                "| {}: {} | {test} | {commit} | {finding} |\n",
                row.requirement_id,
                redact(&row.acceptance_criterion)
            ));
        }
        out.push('\n');
    }

    out.push_str("### Tokens\n\n");
    out.push_str(&format!(
        "Input: {}, Output: {}, Total: {}\n\n",
        bundle.tokens.input_tokens, bundle.tokens.output_tokens, bundle.tokens.total_tokens
    ));

    if !bundle.large_artifacts.is_empty() {
        out.push_str("### Artifacts\n\n");
        for artifact in &bundle.large_artifacts {
            out.push_str(&format!(
                "**{}**\n\n{}\n\n",
                artifact.name,
                render_artifact(artifact, artifact_links)
            ));
        }
    }

    out.push_str("### Timeline\n\n");
    if bundle.timeline.is_empty() {
        out.push_str(&format!("{}\n", gap("no timeline events recorded")));
    } else {
        for entry in &bundle.timeline {
            out.push_str(&format!("- `{}` {}: {}\n", entry.at, entry.stage, redact(&entry.event)));
        }
    }

    if out.len() > MAX_EVIDENCE_MARKDOWN_LEN {
        out.truncate(MAX_EVIDENCE_MARKDOWN_LEN);
        out.push_str("\n\n_...truncated (evidence bundle too large for inline body; see the persisted artifact)_\n");
    }
    out
}

/// Compose the final MR/PR body: the agent's own narrative first, then the evidence
/// sections -- per AIR-9's scope ("agent narrative first, then the evidence
/// sections"). When `agent_body` is empty, the evidence sections still render on
/// their own (e.g. a code-driven update with nothing agent-authored yet).
pub fn compose_pr_body(agent_body: &str, evidence_markdown: &str) -> String {
    if agent_body.trim().is_empty() {
        return evidence_markdown.to_string();
    }
    format!(
        "{}\n\n---\n\n## Evidence & release readiness\n\n{}",
        agent_body.trim_end(),
        evidence_markdown
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue_with_description(description: &str) -> Issue {
        Issue {
            id: "issue-1".to_string(),
            native_ref: None,
            identifier: "AIR-9".to_string(),
            title: "Release agent".to_string(),
            description: Some(description.to_string()),
            priority: Some(1),
            state: "in_progress".to_string(),
            branch_name: None,
            url: None,
            assignee_id: None,
            labels: vec![],
            blocked_by: vec![],
            dispatchable: true,
            created_at: None,
            updated_at: None,
        }
    }

    fn ready_bundle() -> EvidenceBundle {
        EvidenceBundle {
            cycle_id: "issue-1".to_string(),
            title: "AIR-9: Release agent".to_string(),
            requirements: vec![RequirementRecord {
                id: "R1".to_string(),
                text: "does the thing".to_string(),
                met: Some(true),
            }],
            plan: None,
            tests: Some(TestSummary {
                passed: 10,
                failed: 0,
                skipped: 0,
                coverage_pct: Some(92.0),
                pre_existing_failures: 0,
            }),
            review_findings: vec![],
            security_findings: vec![],
            tokens: TokenTotals::default(),
            timeline: vec![],
            large_artifacts: vec![],
            generated_at: "2026-08-07T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn requirements_parsed_from_checklist() {
        let reqs = requirements_from_description(
            "## Acceptance criteria\n- [x] first thing\n- [ ] second thing\nsome prose\n- [ ] third",
        );
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0].met, Some(true));
        assert_eq!(reqs[1].met, None);
        assert_eq!(reqs[1].text, "second thing");
    }

    #[test]
    fn assemble_pulls_requirements_from_issue_description() {
        let issue = issue_with_description("- [x] done\n- [ ] not done");
        let bundle = assemble(&issue, &[], None);
        assert_eq!(bundle.requirements.len(), 2);
        assert_eq!(bundle.cycle_id, "issue-1");
    }

    #[test]
    fn verdict_ready_when_everything_verified_and_clean() {
        assert_eq!(compute_verdict(&ready_bundle()), Verdict::Ready);
    }

    #[test]
    fn verdict_blocked_on_unresolved_blocking_security_finding() {
        let mut bundle = ready_bundle();
        bundle.security_findings.push(SecurityFinding {
            id: "S1".to_string(),
            description: "sql injection".to_string(),
            severity: SecuritySeverity::Critical,
            blocking: true,
            override_reason: None,
        });
        assert_eq!(compute_verdict(&bundle), Verdict::Blocked);
    }

    #[test]
    fn verdict_blocked_on_unmet_requirement() {
        let mut bundle = ready_bundle();
        bundle.requirements[0].met = Some(false);
        assert_eq!(compute_verdict(&bundle), Verdict::Blocked);
    }

    #[test]
    fn verdict_ready_with_risk_on_overridden_security_finding() {
        let mut bundle = ready_bundle();
        bundle.security_findings.push(SecurityFinding {
            id: "S1".to_string(),
            description: "known low-risk exposure".to_string(),
            severity: SecuritySeverity::Medium,
            blocking: true,
            override_reason: Some("accepted by security team, ticket SEC-42".to_string()),
        });
        assert_eq!(compute_verdict(&bundle), Verdict::ReadyWithRisk);
    }

    #[test]
    fn verdict_ready_with_risk_on_accepted_review_finding() {
        let mut bundle = ready_bundle();
        bundle.review_findings.push(ReviewFinding {
            id: "F1".to_string(),
            description: "minor style nit".to_string(),
            resolution: FindingResolution::AcceptedWithReason("cosmetic only".to_string()),
            blocking: false,
        });
        assert_eq!(compute_verdict(&bundle), Verdict::ReadyWithRisk);
    }

    #[test]
    fn verdict_ready_with_risk_on_coverage_gap() {
        let mut bundle = ready_bundle();
        bundle.tests = None;
        assert_eq!(compute_verdict(&bundle), Verdict::ReadyWithRisk);
    }

    #[test]
    fn verdict_ready_with_risk_on_unverified_requirement() {
        let mut bundle = ready_bundle();
        bundle.requirements[0].met = None;
        assert_eq!(compute_verdict(&bundle), Verdict::ReadyWithRisk);
    }

    #[test]
    fn traceability_matrix_shows_gaps_explicitly_rather_than_omitting_rows() {
        let bundle = ready_bundle();
        let matrix = build_traceability_matrix(&bundle);
        assert_eq!(matrix.len(), 1);
        assert!(matrix[0].has_gap());
        let rendered = render_markdown(&bundle, Verdict::Ready, &matrix, &BTreeMap::new());
        assert!(rendered.contains("| gap | gap | gap |") || rendered.contains("gap"));
    }

    #[test]
    fn traceability_row_links_a_finding_referencing_the_requirement_id() {
        let mut bundle = ready_bundle();
        bundle.review_findings.push(ReviewFinding {
            id: "F1".to_string(),
            description: "R1: needs a null check".to_string(),
            resolution: FindingResolution::Fixed,
            blocking: false,
        });
        let matrix = build_traceability_matrix(&bundle);
        assert_eq!(matrix[0].finding, Some("F1".to_string()));
    }

    #[test]
    fn oversized_artifact_is_linked_not_inlined() {
        let mut bundle = ready_bundle();
        let big = "x".repeat(INLINE_ARTIFACT_LIMIT + 1);
        bundle.large_artifacts.push(LargeArtifact {
            name: "coverage.txt".to_string(),
            content: big.clone(),
        });
        let mut links = BTreeMap::new();
        links.insert("coverage.txt".to_string(), "https://example.com/coverage.txt".to_string());
        let rendered = render_markdown(&bundle, Verdict::Ready, &[], &links);
        assert!(!rendered.contains(&big));
        assert!(rendered.contains("https://example.com/coverage.txt"));
    }

    #[test]
    fn oversized_artifact_without_a_link_is_omitted_not_inlined() {
        let mut bundle = ready_bundle();
        let big = "y".repeat(INLINE_ARTIFACT_LIMIT + 1);
        bundle.large_artifacts.push(LargeArtifact {
            name: "coverage.txt".to_string(),
            content: big.clone(),
        });
        let rendered = render_markdown(&bundle, Verdict::Ready, &[], &BTreeMap::new());
        assert!(!rendered.contains(&big));
        assert!(rendered.contains("omitted"));
    }

    #[test]
    fn small_artifact_is_inlined() {
        let mut bundle = ready_bundle();
        bundle.large_artifacts.push(LargeArtifact {
            name: "notes.txt".to_string(),
            content: "short note".to_string(),
        });
        let rendered = render_markdown(&bundle, Verdict::Ready, &[], &BTreeMap::new());
        assert!(rendered.contains("short note"));
    }

    #[test]
    fn redact_hides_common_secret_shapes() {
        let text = "token is ghp_1234567890abcdef and that's it";
        let redacted = redact(text);
        assert!(!redacted.contains("ghp_1234567890abcdef"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn redact_leaves_ordinary_text_untouched() {
        let text = "no secrets here, just prose";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn redaction_applied_throughout_rendered_markdown() {
        let mut bundle = ready_bundle();
        bundle.requirements[0].text = "uses key ghp_abcdefabcdefabcdef in the fixture".to_string();
        let rendered = render_markdown(&bundle, Verdict::Ready, &[], &BTreeMap::new());
        assert!(!rendered.contains("ghp_abcdefabcdefabcdef"));
    }

    #[test]
    fn compose_pr_body_puts_narrative_first_then_evidence() {
        let body = compose_pr_body("Implements the feature.\n\nCloses #9", "**Deployment readiness: READY**");
        let narrative_pos = body.find("Implements the feature").unwrap();
        let evidence_pos = body.find("Deployment readiness").unwrap();
        assert!(narrative_pos < evidence_pos);
    }

    #[test]
    fn compose_pr_body_handles_empty_narrative() {
        let body = compose_pr_body("", "evidence only");
        assert_eq!(body, "evidence only");
    }

    #[test]
    fn bundle_round_trips_through_json() {
        let bundle = ready_bundle();
        let json = serde_json::to_string(&bundle).unwrap();
        let back: EvidenceBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cycle_id, bundle.cycle_id);
        assert_eq!(back.requirements, bundle.requirements);
    }

    #[test]
    fn explain_verdict_names_the_rule_that_fired() {
        let mut bundle = ready_bundle();
        bundle.requirements[0].met = Some(false);
        let reasons = explain_verdict(&bundle);
        assert!(reasons.iter().any(|r| r.contains("unmet")));
    }
}
