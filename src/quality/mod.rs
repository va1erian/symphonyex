//! Test execution and coverage reporting (AIR-6): runs a project's configured test
//! suites and coverage tool through the existing hook plumbing, normalizes the results,
//! maps acceptance criteria to the tests that exercise them, and compares against a
//! merge-base baseline so pre-existing failures aren't blamed on the current change.
//!
//! Deliberately not a new "role" abstraction: the test stage is identified by
//! `pipeline.stages[].id == "test"` plus a configured `pipeline.test` block (see
//! `orchestrator::run_pipeline`), and everything genuinely new lives here as plain
//! functions operating on plain data -- no trait, no framework.

pub mod coverage;

use crate::config::TestConfig;
use crate::hooks;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub use coverage::{Coverage, CoverageFormat};

/// Tail of a failing suite's combined output kept in the persisted `test_report` --
/// enough to see what broke without storing megabytes of CI log per run.
const OUTPUT_TAIL_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteResult {
    pub name: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// `None` when the runner's output didn't match a known test-framework summary
    /// line -- the suite still ran (see `exit_code`), counts just weren't parseable.
    pub passed: Option<u32>,
    pub failed: Option<u32>,
    pub skipped: Option<u32>,
    /// Tail of combined stdout+stderr, present only when the suite failed (non-zero
    /// exit or a `HookError`) -- a passing suite's full output isn't worth persisting.
    pub output_tail: Option<String>,
}

impl SuiteResult {
    pub fn passed_overall(&self) -> bool {
        self.exit_code == Some(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequirementCoverageStatus {
    Covered,
    Gap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequirementCoverage {
    pub ac_id: String,
    pub status: RequirementCoverageStatus,
    /// Suite names whose output referenced this AC id -- empty iff `status == Gap`.
    pub test_refs: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaselineComparison {
    /// Suites failing on both the merge base and the current run -- pre-existing,
    /// not this change's fault.
    pub pre_existing_failures: Vec<String>,
    /// Suites failing now that passed (or didn't exist) on the merge base.
    pub new_failures: Vec<String>,
    /// Suites that failed on the merge base but pass now.
    pub fixed: Vec<String>,
    /// `false` when the baseline run itself couldn't be established (e.g. no
    /// `default_branch` configured, or the git operations failed) -- degrades to "no
    /// regression evidence available", never fails the cycle.
    pub established: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReport {
    pub schema_version: u32,
    pub suites: Vec<SuiteResult>,
    pub requirement_coverage: Vec<RequirementCoverage>,
    pub baseline: BaselineComparison,
}

impl TestReport {
    pub fn total_passed(&self) -> u32 {
        self.suites.iter().filter_map(|s| s.passed).sum()
    }
    pub fn total_failed(&self) -> u32 {
        self.suites.iter().filter_map(|s| s.failed).sum()
    }
    pub fn total_skipped(&self) -> u32 {
        self.suites.iter().filter_map(|s| s.skipped).sum()
    }
    pub fn requirement_gaps(&self) -> Vec<&str> {
        self.requirement_coverage
            .iter()
            .filter(|r| r.status == RequirementCoverageStatus::Gap)
            .map(|r| r.ac_id.as_str())
            .collect()
    }

    /// One-line human summary for the dashboard/report column -- the detailed
    /// per-suite/per-AC breakdown is the full JSON, browsable via `/events`.
    pub fn summary_line(&self) -> String {
        let gaps = self.requirement_gaps().len();
        let regressions = self.baseline.new_failures.len();
        format!(
            "{} passed, {} failed, {} skipped{}{}",
            self.total_passed(),
            self.total_failed(),
            self.total_skipped(),
            if gaps > 0 {
                format!(" · {gaps} requirement gap(s)")
            } else {
                String::new()
            },
            if regressions > 0 {
                format!(" · {regressions} new failure(s)")
            } else {
                String::new()
            },
        )
    }
}

/// Whether a measured coverage percentage clears `min_line_percent`, and whether that
/// matters for the cycle -- `min_line_percent` is advisory unless the stage itself is
/// `blocking: true` (`pipeline.stages[].blocking`, the same flag a stage failure already
/// uses to decide whether to park the issue).
#[derive(Debug, Clone, PartialEq)]
pub enum CoverageGate {
    /// No threshold configured, or nothing measured -- nothing to gate on.
    NotApplicable,
    Ok {
        percent: f64,
    },
    /// Below threshold but the stage isn't `blocking` -- reported, not enforced.
    Advisory {
        percent: f64,
        min_percent: f64,
    },
    /// Below threshold and the stage is `blocking` -- the caller should treat this like
    /// any other blocking stage failure.
    Blocking {
        percent: f64,
        min_percent: f64,
    },
}

pub fn evaluate_coverage_gate(
    cov: &Coverage,
    min_line_percent: Option<f64>,
    stage_blocking: bool,
) -> CoverageGate {
    let (Some(min_percent), Some(percent)) = (min_line_percent, cov.line_percent()) else {
        return CoverageGate::NotApplicable;
    };
    if percent >= min_percent {
        CoverageGate::Ok { percent }
    } else if stage_blocking {
        CoverageGate::Blocking {
            percent,
            min_percent,
        }
    } else {
        CoverageGate::Advisory {
            percent,
            min_percent,
        }
    }
}

/// Cargo's own summary line: `test result: ok. 3 passed; 1 failed; 0 ignored; ...`.
/// The one framework this repo itself uses, and a reasonable first heuristic for other
/// Rust projects; unrecognized output simply leaves `passed`/`failed`/`skipped` as
/// `None` (see `SuiteResult`'s own doc comment) rather than guessing.
fn parse_suite_counts(output: &str) -> (Option<u32>, Option<u32>, Option<u32>) {
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("test result:") {
            let mut passed = None;
            let mut failed = None;
            let mut skipped = None;
            for part in rest.split(';') {
                // Each part looks like "<preamble> <N> <label>", e.g. "FAILED. 2 passed"
                // or " 1 failed" -- take the last two whitespace-separated tokens
                // rather than assuming the whole trimmed part is just "<N> <label>".
                let words: Vec<&str> = part.split_whitespace().collect();
                let (Some(&label), Some(&n_str)) =
                    (words.last(), words.get(words.len().wrapping_sub(2)))
                else {
                    continue;
                };
                let Ok(n) = n_str.parse::<u32>() else {
                    continue;
                };
                match label {
                    "passed" => passed = Some(n),
                    "failed" => failed = Some(n),
                    "ignored" => skipped = Some(n),
                    _ => {}
                }
            }
            if passed.is_some() || failed.is_some() {
                return (passed, failed, skipped);
            }
        }
    }
    (None, None, None)
}

fn tail(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        s.to_string()
    } else {
        // Slice on a char boundary so a multi-byte UTF-8 sequence never gets split.
        let start = s.len() - max_bytes;
        let start = (start..s.len())
            .find(|&i| s.is_char_boundary(i))
            .unwrap_or(s.len());
        format!("...[truncated]...{}", &s[start..])
    }
}

/// Run one configured suite through the hook plumbing (host or Docker, matching every
/// other hook in the codebase) and normalize its result. Returns the full combined
/// output alongside the persisted `SuiteResult` -- callers that need to scan for AC
/// references (`map_requirement_coverage`) need the full text even though only a
/// failing suite's tail is kept in the report itself.
pub async fn execute_suite(
    name: &str,
    command: &str,
    host_root: &Path,
    cwd: &Path,
    timeout_ms: u64,
    container: Option<&crate::container::ContainerHandle>,
) -> (SuiteResult, String) {
    let started = std::time::Instant::now();
    let result = hooks::run_hook_capture_maybe_containerized(
        name, command, host_root, cwd, timeout_ms, container,
    )
    .await;
    let duration_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(out) => {
            let combined = format!("{}{}", out.stdout, out.stderr);
            let (passed, failed, skipped) = parse_suite_counts(&combined);
            let ok = out.exit_code == Some(0);
            let suite = SuiteResult {
                name: name.to_string(),
                command: command.to_string(),
                exit_code: out.exit_code,
                duration_ms,
                passed,
                failed,
                skipped,
                output_tail: if ok {
                    None
                } else {
                    Some(tail(&combined, OUTPUT_TAIL_BYTES))
                },
            };
            (suite, combined)
        }
        Err(e) => {
            let msg = e.to_string();
            let suite = SuiteResult {
                name: name.to_string(),
                command: command.to_string(),
                exit_code: None,
                duration_ms,
                passed: None,
                failed: None,
                skipped: None,
                output_tail: Some(tail(&msg, OUTPUT_TAIL_BYTES)),
            };
            (suite.clone(), msg)
        }
    }
}

pub async fn execute_suites(
    commands: &[(String, String)],
    host_root: &Path,
    cwd: &Path,
    timeout_ms: u64,
    container: Option<&crate::container::ContainerHandle>,
) -> Vec<(SuiteResult, String)> {
    let mut results = Vec::with_capacity(commands.len());
    for (name, command) in commands {
        results.push(execute_suite(name, command, host_root, cwd, timeout_ms, container).await);
    }
    results
}

/// Run the configured coverage command, then read and parse the file it wrote.
/// Degrades to `Coverage::not_measured()` on any failure -- a missing tool, a command
/// that errors, or a file that doesn't parse as the declared format must never fail the
/// cycle (per AIR-6's acceptance criteria).
pub async fn run_coverage(
    cfg: &crate::config::CoverageConfig,
    host_root: &Path,
    cwd: &Path,
    timeout_ms: u64,
    container: Option<&crate::container::ContainerHandle>,
) -> Coverage {
    if cfg.format == CoverageFormat::None {
        return Coverage::not_measured();
    }
    if let Err(e) = hooks::run_hook_capture_maybe_containerized(
        "coverage",
        &cfg.command,
        host_root,
        cwd,
        timeout_ms,
        container,
    )
    .await
    {
        tracing::warn!(error = %e, "coverage command failed; coverage not measured");
        return Coverage::not_measured();
    }

    let output_path = cwd.join(&cfg.path);
    let content = match tokio::fs::read_to_string(&output_path).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(path = %output_path.display(), error = %e, "coverage output file not found; coverage not measured");
            return Coverage::not_measured();
        }
    };
    match coverage::parse(cfg.format, &content) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "coverage output did not parse; coverage not measured");
            Coverage::not_measured()
        }
    }
}

/// Whole-word, case-insensitive substring search: `contains("AC1")` must not match
/// inside `"AC10"`, and must still match a test named `ac1_rejects_empty` (Rust test
/// names are conventionally snake_case, lowercase).
fn contains_word(haystack: &str, needle: &str) -> bool {
    let haystack_lower = haystack.to_lowercase();
    let needle_lower = needle.to_lowercase();
    let bytes = haystack_lower.as_bytes();
    // Alphanumeric only, not `_`: test names conventionally separate an AC id from the
    // rest with an underscore (`ac1_rejects_empty`), which must count as a boundary --
    // only a *digit* directly after the match (as in "ac1" inside "ac10") should not.
    let is_word = |b: u8| b.is_ascii_alphanumeric();
    haystack_lower.match_indices(&needle_lower).any(|(idx, _)| {
        let end = idx + needle_lower.len();
        let before_ok = idx == 0 || !is_word(bytes[idx - 1]);
        let after_ok = end >= bytes.len() || !is_word(bytes[end]);
        before_ok && after_ok
    })
}

/// Map each `AC*` id (from AIR-4's `acceptance_criteria` artifact) to the suites whose
/// output referenced it -- the convention is that a test exercising `AC3` mentions
/// `AC3` somewhere a test runner prints it (typically the test's own name, e.g.
/// `fn ac3_rejects_empty_input()`). An id with no match anywhere is a gap: exactly the
/// signal AIR-9's traceability manifest needs.
pub fn map_requirement_coverage(
    ac_ids: &[String],
    suite_outputs: &[(String, String)],
) -> Vec<RequirementCoverage> {
    ac_ids
        .iter()
        .map(|ac_id| {
            let test_refs: Vec<String> = suite_outputs
                .iter()
                .filter(|(_, output)| contains_word(output, ac_id))
                .map(|(name, _)| name.clone())
                .collect();
            RequirementCoverage {
                ac_id: ac_id.clone(),
                status: if test_refs.is_empty() {
                    RequirementCoverageStatus::Gap
                } else {
                    RequirementCoverageStatus::Covered
                },
                test_refs,
            }
        })
        .collect()
}

/// Best-effort read of AIR-4's `acceptance_criteria` artifact from the workspace
/// (`.symphony/artifacts/acceptance_criteria.json`, the file-based read path AIR-3
/// documents). Returns an empty list -- not an error -- when the file is absent or
/// unparseable, since the requirements stage is itself optional (AIR-4) and this stage
/// must not fail just because nothing declared acceptance criteria yet.
pub fn load_acceptance_criteria_ids(workspace_path: &Path) -> Vec<String> {
    let path = workspace_path
        .join(".symphony")
        .join("artifacts")
        .join("acceptance_criteria.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Vec::new();
    };
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| item.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect()
}

/// Compares `current` suites against `baseline` suites by name. A suite present in
/// `current` but absent from `baseline` (e.g. a new test file) is treated as a new
/// failure if it fails now -- there's no baseline evidence it was already broken.
pub fn compare_to_baseline(
    current: &[SuiteResult],
    baseline: &[SuiteResult],
) -> BaselineComparison {
    let baseline_failed = |name: &str| {
        baseline
            .iter()
            .find(|s| s.name == name)
            .map(|s| !s.passed_overall())
    };

    let mut pre_existing_failures = Vec::new();
    let mut new_failures = Vec::new();
    for suite in current {
        if suite.passed_overall() {
            continue;
        }
        match baseline_failed(&suite.name) {
            Some(true) => pre_existing_failures.push(suite.name.clone()),
            Some(false) | None => new_failures.push(suite.name.clone()),
        }
    }

    let fixed = baseline
        .iter()
        .filter(|b| !b.passed_overall())
        .filter(|b| {
            current
                .iter()
                .find(|c| c.name == b.name)
                .map(|c| c.passed_overall())
                .unwrap_or(false)
        })
        .map(|b| b.name.clone())
        .collect();

    BaselineComparison {
        pre_existing_failures,
        new_failures,
        fixed,
        established: true,
    }
}

async fn run_git_capture(args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "git {} -> {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Regression evidence (AIR-6): runs the configured suites once against the merge base
/// with `default_branch`, so a stage running afterwards on the ticket branch can tell a
/// pre-existing failure from a new one (`compare_to_baseline`). Runs *before* the test
/// stage's own agent turns -- "before touching anything" is literal, see
/// `orchestrator::run_pipeline`.
///
/// Best-effort and self-restoring: any git step failing degrades to `None` (no baseline
/// evidence, `BaselineComparison::established == false` downstream) rather than failing
/// the cycle, and the working tree is always returned to `original_ref` (and any
/// stashed changes restored) before returning, whether or not the suites themselves
/// succeeded.
pub async fn collect_baseline(
    test_cfg: &TestConfig,
    host_root: &Path,
    workspace_path: &Path,
    default_branch: &str,
    timeout_ms: u64,
    container: Option<&crate::container::ContainerHandle>,
) -> Option<Vec<SuiteResult>> {
    let original_ref = run_git_capture(&["rev-parse", "HEAD"], workspace_path)
        .await
        .ok()?;
    let merge_base_target = format!("origin/{default_branch}");
    let merge_base = run_git_capture(&["merge-base", "HEAD", &merge_base_target], workspace_path)
        .await
        .ok()?;

    let dirty = run_git_capture(&["status", "--porcelain"], workspace_path)
        .await
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if dirty
        && run_git_capture(
            &["stash", "push", "-u", "-q", "-m", "symphony-air6-baseline"],
            workspace_path,
        )
        .await
        .is_err()
    {
        return None;
    }

    if run_git_capture(&["checkout", "-q", &merge_base], workspace_path)
        .await
        .is_err()
    {
        if dirty {
            let _ = run_git_capture(&["stash", "pop", "-q"], workspace_path).await;
        }
        return None;
    }

    let executed = execute_suites(
        &test_cfg.commands,
        host_root,
        workspace_path,
        timeout_ms,
        container,
    )
    .await;
    let results: Vec<SuiteResult> = executed.into_iter().map(|(s, _)| s).collect();

    if let Err(e) = run_git_capture(&["checkout", "-q", &original_ref], workspace_path).await {
        tracing::error!(error = %e, "failed to restore original ref after baseline test run -- workspace may be left detached at merge-base");
    }
    if dirty && let Err(e) = run_git_capture(&["stash", "pop", "-q"], workspace_path).await {
        tracing::error!(error = %e, "failed to restore stashed changes after baseline test run");
    }

    Some(results)
}

/// Result of the whole test stage: report plus normalized coverage, and the coverage
/// gate verdict against the configured (or absent) `min_line_percent`.
pub struct TestStageOutcome {
    pub report: TestReport,
    pub coverage: Coverage,
    pub gate: CoverageGate,
}

/// Runs every configured suite (current tree), the coverage command, builds requirement
/// coverage from AIR-4's artifact (if present) and the suites' own output, and compares
/// against `baseline` (already collected separately -- see `orchestrator::run_pipeline`,
/// which runs the baseline pass *before* the agent's own turns so "before touching
/// anything" is literal).
pub async fn run_test_stage(
    test_cfg: &TestConfig,
    host_root: &Path,
    cwd: &Path,
    timeout_ms: u64,
    container: Option<&crate::container::ContainerHandle>,
    baseline: Option<&[SuiteResult]>,
    stage_blocking: bool,
) -> TestStageOutcome {
    let executed = execute_suites(&test_cfg.commands, host_root, cwd, timeout_ms, container).await;
    let suites: Vec<SuiteResult> = executed.iter().map(|(s, _)| s.clone()).collect();
    let outputs: Vec<(String, String)> = executed
        .into_iter()
        .map(|(s, full)| (s.name, full))
        .collect();

    let ac_ids = load_acceptance_criteria_ids(cwd);
    let requirement_coverage = map_requirement_coverage(&ac_ids, &outputs);

    let baseline_comparison = match baseline {
        Some(b) => compare_to_baseline(&suites, b),
        None => BaselineComparison::default(),
    };

    let coverage = match &test_cfg.coverage {
        Some(cov_cfg) => run_coverage(cov_cfg, host_root, cwd, timeout_ms, container).await,
        None => Coverage::not_measured(),
    };
    let min_line_percent = test_cfg.coverage.as_ref().and_then(|c| c.min_line_percent);
    let gate = evaluate_coverage_gate(&coverage, min_line_percent, stage_blocking);

    TestStageOutcome {
        report: TestReport {
            schema_version: 1,
            suites,
            requirement_coverage,
            baseline: baseline_comparison,
        },
        coverage,
        gate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suite(name: &str, exit_code: Option<i32>) -> SuiteResult {
        SuiteResult {
            name: name.to_string(),
            command: "echo".to_string(),
            exit_code,
            duration_ms: 1,
            passed: None,
            failed: None,
            skipped: None,
            output_tail: None,
        }
    }

    #[test]
    fn parse_suite_counts_reads_cargo_test_summary() {
        let output = "running 3 tests\n\ntest result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n";
        let (p, f, s) = parse_suite_counts(output);
        assert_eq!(p, Some(2));
        assert_eq!(f, Some(1));
        assert_eq!(s, Some(0));
    }

    #[test]
    fn parse_suite_counts_returns_none_for_unrecognized_output() {
        let (p, f, s) = parse_suite_counts("some custom test runner output\nOK\n");
        assert_eq!((p, f, s), (None, None, None));
    }

    #[test]
    fn contains_word_does_not_match_ac1_inside_ac10() {
        assert!(!contains_word("test ac10_handles_overflow", "AC1"));
        assert!(contains_word("test ac10_handles_overflow", "AC10"));
        assert!(contains_word("fn ac1_rejects_empty()", "AC1"));
    }

    #[test]
    fn map_requirement_coverage_reports_gap_when_no_suite_mentions_the_ac_id() {
        let ac_ids = vec!["AC1".to_string(), "AC2".to_string()];
        let outputs = vec![("unit".to_string(), "test ac1_basic ... ok".to_string())];
        let mapping = map_requirement_coverage(&ac_ids, &outputs);
        assert_eq!(mapping.len(), 2);
        let ac1 = mapping.iter().find(|r| r.ac_id == "AC1").unwrap();
        assert_eq!(ac1.status, RequirementCoverageStatus::Covered);
        assert_eq!(ac1.test_refs, vec!["unit".to_string()]);
        let ac2 = mapping.iter().find(|r| r.ac_id == "AC2").unwrap();
        assert_eq!(ac2.status, RequirementCoverageStatus::Gap);
        assert!(ac2.test_refs.is_empty());
    }

    #[test]
    fn evaluate_coverage_gate_not_applicable_without_threshold_or_measurement() {
        let cov = Coverage::not_measured();
        assert_eq!(
            evaluate_coverage_gate(&cov, Some(70.0), true),
            CoverageGate::NotApplicable
        );
        let measured = Coverage {
            measured: true,
            lines_covered: 80,
            lines_total: 100,
            files: vec![],
        };
        assert_eq!(
            evaluate_coverage_gate(&measured, None, true),
            CoverageGate::NotApplicable
        );
    }

    #[test]
    fn evaluate_coverage_gate_is_advisory_by_default_and_blocking_when_stage_is_blocking() {
        let low = Coverage {
            measured: true,
            lines_covered: 50,
            lines_total: 100,
            files: vec![],
        };
        assert_eq!(
            evaluate_coverage_gate(&low, Some(70.0), false),
            CoverageGate::Advisory {
                percent: 50.0,
                min_percent: 70.0
            }
        );
        assert_eq!(
            evaluate_coverage_gate(&low, Some(70.0), true),
            CoverageGate::Blocking {
                percent: 50.0,
                min_percent: 70.0
            }
        );
        let high = Coverage {
            measured: true,
            lines_covered: 90,
            lines_total: 100,
            files: vec![],
        };
        assert_eq!(
            evaluate_coverage_gate(&high, Some(70.0), true),
            CoverageGate::Ok { percent: 90.0 }
        );
    }

    #[test]
    fn compare_to_baseline_distinguishes_pre_existing_from_new_failures() {
        let baseline = vec![suite("unit", Some(1)), suite("integration", Some(0))];
        let current = vec![suite("unit", Some(1)), suite("integration", Some(1))];
        let cmp = compare_to_baseline(&current, &baseline);
        assert!(cmp.established);
        assert_eq!(cmp.pre_existing_failures, vec!["unit".to_string()]);
        assert_eq!(cmp.new_failures, vec!["integration".to_string()]);
        assert!(cmp.fixed.is_empty());
    }

    #[test]
    fn compare_to_baseline_reports_a_suite_with_no_baseline_history_as_new() {
        let baseline = vec![suite("unit", Some(0))];
        let current = vec![suite("unit", Some(0)), suite("e2e", Some(1))];
        let cmp = compare_to_baseline(&current, &baseline);
        assert_eq!(cmp.new_failures, vec!["e2e".to_string()]);
    }

    #[test]
    fn compare_to_baseline_reports_fixed_suites() {
        let baseline = vec![suite("unit", Some(1))];
        let current = vec![suite("unit", Some(0))];
        let cmp = compare_to_baseline(&current, &baseline);
        assert_eq!(cmp.fixed, vec!["unit".to_string()]);
        assert!(cmp.new_failures.is_empty());
        assert!(cmp.pre_existing_failures.is_empty());
    }

    #[test]
    fn load_acceptance_criteria_ids_degrades_to_empty_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_acceptance_criteria_ids(dir.path()).is_empty());
    }

    #[test]
    fn load_acceptance_criteria_ids_reads_ids_from_artifact_file() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts_dir = dir.path().join(".symphony").join("artifacts");
        std::fs::create_dir_all(&artifacts_dir).unwrap();
        std::fs::write(
            artifacts_dir.join("acceptance_criteria.json"),
            r#"[{"id": "AC1", "given": "x"}, {"id": "AC2", "given": "y"}]"#,
        )
        .unwrap();
        let ids = load_acceptance_criteria_ids(dir.path());
        assert_eq!(ids, vec!["AC1".to_string(), "AC2".to_string()]);
    }

    #[test]
    fn test_report_summary_line_reports_gaps_and_regressions() {
        let report = TestReport {
            schema_version: 1,
            suites: vec![SuiteResult {
                passed: Some(3),
                failed: Some(1),
                skipped: Some(0),
                ..suite("unit", Some(1))
            }],
            requirement_coverage: vec![RequirementCoverage {
                ac_id: "AC1".to_string(),
                status: RequirementCoverageStatus::Gap,
                test_refs: vec![],
            }],
            baseline: BaselineComparison {
                new_failures: vec!["unit".to_string()],
                established: true,
                ..Default::default()
            },
        };
        let line = report.summary_line();
        assert!(line.contains("3 passed"));
        assert!(line.contains("1 failed"));
        assert!(line.contains("1 requirement gap"));
        assert!(line.contains("1 new failure"));
    }

    #[tokio::test]
    async fn execute_suite_runs_through_host_hooks_and_captures_output() {
        let dir = tempfile::tempdir().unwrap();
        let (result, full_output) = execute_suite(
            "unit",
            "echo hello; exit 1",
            dir.path(),
            dir.path(),
            5000,
            None,
        )
        .await;
        assert_eq!(result.name, "unit");
        assert_eq!(result.exit_code, Some(1));
        assert!(!result.passed_overall());
        assert!(result.output_tail.as_deref().unwrap().contains("hello"));
        assert!(full_output.contains("hello"));
    }

    #[tokio::test]
    async fn execute_suite_omits_output_tail_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let (result, _) = execute_suite(
            "unit",
            "echo hello; exit 0",
            dir.path(),
            dir.path(),
            5000,
            None,
        )
        .await;
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output_tail.is_none());
    }

    #[tokio::test]
    async fn run_coverage_none_format_is_not_measured_without_running_a_command() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::CoverageConfig {
            command: "exit 1".to_string(),
            format: CoverageFormat::None,
            min_line_percent: None,
            path: "coverage.json".to_string(),
        };
        let cov = run_coverage(&cfg, dir.path(), dir.path(), 5000, None).await;
        assert!(!cov.measured);
    }

    #[tokio::test]
    async fn run_coverage_degrades_when_output_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::CoverageConfig {
            command: "true".to_string(),
            format: CoverageFormat::LlvmCov,
            min_line_percent: None,
            path: "does-not-exist.json".to_string(),
        };
        let cov = run_coverage(&cfg, dir.path(), dir.path(), 5000, None).await;
        assert!(!cov.measured);
    }

    #[tokio::test]
    async fn run_coverage_parses_the_command_generated_file() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"data": [{"files": [{"filename": "src/a.rs", "summary": {"lines": {"count": 4, "covered": 2}}}]}]}"#;
        let write_cmd = format!("cat > coverage.json <<'EOF'\n{json}\nEOF\n");
        let cfg = crate::config::CoverageConfig {
            command: write_cmd,
            format: CoverageFormat::LlvmCov,
            min_line_percent: None,
            path: "coverage.json".to_string(),
        };
        let cov = run_coverage(&cfg, dir.path(), dir.path(), 5000, None).await;
        assert!(cov.measured);
        assert_eq!(cov.lines_covered, 2);
        assert_eq!(cov.lines_total, 4);
    }

    async fn git(args: &[&str], cwd: &Path) {
        let status = tokio::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .await
            .unwrap();
        assert!(
            status.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    /// Sets up `origin` (bare) and a `work` clone with `origin/main` one commit behind
    /// `HEAD` -- `work` has a commit adding `marker.txt` that `origin/main` doesn't have.
    async fn baseline_fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let origin = root.path().join("origin");
        let work = root.path().join("work");
        std::fs::create_dir_all(&origin).unwrap();
        git(&["init", "--bare", "-q"], &origin).await;
        git(
            &["clone", "-q", origin.to_str().unwrap(), "work"],
            root.path(),
        )
        .await;
        git(&["config", "user.email", "test@example.com"], &work).await;
        git(&["config", "user.name", "Test"], &work).await;
        // Force the branch name regardless of this machine's `init.defaultBranch`.
        git(&["checkout", "-q", "-B", "main"], &work).await;
        std::fs::write(work.join("base.txt"), "base").unwrap();
        git(&["add", "."], &work).await;
        git(&["commit", "-q", "-m", "initial"], &work).await;
        git(&["push", "-q", "-u", "origin", "main"], &work).await;

        std::fs::write(work.join("marker.txt"), "marker").unwrap();
        git(&["add", "."], &work).await;
        git(&["commit", "-q", "-m", "add marker"], &work).await;
        root
    }

    #[tokio::test]
    async fn collect_baseline_runs_suites_at_merge_base_then_restores_head() {
        let root = baseline_fixture().await;
        let work = root.path().join("work");
        let head_before = run_git_capture(&["rev-parse", "HEAD"], &work)
            .await
            .unwrap();

        let test_cfg = TestConfig {
            commands: vec![(
                "marker_check".to_string(),
                "if [ -f marker.txt ]; then exit 1; else exit 0; fi".to_string(),
            )],
            coverage: None,
        };

        let baseline = collect_baseline(&test_cfg, &work, &work, "main", 5000, None)
            .await
            .expect("baseline should be established");

        assert_eq!(baseline.len(), 1);
        assert_eq!(
            baseline[0].exit_code,
            Some(0),
            "marker.txt must not exist at the merge base"
        );

        let head_after = run_git_capture(&["rev-parse", "HEAD"], &work)
            .await
            .unwrap();
        assert_eq!(
            head_before, head_after,
            "HEAD must be restored after baseline run"
        );
        assert!(
            work.join("marker.txt").exists(),
            "workspace must be restored to its pre-baseline state"
        );
    }

    #[tokio::test]
    async fn collect_baseline_returns_none_when_default_branch_is_unknown() {
        let root = baseline_fixture().await;
        let work = root.path().join("work");
        let test_cfg = TestConfig {
            commands: vec![("unit".to_string(), "exit 0".to_string())],
            coverage: None,
        };
        let baseline =
            collect_baseline(&test_cfg, &work, &work, "no-such-branch", 5000, None).await;
        assert!(baseline.is_none());
    }

    #[tokio::test]
    async fn run_test_stage_end_to_end_with_baseline_and_ac_gap() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts_dir = dir.path().join(".symphony").join("artifacts");
        std::fs::create_dir_all(&artifacts_dir).unwrap();
        std::fs::write(
            artifacts_dir.join("acceptance_criteria.json"),
            r#"[{"id": "AC1"}, {"id": "AC2"}]"#,
        )
        .unwrap();

        let commands = vec![(
            "unit".to_string(),
            "echo 'test ac1_thing ... ok'; exit 0".to_string(),
        )];
        let test_cfg = TestConfig {
            commands: commands.clone(),
            coverage: None,
        };
        let baseline = vec![suite("unit", Some(1))];

        let outcome = run_test_stage(
            &test_cfg,
            dir.path(),
            dir.path(),
            5000,
            None,
            Some(&baseline),
            false,
        )
        .await;

        assert_eq!(outcome.report.suites.len(), 1);
        assert_eq!(outcome.report.total_failed(), 0);
        assert_eq!(outcome.report.baseline.fixed, vec!["unit".to_string()]);
        let gaps = outcome.report.requirement_gaps();
        assert_eq!(gaps, vec!["AC2"]);
        assert!(!outcome.coverage.measured);
        assert_eq!(outcome.gate, CoverageGate::NotApplicable);
    }
}
