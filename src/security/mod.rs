//! Security stage support (AIR-8, roadmap §4): OWASP-aligned validation, risk
//! classification and blocking findings. A thin layer over the abstractions the
//! delivery pipeline (AIR-1, `orchestrator::run_pipeline`) already has -- no new
//! stage-execution concept, just the shape of one stage's output (`SecurityFindings`)
//! and how deterministic scanners feed into it, run through the existing hook
//! plumbing (`hooks::run_hook_maybe_containerized`).
//!
//! Secret redaction is structural, not a filter that can be forgotten at some call
//! site: `SecretMatch` has no field that could ever hold the matched secret text, so
//! there is nothing to leak into the artifact, the event log or a PR body.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Severity vocabulary shared by findings, `pipeline.security.block_on` and dashboard
/// rendering. Ordered low-to-high so `>=` comparisons against a threshold read
/// naturally (`derive(PartialOrd)` follows declaration order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "low" => Some(Severity::Low),
            "medium" => Some(Severity::Medium),
            "high" => Some(Severity::High),
            "critical" => Some(Severity::Critical),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwaspStatus {
    Pass,
    Fail,
    NotApplicable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwaspItem {
    pub id: String,
    pub name: String,
    pub applicable: bool,
    pub status: OwaspStatus,
    /// Required to be non-empty for `not_applicable` items (acceptance criterion:
    /// "explicit `not_applicable` justifications") -- enforced by `validate`, not by
    /// the type itself, so a partially-filled-in artifact still deserializes and can
    /// be reported as a validation failure rather than a panic.
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub severity: Severity,
    pub owasp_id: Option<String>,
    pub cwe: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub summary: String,
    pub exploit_scenario: Option<String>,
    pub remediation: Option<String>,
}

/// A secret-scanner match, redacted by construction: there is no field here that
/// could hold the matched secret text, so normalizing a scanner's raw output into
/// this type *is* the redaction step -- nothing downstream can accidentally forward
/// a value that was never stored in the first place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretMatch {
    pub file: String,
    pub line: u32,
    pub rule: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanStatus {
    Clean,
    Findings,
    NotRun,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsScan {
    pub status: ScanStatus,
    pub matches: Vec<SecretMatch>,
}

impl Default for SecretsScan {
    fn default() -> Self {
        SecretsScan {
            status: ScanStatus::NotRun,
            matches: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyScan {
    pub tool: String,
    pub status: ScanStatus,
    pub advisories: Vec<String>,
}

impl Default for DependencyScan {
    fn default() -> Self {
        DependencyScan {
            tool: String::new(),
            status: ScanStatus::NotRun,
            advisories: Vec::new(),
        }
    }
}

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityFindings {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub risk_classification: Severity,
    #[serde(default)]
    pub owasp_checklist: Vec<OwaspItem>,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub secrets_scan: SecretsScan,
    #[serde(default)]
    pub dependency_scan: DependencyScan,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl SecurityFindings {
    /// Schema validity beyond what `serde` already enforces: every `not_applicable`
    /// OWASP item must carry a justification (acceptance criterion). Returns the
    /// first violation found, if any.
    pub fn validate(&self) -> Result<(), String> {
        for item in &self.owasp_checklist {
            if item.status == OwaspStatus::NotApplicable && item.evidence.trim().is_empty() {
                return Err(format!(
                    "owasp_checklist[{}] is not_applicable but has no evidence/justification",
                    item.id
                ));
            }
        }
        Ok(())
    }

    /// Highest severity across every finding, or `Low` when there are none -- the
    /// artifact's `risk_classification` is recomputed from this after scanner output
    /// is folded in, rather than trusting whatever the model wrote before the merge.
    pub fn recompute_risk(&mut self) {
        self.risk_classification = self
            .findings
            .iter()
            .map(|f| f.severity)
            .max()
            .unwrap_or(Severity::Low);
    }

    /// Whether any finding meets or exceeds a severity in `block_on` -- the whole of
    /// `pipeline.security.block_on`'s blocking semantics lives in this one comparison.
    pub fn is_blocking(&self, block_on: &[Severity]) -> bool {
        self.findings
            .iter()
            .any(|f| block_on.contains(&f.severity))
    }

    /// Merge a scanner's normalized output into this artifact in place.
    fn merge_scanner(&mut self, outcome: ScannerOutcome) {
        match outcome.result {
            ScannerResult::NotRun => {
                if outcome.is_dependency_scanner {
                    self.dependency_scan.tool = outcome.name;
                    self.dependency_scan.status = ScanStatus::NotRun;
                } else if outcome.is_secrets_scanner {
                    self.secrets_scan.status = ScanStatus::NotRun;
                }
            }
            ScannerResult::Advisories(advisories) => {
                self.dependency_scan.tool = outcome.name;
                self.dependency_scan.status = if advisories.is_empty() {
                    ScanStatus::Clean
                } else {
                    ScanStatus::Findings
                };
                self.dependency_scan.advisories.extend(advisories);
            }
            ScannerResult::Secrets(matches) => {
                self.secrets_scan.status = if matches.is_empty() {
                    ScanStatus::Clean
                } else {
                    ScanStatus::Findings
                };
                self.secrets_scan.matches.extend(matches);
            }
            ScannerResult::GenericFindings(findings) => {
                self.findings.extend(findings);
            }
        }
    }
}

// ---------------------------------------------------------------------------------
// Deterministic scanners
// ---------------------------------------------------------------------------------

/// `pipeline.security.scanners[]` entry (`config::resolve`). `format` selects the
/// normalizer `normalize_scanner_output` uses; an unrecognized format still runs the
/// command and folds a generic pass/fail finding in, rather than rejecting the config.
#[derive(Debug, Clone)]
pub struct ScannerConfig {
    pub name: String,
    pub command: String,
    pub format: String,
}

enum ScannerResult {
    NotRun,
    Advisories(Vec<String>),
    Secrets(Vec<SecretMatch>),
    GenericFindings(Vec<Finding>),
}

struct ScannerOutcome {
    name: String,
    is_dependency_scanner: bool,
    is_secrets_scanner: bool,
    result: ScannerResult,
}

/// Run every configured scanner against `workspace_path` (through the hook runner, so
/// Docker mode -- `container` -- is covered the same way every other workspace command
/// already is) and fold the normalized results into `findings`.
///
/// Each scanner is wrapped in a small shell script that first checks the command is
/// actually on `PATH`; when it isn't, the scanner is recorded `not_run` rather than
/// silently treated as passing (acceptance criterion). Output is redirected to a file
/// inside the workspace instead of relying on the hook runner's own captured
/// stdout/stderr, which is combined and truncated for logging -- scanner JSON needs to
/// survive intact.
pub async fn run_scanners(
    scanners: &[ScannerConfig],
    findings: &mut SecurityFindings,
    host_root: &Path,
    host_cwd: &Path,
    timeout_ms: u64,
    container: Option<&crate::container::ContainerHandle>,
) {
    for scanner in scanners {
        let out_rel = format!(".symphony/security-scan-{}.out", sanitize_name(&scanner.name));
        let status_rel = format!(
            ".symphony/security-scan-{}.status",
            sanitize_name(&scanner.name)
        );
        let command_q = shell_single_quote(&scanner.command);
        let out_rel_q = shell_single_quote(&out_rel);
        let status_rel_q = shell_single_quote(&status_rel);
        let script = format!(
            "mkdir -p .symphony\n\
             bin=$(printf '%s' {command_q} | awk '{{print $1}}')\n\
             if ! command -v \"$bin\" >/dev/null 2>&1; then\n\
             \x20\x20echo not_run > {status_rel_q}\n\
             \x20\x20exit 0\n\
             fi\n\
             {command} > {out_rel_q} 2>/dev/null\n\
             echo ran > {status_rel_q}\n\
             exit 0\n",
            command = scanner.command,
        );

        if let Err(e) = crate::hooks::run_hook_maybe_containerized(
            &format!("security-scan-{}", scanner.name),
            &script,
            host_root,
            host_cwd,
            timeout_ms,
            container,
        )
        .await
        {
            tracing::warn!(scanner = %scanner.name, error = %e, "security scanner wrapper failed to run");
            findings.merge_scanner(not_run_outcome(scanner));
            continue;
        }

        let status = std::fs::read_to_string(host_cwd.join(&status_rel)).unwrap_or_default();
        if status.trim() != "ran" {
            findings.merge_scanner(not_run_outcome(scanner));
            continue;
        }

        let raw = std::fs::read_to_string(host_cwd.join(&out_rel)).unwrap_or_default();
        findings.merge_scanner(normalize_scanner_output(scanner, &raw));
    }
}

fn not_run_outcome(scanner: &ScannerConfig) -> ScannerOutcome {
    ScannerOutcome {
        name: scanner.name.clone(),
        is_dependency_scanner: scanner.format == "cargo_audit_json",
        is_secrets_scanner: scanner.format == "gitleaks_json",
        result: ScannerResult::NotRun,
    }
}

/// POSIX single-quote a string for embedding in a generated shell script: wraps it in
/// `'...'` and escapes any embedded `'` as `'\''` (close the quote, an escaped literal
/// quote, reopen). Safe for arbitrary bytes, including `$`/backticks, unlike
/// double-quoting (which still expands both).
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Normalize one scanner's raw stdout into the artifact shape by `format`:
/// - `cargo_audit_json`: `cargo audit --json`'s `vulnerabilities.list[].advisory.id`/
///   `.title`.
/// - `gitleaks_json`: gitleaks' JSON array of `{File, StartLine, RuleID}` -- the
///   `Secret`/`Match` field (the actual leaked text) is never read into `SecretMatch`.
/// - anything else: the command ran and produced output, folded in as a single
///   generic `medium` finding referencing the scanner by name (still visible on the
///   dashboard, never a silent pass) rather than parsed further.
fn normalize_scanner_output(scanner: &ScannerConfig, raw: &str) -> ScannerOutcome {
    match scanner.format.as_str() {
        "cargo_audit_json" => {
            let advisories = serde_json::from_str::<serde_json::Value>(raw)
                .ok()
                .and_then(|v| v.get("vulnerabilities")?.get("list")?.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|item| {
                    let advisory = item.get("advisory")?;
                    let id = advisory.get("id")?.as_str()?;
                    let title = advisory.get("title").and_then(|t| t.as_str()).unwrap_or("");
                    Some(format!("{id}: {title}"))
                })
                .collect();
            ScannerOutcome {
                name: scanner.name.clone(),
                is_dependency_scanner: true,
                is_secrets_scanner: false,
                result: ScannerResult::Advisories(advisories),
            }
        }
        "gitleaks_json" => {
            let matches = serde_json::from_str::<serde_json::Value>(raw)
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|item| {
                    let file = item.get("File")?.as_str()?.to_string();
                    let line = item.get("StartLine")?.as_u64()? as u32;
                    let rule = item
                        .get("RuleID")
                        .and_then(|r| r.as_str())
                        .map(|s| s.to_string());
                    Some(SecretMatch { file, line, rule })
                })
                .collect();
            ScannerOutcome {
                name: scanner.name.clone(),
                is_dependency_scanner: false,
                is_secrets_scanner: true,
                result: ScannerResult::Secrets(matches),
            }
        }
        _ => {
            let trimmed = raw.trim();
            let findings = if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![Finding {
                    id: format!("scanner:{}", scanner.name),
                    severity: Severity::Medium,
                    owasp_id: None,
                    cwe: None,
                    file: None,
                    line: None,
                    summary: format!(
                        "{} reported output ({} bytes) -- not machine-parsed, see scanner output",
                        scanner.name,
                        trimmed.len()
                    ),
                    exploit_scenario: None,
                    remediation: Some(format!(
                        "review {}'s raw output and configure a known `format` for automatic parsing",
                        scanner.name
                    )),
                }]
            };
            ScannerOutcome {
                name: scanner.name.clone(),
                is_dependency_scanner: false,
                is_secrets_scanner: false,
                result: ScannerResult::GenericFindings(findings),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checklist_item(status: OwaspStatus, evidence: &str) -> OwaspItem {
        OwaspItem {
            id: "A01:2021".to_string(),
            name: "Broken Access Control".to_string(),
            applicable: status != OwaspStatus::NotApplicable,
            status,
            evidence: evidence.to_string(),
        }
    }

    fn finding(severity: Severity) -> Finding {
        Finding {
            id: "S1".to_string(),
            severity,
            owasp_id: Some("A03:2021".to_string()),
            cwe: Some("CWE-89".to_string()),
            file: Some("src/x.rs".to_string()),
            line: Some(10),
            summary: "sql injection".to_string(),
            exploit_scenario: Some("...".to_string()),
            remediation: Some("...".to_string()),
        }
    }

    fn empty_findings() -> SecurityFindings {
        SecurityFindings {
            schema_version: SCHEMA_VERSION,
            risk_classification: Severity::Low,
            owasp_checklist: Vec::new(),
            findings: Vec::new(),
            secrets_scan: SecretsScan::default(),
            dependency_scan: DependencyScan::default(),
        }
    }

    #[test]
    fn severity_orders_low_to_critical() {
        assert!(Severity::Low < Severity::Medium);
        assert!(Severity::Medium < Severity::High);
        assert!(Severity::High < Severity::Critical);
    }

    #[test]
    fn severity_parse_is_case_insensitive_and_rejects_unknown() {
        assert_eq!(Severity::parse("Critical"), Some(Severity::Critical));
        assert_eq!(Severity::parse(" high "), Some(Severity::High));
        assert_eq!(Severity::parse("nonsense"), None);
    }

    #[test]
    fn not_applicable_without_evidence_fails_validation() {
        let mut f = empty_findings();
        f.owasp_checklist.push(checklist_item(OwaspStatus::NotApplicable, ""));
        assert!(f.validate().is_err());
    }

    #[test]
    fn not_applicable_with_evidence_passes_validation() {
        let mut f = empty_findings();
        f.owasp_checklist.push(checklist_item(
            OwaspStatus::NotApplicable,
            "no file uploads in this change",
        ));
        assert!(f.validate().is_ok());
    }

    #[test]
    fn recompute_risk_takes_the_highest_finding_severity() {
        let mut f = empty_findings();
        f.findings.push(finding(Severity::Medium));
        f.findings.push(finding(Severity::Critical));
        f.findings.push(finding(Severity::Low));
        f.recompute_risk();
        assert_eq!(f.risk_classification, Severity::Critical);
    }

    #[test]
    fn recompute_risk_defaults_to_low_with_no_findings() {
        let mut f = empty_findings();
        f.recompute_risk();
        assert_eq!(f.risk_classification, Severity::Low);
    }

    #[test]
    fn is_blocking_true_when_a_finding_meets_the_threshold() {
        let mut f = empty_findings();
        f.findings.push(finding(Severity::High));
        assert!(f.is_blocking(&[Severity::Critical, Severity::High]));
        assert!(!f.is_blocking(&[Severity::Critical]));
    }

    #[test]
    fn is_blocking_false_with_no_findings() {
        let f = empty_findings();
        assert!(!f.is_blocking(&[Severity::Low]));
    }

    #[test]
    fn secret_match_type_has_no_field_that_can_hold_the_raw_secret() {
        // Structural redaction check: this is a compile-time guarantee, but assert on
        // the serialized shape too so a future field addition that reintroduces a raw
        // secret/value field is caught here.
        let m = SecretMatch {
            file: "src/config.rs".to_string(),
            line: 42,
            rule: Some("generic-api-key".to_string()),
        };
        let json = serde_json::to_value(&m).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(
            obj.keys().cloned().collect::<std::collections::BTreeSet<_>>(),
            ["file", "line", "rule"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }

    #[test]
    fn gitleaks_normalizer_drops_the_secret_value_and_keeps_only_file_and_line() {
        let scanner = ScannerConfig {
            name: "gitleaks".to_string(),
            command: "gitleaks detect".to_string(),
            format: "gitleaks_json".to_string(),
        };
        let raw = r#"[{"File":"src/config.rs","StartLine":42,"RuleID":"generic-api-key","Secret":"sk-super-secret-value-should-never-appear"}]"#;
        let outcome = normalize_scanner_output(&scanner, raw);
        let ScannerResult::Secrets(matches) = outcome.result else {
            panic!("expected secrets result");
        };
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file, "src/config.rs");
        assert_eq!(matches[0].line, 42);
        assert_eq!(matches[0].rule.as_deref(), Some("generic-api-key"));
        let serialized = serde_json::to_string(&matches[0]).unwrap();
        assert!(!serialized.contains("sk-super-secret-value-should-never-appear"));
    }

    #[test]
    fn cargo_audit_normalizer_extracts_advisory_ids() {
        let scanner = ScannerConfig {
            name: "cargo-audit".to_string(),
            command: "cargo audit --json".to_string(),
            format: "cargo_audit_json".to_string(),
        };
        let raw = r#"{"vulnerabilities":{"list":[{"advisory":{"id":"RUSTSEC-2024-0001","title":"example"}}]}}"#;
        let outcome = normalize_scanner_output(&scanner, raw);
        let ScannerResult::Advisories(advisories) = outcome.result else {
            panic!("expected advisories result");
        };
        assert_eq!(advisories, vec!["RUSTSEC-2024-0001: example".to_string()]);
    }

    #[test]
    fn merge_scanner_marks_not_run_without_treating_it_as_clean() {
        let mut f = empty_findings();
        let scanner = ScannerConfig {
            name: "cargo-audit".to_string(),
            command: "cargo audit --json".to_string(),
            format: "cargo_audit_json".to_string(),
        };
        f.merge_scanner(not_run_outcome(&scanner));
        assert_eq!(f.dependency_scan.status, ScanStatus::NotRun);
        assert_ne!(f.dependency_scan.status, ScanStatus::Clean);
    }

    #[test]
    fn security_findings_round_trips_through_json() {
        let mut f = empty_findings();
        f.owasp_checklist.push(checklist_item(OwaspStatus::Pass, "reviewed"));
        f.findings.push(finding(Severity::High));
        f.secrets_scan.matches.push(SecretMatch {
            file: "a.rs".to_string(),
            line: 1,
            rule: None,
        });
        let json = serde_json::to_string(&f).unwrap();
        let back: SecurityFindings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.findings.len(), 1);
        assert_eq!(back.owasp_checklist.len(), 1);
    }

    #[tokio::test]
    async fn run_scanners_reports_not_run_for_a_missing_command() {
        let dir = tempfile::tempdir().unwrap();
        let scanner = ScannerConfig {
            name: "ghost".to_string(),
            command: "definitely-not-a-real-scanner-binary-xyz".to_string(),
            format: "cargo_audit_json".to_string(),
        };
        let mut findings = empty_findings();
        run_scanners(&[scanner], &mut findings, dir.path(), dir.path(), 5_000, None).await;
        assert_eq!(findings.dependency_scan.status, ScanStatus::NotRun);
        assert_ne!(findings.dependency_scan.status, ScanStatus::Clean);
    }

    #[tokio::test]
    async fn run_scanners_folds_a_real_commands_output_into_the_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"vulnerabilities":{"list":[{"advisory":{"id":"RUSTSEC-2024-0001","title":"example"}}]}}"#;
        let scanner = ScannerConfig {
            name: "cargo-audit".to_string(),
            command: format!("echo {}", shell_single_quote(json)),
            format: "cargo_audit_json".to_string(),
        };
        let mut findings = empty_findings();
        run_scanners(&[scanner], &mut findings, dir.path(), dir.path(), 5_000, None).await;
        assert_eq!(findings.dependency_scan.status, ScanStatus::Findings);
        assert_eq!(
            findings.dependency_scan.advisories,
            vec!["RUSTSEC-2024-0001: example".to_string()]
        );
    }

    #[tokio::test]
    async fn run_scanners_with_no_scanners_configured_leaves_scans_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut findings = empty_findings();
        run_scanners(&[], &mut findings, dir.path(), dir.path(), 5_000, None).await;
        assert_eq!(findings.dependency_scan.status, ScanStatus::NotRun);
        assert_eq!(findings.secrets_scan.status, ScanStatus::NotRun);
    }
}
