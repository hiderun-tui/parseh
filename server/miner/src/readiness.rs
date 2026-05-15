//! Readiness report — a small JSON-serialisable snapshot of the miner's
//! state after the swarm has come up but before it enters its main event
//! loop.
//!
//! Two consumers:
//!
//!   1. Humans running `parseh-miner --show-readiness`. The binary prints
//!      one JSON object and exits 0 so monitoring scripts and curious
//!      operators can verify the node is wired correctly without tailing
//!      the structured-log stream.
//!
//!   2. The startup log line — even without `--show-readiness` we emit a
//!      single info-level summary so operators see at a glance that
//!      identity, listen addrs, peers-seen, and LLM detection all
//!      resolved. The JSON form here is the long-form counterpart.
//!
//! Field names are stable across V0.1 patch releases (monitoring scripts
//! depend on them). Adding fields is fine; removing or renaming requires
//! a CHANGELOG entry and is a V0.2 concern.

use serde::Serialize;

use parseh_llm_detect::RecommendedRuntime;
use parseh_core::NodeCapabilities;

use crate::{OpenQuorumSummary, SharedStateSnapshot};

/// One-shot readiness snapshot. Cheap to construct, cheap to serialise.
///
/// Constructed by the start path of `parseh-miner` and either printed +
/// `process::exit`-ed (when `--show-readiness` is passed) or held briefly
/// for the startup log line.
#[derive(Debug, Clone, Serialize)]
pub struct ReadinessReport {
    /// Libp2p PeerId of this miner. Stable across restarts because the
    /// underlying ed25519 keypair lives in the config dir.
    pub identity_peer_id: String,

    /// Multiaddrs the swarm is currently bound to. Order is not
    /// significant. Empty on a brand-new miner that has not yet
    /// surfaced a `NewListenAddr` event.
    pub listen_addrs: Vec<String>,

    /// Number of peers the local capability cache has heard from. Zero
    /// at startup; non-zero once peers start gossiping on
    /// `parseh.caps.v1`.
    pub known_peers: usize,

    /// Recommended local LLM runtime, if any was detected. `None` means
    /// the miner advertises non-inference capabilities only — operators
    /// can pass `--auto-download-llm` or install Ollama and restart.
    pub llm_runtime: Option<RecommendedRuntime>,

    /// What this miner *advertises* on `parseh.caps.v1`. Copied
    /// verbatim from the loaded TOML config so the readiness output
    /// reflects on-disk truth rather than runtime opinion.
    pub services_advertised: NodeCapabilities,

    /// `CARGO_PKG_VERSION` of the running binary. Mirrors `--version`
    /// so a monitoring script does not need a second exec to learn it.
    pub version: String,

    /// V0.2 SharedState surface — path + per-table counts +
    /// established-peer count + local reputation tally. Populated when
    /// the miner runs the V0.2 code paths (every V0.2.1+ build does);
    /// `None` only when the readiness report was constructed before
    /// `shared-state` was opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_state: Option<SharedStateSnapshot>,

    /// Compact summary of any currently-open `parseh-verify::Quorum`s.
    /// Empty list at startup; populated as the miner observes
    /// `JobResult` envelopes on `parseh.verify.v1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_quorums: Option<Vec<OpenQuorumSummary>>,
}
