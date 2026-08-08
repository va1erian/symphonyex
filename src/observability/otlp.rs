//! OTLP metrics-query backend adapter (`observability.backend: otlp`) -- the Open
//! Observability Platform side of the Roadmap §2 Datadog migration. Shares
//! `super::fetch_generic_metric` with `prometheus::PrometheusBackend`: both speak the
//! same generic "GET query -> `{"value": <f64>}`" contract a metrics-query gateway in
//! front of either backend commonly exposes, which is exactly what makes swapping
//! between them (or moving off Datadog onto this one) a config change rather than new
//! adapter code -- see `super::validate`'s doc comment.

use super::{ObservabilityBackend, fetch_generic_metric};
use crate::config::ObservabilityCheck;
use async_trait::async_trait;

#[derive(Debug)]
pub struct OtlpBackend {
    client: reqwest::Client,
    query_url: String,
    token: Option<String>,
}

impl OtlpBackend {
    pub fn new(query_url: String, token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            query_url,
            token,
        }
    }
}

#[async_trait]
impl ObservabilityBackend for OtlpBackend {
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
            query: "otel_error_rate".to_string(),
            max: 0.01,
        }
    }

    #[tokio::test]
    async fn returns_the_value_the_backend_reports() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("query", "otel_error_rate"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"value": 0.002})),
            )
            .mount(&server)
            .await;
        let backend = OtlpBackend::new(server.uri(), None);
        let value = backend.query(&check("error_rate"), 30).await.unwrap();
        assert_eq!(value, Some(0.002));
    }

    #[tokio::test]
    async fn a_404_is_no_data_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let backend = OtlpBackend::new(server.uri(), None);
        let value = backend.query(&check("error_rate"), 30).await.unwrap();
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn a_server_error_is_returned_as_err() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let backend = OtlpBackend::new(server.uri(), None);
        assert!(backend.query(&check("error_rate"), 30).await.is_err());
    }
}
