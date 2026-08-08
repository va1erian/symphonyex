//! Observability Agent (AI Roadmap 2026 §4, AIR-10): two independent jobs sharing one
//! provider-neutral backend abstraction.
//!
//! - `evidence` (pre-merge): static scan of a diff for structured log/span/metric
//!   signals and secret/PII exposure in telemetry -- produces `TelemetryEvidence`.
//! - `production_validation` (post-deploy, opt-in): after a deploy is detected
//!   (`deploy`), queries `ObservabilityBackend` for the deployed change's health over
//!   `observability.validation.window_minutes` and records a `healthy | degraded |
//!   unknown` verdict via `validate` below.
//!
//! Provider-neutrality (Roadmap §2's Datadog -> Open Observability Platform
//! migration) lives entirely in `ObservabilityBackend`: `otlp`/`prometheus`/`datadog`
//! are separate adapters (one struct per backend, so each can speak its provider's
//! own wire format), but every adapter's result flows through the one `validate`
//! function below -- migrating backends is a `observability.backend:` config change,
//! never a rewrite of the check-evaluation logic itself.

pub mod datadog;
pub mod deploy;
pub mod evidence;
pub mod otlp;
pub mod pre_merge;
pub mod production_validation;
pub mod prometheus;

use crate::config::{ObservabilityBackendKind, ObservabilityCheck, ObservabilityConfig};
use async_trait::async_trait;
use serde::Serialize;

/// A validation verdict. `Unknown` is a first-class outcome, never conflated with
/// `Healthy` -- it means "the backend was unreachable or returned no data for at
/// least one check," which is exactly the case a rollback decision must not be made
/// on (see this ticket's "Out of scope": rollback stays a CI/CD + human decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Healthy,
    Degraded,
    Unknown,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Healthy => "healthy",
            Verdict::Degraded => "degraded",
            Verdict::Unknown => "unknown",
        }
    }
}

/// One check's result: `value: None` means the backend returned no data for it
/// (query unreachable or genuinely empty), always treated as `passed: false` so a
/// missing signal can never silently read as passing.
#[derive(Debug, Clone, Serialize)]
pub struct CheckOutcome {
    pub name: String,
    pub value: Option<f64>,
    pub max: f64,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationResult {
    pub verdict: Verdict,
    pub window_minutes: u64,
    pub checks: Vec<CheckOutcome>,
    /// Human-readable explanation surfaced on the dashboard -- which check(s) failed,
    /// or why the verdict came back `unknown` (constraint 2's "explanation surface").
    pub reason: String,
}

/// Provider-neutral backend contract. Implemented once per backend
/// (`otlp::OtlpBackend`, `prometheus::PrometheusBackend`, `datadog::DatadogBackend`)
/// so each adapter owns its provider's own request/response shape and auth
/// convention; every implementor is driven identically by `validate` below.
#[async_trait]
pub trait ObservabilityBackend: Send + Sync {
    /// The check's query value over the last `window_minutes`. `Ok(None)` means the
    /// backend was reached but returned no data point for this query -- distinct
    /// from `Err`, which means the backend itself could not be reached/queried.
    async fn query(
        &self,
        check: &ObservabilityCheck,
        window_minutes: u64,
    ) -> Result<Option<f64>, String>;
}

/// The one place check results become a verdict -- shared by every backend so
/// migrating from Datadog to OTLP/Prometheus (or the reverse) can never change how a
/// check's pass/fail is decided, only how its raw value is fetched.
pub async fn validate(
    backend: &dyn ObservabilityBackend,
    checks: &[ObservabilityCheck],
    window_minutes: u64,
) -> ValidationResult {
    if checks.is_empty() {
        return ValidationResult {
            verdict: Verdict::Unknown,
            window_minutes,
            checks: Vec::new(),
            reason: "no checks configured".to_string(),
        };
    }

    let mut outcomes = Vec::with_capacity(checks.len());
    let mut unknown_reason: Option<String> = None;
    for check in checks {
        match backend.query(check, window_minutes).await {
            Ok(Some(value)) => outcomes.push(CheckOutcome {
                name: check.name.clone(),
                value: Some(value),
                max: check.max,
                passed: value <= check.max,
            }),
            Ok(None) => {
                unknown_reason
                    .get_or_insert_with(|| format!("no data returned for check '{}'", check.name));
                outcomes.push(CheckOutcome {
                    name: check.name.clone(),
                    value: None,
                    max: check.max,
                    passed: false,
                });
            }
            Err(e) => {
                unknown_reason.get_or_insert_with(|| {
                    format!(
                        "backend unreachable while evaluating check '{}': {e}",
                        check.name
                    )
                });
                outcomes.push(CheckOutcome {
                    name: check.name.clone(),
                    value: None,
                    max: check.max,
                    passed: false,
                });
            }
        }
    }

    if let Some(reason) = unknown_reason {
        return ValidationResult {
            verdict: Verdict::Unknown,
            window_minutes,
            checks: outcomes,
            reason,
        };
    }

    if outcomes.iter().all(|o| o.passed) {
        ValidationResult {
            verdict: Verdict::Healthy,
            window_minutes,
            checks: outcomes,
            reason: "all checks within threshold".to_string(),
        }
    } else {
        let failing: Vec<&str> = outcomes
            .iter()
            .filter(|o| !o.passed)
            .map(|o| o.name.as_str())
            .collect();
        let reason = format!("check(s) exceeded threshold: {}", failing.join(", "));
        ValidationResult {
            verdict: Verdict::Degraded,
            window_minutes,
            checks: outcomes,
            reason,
        }
    }
}

/// Construct the configured backend. `None` for `backend: none` (the default) or a
/// missing `query_url` -- either way `production_validation`'s poller simply never
/// spawns (see `orchestrator::run_inner`), leaving the daemon's behavior unchanged.
pub fn build_backend(cfg: &ObservabilityConfig) -> Option<Box<dyn ObservabilityBackend>> {
    let query_url = cfg.query_url.clone()?;
    let token = cfg
        .token_env
        .as_ref()
        .and_then(|var| std::env::var(var).ok())
        .filter(|v| !v.is_empty());
    match cfg.backend {
        ObservabilityBackendKind::None => None,
        ObservabilityBackendKind::Otlp => Some(Box::new(otlp::OtlpBackend::new(query_url, token))),
        ObservabilityBackendKind::Prometheus => Some(Box::new(prometheus::PrometheusBackend::new(
            query_url, token,
        ))),
        ObservabilityBackendKind::Datadog => {
            Some(Box::new(datadog::DatadogBackend::new(query_url, token)))
        }
    }
}

/// Shared HTTP fetch for the OTLP and Prometheus adapters: `GET query_url?query=
/// <check.query>&window_minutes=<n>` expecting `{"value": <f64>}` JSON, or a 404 /
/// value-less body meaning "no data." A real OTLP/Prometheus deployment normally
/// sits behind a metrics-query gateway speaking exactly this shape (or one a thin
/// proxy can trivially expose); modeling both adapters against one generic contract
/// is what makes switching between them a config change rather than new code, per
/// this ticket's implementation notes.
pub(crate) async fn fetch_generic_metric(
    client: &reqwest::Client,
    query_url: &str,
    token: Option<&str>,
    check: &ObservabilityCheck,
    window_minutes: u64,
) -> Result<Option<f64>, String> {
    let mut req = client
        .get(query_url)
        .query(&[("query", check.query.as_str())])
        .query(&[("window_minutes", window_minutes.to_string())]);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("GET {query_url} -> {status}: {body}"));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(body.get("value").and_then(|v| v.as_f64()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubBackend {
        values: std::collections::HashMap<String, Result<Option<f64>, String>>,
    }

    #[async_trait]
    impl ObservabilityBackend for StubBackend {
        async fn query(
            &self,
            check: &ObservabilityCheck,
            _window_minutes: u64,
        ) -> Result<Option<f64>, String> {
            self.values.get(&check.name).cloned().unwrap_or(Ok(None))
        }
    }

    fn check(name: &str, max: f64) -> ObservabilityCheck {
        ObservabilityCheck {
            name: name.to_string(),
            query: format!("query for {name}"),
            max,
        }
    }

    #[tokio::test]
    async fn all_checks_within_threshold_is_healthy() {
        let backend = StubBackend {
            values: [
                ("error_rate".to_string(), Ok(Some(0.001))),
                ("p95_latency_ms".to_string(), Ok(Some(200.0))),
            ]
            .into_iter()
            .collect(),
        };
        let checks = vec![check("error_rate", 0.01), check("p95_latency_ms", 400.0)];
        let result = validate(&backend, &checks, 30).await;
        assert_eq!(result.verdict, Verdict::Healthy);
        assert!(result.checks.iter().all(|c| c.passed));
    }

    #[tokio::test]
    async fn a_check_exceeding_max_is_degraded_not_unknown() {
        let backend = StubBackend {
            values: [("error_rate".to_string(), Ok(Some(0.5)))]
                .into_iter()
                .collect(),
        };
        let checks = vec![check("error_rate", 0.01)];
        let result = validate(&backend, &checks, 30).await;
        assert_eq!(result.verdict, Verdict::Degraded);
        assert!(result.reason.contains("error_rate"));
    }

    #[tokio::test]
    async fn missing_data_is_unknown_never_healthy() {
        let backend = StubBackend {
            values: [("error_rate".to_string(), Ok(None))].into_iter().collect(),
        };
        let checks = vec![check("error_rate", 0.01)];
        let result = validate(&backend, &checks, 30).await;
        assert_eq!(result.verdict, Verdict::Unknown);
        assert_ne!(result.verdict, Verdict::Healthy);
    }

    #[tokio::test]
    async fn unreachable_backend_is_unknown_never_healthy() {
        let backend = StubBackend {
            values: [(
                "error_rate".to_string(),
                Err("connection refused".to_string()),
            )]
            .into_iter()
            .collect(),
        };
        let checks = vec![check("error_rate", 0.01)];
        let result = validate(&backend, &checks, 30).await;
        assert_eq!(result.verdict, Verdict::Unknown);
        assert!(result.reason.contains("unreachable"));
    }

    #[tokio::test]
    async fn no_checks_configured_is_unknown() {
        let backend = StubBackend {
            values: std::collections::HashMap::new(),
        };
        let result = validate(&backend, &[], 30).await;
        assert_eq!(result.verdict, Verdict::Unknown);
    }

    #[test]
    fn backend_none_builds_nothing() {
        let cfg = ObservabilityConfig::default();
        assert!(build_backend(&cfg).is_none());
    }
}
