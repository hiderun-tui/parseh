//! Exit-peer selection.
//!
//! # Selection algorithm (V0.2.5)
//!
//! 1. Snapshot every peer the registry currently knows about.
//! 2. Keep only peers that advertise `Relay` and are *ready* with
//!    external-internet capability. In V0.2.5 the canonical predicate is
//!    `PeerRegistry::ready_peers_with_external_internet()` (added by the
//!    parallel `feat/peer-identity-registry-v0-2-5` agent); until that
//!    method lands on `main` we approximate via [`ExitCandidate::from_advertisement`]
//!    which treats any `Relay`-capable advertisement as a candidate exit.
//! 3. Sort by bandwidth descending, with `peer_id` as a deterministic
//!    tiebreaker. (Reputation-band tiebreak lands when reputation is
//!    plumbed through the registry — also V0.2.5 work in the parallel
//!    agent.)
//! 4. Return the highest-ranked candidate; on failover, return the
//!    next-highest excluding the failed `PeerId`.
//!
//! # Why not random selection
//!
//! A random pick would distribute load uniformly across exits but maps
//! every client to a wide footprint of "who saw which target". A
//! bandwidth-weighted preference (with deterministic tiebreaks) lets
//! clients exhibit a smaller mean exit footprint, which is the better
//! single-hop privacy posture — short of multi-hop circuits, which are
//! V0.3+ and add latency we do not yet have a budget for.
//!
//! # Future work
//!
//! - Reputation-band tiebreak once the registry exposes it.
//! - Sticky-exit option for long-lived sessions (browser TLS connection
//!   reuse) to reduce target-correlation across rapid reconnects.
//! - Jurisdictional preference (avoid exits in the same jurisdiction as
//!   the censor's regulatory reach).

use std::sync::Arc;

use libp2p::{Multiaddr, PeerId};
use parseh_core::peer_registry::{CapabilityAdvertisement, ServiceKind};
use parseh_core::PeerRegistry;

/// A peer that could serve as a tunnel exit, with the fields the router
/// needs to rank and dial it.
///
/// This is a router-local view; it is intentionally NOT the same struct
/// as `PeerIdentity` (added by the parallel V0.2.5 agent) — that one is
/// a registry-level type. We project from `CapabilityAdvertisement` into
/// this view so the router has a single ranking shape regardless of
/// which advertisement variant the registry persists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitCandidate {
    /// libp2p identity of the candidate exit.
    pub peer_id: PeerId,
    /// Multiaddr the swarm should dial to reach this peer.
    pub network_address: Multiaddr,
    /// Self-reported external bandwidth in Mbps. Used as the primary
    /// ranking key. `0` for peers that did not advertise a relay
    /// capability (we conservatively rank them last but still keep them
    /// as last-resort candidates).
    pub bandwidth_mbps_external: u32,
    /// Whether this candidate is known to have external-internet
    /// capability. For V0.2.5 this is `true` for any peer advertising
    /// `ServiceKind::Relay`; once `PeerIdentity::has_external_internet`
    /// lands in the registry this projection narrows to the strict
    /// predicate.
    pub has_external_internet: bool,
}

impl ExitCandidate {
    /// Project a [`CapabilityAdvertisement`] into an [`ExitCandidate`],
    /// returning `None` if the peer does not advertise a relay service.
    ///
    /// This is the V0.2.5 transition-period shim: when the parallel
    /// `PeerIdentity::has_external_internet` field lands on main, the
    /// projection will read it directly instead of inferring from
    /// `services`.
    pub fn from_advertisement(ad: &CapabilityAdvertisement) -> Option<Self> {
        if !ad.services.contains(&ServiceKind::Relay) {
            return None;
        }
        let bandwidth = ad.relay.as_ref().map(|r| r.bandwidth_mbps).unwrap_or(0);
        Some(Self {
            peer_id: ad.peer_id,
            network_address: ad.network_address.clone(),
            bandwidth_mbps_external: bandwidth,
            has_external_internet: true,
        })
    }
}

/// Ranking + selection of exit peers. Holds an `Arc<PeerRegistry>` so the
/// same selector can be cloned cheaply across SOCKS5 accept tasks.
#[derive(Clone)]
pub struct ExitSelector {
    registry: Arc<PeerRegistry>,
}

impl ExitSelector {
    /// Wrap a shared registry. The `Arc` lets the selector be cloned
    /// into per-connection tasks without an extra indirection.
    pub fn new(registry: Arc<PeerRegistry>) -> Self {
        Self { registry }
    }

    /// Snapshot the registry and return the ranked candidate list.
    ///
    /// Made public so callers (e.g. the `parseh-tunnel status` CLI
    /// subcommand) can inspect the same set the router would pick from.
    pub fn ranked_candidates(&self) -> Vec<ExitCandidate> {
        let mut candidates: Vec<ExitCandidate> = self
            .registry
            .snapshot()
            .iter()
            .filter_map(ExitCandidate::from_advertisement)
            .filter(|c| c.has_external_internet)
            .collect();
        // Primary key: bandwidth descending. Tiebreaker: deterministic
        // PeerId ordering (stable across clones; not security-meaningful
        // but reproducible for tests + bug reports).
        candidates.sort_by(|a, b| {
            b.bandwidth_mbps_external
                .cmp(&a.bandwidth_mbps_external)
                .then_with(|| a.peer_id.to_bytes().cmp(&b.peer_id.to_bytes()))
        });
        candidates
    }

    /// Pick the highest-ranked exit. The `_target` parameter is reserved
    /// for V0.3+ jurisdictional preference; V0.2.5 ignores it.
    pub fn pick_exit(&self, _target: &str) -> Option<ExitCandidate> {
        self.ranked_candidates().into_iter().next()
    }

    /// Pick the next-best exit, excluding the failed one. Used by
    /// [`crate::tunnel`] when the primary exit returned a [`crate::protocol::RejectionReason`].
    pub fn failover(&self, failed: &PeerId, _target: &str) -> Option<ExitCandidate> {
        self.ranked_candidates()
            .into_iter()
            .find(|c| &c.peer_id != failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::identity::Keypair;
    use parseh_core::peer_registry::{
        CapabilityAdvertisement, RelayCapability, ServiceKind,
    };

    fn fresh_peer() -> PeerId {
        PeerId::from(Keypair::generate_ed25519().public())
    }

    fn loopback() -> Multiaddr {
        "/ip4/127.0.0.1/tcp/8421".parse().unwrap()
    }

    fn relay_ad(peer: PeerId, bandwidth_mbps: u32) -> CapabilityAdvertisement {
        CapabilityAdvertisement {
            peer_id: peer,
            version: 1,
            services: vec![ServiceKind::Relay],
            inference: None,
            relay: Some(RelayCapability {
                bandwidth_mbps,
                transport_kinds: vec![],
            }),
            storage: None,
            network_address: loopback(),
            signed_at: 1_000,
            ttl_seconds: 600,
        }
    }

    fn inference_only_ad(peer: PeerId) -> CapabilityAdvertisement {
        CapabilityAdvertisement {
            peer_id: peer,
            version: 1,
            services: vec![ServiceKind::Inference],
            inference: None,
            relay: None,
            storage: None,
            network_address: loopback(),
            signed_at: 1_000,
            ttl_seconds: 600,
        }
    }

    #[test]
    fn from_advertisement_returns_none_for_non_relay() {
        let peer = fresh_peer();
        let ad = inference_only_ad(peer);
        assert!(ExitCandidate::from_advertisement(&ad).is_none());
    }

    #[test]
    fn pick_exit_returns_highest_bandwidth() {
        let registry = Arc::new(PeerRegistry::new());
        let slow = fresh_peer();
        let fast = fresh_peer();
        registry.upsert(relay_ad(slow, 10));
        registry.upsert(relay_ad(fast, 1_000));
        let selector = ExitSelector::new(registry);
        let chosen = selector.pick_exit("whatsapp.com:443").expect("a peer");
        assert_eq!(chosen.peer_id, fast);
        assert_eq!(chosen.bandwidth_mbps_external, 1_000);
    }

    #[test]
    fn pick_exit_returns_none_when_no_relays() {
        let registry = Arc::new(PeerRegistry::new());
        registry.upsert(inference_only_ad(fresh_peer()));
        let selector = ExitSelector::new(registry);
        assert!(selector.pick_exit("example.com:443").is_none());
    }

    #[test]
    fn failover_excludes_failed_peer() {
        let registry = Arc::new(PeerRegistry::new());
        let slow = fresh_peer();
        let fast = fresh_peer();
        registry.upsert(relay_ad(slow, 10));
        registry.upsert(relay_ad(fast, 1_000));
        let selector = ExitSelector::new(registry);
        let next = selector.failover(&fast, "example.com:443").expect("a backup");
        assert_eq!(next.peer_id, slow);
    }

    #[test]
    fn ranked_candidates_is_sorted_descending_by_bandwidth() {
        let registry = Arc::new(PeerRegistry::new());
        let peers: Vec<_> = (0..5).map(|_| fresh_peer()).collect();
        let bandwidths = [50, 1_000, 10, 100, 500];
        for (p, b) in peers.iter().zip(bandwidths.iter()) {
            registry.upsert(relay_ad(*p, *b));
        }
        let selector = ExitSelector::new(registry);
        let ranked = selector.ranked_candidates();
        assert_eq!(ranked.len(), 5);
        let observed: Vec<u32> = ranked.iter().map(|c| c.bandwidth_mbps_external).collect();
        assert_eq!(observed, vec![1_000, 500, 100, 50, 10]);
    }
}
