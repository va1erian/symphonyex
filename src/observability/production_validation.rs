//! Post-deploy validation driver (AIR-10, `observability.validation.after_deploy`):
//! polls for a new deploy (`deploy::DeploySignal`), waits out
//! `observability.validation.window_minutes` so the deployed change has real traffic
//! behind it, then evaluates `observability.validation.checks` against the
//! configured backend and records the verdict.
//!
//! Spawned once at startup (`orchestrator::run_inner`), same posture as
//! `swebot::run`'s own background task -- a config change here needs a restart to
//! take effect, not hot-reloaded by `maybe_reload`.

use super::deploy::{CommandDeploySignal, DeployEvent, DeploySignal, HostDeploySignal};
use super::{ObservabilityBackend, ValidationResult, validate};
use crate::config::{EffectiveConfig, ObservabilityValidationConfig};
use crate::eventlog::NewEvent;
use crate::repo_host::RepoHost;
use tokio::sync::mpsc::UnboundedSender;

/// One poll tick: check every configured signal for a deploy not yet seen (by
/// `identifier`), and if found, wait the validation window then record a verdict.
/// Split out from `run`'s loop so it's independently testable without a real clock
/// loop or network poller.
pub async fn validate_new_deploy(
    backend: &dyn ObservabilityBackend,
    validation: &ObservabilityValidationConfig,
    sleep: impl std::future::Future<Output = ()>,
) -> ValidationResult {
    sleep.await;
    validate(backend, &validation.checks, validation.window_minutes).await
}

fn event_issue_id(event: &DeployEvent) -> String {
    event
        .issue_id
        .clone()
        .unwrap_or_else(|| format!("deploy:{}", event.sha))
}

/// Serialize a `ValidationResult` into the eventlog's `message` field -- the
/// dashboard (`status.rs`'s `/observability` page) reads it back with
/// `serde_json::from_str` to render the verdict and its explanation.
pub fn record_validation_event(
    event_tx: &UnboundedSender<NewEvent>,
    event: &DeployEvent,
    result: &ValidationResult,
) {
    let message = serde_json::to_string(result).unwrap_or_default();
    let _ = event_tx.send(NewEvent {
        issue_id: event_issue_id(event),
        identifier: event_issue_id(event),
        title: format!("deploy {}", short_sha(&event.sha)),
        session_id: None,
        event_type: "production_validation".to_string(),
        message: Some(message),
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
    });
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// Background loop: on every `poll_interval_ms` tick, ask each configured
/// `DeploySignal` for its latest deploy; a deploy whose `identifier` hasn't been seen
/// before triggers a validation. Runs for the process lifetime -- there's no
/// shutdown signal threaded in here, matching `swebot::run`'s own fire-and-forget
/// spawn in `orchestrator::run_inner`.
pub async fn run(cfg: EffectiveConfig, event_tx: UnboundedSender<NewEvent>) {
    let Some(backend) = super::build_backend(&cfg.observability) else {
        return;
    };
    if !cfg.observability.validation.after_deploy {
        return;
    }

    let repo_host = cfg
        .repo
        .as_ref()
        .and_then(|r| crate::repo_host::build(r).ok());
    let deploy_command = cfg.observability.validation.deploy_command.clone();

    let mut seen = std::collections::HashSet::new();
    let mut interval =
        tokio::time::interval(std::time::Duration::from_millis(cfg.poll_interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        interval.tick().await;
        let detected = detect_latest(repo_host.as_deref(), deploy_command.as_deref()).await;
        let Some(event) = detected else { continue };
        if !seen.insert(event.identifier.clone()) {
            continue;
        }

        tracing::info!(sha = %event.sha, "observability: new deploy detected, scheduling production validation");
        let window =
            std::time::Duration::from_secs(cfg.observability.validation.window_minutes * 60);
        let result = validate_new_deploy(
            backend.as_ref(),
            &cfg.observability.validation,
            tokio::time::sleep(window),
        )
        .await;
        tracing::info!(sha = %event.sha, verdict = result.verdict.as_str(), "observability: production validation recorded");
        record_validation_event(&event_tx, &event, &result);
    }
}

async fn detect_latest(
    repo_host: Option<&dyn RepoHost>,
    deploy_command: Option<&str>,
) -> Option<DeployEvent> {
    if let Some(command) = deploy_command {
        let signal = CommandDeploySignal {
            command: command.to_string(),
        };
        match signal.latest_deploy().await {
            Ok(Some(event)) => return Some(event),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "observability: deploy_command failed (ignored this tick)")
            }
        }
    }
    if let Some(host) = repo_host {
        let signal = HostDeploySignal { host };
        match signal.latest_deploy().await {
            Ok(Some(event)) => return Some(event),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "observability: host deploy-status API failed (ignored this tick)")
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ObservabilityCheck;
    use async_trait::async_trait;

    struct InstantBackend;
    #[async_trait]
    impl ObservabilityBackend for InstantBackend {
        async fn query(
            &self,
            check: &ObservabilityCheck,
            _window_minutes: u64,
        ) -> Result<Option<f64>, String> {
            Ok(Some(if check.name == "error_rate" {
                0.001
            } else {
                0.0
            }))
        }
    }

    #[tokio::test]
    async fn validate_new_deploy_waits_then_evaluates() {
        let validation = ObservabilityValidationConfig {
            after_deploy: true,
            window_minutes: 0,
            checks: vec![ObservabilityCheck {
                name: "error_rate".to_string(),
                query: "q".to_string(),
                max: 0.01,
            }],
            deploy_command: None,
        };
        let backend = InstantBackend;
        let result = validate_new_deploy(&backend, &validation, async {}).await;
        assert_eq!(result.verdict, super::super::Verdict::Healthy);
    }

    #[test]
    fn event_issue_id_falls_back_to_the_deploy_sha_when_unresolved() {
        let event = DeployEvent {
            sha: "abc123".to_string(),
            identifier: "abc123".to_string(),
            issue_id: None,
        };
        assert_eq!(event_issue_id(&event), "deploy:abc123");
    }

    #[test]
    fn event_issue_id_prefers_a_resolved_issue_id() {
        let event = DeployEvent {
            sha: "abc123".to_string(),
            identifier: "abc123".to_string(),
            issue_id: Some("AIR-10".to_string()),
        };
        assert_eq!(event_issue_id(&event), "AIR-10");
    }

    #[test]
    fn record_validation_event_serializes_the_verdict_into_the_message() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let event = DeployEvent {
            sha: "abc123".to_string(),
            identifier: "abc123".to_string(),
            issue_id: None,
        };
        let result = ValidationResult {
            verdict: super::super::Verdict::Unknown,
            window_minutes: 30,
            checks: vec![],
            reason: "no checks configured".to_string(),
        };
        record_validation_event(&tx, &event, &result);
        let recorded = rx.try_recv().unwrap();
        assert_eq!(recorded.event_type, "production_validation");
        assert!(recorded.message.unwrap().contains("unknown"));
    }
}
