# `parseh-llm-detect`

**Probe the local machine for installed LLM runtimes.**

Used by `parseh-miner` on startup to decide whether the node can advertise inference capability immediately, or whether it must prompt the user to download a model.

## What it detects

- **Ollama** at `http://localhost:11434` — lists available pulled models
- **llama.cpp** binaries in PATH (`llama-server`, `llama-cli`, `server.exe`, etc.)
- **GGUF model files** in standard cache locations (`~/.ollama/models/`, `~/.cache/lm-studio/models/`, `~/.cache/huggingface/hub/`, `~/.parseh/models/`)
- **NVIDIA GPU** via `nvidia-smi --query-gpu=name,memory.total --format=csv`
- **Windows AMD / Intel iGPU** via `wmic path Win32_VideoController`

All probes have explicit 1.5–2 s timeouts. No external network traffic.

## Public API

```rust
use parseh_llm_detect::*;

let result: DetectionResult = parseh_llm_detect::detect_all().await?;

if result.is_empty() {
    // No LLM found locally; ask permission to download
} else {
    match result.recommended_runtime() {
        Some(RecommendedRuntime::Ollama { endpoint, model }) => { /* prefer this */ },
        Some(RecommendedRuntime::LocalGguf { path, size_mb }) => { /* fall back */ },
        None => unreachable!("is_empty would have returned true"),
    }
}
```

`detect_all` runs all four probes concurrently. Total typical latency: < 2 s (Ollama's HTTP probe is the longest path).

## Selection priority

1. Ollama with ≥1 pulled model (fastest path; no extra setup)
2. Newest local GGUF file
3. None

GPU info is reported separately (`DetectionResult::gpu`) but does NOT participate in runtime selection — the inference host owns the offload decision.

## Test count

**14 unit + integration tests · all passing.**

```bash
cargo test -p parseh-llm-detect --release
```

Includes a wiremock-based fake Ollama for the `/api/tags` probe.

## Status

✅ Shipped V0.2 · 2026-05-14.

Apache-2.0.
