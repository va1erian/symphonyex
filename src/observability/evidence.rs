//! Pre-merge stage: validates that a change is observable *before* it ships. A
//! static scan over a unified diff's added lines (the same `git diff` text
//! `swebot::git::diff_against` already produces for PR review) -- no agent turn
//! needed for this half, since "does a structured log/span/metric call exist, and
//! does any of it leak a secret field" is a pattern match, not a judgement call.
//!
//! Produces `TelemetryEvidence`: every detected signal (with its file:line and, when
//! it can be matched, the observability requirement it satisfies), plus any
//! secret/PII findings -- the acceptance criterion's "mapping each observability
//! requirement to a verified signal" and "flags secrets/PII in telemetry."

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalKind {
    Log,
    Span,
    Metric,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelemetrySignal {
    pub kind: SignalKind,
    /// `<file>:<line>` -- `line` is the position within the diff's added-line stream
    /// (not the target file's real line number: a diff without full `-U0` context
    /// doesn't reliably expose that without parsing every hunk header), enough for a
    /// human to jump to the right hunk.
    pub location: String,
    pub snippet: String,
    /// Which `requirements` entry (by id) this signal satisfies, when one could be
    /// matched -- see `collect_telemetry_evidence`'s matching rule. `None` means the
    /// signal exists but isn't yet tied to a documented requirement.
    pub requirement_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelemetryFinding {
    pub location: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct TelemetryEvidence {
    pub signals: Vec<TelemetrySignal>,
    pub findings: Vec<TelemetryFinding>,
}

const LOG_MACROS: &[&str] = &[
    "tracing::error!",
    "tracing::warn!",
    "tracing::info!",
    "tracing::debug!",
    "tracing::trace!",
    "error!(",
    "warn!(",
    "info!(",
    "debug!(",
];
const SPAN_MACROS: &[&str] = &[
    "tracing::info_span!",
    "tracing::span!",
    "#[instrument",
    "in_current_span",
];
const METRIC_MARKERS: &[&str] = &["metrics::", ".increment(", ".record(", ".observe("];

/// Field names/substrings that flag a log/span/metric call as potentially emitting a
/// secret or PII value. Intentionally broad (substring match) -- a false positive
/// here is a finding a human dismisses in review; a false negative is a leaked
/// credential in a telemetry backend.
const SECRET_FIELD_HINTS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "credential",
    "private_key",
    "ssn",
    "credit_card",
    "email",
];

fn classify(content: &str) -> Option<SignalKind> {
    if LOG_MACROS.iter().any(|m| content.contains(m)) {
        Some(SignalKind::Log)
    } else if SPAN_MACROS.iter().any(|m| content.contains(m)) {
        Some(SignalKind::Span)
    } else if METRIC_MARKERS.iter().any(|m| content.contains(m)) {
        Some(SignalKind::Metric)
    } else {
        None
    }
}

/// Crude keyword-overlap match between a requirement's free text and a signal's code
/// snippet: any word of 4+ characters in the requirement that also appears in the
/// snippet counts as a match. Good enough to link e.g. a requirement mentioning
/// "checkout latency" to a `checkout_latency_ms` metric call without needing a full
/// NLP pipeline -- a human reviews the mapping either way (constraint 2's
/// explanation surface).
fn requirement_matches(requirement_text: &str, snippet: &str) -> bool {
    let snippet_lower = snippet.to_lowercase();
    requirement_text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 4)
        .any(|w| snippet_lower.contains(w))
}

/// Scan `diff` (a unified diff, e.g. from `swebot::git::diff_against`) for
/// observability signals and secret/PII exposure among added lines, then map each
/// signal to the first `requirements` entry (`(id, text)`) whose text overlaps its
/// snippet.
pub fn collect_telemetry_evidence(
    diff: &str,
    requirements: &[(String, String)],
) -> TelemetryEvidence {
    let mut evidence = TelemetryEvidence::default();
    let mut current_file = "(unknown file)".to_string();

    for (i, line) in diff.lines().enumerate() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_file = path.to_string();
            continue;
        }
        if line.starts_with("+++") || !line.starts_with('+') {
            continue;
        }
        let content = line[1..].trim();
        if content.is_empty() {
            continue;
        }
        let location = format!("{current_file}:{}", i + 1);

        if let Some(kind) = classify(content) {
            let lower = content.to_lowercase();
            if let Some(hint) = SECRET_FIELD_HINTS.iter().find(|h| lower.contains(**h)) {
                evidence.findings.push(TelemetryFinding {
                    location: location.clone(),
                    description: format!(
                        "telemetry call appears to include a '{hint}' field: {content}"
                    ),
                });
            }

            let requirement_id = requirements
                .iter()
                .find(|(_, text)| requirement_matches(text, content))
                .map(|(id, _)| id.clone());

            evidence.signals.push(TelemetrySignal {
                kind,
                location,
                snippet: content.to_string(),
                requirement_id,
            });
        }
    }

    evidence
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_structured_log_signal() {
        let diff = "diff --git a/src/checkout.rs b/src/checkout.rs\n\
             +++ b/src/checkout.rs\n\
             @@ -1,2 +1,3 @@\n\
             +tracing::info!(order_id = %order.id, total_cents, \"checkout completed\");\n";
        let evidence = collect_telemetry_evidence(diff, &[]);
        assert_eq!(evidence.signals.len(), 1);
        assert_eq!(evidence.signals[0].kind, SignalKind::Log);
        assert!(evidence.signals[0].location.starts_with("src/checkout.rs:"));
        assert!(evidence.findings.is_empty());
    }

    #[test]
    fn detects_a_span_signal() {
        let diff = "+++ b/src/lib.rs\n+#[instrument(skip(self))]\n+async fn handle(&self) {}\n";
        let evidence = collect_telemetry_evidence(diff, &[]);
        assert_eq!(evidence.signals.len(), 1);
        assert_eq!(evidence.signals[0].kind, SignalKind::Span);
    }

    #[test]
    fn detects_a_metric_signal() {
        let diff = "+++ b/src/lib.rs\n+metrics::counter(\"orders_total\").increment(1);\n";
        let evidence = collect_telemetry_evidence(diff, &[]);
        assert_eq!(evidence.signals.len(), 1);
        assert_eq!(evidence.signals[0].kind, SignalKind::Metric);
    }

    #[test]
    fn flags_a_secret_field_inside_a_log_call() {
        let diff = "+++ b/src/auth.rs\n\
             +tracing::info!(user_email = %user.email, api_key = %creds.api_key, \"login\");\n";
        let evidence = collect_telemetry_evidence(diff, &[]);
        assert_eq!(evidence.signals.len(), 1);
        assert!(!evidence.findings.is_empty());
        assert!(
            evidence.findings[0].description.contains("api_key")
                || evidence.findings[0].description.contains("email")
        );
    }

    #[test]
    fn plain_code_without_telemetry_calls_is_ignored() {
        let diff = "+++ b/src/lib.rs\n+let total = price * quantity;\n";
        let evidence = collect_telemetry_evidence(diff, &[]);
        assert!(evidence.signals.is_empty());
        assert!(evidence.findings.is_empty());
    }

    #[test]
    fn removed_lines_are_not_scanned() {
        let diff = "+++ b/src/lib.rs\n-tracing::info!(password = %pw, \"old\");\n";
        let evidence = collect_telemetry_evidence(diff, &[]);
        assert!(evidence.signals.is_empty());
    }

    #[test]
    fn maps_a_signal_to_its_matching_requirement() {
        let diff = "+++ b/src/checkout.rs\n\
             +tracing::info!(checkout_latency_ms, \"checkout finished\");\n";
        let requirements = vec![(
            "R1".to_string(),
            "Emit checkout latency as a structured log field".to_string(),
        )];
        let evidence = collect_telemetry_evidence(diff, &requirements);
        assert_eq!(evidence.signals.len(), 1);
        assert_eq!(evidence.signals[0].requirement_id.as_deref(), Some("R1"));
    }

    #[test]
    fn an_unmatched_requirement_leaves_signal_requirement_id_none() {
        let diff = "+++ b/src/checkout.rs\n+tracing::info!(\"unrelated message\");\n";
        let requirements = vec![("R1".to_string(), "Emit refund latency".to_string())];
        let evidence = collect_telemetry_evidence(diff, &requirements);
        assert_eq!(evidence.signals.len(), 1);
        assert_eq!(evidence.signals[0].requirement_id, None);
    }
}
