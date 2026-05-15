//! `parseh-llm-downloader` — consent-gated LLM model download.
//!
//! This is the ONLY external HTTP egress in the PARSEH miner. The download
//! is gated behind explicit user consent (the caller obtains consent via UI
//! and passes a [`Consent`] token).
//!
//! ## security model summary
//!
//! - The crate exposes **no** function that performs network I/O without a
//!   [`Consent`] token. [`Consent`] is a zero-sized type whose constructor is
//!   private; it can only be produced by [`Consent::obtain`], which forces
//!   the caller to await a user-facing prompt and check the returned bool.
//! - All downloads stream chunk-by-chunk so we never buffer ~640 MB in RAM.
//! - The downloaded file's SHA-256 is computed during streaming and compared
//!   against a constant pinned in [`models`]. The `sha256_verified` flag on
//!   [`DownloadResult`] tells the caller whether the file matched the pinned
//!   hash. The V0.1 ships with a placeholder hash; see `models::TINYLLAMA_*`.
//! - The destination directory defaults to `~/.parseh/models/` and is created
//!   if missing.

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;

mod consent;
mod models;

pub use consent::{Consent, ConsentDenied};
pub use models::{ModelCatalog, ModelSpec, MODELS, TINYLLAMA_1_1B_Q4_K_M};

/// Progress callback type. Called with `(bytes_downloaded, total_bytes)`.
///
/// The callback runs on the download task's tokio thread, so it should be
/// cheap (a channel send, an atomic store, a log line). Heavy work belongs
/// elsewhere.
pub type ProgressFn = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// Result of a [`download_model`] call.
///
/// `Serialize` only — `Deserialize` is intentionally omitted because the
/// nested [`ModelSpec`] holds `&'static str` fields (the catalog lives in
/// `.rodata`) and those cannot be deserialized into. Downstream code that
/// needs to round-trip this struct through IPC should map it to an owned
/// representation at the boundary.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadResult {
    /// The spec that was downloaded (cloned from the catalog).
    pub model: ModelSpec,
    /// Absolute path the file landed at.
    pub path: PathBuf,
    /// Bytes written to disk (matches the streamed length).
    pub bytes_written: u64,
    /// Lowercase hex SHA-256 of the bytes written.
    pub sha256: String,
    /// `true` iff `sha256 == model.expected_sha256`.
    pub sha256_verified: bool,
}

/// Download the given model to the user's `~/.parseh/models/` directory.
///
/// `consent` is required and MUST come from a UI confirmation step. The
/// caller is responsible for showing a dialog explaining:
///   - what is being downloaded (model name, size),
///   - where it goes (the resolved path),
///   - that this is the only external HTTP request the miner makes,
///   - the SHA-256 verification step.
///
/// After consent, this function streams the file to disk, computing SHA-256
/// in parallel, and verifies against the spec's expected hash.
///
/// If the destination file already exists with the expected byte length, it
/// is re-hashed and returned without re-downloading. This is the cheap
/// idempotency case; a real resume-from-partial implementation is V0.2 work
/// (see crate-level docs).
pub async fn download_model(
    model: &ModelSpec,
    _consent: Consent,
    progress: Option<ProgressFn>,
) -> Result<DownloadResult> {
    let dest_dir = default_models_dir()?;
    fs::create_dir_all(&dest_dir)
        .await
        .with_context(|| format!("cannot create models dir: {}", dest_dir.display()))?;

    let dest = dest_dir.join(model.filename);

    // Fast path: file already on disk with the expected length. Re-hash and
    // return; do not redownload.
    if let Ok(meta) = fs::metadata(&dest).await {
        if meta.len() == model.expected_size_bytes {
            let computed = compute_sha256(&dest).await?;
            let verified = computed == model.expected_sha256;
            tracing::info!(
                path = %dest.display(),
                bytes = meta.len(),
                verified,
                "model already present on disk; skipping download"
            );
            return Ok(DownloadResult {
                model: *model,
                path: dest,
                bytes_written: meta.len(),
                sha256: computed,
                sha256_verified: verified,
            });
        }
    }

    download_to_path(model, &dest, progress).await
}

/// Download `model` directly to `dest`, regardless of `~/.parseh/models/`.
///
/// Used by tests (with a wiremock URL written into a `ModelSpec`) and by
/// callers that need a custom destination. Still requires a [`Consent`].
pub async fn download_model_to(
    model: &ModelSpec,
    dest: &Path,
    _consent: Consent,
    progress: Option<ProgressFn>,
) -> Result<DownloadResult> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("cannot create parent dir: {}", parent.display()))?;
    }
    download_to_path(model, dest, progress).await
}

/// Inner streaming download. Caller is responsible for consent + dirs.
async fn download_to_path(
    model: &ModelSpec,
    dest: &Path,
    progress: Option<ProgressFn>,
) -> Result<DownloadResult> {
    tracing::info!(
        url = model.url,
        path = %dest.display(),
        expected_size = model.expected_size_bytes,
        "starting model download"
    );

    let response = reqwest::Client::new()
        .get(model.url)
        .send()
        .await
        .context("HTTP request failed")?
        .error_for_status()
        .context("HTTP status not 2xx")?;

    let total = response
        .content_length()
        .unwrap_or(model.expected_size_bytes);
    let mut downloaded: u64 = 0;
    let mut hasher = Sha256::new();

    let mut file = fs::File::create(dest)
        .await
        .with_context(|| format!("cannot create dest file: {}", dest.display()))?;

    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("stream error during download")?;
        file.write_all(&chunk)
            .await
            .context("write to dest file failed")?;
        hasher.update(&chunk);
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if let Some(cb) = progress.as_ref() {
            cb(downloaded, total);
        }
    }
    file.flush().await.context("final flush failed")?;
    drop(file);

    let computed = format!("{:x}", hasher.finalize());
    let verified = computed == model.expected_sha256;

    if !verified {
        // V0.1: the catalog ships with a placeholder hash so the first real
        // download is *expected* to mismatch. Once the hash is pinned this
        // becomes a hard error path the caller must surface to the user
        // (file should be deleted and the download retried from a mirror).
        tracing::warn!(
            expected = model.expected_sha256,
            computed = %computed,
            "downloaded file SHA-256 mismatch; in V0.1 the placeholder hash is expected to mismatch"
        );
    } else {
        tracing::info!(
            sha256 = %computed,
            bytes = downloaded,
            "download complete and SHA-256 verified"
        );
    }

    Ok(DownloadResult {
        model: *model,
        path: dest.to_path_buf(),
        bytes_written: downloaded,
        sha256: computed,
        sha256_verified: verified,
    })
}

/// Resolve `~/.parseh/models/`. Public so callers can pre-create or display.
pub fn default_models_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not resolve home dir")?;
    Ok(home.join(".parseh").join("models"))
}

/// Compute SHA-256 of a file on disk (used for the already-on-disk fast path).
async fn compute_sha256(path: &Path) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut file = fs::File::open(path)
        .await
        .with_context(|| format!("cannot open file for hashing: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await.context("read during hashing failed")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_models_dir_lives_under_dot_parseh() {
        let dir = default_models_dir().expect("home should resolve in test env");
        let s = dir.to_string_lossy();
        assert!(s.ends_with(".parseh/models") || s.ends_with(".parseh\\models"));
    }

    #[tokio::test]
    async fn compute_sha256_matches_known_vector() {
        // SHA-256 of the empty file is the well-known constant.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty");
        tokio::fs::write(&p, b"").await.unwrap();
        let h = compute_sha256(&p).await.unwrap();
        assert_eq!(
            h,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[tokio::test]
    async fn compute_sha256_matches_short_string() {
        // SHA-256("abc") = ba7816bf...
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc");
        tokio::fs::write(&p, b"abc").await.unwrap();
        let h = compute_sha256(&p).await.unwrap();
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
