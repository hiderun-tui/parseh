//! Ollama daemon probe.
//!
//! Hits `GET /api/tags` and `GET /api/version` on `localhost:11434` with a
//! short timeout. Anything other than `200 OK` is treated as "not running".

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Default Ollama endpoint. Configurable in the future via env var.
const DEFAULT_ENDPOINT: &str = "http://localhost:11434";

/// Total time budget for a single Ollama HTTP probe (connect + read).
const HTTP_TIMEOUT: Duration = Duration::from_millis(1_500);

/// Information about a reachable Ollama daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaInfo {
    /// Base URL of the daemon (no trailing slash).
    pub endpoint: String,
    /// Daemon version reported by `/api/version` (best-effort, may be empty).
    pub version: String,
    /// Models the daemon currently has pulled.
    pub models: Vec<OllamaModel>,
}

/// One row from Ollama's `/api/tags` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaModel {
    pub name: String,
    #[serde(default)]
    pub modified_at: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<OllamaModel>,
}

#[derive(Debug, Deserialize)]
struct VersionResponse {
    #[serde(default)]
    version: String,
}

/// Probe the default Ollama endpoint.
pub async fn probe() -> anyhow::Result<OllamaInfo> {
    probe_endpoint(DEFAULT_ENDPOINT).await
}

/// Probe a specific Ollama endpoint. Exposed for tests via [`crate::ollama`]
/// internals; keep it `pub(crate)` so the public surface stays minimal.
pub(crate) async fn probe_endpoint(endpoint: &str) -> anyhow::Result<OllamaInfo> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("build reqwest client")?;

    let tags_url = format!("{endpoint}/api/tags");
    let resp = client
        .get(&tags_url)
        .send()
        .await
        .with_context(|| format!("GET {tags_url}"))?;

    if !resp.status().is_success() {
        anyhow::bail!("ollama /api/tags returned HTTP {}", resp.status());
    }

    let tags: TagsResponse = resp
        .json()
        .await
        .context("parse /api/tags JSON")?;

    // Version is best-effort: an old Ollama may not expose /api/version.
    let version = match client
        .get(format!("{endpoint}/api/version"))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r
            .json::<VersionResponse>()
            .await
            .map(|v| v.version)
            .unwrap_or_default(),
        _ => String::new(),
    };

    Ok(OllamaInfo {
        endpoint: endpoint.to_string(),
        version,
        models: tags.models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn parses_tags_and_version() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    { "name": "qwen2.5:7b", "modified_at": "2025-01-01T00:00:00Z", "size": 4_700_000_000u64 }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/version"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "version": "0.5.4"
            })))
            .mount(&server)
            .await;

        let info = probe_endpoint(&server.uri()).await.expect("probe ok");
        assert_eq!(info.models.len(), 1);
        assert_eq!(info.models[0].name, "qwen2.5:7b");
        assert_eq!(info.version, "0.5.4");
    }

    #[tokio::test]
    async fn handles_empty_model_list() {
        // Edge case: Ollama is running but no models have been pulled yet.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "models": [] })),
            )
            .mount(&server)
            .await;
        // No /api/version mount — must still succeed with empty version.

        let info = probe_endpoint(&server.uri()).await.expect("probe ok");
        assert!(info.models.is_empty());
        assert!(info.version.is_empty());
    }

    #[tokio::test]
    async fn http_500_is_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        assert!(probe_endpoint(&server.uri()).await.is_err());
    }
}
