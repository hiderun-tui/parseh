//! llama.cpp binary probe.
//!
//! Looks for a llama.cpp executable on `PATH` under any of its common names,
//! then captures the binary path and `--version` output.

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// Common llama.cpp binary names across releases & platforms.
/// Order matters: prefer the server binary because that's what the miner
/// will spawn long-term.
const CANDIDATES: &[&str] = &[
    "llama-server",
    "llama-cli",
    "llama.cpp",
    "server.exe",
    "llama-server.exe",
];

/// Time budget for `llama-* --version` execution.
const VERSION_TIMEOUT: Duration = Duration::from_secs(2);

/// Discovered llama.cpp binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlamaCppInfo {
    pub binary_path: PathBuf,
    /// `--version` output, trimmed. Empty if the binary refused to print one.
    pub version: String,
}

pub async fn probe() -> anyhow::Result<LlamaCppInfo> {
    for name in CANDIDATES {
        if let Ok(path) = which::which(name) {
            let version = capture_version(&path).await.unwrap_or_default();
            return Ok(LlamaCppInfo {
                binary_path: path,
                version,
            });
        }
    }
    Err(anyhow!("no llama.cpp binary found on PATH"))
}

async fn capture_version(path: &std::path::Path) -> anyhow::Result<String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version");
    let fut = cmd.output();
    let output = timeout(VERSION_TIMEOUT, fut)
        .await
        .context("llama.cpp --version timed out")?
        .context("spawn llama.cpp --version")?;

    // llama.cpp historically writes its banner to stderr.
    let mut s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        s = String::from_utf8_lossy(&output.stderr).trim().to_string();
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_binary_is_err() {
        // On a CI runner without llama.cpp installed, this must not panic.
        match probe().await {
            Ok(info) => {
                // If by chance llama-cli/server *is* installed, sanity-check the result.
                assert!(info.binary_path.exists());
            }
            Err(e) => {
                assert!(e.to_string().contains("no llama.cpp"));
            }
        }
    }
}
