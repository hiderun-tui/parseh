//! TOML config for parseh-miner.
//!
//! The config file lives at `<config_dir>/miner.toml` and is created
//! with sensible defaults on first run.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use parseh_core::NodeCapabilities;

/// Top-level miner config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinerConfig {
    /// Display name (not protocol-significant).
    pub moniker: String,
    /// Service capabilities this miner advertises.
    #[serde(default)]
    pub capabilities: NodeCapabilities,
    /// Bootstrap peer multiaddrs.
    #[serde(default)]
    pub bootstrap: Vec<String>,
    /// PARSEH chain RPC endpoint (used when wallet integration lands in V0.1).
    #[serde(default = "default_chain_rpc")]
    pub chain_rpc: String,
}

fn default_chain_rpc() -> String { "https://rpc.parseh.network".into() }

impl Default for MinerConfig {
    fn default() -> Self {
        Self {
            moniker: hostname_or("parseh-miner"),
            capabilities: NodeCapabilities {
                relay: true,
                inference: false,
                gpu_memory_mb: 0,
                model_tags: vec![],
                uplink_mbps: 0,
            },
            bootstrap: vec![],
            chain_rpc: default_chain_rpc(),
        }
    }
}

impl MinerConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let s = fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: MinerConfig =
            toml::from_str(&s).with_context(|| format!("parse config {}", path.display()))?;
        Ok(cfg)
    }
}

/// Write a commented default TOML to `path`, refusing to clobber.
pub fn write_default(path: &Path) -> Result<()> {
    if path.exists() {
        anyhow::bail!("refusing to overwrite existing config at {}", path.display());
    }
    let default = MinerConfig::default();
    let body = format!(
        r#"# PARSEH miner configuration
# Edit any value, save the file, and restart the miner.

# Display name. Shown to other peers via the Identify protocol. Not signed.
moniker = "{moniker}"

# PARSEH chain RPC endpoint. Used by the wallet to query balance and submit
# txs. Override to a self-hosted node if you don't trust the public endpoint.
chain_rpc = "{rpc}"

# Bootstrap peers (libp2p multiaddrs). Empty list = wait for inbound.
bootstrap = [
  # "/dns/seed.parseh.network/tcp/8421/p2p/12D3KooW...",
]

[capabilities]
# Will you route encrypted traffic on behalf of users behind a national filter?
relay = {relay}

# Will you serve LLM inference jobs? Requires GPU; capability is auto-
# disabled if gpu_memory_mb == 0.
inference = {inference}

# Self-reported VRAM in MB. The bigger this is, the bigger the models you
# can serve. The on-chain audit catches lies (redundant execution).
gpu_memory_mb = {gpu}

# Comma-separated model tags you can run, e.g. ["qwen2.5:7b", "llama-3.1:8b"].
model_tags = []

# Your uplink in megabits per second. Used by the router to size relay jobs.
uplink_mbps = {uplink}
"#,
        moniker   = default.moniker,
        rpc       = default.chain_rpc,
        relay     = default.capabilities.relay,
        inference = default.capabilities.inference,
        gpu       = default.capabilities.gpu_memory_mb,
        uplink    = default.capabilities.uplink_mbps,
    );
    fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Resolve the OS-conventional config directory for parseh-miner.
pub fn default_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("parseh"))
}

fn hostname_or(fallback: &str) -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| fallback.into())
}
