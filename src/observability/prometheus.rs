//! Prometheus metrics-query backend adapter (`observability.backend: prometheus`).
//! Shares `super::fetch_generic_metric` with `otlp::OtlpBackend` -- see that module's
//! doc comment for why the two adapters intentionally speak the same generic
//! contract.

use super::{ObservabilityBackend, fetch_generic_metric};
use crate::config::ObservabilityCheck;
use async_trait::async_trait;

#[derive(Debug)]
pub struct PrometheusBackend {
    client: reqwest::Client,
    query_url: String,
    token: Option<String>,
}

impl PrometheusBackend {
    pub fn new(query_url: String, token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            query_url,
            token,
        }
    }
}

#[async_trait]
impl ObservabilityBackend for PrometheusBackend {
    async fn query(
        &self,
        check: &ObservabilityCheck,
        window_minutes: u64,
    ) -> Result<Option<f64>, String> {
        fetch_generic_metric(
            &self.client,
            &self.query_url,
            self.token.as_deref(),
            check,
            window_minutes,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn check(name: &str) -> ObservabilityCheck {
        ObservabilityCheck {
            name: name.to_string(),
            query: "histogram_quantile(0.95, http_request_duration_ms_bucket)".to_string(),
            max: 400.0,
        }
    }

    #[tokio::test]
    async fn returns_the_value_the_backend_reports() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param(
                "query",
                "histogram_quantile(0.95, http_request_duration_ms_bucket)",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"value": 250.0})),
            )
            .mount(&server)
            .await;
        let backend = PrometheusBackend::new(server.uri(), None);
        let value = backend.query(&check("p95_latency_ms"), 30).await.unwrap();
        assert_eq!(value, Some(250.0));
    }

    #[tokio::test]
    async fn a_value_less_body_is_no_data() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let backend = PrometheusBackend::new(server.uri(), None);
        let value = backend.query(&check("p95_latency_ms"), 30).await.unwrap();
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn same_evaluation_logic_as_otlp_produces_the_same_verdict_shape() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"value": 500.0})),
            )
            .mount(&server)
            .await;
        let backend = PrometheusBackend::new(server.uri(), None);
        let checks = vec![check("p95_latency_ms")];
        let result = super::super::validate(&backend, &checks, 30).await;
        assert_eq!(result.verdict, super::super::Verdict::Degraded);
    }
}
