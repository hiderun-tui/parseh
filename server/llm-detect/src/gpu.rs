//! GPU probe.
//!
//! Primary: `nvidia-smi --query-gpu=name,memory.total --format=csv,noheader`.
//! Fallback on Windows: `wmic path Win32_VideoController get Name,AdapterRAM /value`
//! (covers AMD and Intel iGPU). Apple Metal is deliberately not probed —
//! V0.1 targets Windows.

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// One detected GPU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    /// Marketing name, e.g. `NVIDIA GeForce RTX 4070`.
    pub name: String,
    /// Total VRAM in megabytes.
    pub vram_mb: u64,
    /// Where this came from — `"nvidia-smi"` or `"wmic"`.
    pub source: String,
}

pub async fn probe() -> anyhow::Result<GpuInfo> {
    match try_nvidia_smi().await {
        Ok(info) => return Ok(info),
        Err(e) => tracing::debug!(error = %e, "nvidia-smi unavailable"),
    }
    if cfg!(target_os = "windows") {
        if let Ok(info) = try_wmic().await {
            return Ok(info);
        }
    }
    Err(anyhow!("no GPU detected"))
}

async fn try_nvidia_smi() -> anyhow::Result<GpuInfo> {
    let mut cmd = Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=name,memory.total",
        "--format=csv,noheader,nounits",
    ]);
    let output = timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .context("nvidia-smi timed out")?
        .context("spawn nvidia-smi")?;

    if !output.status.success() {
        anyhow::bail!("nvidia-smi exited with {}", output.status);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout
        .lines()
        .next()
        .ok_or_else(|| anyhow!("nvidia-smi returned empty output"))?;

    // Expected format: "NVIDIA GeForce RTX 4070, 12282"
    let mut parts = first_line.splitn(2, ',');
    let name = parts
        .next()
        .ok_or_else(|| anyhow!("nvidia-smi: missing name column"))?
        .trim()
        .to_string();
    let vram_mb: u64 = parts
        .next()
        .ok_or_else(|| anyhow!("nvidia-smi: missing memory column"))?
        .trim()
        .parse()
        .context("parse nvidia-smi memory.total")?;

    Ok(GpuInfo {
        name,
        vram_mb,
        source: "nvidia-smi".into(),
    })
}

async fn try_wmic() -> anyhow::Result<GpuInfo> {
    let mut cmd = Command::new("wmic");
    cmd.args([
        "path",
        "Win32_VideoController",
        "get",
        "Name,AdapterRAM",
        "/value",
    ]);
    let output = timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .context("wmic timed out")?
        .context("spawn wmic")?;

    if !output.status.success() {
        anyhow::bail!("wmic exited with {}", output.status);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // wmic /value emits `Key=Value` lines separated by blank lines. We pull
    // the first record's Name + AdapterRAM (bytes).
    let mut name: Option<String> = None;
    let mut adapter_ram: Option<u64> = None;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            // End of a record. Stop if we've got a complete one.
            if name.is_some() && adapter_ram.is_some() {
                break;
            }
            name = None;
            adapter_ram = None;
            continue;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            name = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("AdapterRAM=") {
            adapter_ram = v.parse::<u64>().ok();
        }
    }

    let name = name.ok_or_else(|| anyhow!("wmic: no Name field"))?;
    let bytes = adapter_ram.ok_or_else(|| anyhow!("wmic: no AdapterRAM field"))?;
    Ok(GpuInfo {
        name,
        vram_mb: bytes / (1024 * 1024),
        source: "wmic".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_does_not_panic() {
        // On a CI runner without a GPU, this just returns Err. We assert
        // only that the call completes.
        let _ = probe().await;
    }
}
