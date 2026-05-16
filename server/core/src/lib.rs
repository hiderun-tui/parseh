//! `parseh-core` — types and configuration shared across all server crates.
//!
//! This crate intentionally does very little. Its job is to define the
//! data model (NodeConfig, NodeId, capabilities) once so the relay,
//! inference host, and wallet crates can pass it around without depending
//! on each other.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

pub mod peer_registry;

pub use peer_registry::{
    decode_advertisement, encode_advertisement, CapabilityAdvertisement, InferenceCapability,
    PeerIdentity, PeerRegistry, ReadinessState, RelayCapability, ReputationBand, ServiceKind,
    StorageCapability, CAPS_WIRE_VERSION,
};

/// Crate version surfaced via `parseh_core::VERSION`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A node's public identity. Today it wraps a libp2p PeerId string.
/// V0.1 will swap to a bech32-encoded PARSEH address derived from the
/// node's ed25519 chain key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeId(pub String);

/// Self-reported capabilities. The chain may slash a node that lies here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeCapabilities {
    /// True if this node will route encrypted traffic.
    pub relay: bool,
    /// True if this node will host LLM inference.
    pub inference: bool,
    /// Self-reported VRAM in megabytes. Only meaningful if `inference == true`.
    pub gpu_memory_mb: u32,
    /// Comma-separated model tags, e.g. `"llama-3.1:8b,qwen2.5:7b"`.
    pub model_tags: Vec<String>,
    /// Self-reported uplink bandwidth, megabits per second.
    pub uplink_mbps: u32,
}

/// Full node configuration. Persisted as TOML at `$PARSEH_HOME/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Display name, not protocol-significant.
    pub moniker: String,
    /// libp2p listening multiaddr (e.g. `/ip4/0.0.0.0/tcp/8421`).
    pub listen_addr: String,
    /// Bootstrap peers (multiaddrs).
    pub bootstrap: Vec<String>,
    /// Self-reported capabilities.
    pub capabilities: NodeCapabilities,
    /// PARSEH chain RPC endpoint.
    pub chain_rpc: String,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            moniker: "parseh-node".into(),
            listen_addr: "/ip4/0.0.0.0/tcp/8421".into(),
            bootstrap: vec![],
            capabilities: NodeCapabilities::default(),
            chain_rpc: "http://localhost:26657".into(),
        }
    }
}

/// Errors returned by core utilities. Crates extend with their own.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Config file could not be read or parsed.
    #[error("config: {0}")]
    Config(String),
    /// Identity key could not be loaded or generated.
    #[error("identity: {0}")]
    Identity(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_serialises() {
        let c = NodeConfig::default();
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains("parseh-node"));
        assert!(s.contains("8421"));
    }
}
