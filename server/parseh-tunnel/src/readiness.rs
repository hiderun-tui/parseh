//! Readiness / status snapshot used by `parseh-tunnel status`.
//!
//! The shape is deliberately small. We mirror the miner's
//! `ReadinessReport` style (JSON-serialisable, one line of `info!` plus
//! an optional full dump) so an operator running both binaries gets
//! similar diagnostic output.

use serde::Serialize;

use crate::router::ExitCandidate;

/// One row in the exit-candidate snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct ExitRow {
    /// Stringified PeerId.
    pub peer_id: String,
    /// Multiaddr we would dial.
    pub network_address: String,
    /// Self-reported external bandwidth in Mbps.
    pub bandwidth_mbps_external: u32,
    /// True for V0.2.5 once `PeerIdentity::has_external_internet` lands;
    /// today this is `true` for every relay-advertising peer.
    pub has_external_internet: bool,
}

impl From<&ExitCandidate> for ExitRow {
    fn from(c: &ExitCandidate) -> Self {
        Self {
            peer_id: c.peer_id.to_string(),
            network_address: c.network_address.to_string(),
            bandwidth_mbps_external: c.bandwidth_mbps_external,
            has_external_internet: c.has_external_internet,
        }
    }
}

/// Full status payload. Stable across V0.2.5 patch releases.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    /// Crate version of the running binary.
    pub version: String,
    /// Stringified local PeerId. `None` if no identity has been built
    /// yet (status was queried before swarm bring-up).
    pub local_peer_id: Option<String>,
    /// SOCKS5 listener address the binary would bind / has bound.
    pub socks5_listen: String,
    /// All known exit candidates, ranked by [`crate::router::ExitSelector`].
    pub ranked_exits: Vec<ExitRow>,
    /// Honest disclaimer surfaced in machine-readable output, so any
    /// downstream tool consuming the JSON sees the same caveat as the
    /// README. Stable string; do not localise.
    pub disclaimer: &'static str,
}

/// The single source of truth for the disclaimer string. We surface it
/// in JSON status output, in `--help`, and in the README, so a user who
/// touches the binary in any way sees the same words.
pub const DISCLAIMER: &str =
    "V0.2.5 scaffold. Single-hop tunnel; reveals target host to exit operator. \
     No anonymity claim. No censorship-resistance claim until V0.2.5 \
     hostile-network measurement data exists. See README.";
