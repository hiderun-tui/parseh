//! GGUF model file discovery.
//!
//! Walks the standard locations used by Ollama, LM Studio, HuggingFace's
//! cache, and PARSEH's own model directory, returning every `*.gguf` file
//! found (capped to avoid pathological hangs on huge HF caches).

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Hard cap on number of files we'll return — guards against a wildly
/// over-populated HuggingFace cache.
const MAX_FILES: usize = 1000;

/// How deep into each scan root we descend. Three is enough to reach
/// `~/.cache/huggingface/hub/<repo>/snapshots/<rev>/<file>.gguf`.
const MAX_DEPTH: usize = 6;

/// One discovered GGUF model file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GgufFile {
    pub path: PathBuf,
    pub size_mb: u64,
    /// File mtime as a Unix timestamp (seconds). 0 if unavailable.
    pub modified_at: u64,
}

/// Roots to scan, in the order we want results aggregated.
fn standard_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".ollama").join("models").join("blobs"));
        roots.push(home.join(".parseh").join("models"));
    }
    if let Some(cache) = dirs::cache_dir() {
        roots.push(cache.join("lm-studio").join("models"));
        roots.push(cache.join("huggingface").join("hub"));
    }
    // Windows-specific Ollama install location: `%LOCALAPPDATA%\Programs\Ollama\models`.
    if let Some(local) = dirs::data_local_dir() {
        roots.push(local.join("Programs").join("Ollama").join("models"));
    }

    roots
}

/// Scan the standard model directories. Never errors — missing roots are
/// simply skipped. Returns files sorted by mtime descending.
pub async fn scan_standard_paths() -> anyhow::Result<Vec<GgufFile>> {
    let roots = standard_roots();
    // The scan is filesystem-bound; run it on the blocking pool so we don't
    // stall the async reactor on a slow disk.
    let files = tokio::task::spawn_blocking(move || scan_blocking(&roots))
        .await
        .context("spawn gguf scan task")??;
    Ok(files)
}

fn scan_blocking(roots: &[PathBuf]) -> anyhow::Result<Vec<GgufFile>> {
    let mut out: Vec<GgufFile> = Vec::new();

    for root in roots {
        if !root.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(root)
            .max_depth(MAX_DEPTH)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if out.len() >= MAX_FILES {
                tracing::warn!(
                    "gguf scan hit cap of {} files; truncating results",
                    MAX_FILES
                );
                break;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            if !is_gguf(entry.path()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let size_mb = meta.len() / (1024 * 1024);
            let modified_at = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.push(GgufFile {
                path: entry.path().to_path_buf(),
                size_mb,
                modified_at,
            });
        }
        if out.len() >= MAX_FILES {
            break;
        }
    }

    // Newest first — likely the user's most relevant download.
    out.sort_by_key(|f| std::cmp::Reverse(f.modified_at));
    Ok(out)
}

fn is_gguf(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("gguf"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn scan_picks_up_a_fake_gguf() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("nested").join("model.gguf");
        std::fs::create_dir_all(fake.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&fake).unwrap();
        // ~1 MiB of zeroes so size_mb >= 1.
        f.write_all(&vec![0u8; 1_500_000]).unwrap();

        let roots = vec![tmp.path().to_path_buf()];
        let files = tokio::task::spawn_blocking(move || scan_blocking(&roots))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, fake);
        assert!(files[0].size_mb >= 1);
    }

    #[tokio::test]
    async fn nonexistent_root_is_silent() {
        let roots = vec![PathBuf::from("/definitely/not/a/real/path/parseh-xxx")];
        let files = tokio::task::spawn_blocking(move || scan_blocking(&roots))
            .await
            .unwrap()
            .unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn is_gguf_is_case_insensitive() {
        assert!(is_gguf(Path::new("a.gguf")));
        assert!(is_gguf(Path::new("a.GGUF")));
        assert!(!is_gguf(Path::new("a.bin")));
        assert!(!is_gguf(Path::new("a")));
    }
}
