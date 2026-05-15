//! `parseh-sdk` — cross-platform PARSEH core.
//!
//! UniFFI-friendly Rust API consumed by:
//!   - Hiderun desktop (Tauri / Rust)
//!   - Hiderun Android (Kotlin via JNI)
//!   - Hiderun iOS (Swift via FFI)
//!
//! The contract is defined in `src/parseh.udl`. UniFFI generates the
//! per-language bindings at build time (see `build.rs`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

uniffi::include_scaffolding!("parseh");

/// Returns the SDK version string.
pub fn sdk_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Returns a default client config as a JSON blob.
pub fn default_client_config() -> String {
    serde_json::to_string_pretty(&ClientConfig::default()).unwrap_or_default()
}

/// Persistent client configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// User-facing display name. Stored locally only.
    pub display_name: String,
    /// Local UI language. "en", "fa", or "auto".
    pub language: String,
    /// Bootstrap multiaddrs to use until the local cache has live peers.
    pub bootstrap: Vec<String>,
    /// PARSEH chain RPC endpoint.
    pub chain_rpc: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            display_name: "Anonymous".into(),
            language: "auto".into(),
            bootstrap: vec![],
            chain_rpc: "https://rpc.parseh.network".into(),
        }
    }
}

/// Connection lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Failed,
}

/// Snapshot returned by `Client::status`.
#[derive(Debug, Clone)]
pub struct NetworkStatus {
    pub state: ConnectionState,
    pub peers: u32,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub last_error: Option<String>,
}

/// The thin, UniFFI-exposed client object.
///
/// Today this is a state holder with no network worker behind it. In V0.1
/// it will own a tokio runtime, a libp2p swarm, and a chain RPC client.
pub struct Client {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    config: ClientConfig,
    state: ConnectionState,
    peers: u32,
    bytes_in: u64,
    bytes_out: u64,
    last_error: Option<String>,
}

impl Client {
    /// Construct a Client from a JSON config blob (see `default_client_config`).
    ///
    /// UniFFI 0.27 requires interface constructors to hand back an
    /// `Arc<Self>` so the runtime can manage cross-language reference
    /// counting. Direct Rust callers (Tauri) `clone()` the Arc as needed.
    pub fn new(config_json: String) -> Arc<Self> {
        let config: ClientConfig = serde_json::from_str(&config_json).unwrap_or_default();
        Arc::new(Self {
            inner: Mutex::new(Inner {
                config,
                state: ConnectionState::Disconnected,
                peers: 0,
                bytes_in: 0,
                bytes_out: 0,
                last_error: None,
            }),
        })
    }

    /// Start connecting. Non-blocking; status moves to `Connecting`.
    pub fn connect(&self) {
        let mut g = self.inner.lock().expect("client poisoned");
        g.state = ConnectionState::Connecting;
        // V0.1: spawn the tokio runtime + libp2p worker here.
    }

    /// Stop the worker and release resources.
    pub fn disconnect(&self) {
        let mut g = self.inner.lock().expect("client poisoned");
        g.state = ConnectionState::Disconnected;
        g.peers = 0;
    }

    /// Take a snapshot of the current network status.
    pub fn status(&self) -> NetworkStatus {
        let g = self.inner.lock().expect("client poisoned");
        NetworkStatus {
            state: g.state,
            peers: g.peers,
            bytes_in: g.bytes_in,
            bytes_out: g.bytes_out,
            last_error: g.last_error.clone(),
        }
    }
}
