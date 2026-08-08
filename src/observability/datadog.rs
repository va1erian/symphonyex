//! Datadog backend adapter (`observability.backend: datadog`) -- the migration
//! *source* in Roadmap §2's Datadog -> Open Observability Platform move. Deliberately
//! speaks Datadog's own response shape (`series[].pointlist`, `DD-API-KEY` auth)
//! rather than the generic contract `otlp`/`prometheus` share: the point of the
//! shared `super::validate` evaluation logic is that this adapter's differences stay
//! contained to fetching a raw value, never leaking into how that value is judged.

use super::ObservabilityBackend;
use crate::config::ObservabilityCheck;
use async_trait::async_trait;
use serde::Deserialize;

#[derive(Debug)]
pub struct DatadogBackend {
    client: reqwest::Client,
    query_url: String,
    token: Option<String>,
}

impl DatadogBackend {
    pub fn new(query_url: String, token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            query_url,
            token,
        }
    }
}

#[derive(Debug, Deserialize)]
struct DdQueryResponse {
    #[serde(default)]
    series: Vec<DdSeries>,
}

#[derive(Debug, Deserialize)]
struct DdSeries {
    #[serde(default)]
    pointlist: Vec<[f64; 2]>,
}

#[async_trait]
impl ObservabilityBackend for DatadogBackend {
    async fn query(
        &self,
        check: &ObservabilityCheck,
        window_minutes: u64,
    ) -> Result<Option<f64>, String> {
        let url = format!("{}/api/v1/query", self.query_url.trim_end_matches('/'));
        let mut req = self
            .client
            .get(&url)
            .query(&[("query", check.query.as_str())])
            .query(&[("window_minutes", window_minutes.to_string())]);
        if let Some(t) = &self.token {
            req = req.header("DD-API-KEY", t);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("GET {url} -> {status}: {body}"));
        }
        let parsed: DdQueryResponse = resp.json().await.map_err(|e| e.to_string())?;
        Ok(parsed
            .series
            .first()
            .and_then(|s| s.pointlist.last())
            .map(|point| point[1]))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Verdict, validate};
    use super::*;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn check(name: &str, max: f64) -> ObservabilityCheck {
        ObservabilityCheck {
            name: name.to_string(),
            query: "avg:trace.http.request.errors{env:prod}".to_string(),
            max,
        }
    }

    #[tokio::test]
    async fn parses_the_last_pointlist_value() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header("DD-API-KEY", "dd-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "series": [{"pointlist": [[1700000000000.0, 0.02], [1700000060000.0, 0.005]]}]
            })))
            .mount(&server)
            .await;
        let backend = DatadogBackend::new(server.uri(), Some("dd-token".to_string()));
        let value = backend.query(&check("error_rate", 0.01), 30).await.unwrap();
        assert_eq!(value, Some(0.005));
    }

    #[tokio::test]
    async fn empty_series_is_no_data() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"series": []})),
            )
            .mount(&server)
            .await;
        let backend = DatadogBackend::new(server.uri(), None);
        let value = backend.query(&check("error_rate", 0.01), 30).await.unwrap();
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn evaluates_identically_through_the_shared_logic() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "series": [{"pointlist": [[1700000000000.0, 0.005]]}]
            })))
            .mount(&server)
            .await;
        let backend = DatadogBackend::new(server.uri(), None);
        let checks = vec![check("error_rate", 0.01)];
        let result = validate(&backend, &checks, 30).await;
        assert_eq!(result.verdict, Verdict::Healthy);
    }

    #[tokio::test]
    async fn unreachable_backend_is_unknown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let backend = DatadogBackend::new(server.uri(), None);
        let checks = vec![check("error_rate", 0.01)];
        let result = validate(&backend, &checks, 30).await;
        assert_eq!(result.verdict, Verdict::Unknown);
    }
}
