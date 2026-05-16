//! Pluggable LLM backend.
//!
//! Two backends ship:
//!
//! - [`OllamaBackend`] — a local Ollama daemon over HTTP. Endpoint is
//!   discovered via `parseh-llm-detect` (or supplied explicitly). The
//!   `/api/generate` call pins `options.seed` + `options.temperature =
//!   0` so two executions with the same seed produce byte-identical
//!   output *on the same model* — the property `parseh-verify` relies
//!   on for deterministic-mode re-execution.
//! - [`MockBackend`] — a deterministic, hash-derived canned response.
//!   No network. Used by tests so CI never depends on a running daemon.
//!
//! ## Determinism caveat (recorded honestly)
//!
//! Ollama's seed determinism is *model- and runtime-version-dependent*.
//! A pinned seed gives byte-identical output only when the verifier
//! uses the **same model weights and the same Ollama build**. The seed
//! alone is not sufficient; [`LlmBackend::model_id`] is surfaced into
//! the signed `JobResult` precisely so a verifier can refuse to
//! re-execute against a mismatched model rather than produce a false
//! disagreement. Cross-model deterministic verification is out of
//! scope (V0.3+).

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::time::Duration;
use thiserror::Error;

/// Errors raised by an [`LlmBackend`].
#[derive(Error, Debug)]
pub enum BackendError {
    /// The backend could not be reached (connection refused, timeout).
    #[error("backend unreachable: {0}")]
    Unreachable(String),
    /// The backend returned a non-success HTTP status.
    #[error("backend returned HTTP {status}: {body}")]
    HttpStatus {
        /// HTTP status code.
        status: u16,
        /// Response body (truncated by the caller if large).
        body: String,
    },
    /// The backend's response could not be parsed.
    #[error("backend response malformed: {0}")]
    MalformedResponse(String),
    /// No local LLM runtime was discovered by `parseh-llm-detect`.
    #[error("no local Ollama runtime detected — install Ollama and pull a model, or use MockBackend")]
    NoRuntime,
}

/// A deterministic LLM completion backend.
///
/// `seed` MUST be honoured: `parseh-verify` re-executes with the same
/// seed and expects byte-identical output (on the same model). A
/// backend that ignores the seed silently breaks deterministic-mode
/// verification.
#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// Execute `prompt` deterministically with the given `seed` and
    /// token budget. Returns the raw completion string.
    async fn complete(
        &self,
        prompt: &str,
        seed: u64,
        max_tokens: u32,
    ) -> Result<String, BackendError>;

    /// Identifier of the model the backend will use (e.g.
    /// `"qwen2.5:7b"`). Surfaced into the signed `JobResult` so
    /// verifiers can refuse a model mismatch.
    fn model_id(&self) -> String;
}

/// Ollama HTTP backend. Endpoint discovered via `parseh-llm-detect`.
pub struct OllamaBackend {
    endpoint: String,
    model: String,
    timeout: Duration,
}

impl OllamaBackend {
    /// Construct against an explicit endpoint + model. The endpoint
    /// must not have a trailing slash (matches `parseh-llm-detect`'s
    /// `OllamaInfo::endpoint` convention).
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
            // Inference can be slow on CPU-only nodes; generous budget.
            timeout: Duration::from_secs(600),
        }
    }

    /// Discover a local Ollama runtime via `parseh-llm-detect` and
    /// construct a backend against its first available model.
    ///
    /// # Errors
    ///
    /// [`BackendError::NoRuntime`] if no Ollama daemon with at least
    /// one pulled model is reachable.
    pub async fn discover() -> Result<Self, BackendError> {
        let detection = parseh_llm_detect::detect_all()
            .await
            .map_err(|e| BackendError::Unreachable(e.to_string()))?;
        match detection.recommended_runtime() {
            Some(parseh_llm_detect::RecommendedRuntime::Ollama { endpoint, model }) => {
                Ok(Self::new(endpoint, model))
            }
            _ => Err(BackendError::NoRuntime),
        }
    }

    /// Override the per-request timeout (default 600 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl LlmBackend for OllamaBackend {
    async fn complete(
        &self,
        prompt: &str,
        seed: u64,
        max_tokens: u32,
    ) -> Result<String, BackendError> {
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| BackendError::Unreachable(e.to_string()))?;

        // Deterministic-mode request body. `temperature: 0` + a pinned
        // `seed` is Ollama's documented recipe for reproducible output.
        // `num_predict` caps the token budget. `stream: false` so the
        // whole completion arrives in one JSON object.
        let body = serde_json::json!({
            "model": self.model,
            "prompt": prompt,
            "stream": false,
            "options": {
                "seed": seed,
                "temperature": 0,
                "num_predict": max_tokens,
            }
        });

        let url = format!("{}/api/generate", self.endpoint);
        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| BackendError::Unreachable(format!("POST {url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::HttpStatus {
                status: status.as_u16(),
                body: body.chars().take(512).collect(),
            });
        }

        // Ollama non-stream response: { "response": "...", ... }.
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BackendError::MalformedResponse(e.to_string()))?;
        json.get("response")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                BackendError::MalformedResponse(
                    "ollama response missing string `response` field".to_string(),
                )
            })
    }

    fn model_id(&self) -> String {
        self.model.clone()
    }
}

/// Deterministic mock backend for tests.
///
/// Returns a SHA-256-derived canned string keyed by `(prompt, seed)`.
/// Same input → same output, with no network — exactly the property a
/// deterministic-mode verifier needs, so the mock doubles as a
/// re-execution oracle in tests.
pub struct MockBackend {
    model: String,
}

impl MockBackend {
    /// Construct with a stated pseudo model id.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
        }
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new("mock-deterministic-v1")
    }
}

#[async_trait]
impl LlmBackend for MockBackend {
    async fn complete(
        &self,
        prompt: &str,
        seed: u64,
        max_tokens: u32,
    ) -> Result<String, BackendError> {
        // Hash (prompt, seed) so the response is reproducible but
        // input-sensitive. We emit a JSON object so the mock can drive
        // agents whose output_schema expects structured output: the
        // common case in the test-suite.
        let mut hasher = Sha256::new();
        hasher.update(prompt.as_bytes());
        hasher.update(seed.to_le_bytes());
        hasher.update(max_tokens.to_le_bytes());
        let digest = hex_lower(&hasher.finalize());
        Ok(format!(
            r#"{{"answer":"mock-{}","seed":{}}}"#,
            &digest[..16],
            seed
        ))
    }

    fn model_id(&self) -> String {
        self.model.clone()
    }
}

/// Lower-case hex without pulling the `hex` crate into this leaf module.
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
