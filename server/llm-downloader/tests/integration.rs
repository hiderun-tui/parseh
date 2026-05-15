//! Integration tests for `parseh-llm-downloader`.
//!
//! These cover the consent gate, the catalog surface, and the streaming
//! download path against a wiremock server (so no real HuggingFace traffic
//! ever leaves the test binary).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parseh_llm_downloader::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn consent_denied_blocks_download() {
    let consent = Consent::obtain(|| async { false }).await;
    assert!(consent.is_err());
}

#[tokio::test]
async fn consent_granted_allows_construction() {
    let consent = Consent::obtain(|| async { true }).await;
    assert!(consent.is_ok());
}

#[tokio::test]
async fn consent_prompt_actually_runs() {
    // The closure must be polled; we use it to flip a boolean.
    let was_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let was_called_inner = was_called.clone();
    let _ = Consent::obtain(|| async move {
        was_called_inner.store(true, std::sync::atomic::Ordering::SeqCst);
        true
    })
    .await
    .expect("consent should be granted");
    assert!(was_called.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn model_catalog_default_recommended_is_tinyllama() {
    let m = ModelCatalog::default_recommended();
    assert_eq!(m.name, "tinyllama-1.1b-chat-q4_k_m");
    assert!(m.url.contains("TinyLlama"));
}

#[test]
fn model_catalog_lookup_round_trips() {
    let by_name = ModelCatalog::get("tinyllama-1.1b-chat-q4_k_m")
        .expect("default model must be findable by name");
    let default = ModelCatalog::default_recommended();
    assert_eq!(by_name.url, default.url);
    assert_eq!(by_name.expected_size_bytes, default.expected_size_bytes);
}

#[tokio::test]
async fn download_streams_to_disk_and_hashes() {
    // Bring up a local mock server, point a synthetic ModelSpec at it, and
    // verify that the downloader writes the file and computes a SHA-256.
    let server = MockServer::start().await;

    let body: Vec<u8> = (0..(64 * 1024 + 17)).map(|i| (i % 251) as u8).collect();
    // Pre-compute the expected SHA-256 of `body` so we can assert the
    // verified flag flips to `true` on a match.
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(&body);
    let expected_hex = format!("{:x}", h.finalize());

    Mock::given(method("GET"))
        .and(path("/tinyllama.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;

    // Synthesise a ModelSpec pointing at the mock server. We have to leak
    // the strings to satisfy the `&'static str` fields, which is fine in a
    // test binary.
    let url: &'static str = Box::leak(format!("{}/tinyllama.gguf", server.uri()).into_boxed_str());
    let sha: &'static str = Box::leak(expected_hex.clone().into_boxed_str());
    let spec = ModelSpec {
        name: "test-model",
        description: "synthetic test model",
        filename: "test-model.gguf",
        url,
        expected_size_bytes: body.len() as u64,
        expected_sha256: sha,
    };

    let consent = Consent::obtain(|| async { true })
        .await
        .expect("consent");

    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("test-model.gguf");

    let progress_calls = Arc::new(AtomicU64::new(0));
    let progress_calls_inner = progress_calls.clone();
    let progress: ProgressFn = Arc::new(move |downloaded, _total| {
        progress_calls_inner.fetch_add(1, Ordering::SeqCst);
        assert!(downloaded > 0);
    });

    let result = download_model_to(&spec, &dest, consent, Some(progress))
        .await
        .expect("download should succeed");

    assert_eq!(result.bytes_written, body.len() as u64);
    assert_eq!(result.sha256, expected_hex);
    assert!(
        result.sha256_verified,
        "computed hash must equal the synthesised expected hash"
    );
    assert!(progress_calls.load(Ordering::SeqCst) >= 1);

    let written = tokio::fs::read(&dest).await.unwrap();
    assert_eq!(written, body);
}

#[tokio::test]
async fn download_flags_sha_mismatch_when_hash_differs() {
    let server = MockServer::start().await;
    let body = b"hello-world-not-a-real-model".to_vec();

    Mock::given(method("GET"))
        .and(path("/wrong.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;

    let url: &'static str = Box::leak(format!("{}/wrong.gguf", server.uri()).into_boxed_str());
    let spec = ModelSpec {
        name: "test-mismatch",
        description: "deliberate hash mismatch",
        filename: "wrong.gguf",
        url,
        expected_size_bytes: body.len() as u64,
        expected_sha256: "0000000000000000000000000000000000000000000000000000000000000000",
    };

    let consent = Consent::obtain(|| async { true }).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("wrong.gguf");

    let result = download_model_to(&spec, &dest, consent, None)
        .await
        .expect("download should still succeed even with a bad hash");

    assert_eq!(result.bytes_written, body.len() as u64);
    assert!(
        !result.sha256_verified,
        "hash mismatch must surface as sha256_verified=false"
    );
    assert_ne!(result.sha256, spec.expected_sha256);
}

#[tokio::test]
async fn download_propagates_http_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing.gguf"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let url: &'static str =
        Box::leak(format!("{}/missing.gguf", server.uri()).into_boxed_str());
    let spec = ModelSpec {
        name: "test-404",
        description: "deliberate 404",
        filename: "missing.gguf",
        url,
        expected_size_bytes: 0,
        expected_sha256: "PLACEHOLDER",
    };

    let consent = Consent::obtain(|| async { true }).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("missing.gguf");

    let err = download_model_to(&spec, &dest, consent, None)
        .await
        .expect_err("404 must surface as an error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("status") || msg.contains("404"),
        "error chain should mention the HTTP status, got: {msg}"
    );
}
