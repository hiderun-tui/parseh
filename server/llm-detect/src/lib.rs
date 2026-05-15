//! `parseh-llm-detect` — probe the local machine for installed LLM runtimes.
//!
//! Used by `parseh-miner` on startup to decide whether the node can advertise
//! inference capability immediately, or whether it must prompt the user to
//! download a model.
//!
//! All probes run concurrently and each has an explicit timeout — a full
//! [`detect_all`] call typically completes in well under two seconds even
//! when nothing is installed.

use serde::{Deserialize, Serialize};

mod gguf;
mod gpu;
mod llama_cpp;
mod ollama;

pub use gguf::GgufFile;
pub use gpu::GpuInfo;
pub use llama_cpp::LlamaCppInfo;
pub use ollama::{OllamaInfo, OllamaModel};

/// Aggregated result of probing all known LLM surfaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionResult {
    /// Information about a running Ollama daemon, if reachable.
    pub ollama: Option<OllamaInfo>,
    /// llama.cpp binary discovered on `PATH`, if any.
    pub llama_cpp: Option<LlamaCppInfo>,
    /// GGUF model files discovered in standard locations.
    pub gguf_files: Vec<GgufFile>,
    /// First detected GPU (nvidia-smi or Windows WMI), if any.
    pub gpu: Option<GpuInfo>,
}

impl DetectionResult {
    /// `true` when there is no usable LLM runtime on this machine —
    /// the miner should then prompt for download.
    pub fn is_empty(&self) -> bool {
        self.ollama
            .as_ref()
            .map(|o| o.models.is_empty())
            .unwrap_or(true)
            && self.llama_cpp.is_none()
            && self.gguf_files.is_empty()
    }

    /// Best-available model selection: prefer Ollama (zero setup) over a
    /// local GGUF (manual wiring required). GPU presence is reported
    /// separately via [`DetectionResult::gpu`] and not part of the
    /// runtime selection — the inference host decides whether to offload.
    pub fn recommended_runtime(&self) -> Option<RecommendedRuntime> {
        if let Some(o) = &self.ollama {
            if !o.models.is_empty() {
                return Some(RecommendedRuntime::Ollama {
                    endpoint: o.endpoint.clone(),
                    model: o.models[0].name.clone(),
                });
            }
        }
        if !self.gguf_files.is_empty() {
            return Some(RecommendedRuntime::LocalGguf {
                path: self.gguf_files[0].path.clone(),
                size_mb: self.gguf_files[0].size_mb,
            });
        }
        None
    }
}

/// What the miner should use as its inference backend, in priority order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RecommendedRuntime {
    /// Use a locally running Ollama daemon.
    Ollama { endpoint: String, model: String },
    /// Load a GGUF file directly via llama.cpp.
    LocalGguf {
        path: std::path::PathBuf,
        size_mb: u64,
    },
}

/// Run all probes concurrently and return the aggregated result.
///
/// Takes well under two seconds typically; the longest path is the Ollama
/// HTTP probe which has a 1.5 second timeout.
pub async fn detect_all() -> anyhow::Result<DetectionResult> {
    let (ollama_res, llama_cpp_res, gguf_res, gpu_res) = tokio::join!(
        ollama::probe(),
        llama_cpp::probe(),
        gguf::scan_standard_paths(),
        gpu::probe(),
    );

    if let Err(ref e) = ollama_res {
        tracing::debug!(error = %e, "ollama probe failed (treated as not-present)");
    }
    if let Err(ref e) = llama_cpp_res {
        tracing::debug!(error = %e, "llama.cpp probe failed (treated as not-present)");
    }
    if let Err(ref e) = gpu_res {
        tracing::debug!(error = %e, "gpu probe failed (treated as not-present)");
    }

    Ok(DetectionResult {
        ollama: ollama_res.ok(),
        llama_cpp: llama_cpp_res.ok(),
        gguf_files: gguf_res.unwrap_or_default(),
        gpu: gpu_res.ok(),
    })
}
