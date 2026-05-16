# `parseh-llm-downloader`

**Consent-gated LLM model downloader.**

The **only** place in `parseh-miner` that makes an external HTTP request. The download is gated behind explicit user consent (the caller obtains consent via UI and passes a `Consent` token).

## Public API

```rust
use parseh_llm_downloader::*;

// 1. UI prompts user; on YES:
let consent = Consent::obtain(|| async {
    /* show UI dialog; return user's yes/no */
    user_clicked_yes()
}).await?;

// 2. Pick the model (V0.2 catalog has TinyLlama 1.1B Q4_K_M):
let spec = ModelCatalog::default_recommended();

// 3. Optional progress callback:
let progress: ProgressFn = Arc::new(|done, total| {
    println!("download progress: {} / {} bytes", done, total);
});

// 4. Download with SHA-256 verification:
let result = download_model(spec, consent, Some(progress)).await?;
```

## The `Consent` design

`Consent(())` has a private field; the only way to construct a value is `Consent::obtain(prompt).await` where `prompt` is a closure returning `bool`. **This means: no code path in this crate can download a model without going through a UI confirmation step.**

The async signature lets the dialog be awaited. The closure must actually run a UI prompt (a constant `|| async { true }` would technically work but is implicit consent and should only appear when the caller has a meaningful guard, e.g., a `--auto-download-llm` CLI flag).

## V0.2 model catalog

Currently ships with one model:

- **TinyLlama 1.1B Chat v1.0 Q4_K_M** GGUF (~640 MB, ~8 tok/s on CPU)
- Source: HuggingFace `TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF`

V0.3+ adds Phi-3-mini, Llama-3.2-1B, etc.

## SHA-256 verification scaffolding

The catalog has `expected_sha256: &str` per model. **V0.2 ships with a `PLACEHOLDER` value** flagged by a passing-by-design test (`placeholder_hash_is_flagged`). The first maintainer to run a real download must replace the placeholder with the observed hash and bump the crate version.

## What this crate does NOT include yet

- Resume partial downloads (HTTP Range) — V0.2.5
- Multi-mirror failover (IPFS pin, Cloudflare R2) — V0.3+
- Cosign / Sigstore signature verification on the GGUF — V0.3+
- Bandwidth throttling for residential users — V0.3+

## Test count

**16 tests · all passing.** Includes wiremock-based streaming-download test, SHA mismatch flag, HTTP 404 propagation, consent denial blocking download.

```bash
cargo test -p parseh-llm-downloader --release
```

## Status

✅ Shipped V0.2 · 2026-05-14.

Apache-2.0.
