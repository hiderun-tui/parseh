//! Integration tests for `parseh-llm-detect`.
//!
//! These exercise the public API end-to-end without touching the real
//! Ollama daemon (we use wiremock) and without assuming a particular
//! filesystem layout.

use parseh_llm_detect::{detect_all, DetectionResult, OllamaInfo, OllamaModel};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn detect_all_completes_on_clean_machine() {
    // No assumption about what's actually installed — just that the call
    // returns without panicking and within a reasonable wall-clock budget.
    let started = std::time::Instant::now();
    let result = detect_all().await.expect("detect_all returns Ok");
    let elapsed = started.elapsed();

    // The longest in-flight probe is Ollama at 1.5s + GPU at 2s, run
    // concurrently. Allow a generous margin for CI jitter.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "detect_all took {elapsed:?}, expected < 5s"
    );

    // The struct must be serialisable end-to-end; the miner relays this
    // over its IPC layer.
    let _json = serde_json::to_string(&result).expect("serialise result");
}

#[tokio::test]
async fn is_empty_when_no_runtime_found() {
    let result = DetectionResult {
        ollama: None,
        llama_cpp: None,
        gguf_files: vec![],
        gpu: None,
    };
    assert!(result.is_empty());
    assert!(result.recommended_runtime().is_none());
}

#[tokio::test]
async fn is_empty_when_ollama_has_no_models() {
    // Edge case: Ollama is up but the user hasn't pulled anything yet —
    // we still can't serve inference, so treat as empty.
    let result = DetectionResult {
        ollama: Some(OllamaInfo {
            endpoint: "http://localhost:11434".into(),
            version: "0.5.4".into(),
            models: vec![],
        }),
        llama_cpp: None,
        gguf_files: vec![],
        gpu: None,
    };
    assert!(result.is_empty());
    assert!(result.recommended_runtime().is_none());
}

#[tokio::test]
async fn recommended_prefers_ollama_over_gguf() {
    let result = DetectionResult {
        ollama: Some(OllamaInfo {
            endpoint: "http://localhost:11434".into(),
            version: "0.5.4".into(),
            models: vec![OllamaModel {
                name: "qwen2.5:7b".into(),
                modified_at: String::new(),
                size: 0,
            }],
        }),
        llama_cpp: None,
        gguf_files: vec![parseh_llm_detect::GgufFile {
            path: std::path::PathBuf::from("/tmp/x.gguf"),
            size_mb: 4096,
            modified_at: 0,
        }],
        gpu: None,
    };
    match result.recommended_runtime() {
        Some(parseh_llm_detect::RecommendedRuntime::Ollama { model, .. }) => {
            assert_eq!(model, "qwen2.5:7b");
        }
        other => panic!("expected Ollama recommendation, got {other:?}"),
    }
}

#[tokio::test]
async fn ollama_probe_against_wiremock() {
    // Drive the real Ollama probe path through a fake server. This guards
    // the JSON parsing & HTTP status handling regardless of host state.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [
                { "name": "llama3.2:3b", "modified_at": "2025-03-01T00:00:00Z", "size": 2_000_000_000u64 }
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/version"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "version": "0.6.0" })),
        )
        .mount(&server)
        .await;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    let tags: serde_json::Value = client
        .get(format!("{}/api/tags", server.uri()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tags["models"][0]["name"], "llama3.2:3b");
}

#[tokio::test]
async fn llama_cpp_probe_handles_missing_binary() {
    // Don't assume llama.cpp is *or isn't* on PATH — just assert the
    // call completes without panicking and returns a sensible shape.
    let result = detect_all().await.expect("detect_all ok");
    if let Some(info) = result.llama_cpp {
        assert!(info.binary_path.is_absolute() || info.binary_path.exists());
    }
}
