//! Candle-based LLM runtime — V0.1 LOAD-ONLY proof-of-concept.
//!
//! Loads a quantised GGUF model and verifies it parses. Does NOT yet
//! implement the full inference loop — that's V0.2. The purpose today
//! is to prove the binary can advertise inference capability without
//! Ollama being installed.

#![cfg(feature = "candle")]

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelInfo {
    pub path: PathBuf,
    pub format: ModelFormat,
    pub size_bytes: u64,
    pub n_parameters: Option<u64>,
    pub context_size: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelFormat {
    Gguf,
    Safetensors,
    Unknown,
}

/// Verify a model file loads. Returns metadata; does NOT actually run inference.
pub async fn verify_model_loads(path: &Path) -> Result<ModelInfo> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("cannot stat model file: {}", path.display()))?
        .len();

    let format = detect_format(path)?;

    match format {
        ModelFormat::Gguf => verify_gguf_loads(path, size).await,
        ModelFormat::Safetensors => verify_safetensors_loads(path, size).await,
        ModelFormat::Unknown => anyhow::bail!("unrecognised model format: {}", path.display()),
    }
}

fn detect_format(path: &Path) -> Result<ModelFormat> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    Ok(match ext.as_str() {
        "gguf" => ModelFormat::Gguf,
        "safetensors" => ModelFormat::Safetensors,
        _ => ModelFormat::Unknown,
    })
}

async fn verify_gguf_loads(path: &Path, size: u64) -> Result<ModelInfo> {
    use candle_core::quantized::gguf_file;

    let path_owned = path.to_path_buf();

    // Read GGUF header on a blocking thread (file I/O).
    let metadata = tokio::task::spawn_blocking(move || -> Result<(u64, Option<u32>)> {
        let mut file = std::fs::File::open(&path_owned)
            .with_context(|| format!("open GGUF file: {}", path_owned.display()))?;
        let content = gguf_file::Content::read(&mut file)
            .with_context(|| format!("parse GGUF header: {}", path_owned.display()))?;

        // Extract useful metadata: parameter count, context size.
        // NB: in candle 0.7 the field is `tensor_infos`, keyed by tensor name.
        let n_params: u64 = content
            .tensor_infos
            .iter()
            .map(|(_, info)| info.shape.dims().iter().product::<usize>() as u64)
            .sum();

        let context_size = content
            .metadata
            .get("llama.context_length")
            .or_else(|| content.metadata.get("general.context_length"))
            .and_then(|v| match v {
                gguf_file::Value::U32(n) => Some(*n),
                gguf_file::Value::U64(n) => Some(*n as u32),
                _ => None,
            });

        Ok((n_params, context_size))
    })
    .await
    .context("GGUF header read task panicked")??;

    let (n_params, context_size) = metadata;

    Ok(ModelInfo {
        path: path.to_path_buf(),
        format: ModelFormat::Gguf,
        size_bytes: size,
        n_parameters: Some(n_params),
        context_size,
    })
}

async fn verify_safetensors_loads(path: &Path, size: u64) -> Result<ModelInfo> {
    // For V0.1, just verify the file is non-empty and has enough bytes for the
    // safetensors header-length prefix. Full parse requires the candle Var API
    // which is heavier and not needed for capability advertising.
    if size < 8 {
        anyhow::bail!("safetensors file too small: {} bytes", size);
    }

    Ok(ModelInfo {
        path: path.to_path_buf(),
        format: ModelFormat::Safetensors,
        size_bytes: size,
        n_parameters: None,
        context_size: None,
    })
}

/// Estimated tokens/sec for capability advertising. Pure heuristic — based on
/// model parameter count and assumed CPU-only inference (no GPU detection yet).
pub fn estimated_tokens_per_sec(info: &ModelInfo) -> u32 {
    match info.n_parameters {
        Some(n) if n < 2_000_000_000 => 8, // TinyLlama 1.1B: ~8 tok/s on modern CPU
        Some(n) if n < 8_000_000_000 => 3, // 7B: ~3 tok/s CPU
        Some(_) => 1,                       // larger: ~1 tok/s CPU
        None => 5,                          // unknown: conservative
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detect_format_recognises_gguf_extension() {
        let format = detect_format(Path::new("/tmp/foo.gguf")).unwrap();
        assert_eq!(format, ModelFormat::Gguf);
    }

    #[tokio::test]
    async fn detect_format_recognises_safetensors_extension() {
        let format = detect_format(Path::new("/tmp/foo.safetensors")).unwrap();
        assert_eq!(format, ModelFormat::Safetensors);
    }

    #[tokio::test]
    async fn detect_format_unknown_returns_unknown() {
        let format = detect_format(Path::new("/tmp/foo.bin")).unwrap();
        assert_eq!(format, ModelFormat::Unknown);
    }

    #[tokio::test]
    async fn verify_model_loads_rejects_missing_file() {
        let result =
            verify_model_loads(Path::new("/tmp/this-file-does-not-exist-parseh.gguf")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn verify_safetensors_rejects_tiny_files() {
        // tempfile 3.x: `with_suffix` lives on `Builder`, not `NamedTempFile`.
        let tmp = tempfile::Builder::new()
            .suffix(".safetensors")
            .tempfile()
            .unwrap();
        let result = verify_model_loads(tmp.path()).await;
        // Empty file (zero bytes) is below the 8-byte minimum → must fail.
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn estimated_tokens_per_sec_returns_sane_values() {
        let info = ModelInfo {
            path: PathBuf::new(),
            format: ModelFormat::Gguf,
            size_bytes: 0,
            n_parameters: Some(1_100_000_000),
            context_size: None,
        };
        let tps = estimated_tokens_per_sec(&info);
        assert!(
            (5..=15).contains(&tps),
            "TinyLlama-class tps estimate out of range: {tps}"
        );
    }
}
