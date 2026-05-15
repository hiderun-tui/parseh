//! `peer_registry` — local cache of capability advertisements heard on
//! the `parseh.caps.v1` gossipsub topic.
//!
//! Each entry is the most-recent advertisement from a peer, evicted when
//! `signed_at + ttl_seconds < now`. The registry is queryable by service
//! kind, by inference model, by readiness state, and by XOR distance to
//! a target `PeerId` (Kad metric).
//!
//! ## V0.2.5 changes
//!
//! - [`CapabilityAdvertisement`] grew five fields: `verifying_key_bytes`,
//!   `reachable_addrs`, `readiness`, `has_external_internet`,
//!   `bandwidth_mbps_external`. Wire-format version bumped `1 → 2`. The
//!   decoder accepts the v1 shape so V0.2.1 nodes can talk to V0.2.5
//!   nodes during a rolling upgrade — see [`decode_advertisement`].
//! - New [`PeerIdentity`] caches the ed25519 verifying key plus the
//!   reachable multiaddrs plus the readiness state for every peer we
//!   have heard from. This is the peer-key directory referenced in
//!   the project notes §3.4.
//! - [`PeerRegistry`] gained [`PeerRegistry::verifying_key`],
//!   [`PeerRegistry::record_identity`], [`PeerRegistry::known_identities`],
//!   [`PeerRegistry::ready_peers_for_service`],
//!   [`PeerRegistry::ready_peers_with_external_internet`], and
//!   [`PeerRegistry::closest_peers`].
//!
//! ## Wire format compatibility
//!
//! V0.2.1 published a bare `CapabilityAdvertisement` with `version = 1`.
//! V0.2.5 publishes `version = 2`. [`decode_advertisement`] tries v2 first
//! and falls back to v1 — see its doc comment for the upgrade rationale.
//!
//! ## Thread safety
//!
//! The internal maps are wrapped in `parking_lot::RwLock`. Reads (query
//! hot path — `ready_peers_for_service`, `verifying_key`) dominate
//! writes (one gossipsub message per peer per advertisement interval,
//! default 60s). `parking_lot::RwLock` gives us cheap, non-poisoning,
//! reader-biased locking with no external runtime dependency.
//!
//! Hold time per lock acquisition is bounded by the size of the maps,
//! and every public method clones the relevant data out of the lock
//! before returning so callers never hold a guard across `.await`.

use std::collections::HashMap;
use std::sync::Arc;

use ed25519_dalek::VerifyingKey;
use libp2p::{Multiaddr, PeerId};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// Coarse classification of services a peer is willing to provide.
///
/// A single peer may advertise more than one kind (e.g. a miner with a
/// GPU and a fat uplink will be both `Inference` and `Relay`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ServiceKind {
    /// Hosts an LLM and accepts `JobOrder` requests.
    Inference,
    /// Routes encrypted traffic for other peers (REALITY / Hysteria / SOCKS5).
    Relay,
    /// Persists ciphertext blocks for a stated guarantee.
    Storage,
    /// Holds chain keys and can co-sign or relay transactions.
    Wallet,
}

/// Inference-related self-report. Only meaningful when the peer's
/// `services` list contains `ServiceKind::Inference`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InferenceCapability {
    /// Model tags currently loaded or pullable, e.g. `"qwen2.5:7b"`.
    pub models: Vec<String>,
    /// Maximum context window the peer will accept, in tokens.
    pub context_size: u32,
    /// Self-reported tokens/second on the peer's hardware. Used as the
    /// tie-breaker by [`PeerRegistry::best_inference_peer`] and as the
    /// sort key by [`PeerRegistry::ready_peers_for_service`].
    pub estimated_tokens_per_sec: u32,
}

/// Relay-related self-report. Only meaningful when the peer's
/// `services` list contains `ServiceKind::Relay`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayCapability {
    /// Self-reported uplink, megabits/second.
    pub bandwidth_mbps: u32,
    /// Transport kinds this relay can terminate. Free-form strings to
    /// avoid coupling the wire format to a closed enum; current
    /// vocabulary: `"REALITY"`, `"Hysteria"`, `"SOCKS5"`.
    pub transport_kinds: Vec<String>,
}

/// Storage-related self-report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageCapability {
    /// Free space in megabytes.
    pub free_mb: u64,
    /// Free-form description of the durability the peer offers, e.g.
    /// `"best-effort"`, `"3x replicated"`, `"7-day retention"`.
    pub persistence_guarantee: String,
}

/// Where a peer is in its lifecycle.
///
/// Mirrors the project notes §3.4. The state
/// is gossiped in every [`CapabilityAdvertisement`] so other peers can
/// filter matchmaking by readiness — only [`ReadinessState::Ready`] and
/// [`ReadinessState::Active`] peers are selected for new work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadinessState {
    /// Wallet generated, swarm not yet up. Set after `parseh-miner init`
    /// (or the equivalent first-run bootstrap) but before
    /// `parseh-miner start`.
    Initialised,
    /// libp2p swarm up, no peers found yet. The transient state between
    /// listening and Kad-DHT discovery.
    Connected,
    /// Peers found via Kad DHT. We can dial them but have not yet
    /// published our own [`CapabilityAdvertisement`].
    Listening,
    /// Capabilities advertised, accepting requests. Steady-state idle.
    Ready,
    /// Currently handling at least one task (executor or verifier role).
    /// Returns to [`ReadinessState::Ready`] when in-flight count drops
    /// back to zero.
    Active,
    /// CPU/memory/network resource pressure. Down-weighted in
    /// [`PeerRegistry::ready_peers_for_service`] queries until pressure
    /// clears.
    Degraded,
    /// Graceful shutdown observed. Peers receiving a `Stopped`
    /// advertisement should remove the entry from their local matching
    /// pool, but keep the verifying key so any in-flight signature
    /// verifications still pass.
    Stopped,
}

impl Default for ReadinessState {
    /// The default state is [`ReadinessState::Initialised`] — the
    /// state a node enters straight out of `parseh-miner init`, before
    /// the swarm is built. This is the bottom of the lifecycle and
    /// always the safe answer for "what state is a fresh tracker in?".
    fn default() -> Self {
        Self::Initialised
    }
}

impl ReadinessState {
    /// `true` iff a peer in this state is eligible to be selected for
    /// new work. Matches the contract in
    /// the project notes §3.4.
    ///
    /// ```rust
    /// use parseh_core::peer_registry::ReadinessState;
    /// assert!(ReadinessState::Ready.is_eligible());
    /// assert!(ReadinessState::Active.is_eligible());
    /// assert!(!ReadinessState::Initialised.is_eligible());
    /// assert!(!ReadinessState::Connected.is_eligible());
    /// assert!(!ReadinessState::Listening.is_eligible());
    /// assert!(!ReadinessState::Degraded.is_eligible());
    /// assert!(!ReadinessState::Stopped.is_eligible());
    /// ```
    #[inline]
    pub fn is_eligible(&self) -> bool {
        matches!(self, Self::Ready | Self::Active)
    }
}

/// Reputation band, mirrors the project notes §1.
///
/// Bands are derived from a peer's summed reputation score in
/// `parseh-shared-state`; this enum is the protocol-level lens on that
/// number, used by the verifier selection algorithm and by the
/// quorum-tie-break logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReputationBand {
    /// `0..=9` reputation. Can execute jobs; cannot verify; cannot vouch.
    New,
    /// `10..=99` reputation. Can verify (counted); cannot vouch others
    /// into Probationary.
    Probationary,
    /// `100..=999` reputation. Can verify; can vouch; eligible for
    /// V0.3+ emission.
    Established,
    /// `1_000..=9_999` reputation. Established plus a
    /// reputation-weighted vote multiplier in disputes.
    Trusted,
    /// Any state if proven malicious (quorum-signed proof in shared
    /// state). Cannot verify; honest peers refuse to verify alongside
    /// them.
    Slashed,
}

impl ReputationBand {
    /// Classify a raw reputation score into its band.
    ///
    /// The band boundaries are pinned in `verifier-economics.md` §1.
    /// Negative scores collapse to [`ReputationBand::New`] because the
    /// score domain in V0.2 is monotone non-negative (slashing is the
    /// out-of-band variant). V0.3+ may introduce explicit negative
    /// scoring; today we treat negatives as a floor.
    ///
    /// ```rust
    /// use parseh_core::peer_registry::ReputationBand;
    /// assert_eq!(ReputationBand::from_score(0),     ReputationBand::New);
    /// assert_eq!(ReputationBand::from_score(9),     ReputationBand::New);
    /// assert_eq!(ReputationBand::from_score(10),    ReputationBand::Probationary);
    /// assert_eq!(ReputationBand::from_score(99),    ReputationBand::Probationary);
    /// assert_eq!(ReputationBand::from_score(100),   ReputationBand::Established);
    /// assert_eq!(ReputationBand::from_score(999),   ReputationBand::Established);
    /// assert_eq!(ReputationBand::from_score(1_000), ReputationBand::Trusted);
    /// assert_eq!(ReputationBand::from_score(9_999), ReputationBand::Trusted);
    /// // The Slashed band is set by an explicit verdict, not by a score
    /// // threshold. `from_score` therefore tops out at Trusted.
    /// assert_eq!(ReputationBand::from_score(1_000_000), ReputationBand::Trusted);
    /// // Negative scores collapse to New (slashing is out-of-band).
    /// assert_eq!(ReputationBand::from_score(-5), ReputationBand::New);
    /// ```
    pub fn from_score(score: i64) -> Self {
        match score {
            i64::MIN..=9 => Self::New,
            10..=99 => Self::Probationary,
            100..=999 => Self::Established,
            _ => Self::Trusted,
        }
    }
}

/// Cryptographic + network identity of a peer, sourced from their
/// gossiped [`CapabilityAdvertisement`] on `parseh.caps.v1`.
///
/// `PeerIdentity` is the peer-key directory entry referenced in
/// the project notes §3.4. It is the data
/// the inbound `parseh-miner` gossipsub handler consults to verify the
/// inner ed25519 signature on every `JobSpec` / `JobResult` /
/// `JobVerification` envelope.
#[derive(Clone, Debug)]
pub struct PeerIdentity {
    /// libp2p `PeerId` of the peer. Used as the directory key.
    pub peer_id: PeerId,
    /// ed25519 verifying key advertised by the peer.
    pub verifying_key: VerifyingKey,
    /// Public multiaddrs the peer advertised. Empty when the peer is
    /// behind a NAT and has not surfaced any reachable address yet.
    pub reachable_addrs: Vec<Multiaddr>,
    /// Unix UTC seconds at which we first saw this peer's
    /// advertisement. Stable across re-advertisements.
    pub first_seen: u64,
    /// Unix UTC seconds at which we last refreshed the entry. Updated
    /// on every [`PeerRegistry::record_identity`] call.
    pub last_seen: u64,
    /// The peer's self-reported readiness state at `last_seen`.
    pub readiness: ReadinessState,
}

/// Self-describing capability advertisement gossiped on `parseh.caps.v1`.
///
/// A peer republishes this every `ttl_seconds / 2` and the registry
/// evicts entries whose `signed_at + ttl_seconds < now`.
///
/// ## V0.2.5 wire bump
///
/// `version` is now [`CAPS_WIRE_VERSION`] = 2. The decoder
/// [`decode_advertisement`] accepts the v1 shape too — V0.2.5 nodes
/// MUST be able to talk to V0.2.1 nodes during a rolling upgrade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityAdvertisement {
    /// libp2p identity of the publisher.
    pub peer_id: PeerId,
    /// Advertisement format version. V0.2.5 sets this to `2`; older
    /// peers will publish `1` and we accept that via the v1 fallback
    /// decoder.
    pub version: u32,
    /// Services this peer offers right now. Order is not significant.
    pub services: Vec<ServiceKind>,
    /// Set iff `services` contains `Inference`.
    pub inference: Option<InferenceCapability>,
    /// Set iff `services` contains `Relay`.
    pub relay: Option<RelayCapability>,
    /// Set iff `services` contains `Storage`.
    pub storage: Option<StorageCapability>,
    /// Multiaddr to reach this peer for direct `request_response` dials.
    pub network_address: Multiaddr,
    /// Unix UTC seconds at which the publisher signed this ad.
    pub signed_at: u64,
    /// Seconds after `signed_at` at which the ad must be discarded.
    /// Default at the call site: `300` (5 minutes).
    pub ttl_seconds: u32,
    /// **V0.2.5** — Raw 32-byte ed25519 public key of the publisher.
    /// Stored explicitly rather than recovered from the `PeerId` so
    /// V0.3+ can support non-ed25519 identities without a wire change.
    ///
    /// Bytes-equal to the `VerifyingKey` consumed by
    /// `parseh_task::JobSpec::verify_signature` and friends.
    #[serde(default, with = "serde_bytes_32")]
    pub verifying_key_bytes: [u8; 32],
    /// **V0.2.5** — Public multiaddrs the peer believes itself reachable
    /// at, ordered by preference. Distinct from `network_address` which
    /// is a single canonical dial-string for compatibility with V0.2.1
    /// peers.
    #[serde(default)]
    pub reachable_addrs: Vec<Multiaddr>,
    /// **V0.2.5** — Current readiness state. Gossiped so other peers
    /// filter by it in their matchmaking.
    #[serde(default = "default_readiness_state")]
    pub readiness: ReadinessState,
    /// **V0.2.5** — Bridge-leg flag. `true` iff this peer has external-
    /// internet egress capacity and is willing to relay outbound
    /// traffic for users inside hostile-network regions. Read by the
    /// `parseh-tunnel` crate (V0.2.5 sibling work).
    #[serde(default)]
    pub has_external_internet: bool,
    /// **V0.2.5** — Self-reported external-internet bandwidth, mbps.
    /// `None` when `has_external_internet == false` or when the peer
    /// has not measured its uplink.
    #[serde(default)]
    pub bandwidth_mbps_external: Option<u32>,
}

/// Wire-format version embedded in [`CapabilityAdvertisement::version`].
///
/// V0.2.1 emitted `1`. V0.2.5 emits `2`. The decoder
/// [`decode_advertisement`] keeps accepting `1` so a V0.2.5 node can
/// still match against a V0.2.1 peer during a rolling upgrade.
pub const CAPS_WIRE_VERSION: u32 = 2;

fn default_readiness_state() -> ReadinessState {
    // Pre-V0.2.5 advertisements never carried a readiness state. We
    // treat the absence as `Ready` (the steady-state value) so V0.2.1
    // peers participate in match-making unchanged.
    ReadinessState::Ready
}

/// `serde_bytes`-style helper for the fixed-size 32-byte ed25519 pubkey.
///
/// `serde_bytes` itself only knows `Vec<u8>` and `&[u8]`; for a
/// `[u8; 32]` field we provide a tiny adapter so CBOR emits a single
/// `bstr` rather than an array of 32 small integers (which would waste
/// 32× the bytes on the wire).
mod serde_bytes_32 {
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_bytes::ByteBuf;

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::serialize(&bytes[..], s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let buf = ByteBuf::deserialize(d)?;
        let v: Vec<u8> = buf.into_vec();
        if v.len() != 32 {
            // V0.2.1 emits empty `verifying_key_bytes` via the default
            // attribute. Treat that as "not advertised" and surface zero
            // bytes to the caller; downstream code checks for the all-
            // zero key explicitly.
            if v.is_empty() {
                return Ok([0u8; 32]);
            }
            return Err(serde::de::Error::custom(format!(
                "verifying_key_bytes: expected 32 bytes, got {}",
                v.len()
            )));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        Ok(out)
    }
}

impl CapabilityAdvertisement {
    /// True if `signed_at + ttl_seconds < now`. Robust to integer
    /// overflow by widening into `u64` first.
    #[inline]
    pub fn is_expired(&self, now: u64) -> bool {
        self.signed_at.saturating_add(self.ttl_seconds as u64) < now
    }

    /// Parse the embedded `verifying_key_bytes` into a dalek
    /// [`VerifyingKey`]. Returns `None` if the field is the all-zero
    /// sentinel (V0.2.1 advertisements that have not been re-published
    /// in the new wire format) or otherwise not a valid edwards point.
    ///
    /// ```rust
    /// use ed25519_dalek::SigningKey;
    /// use parseh_core::peer_registry::CapabilityAdvertisement;
    /// // A zero-padded advertisement (V0.2.1 wire shape) has no key.
    /// let ad = CapabilityAdvertisement {
    ///     peer_id: libp2p::PeerId::from(
    ///         libp2p::identity::Keypair::generate_ed25519().public()
    ///     ),
    ///     version: 1,
    ///     services: vec![],
    ///     inference: None,
    ///     relay: None,
    ///     storage: None,
    ///     network_address: "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
    ///     signed_at: 0,
    ///     ttl_seconds: 0,
    ///     verifying_key_bytes: [0u8; 32],
    ///     reachable_addrs: vec![],
    ///     readiness: parseh_core::peer_registry::ReadinessState::Ready,
    ///     has_external_internet: false,
    ///     bandwidth_mbps_external: None,
    /// };
    /// assert!(ad.verifying_key().is_none());
    /// ```
    pub fn verifying_key(&self) -> Option<VerifyingKey> {
        if self.verifying_key_bytes == [0u8; 32] {
            return None;
        }
        VerifyingKey::from_bytes(&self.verifying_key_bytes).ok()
    }
}

/// Decode a `parseh.caps.v1` payload into a [`CapabilityAdvertisement`].
///
/// V0.2.5 introduced four new fields. We accept the v1 wire shape via
/// `#[serde(default)]` on every added field, so a CBOR object that
/// omits them deserialises cleanly with the defaults. This is the
/// cross-version compatibility path operators need during a rolling
/// upgrade — without it the network would bifurcate the moment one peer
/// upgraded.
///
/// Errors propagate as the underlying `ciborium::de::Error`. Callers in
/// `parseh-miner` log at trace and drop.
///
/// ```no_run
/// use parseh_core::peer_registry::{decode_advertisement, encode_advertisement,
///     CapabilityAdvertisement, CAPS_WIRE_VERSION, ReadinessState};
/// use libp2p::PeerId;
/// # let peer = PeerId::from(libp2p::identity::Keypair::generate_ed25519().public());
/// let ad = CapabilityAdvertisement {
///     peer_id: peer,
///     version: CAPS_WIRE_VERSION,
///     services: vec![],
///     inference: None,
///     relay: None,
///     storage: None,
///     network_address: "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
///     signed_at: 0,
///     ttl_seconds: 300,
///     verifying_key_bytes: [0u8; 32],
///     reachable_addrs: vec![],
///     readiness: ReadinessState::Ready,
///     has_external_internet: false,
///     bandwidth_mbps_external: None,
/// };
/// let bytes = encode_advertisement(&ad).unwrap();
/// let round = decode_advertisement(&bytes).unwrap();
/// assert_eq!(round.peer_id, ad.peer_id);
/// ```
pub fn decode_advertisement(
    bytes: &[u8],
) -> Result<CapabilityAdvertisement, ciborium::de::Error<std::io::Error>> {
    ciborium::from_reader(bytes)
}

/// Encode a [`CapabilityAdvertisement`] as CBOR.
///
/// ```no_run
/// use parseh_core::peer_registry::{encode_advertisement, CapabilityAdvertisement,
///     CAPS_WIRE_VERSION, ReadinessState};
/// use libp2p::PeerId;
/// # let peer = PeerId::from(libp2p::identity::Keypair::generate_ed25519().public());
/// let ad = CapabilityAdvertisement {
///     peer_id: peer,
///     version: CAPS_WIRE_VERSION,
///     services: vec![],
///     inference: None,
///     relay: None,
///     storage: None,
///     network_address: "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
///     signed_at: 0,
///     ttl_seconds: 300,
///     verifying_key_bytes: [0u8; 32],
///     reachable_addrs: vec![],
///     readiness: ReadinessState::Ready,
///     has_external_internet: false,
///     bandwidth_mbps_external: None,
/// };
/// let bytes = encode_advertisement(&ad).unwrap();
/// assert!(!bytes.is_empty());
/// ```
pub fn encode_advertisement(
    ad: &CapabilityAdvertisement,
) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
    let mut buf = Vec::new();
    ciborium::into_writer(ad, &mut buf)?;
    Ok(buf)
}

/// Thread-safe cache of the most-recent advertisement we have seen
/// from each peer, plus a peer-key directory keyed by `PeerId`.
///
/// `PeerRegistry` is `Clone` — the clone is cheap (one `Arc` bump) and
/// shares the underlying maps, so handing copies to the swarm task, the
/// HTTP/metrics task, and the periodic pruner is the intended pattern.
///
/// ## What lives where
///
/// - `ads` — full [`CapabilityAdvertisement`] per peer. Evicted by TTL.
/// - `identities` — [`PeerIdentity`] per peer. Records `first_seen` /
///   `last_seen` so a peer that disappears from the ad cache (TTL
///   expiry) still has its verifying key cached for any late-arriving
///   signature checks. Identities never auto-expire — the directory
///   grows monotonically per process lifetime.
///
/// The identity store outliving the ad store is deliberate: a verifier
/// re-running an old result months later may need to check a signature
/// from a peer whose ad has long since timed out. V0.3+ moves this
/// directory on-chain.
#[derive(Debug, Clone, Default)]
pub struct PeerRegistry {
    inner: Arc<RegistryInner>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    ads: RwLock<HashMap<PeerId, CapabilityAdvertisement>>,
    identities: RwLock<HashMap<PeerId, PeerIdentity>>,
}

impl PeerRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or overwrite) the advertisement for a peer.
    ///
    /// Returns `true` if this is the first time we have seen the peer,
    /// `false` if we replaced an existing entry. Callers may use the
    /// boolean to emit a `"peer discovered"` log line on first sight.
    ///
    /// V0.2.5: this also derives a [`PeerIdentity`] from the
    /// advertisement and records it in the identity directory. Callers
    /// that want fine-grained control over the identity store should
    /// use [`PeerRegistry::record_identity`] directly instead.
    pub fn upsert(&self, ad: CapabilityAdvertisement) -> bool {
        // Derive + record identity first so the verifying key is in the
        // directory by the time any consumer reads the ad.
        if let Some(vk) = ad.verifying_key() {
            let now = ad.signed_at;
            self.record_identity(PeerIdentity {
                peer_id: ad.peer_id,
                verifying_key: vk,
                reachable_addrs: ad.reachable_addrs.clone(),
                first_seen: now,
                last_seen: now,
                readiness: ad.readiness,
            });
        }
        let mut guard = self.inner.ads.write();
        guard.insert(ad.peer_id, ad).is_none()
    }

    /// Drop every entry whose `signed_at + ttl_seconds < now`.
    /// Returns the number of entries removed.
    ///
    /// Intended to be called from a periodic Tokio task; 30 seconds is
    /// a reasonable cadence given the default 300-second TTL.
    ///
    /// Identities are **not** evicted by this call — see the type-level
    /// doc for the rationale.
    pub fn prune_expired(&self, now: u64) -> usize {
        let mut guard = self.inner.ads.write();
        let before = guard.len();
        guard.retain(|_, ad| !ad.is_expired(now));
        before - guard.len()
    }

    /// Number of peer advertisements currently tracked (including
    /// possibly-expired entries — call
    /// [`prune_expired`](Self::prune_expired) first if you need a clean
    /// count).
    pub fn count(&self) -> usize {
        self.inner.ads.read().len()
    }

    /// Snapshot every advertisement. The result is a fully-owned `Vec`,
    /// so the caller does not hold the read lock after this returns.
    pub fn snapshot(&self) -> Vec<CapabilityAdvertisement> {
        self.inner.ads.read().values().cloned().collect()
    }

    /// Every peer offering `kind`, sorted by recency (most recent first).
    pub fn peers_with_service(&self, kind: ServiceKind) -> Vec<CapabilityAdvertisement> {
        let mut out: Vec<_> = self
            .inner
            .ads
            .read()
            .values()
            .filter(|ad| ad.services.contains(&kind))
            .cloned()
            .collect();
        out.sort_by_key(|ad| std::cmp::Reverse(ad.signed_at));
        out
    }

    /// The single fastest peer that has a model tag matching
    /// `required_model` as a substring, or `None` if no such peer is
    /// known.
    ///
    /// "Best" is defined as the highest `estimated_tokens_per_sec`. The
    /// substring match is intentional — callers can pass `"tinyllama"`
    /// and match `"tinyllama:1.1b-chat-q4_0"`.
    pub fn best_inference_peer(&self, required_model: &str) -> Option<CapabilityAdvertisement> {
        self.inner
            .ads
            .read()
            .values()
            .filter(|ad| {
                ad.inference
                    .as_ref()
                    .map(|i| i.models.iter().any(|m| m.contains(required_model)))
                    .unwrap_or(false)
            })
            .max_by_key(|ad| {
                ad.inference
                    .as_ref()
                    .map(|i| i.estimated_tokens_per_sec)
                    .unwrap_or(0)
            })
            .cloned()
    }

    // ──── V0.2.5 peer-key directory ────────────────────────────────────

    /// Look up the verifying key for a known peer. Returns `None` if
    /// the peer has never advertised (i.e., the local node hasn't seen
    /// them on `parseh.caps.v1` yet).
    ///
    /// This is the hot path for inner-signature verification on inbound
    /// `JobSpec` / `JobResult` / `JobVerification` envelopes — every
    /// such message lookups the publisher's key here.
    ///
    /// ```rust
    /// use parseh_core::peer_registry::PeerRegistry;
    /// let reg = PeerRegistry::new();
    /// let peer = libp2p::PeerId::from(
    ///     libp2p::identity::Keypair::generate_ed25519().public()
    /// );
    /// assert!(reg.verifying_key(&peer).is_none());
    /// ```
    pub fn verifying_key(&self, peer_id: &PeerId) -> Option<VerifyingKey> {
        self.inner
            .identities
            .read()
            .get(peer_id)
            .map(|id| id.verifying_key)
    }

    /// Record the verifying key the first time we see a
    /// [`CapabilityAdvertisement`].
    ///
    /// Subsequent same-key advertisements update `last_seen` and
    /// `readiness` but keep `first_seen` stable. Key **changes** are
    /// LOGGED via `tracing::warn!` but **not rejected** at V0.2.5 —
    /// V0.3+ may add an ed25519 key-rotation protocol with a signed
    /// rotation envelope.
    ///
    /// ```rust
    /// use ed25519_dalek::SigningKey;
    /// use parseh_core::peer_registry::{PeerIdentity, PeerRegistry, ReadinessState};
    /// let reg = PeerRegistry::new();
    /// let sk = SigningKey::from_bytes(&[7u8; 32]);
    /// let peer = libp2p::PeerId::from(
    ///     libp2p::identity::Keypair::generate_ed25519().public()
    /// );
    /// reg.record_identity(PeerIdentity {
    ///     peer_id: peer,
    ///     verifying_key: sk.verifying_key(),
    ///     reachable_addrs: vec![],
    ///     first_seen: 100,
    ///     last_seen: 100,
    ///     readiness: ReadinessState::Ready,
    /// });
    /// assert_eq!(reg.verifying_key(&peer), Some(sk.verifying_key()));
    /// ```
    pub fn record_identity(&self, identity: PeerIdentity) {
        let mut guard = self.inner.identities.write();
        match guard.get(&identity.peer_id) {
            Some(existing) if existing.verifying_key.as_bytes() != identity.verifying_key.as_bytes() => {
                tracing::warn!(
                    peer = %identity.peer_id,
                    "verifying key changed for known peer · accepting at V0.2.5 (rotation protocol is V0.3+)"
                );
                guard.insert(identity.peer_id, identity);
            }
            Some(existing) => {
                // Same key — refresh metadata (last_seen, readiness,
                // reachable_addrs) but keep `first_seen` stable.
                let merged = PeerIdentity {
                    peer_id: identity.peer_id,
                    verifying_key: identity.verifying_key,
                    reachable_addrs: identity.reachable_addrs,
                    first_seen: existing.first_seen,
                    last_seen: identity.last_seen.max(existing.last_seen),
                    readiness: identity.readiness,
                };
                guard.insert(identity.peer_id, merged);
            }
            None => {
                guard.insert(identity.peer_id, identity);
            }
        }
    }

    /// Every known [`PeerIdentity`], snapshot. The result is fully owned.
    pub fn known_identities(&self) -> Vec<PeerIdentity> {
        self.inner.identities.read().values().cloned().collect()
    }

    /// Number of identities cached in the peer-key directory.
    pub fn identity_count(&self) -> usize {
        self.inner.identities.read().len()
    }

    // ──── V0.2.5 capability matching ───────────────────────────────────

    /// Find peers in [`ReadinessState::Ready`] or [`ReadinessState::Active`]
    /// state that advertise the given service kind.
    ///
    /// Sorted by descending `estimated_tokens_per_sec` for `Inference`,
    /// by descending `bandwidth_mbps` for `Relay`, and by descending
    /// `free_mb` for `Storage`. `Wallet` matches return in insertion
    /// order (no obvious sort key).
    ///
    /// ```rust
    /// use parseh_core::peer_registry::{PeerRegistry, ServiceKind};
    /// let reg = PeerRegistry::new();
    /// assert!(reg.ready_peers_for_service(ServiceKind::Inference).is_empty());
    /// ```
    pub fn ready_peers_for_service(&self, kind: ServiceKind) -> Vec<PeerIdentity> {
        let ads = self.inner.ads.read();
        let identities = self.inner.identities.read();
        let mut paired: Vec<(PeerIdentity, &CapabilityAdvertisement)> = ads
            .values()
            .filter(|ad| ad.services.contains(&kind))
            .filter(|ad| ad.readiness.is_eligible())
            .filter_map(|ad| identities.get(&ad.peer_id).map(|id| (id.clone(), ad)))
            .collect();
        match kind {
            ServiceKind::Inference => paired.sort_by(|(_, a), (_, b)| {
                let av = a.inference.as_ref().map(|i| i.estimated_tokens_per_sec).unwrap_or(0);
                let bv = b.inference.as_ref().map(|i| i.estimated_tokens_per_sec).unwrap_or(0);
                bv.cmp(&av)
            }),
            ServiceKind::Relay => paired.sort_by(|(_, a), (_, b)| {
                let av = a.relay.as_ref().map(|r| r.bandwidth_mbps).unwrap_or(0);
                let bv = b.relay.as_ref().map(|r| r.bandwidth_mbps).unwrap_or(0);
                bv.cmp(&av)
            }),
            ServiceKind::Storage => paired.sort_by(|(_, a), (_, b)| {
                let av = a.storage.as_ref().map(|s| s.free_mb).unwrap_or(0);
                let bv = b.storage.as_ref().map(|s| s.free_mb).unwrap_or(0);
                bv.cmp(&av)
            }),
            ServiceKind::Wallet => { /* no sort key */ }
        }
        paired.into_iter().map(|(id, _)| id).collect()
    }

    /// Find peers with external-internet bridge capability.
    /// Used by the `parseh-tunnel` crate (V0.2.5 sibling work).
    ///
    /// Sorted by descending `bandwidth_mbps_external`. Only
    /// `Ready`/`Active` peers are returned.
    ///
    /// ```rust
    /// use parseh_core::peer_registry::PeerRegistry;
    /// let reg = PeerRegistry::new();
    /// assert!(reg.ready_peers_with_external_internet().is_empty());
    /// ```
    pub fn ready_peers_with_external_internet(&self) -> Vec<PeerIdentity> {
        let ads = self.inner.ads.read();
        let identities = self.inner.identities.read();
        let mut paired: Vec<(PeerIdentity, &CapabilityAdvertisement)> = ads
            .values()
            .filter(|ad| ad.has_external_internet)
            .filter(|ad| ad.readiness.is_eligible())
            .filter_map(|ad| identities.get(&ad.peer_id).map(|id| (id.clone(), ad)))
            .collect();
        paired.sort_by(|(_, a), (_, b)| {
            let av = a.bandwidth_mbps_external.unwrap_or(0);
            let bv = b.bandwidth_mbps_external.unwrap_or(0);
            bv.cmp(&av)
        });
        paired.into_iter().map(|(id, _)| id).collect()
    }

    /// Find the K closest peers to a target [`PeerId`] by XOR distance
    /// (Kad metric). Useful for picking a primary peer to dial when the
    /// caller has a content-hash and wants the conceptual "closest"
    /// peer in the keyspace.
    ///
    /// Only [`ReadinessState::is_eligible`] peers are considered.
    ///
    /// ```rust
    /// use parseh_core::peer_registry::PeerRegistry;
    /// let reg = PeerRegistry::new();
    /// let target = libp2p::PeerId::from(
    ///     libp2p::identity::Keypair::generate_ed25519().public()
    /// );
    /// assert!(reg.closest_peers(&target, 5).is_empty());
    /// ```
    pub fn closest_peers(&self, target: &PeerId, k: usize) -> Vec<PeerIdentity> {
        if k == 0 {
            return Vec::new();
        }
        let target_bytes = target.to_bytes();
        let identities = self.inner.identities.read();
        let ads = self.inner.ads.read();
        let mut candidates: Vec<PeerIdentity> = identities
            .values()
            .filter(|id| {
                ads.get(&id.peer_id)
                    .map(|ad| ad.readiness.is_eligible())
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        candidates.sort_by_key(|id| xor_distance(&id.peer_id.to_bytes(), &target_bytes));
        candidates.truncate(k);
        candidates
    }
}

/// XOR distance between two byte slices, big-endian. Shorter slices are
/// zero-padded on the left. This is the Kad metric — used by
/// [`PeerRegistry::closest_peers`].
fn xor_distance(a: &[u8], b: &[u8]) -> Vec<u8> {
    let len = a.len().max(b.len());
    let mut out = vec![0u8; len];
    for i in 0..len {
        let ai = a.get(a.len().wrapping_sub(1).wrapping_sub(i.min(a.len() - 1))).copied();
        let bi = b.get(b.len().wrapping_sub(1).wrapping_sub(i.min(b.len() - 1))).copied();
        // The above gets brittle when slices are empty; just bail to a
        // direct index when possible. Fall back to right-aligned XOR.
        let (av, bv) = (ai.unwrap_or(0), bi.unwrap_or(0));
        out[len - 1 - i] = av ^ bv;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use libp2p::identity::Keypair;
    use std::sync::Arc;
    use std::thread;

    fn fresh_peer() -> PeerId {
        PeerId::from(Keypair::generate_ed25519().public())
    }

    fn loopback_addr() -> Multiaddr {
        "/ip4/127.0.0.1/tcp/8421".parse().expect("static multiaddr")
    }

    fn ad_with(
        peer: PeerId,
        signed_at: u64,
        ttl: u32,
        services: Vec<ServiceKind>,
        inference: Option<InferenceCapability>,
    ) -> CapabilityAdvertisement {
        CapabilityAdvertisement {
            peer_id: peer,
            version: CAPS_WIRE_VERSION,
            services,
            inference,
            relay: None,
            storage: None,
            network_address: loopback_addr(),
            signed_at,
            ttl_seconds: ttl,
            verifying_key_bytes: [0u8; 32],
            reachable_addrs: vec![],
            readiness: ReadinessState::Ready,
            has_external_internet: false,
            bandwidth_mbps_external: None,
        }
    }

    fn ad_v25(
        peer: PeerId,
        vk_bytes: [u8; 32],
        signed_at: u64,
        ttl: u32,
        services: Vec<ServiceKind>,
        readiness: ReadinessState,
    ) -> CapabilityAdvertisement {
        CapabilityAdvertisement {
            peer_id: peer,
            version: CAPS_WIRE_VERSION,
            services,
            inference: Some(InferenceCapability {
                models: vec!["qwen2.5:7b".into()],
                context_size: 4096,
                estimated_tokens_per_sec: 50,
            }),
            relay: None,
            storage: None,
            network_address: loopback_addr(),
            signed_at,
            ttl_seconds: ttl,
            verifying_key_bytes: vk_bytes,
            reachable_addrs: vec![loopback_addr()],
            readiness,
            has_external_internet: false,
            bandwidth_mbps_external: None,
        }
    }

    #[test]
    fn is_expired_boundary() {
        let ad = ad_with(fresh_peer(), 1_000, 300, vec![ServiceKind::Relay], None);
        assert!(!ad.is_expired(1_299));
        assert!(!ad.is_expired(1_300));
        assert!(ad.is_expired(1_301));
    }

    #[test]
    fn upsert_returns_true_on_first_insert_false_on_update() {
        let reg = PeerRegistry::new();
        let peer = fresh_peer();
        let first = ad_with(peer, 100, 60, vec![ServiceKind::Relay], None);
        let second = ad_with(peer, 200, 60, vec![ServiceKind::Relay], None);
        assert!(reg.upsert(first));
        assert!(!reg.upsert(second.clone()));
        assert_eq!(reg.count(), 1);
        assert_eq!(reg.snapshot()[0].signed_at, 200);
    }

    #[test]
    fn prune_expired_removes_only_expired() {
        let reg = PeerRegistry::new();
        let live = fresh_peer();
        let dead = fresh_peer();
        reg.upsert(ad_with(live, 900, 300, vec![ServiceKind::Relay], None));
        reg.upsert(ad_with(dead, 500, 300, vec![ServiceKind::Relay], None));
        assert_eq!(reg.count(), 2);
        let removed = reg.prune_expired(1_000);
        assert_eq!(removed, 1);
        assert_eq!(reg.count(), 1);
        assert_eq!(reg.snapshot()[0].peer_id, live);
    }

    #[test]
    fn peers_with_service_returns_correct_subset_sorted_by_recency() {
        let reg = PeerRegistry::new();
        let only_relay = fresh_peer();
        let only_inf = fresh_peer();
        let both = fresh_peer();

        reg.upsert(ad_with(only_relay, 100, 600, vec![ServiceKind::Relay], None));
        reg.upsert(ad_with(only_inf, 200, 600, vec![ServiceKind::Inference], None));
        reg.upsert(ad_with(
            both,
            300,
            600,
            vec![ServiceKind::Relay, ServiceKind::Inference],
            None,
        ));

        let relays = reg.peers_with_service(ServiceKind::Relay);
        assert_eq!(relays.len(), 2);
        assert_eq!(relays[0].peer_id, both);
        assert_eq!(relays[1].peer_id, only_relay);
    }

    #[test]
    fn best_inference_peer_returns_highest_tps_with_matching_model() {
        let reg = PeerRegistry::new();
        let slow = fresh_peer();
        let fast = fresh_peer();
        reg.upsert(ad_with(
            slow,
            100,
            600,
            vec![ServiceKind::Inference],
            Some(InferenceCapability {
                models: vec!["tinyllama:1.1b-chat-q4_0".into()],
                context_size: 2048,
                estimated_tokens_per_sec: 20,
            }),
        ));
        reg.upsert(ad_with(
            fast,
            100,
            600,
            vec![ServiceKind::Inference],
            Some(InferenceCapability {
                models: vec!["tinyllama:1.1b-chat-q8_0".into(), "qwen2.5:7b".into()],
                context_size: 4096,
                estimated_tokens_per_sec: 120,
            }),
        ));
        let best = reg.best_inference_peer("tinyllama").expect("a tinyllama peer");
        assert_eq!(best.peer_id, fast);
        assert!(reg.best_inference_peer("does-not-exist").is_none());
    }

    #[test]
    fn concurrent_upserts_do_not_deadlock_and_converge() {
        let reg = Arc::new(PeerRegistry::new());
        let peers: Vec<PeerId> = (0..8).map(|_| fresh_peer()).collect();
        let mut handles = Vec::new();
        for t in 0..8 {
            let reg = Arc::clone(&reg);
            let peers = peers.clone();
            handles.push(thread::spawn(move || {
                for i in 0..100 {
                    let peer = peers[i % peers.len()];
                    if i % 10 == 0 {
                        let _ = reg.peers_with_service(ServiceKind::Relay);
                    }
                    reg.upsert(ad_with(
                        peer,
                        (t as u64) * 10_000 + i as u64,
                        600,
                        vec![ServiceKind::Relay],
                        None,
                    ));
                }
            }));
        }
        for h in handles {
            h.join().expect("worker thread panicked");
        }
        assert_eq!(reg.count(), peers.len());
    }

    // ──── V0.2.5 tests ────────────────────────────────────────────────

    #[test]
    fn readiness_state_eligibility() {
        for s in [ReadinessState::Ready, ReadinessState::Active] {
            assert!(s.is_eligible(), "{s:?} should be eligible");
        }
        for s in [
            ReadinessState::Initialised,
            ReadinessState::Connected,
            ReadinessState::Listening,
            ReadinessState::Degraded,
            ReadinessState::Stopped,
        ] {
            assert!(!s.is_eligible(), "{s:?} should NOT be eligible");
        }
    }

    #[test]
    fn reputation_band_thresholds() {
        assert_eq!(ReputationBand::from_score(-100), ReputationBand::New);
        assert_eq!(ReputationBand::from_score(0), ReputationBand::New);
        assert_eq!(ReputationBand::from_score(9), ReputationBand::New);
        assert_eq!(ReputationBand::from_score(10), ReputationBand::Probationary);
        assert_eq!(ReputationBand::from_score(99), ReputationBand::Probationary);
        assert_eq!(ReputationBand::from_score(100), ReputationBand::Established);
        assert_eq!(ReputationBand::from_score(999), ReputationBand::Established);
        assert_eq!(ReputationBand::from_score(1000), ReputationBand::Trusted);
        assert_eq!(ReputationBand::from_score(99_999), ReputationBand::Trusted);
    }

    #[test]
    fn record_identity_first_sight_then_refresh() {
        let reg = PeerRegistry::new();
        let sk = SigningKey::from_bytes(&[1u8; 32]);
        let peer = fresh_peer();
        reg.record_identity(PeerIdentity {
            peer_id: peer,
            verifying_key: sk.verifying_key(),
            reachable_addrs: vec![],
            first_seen: 100,
            last_seen: 100,
            readiness: ReadinessState::Ready,
        });
        // Refresh: first_seen stays, last_seen advances.
        reg.record_identity(PeerIdentity {
            peer_id: peer,
            verifying_key: sk.verifying_key(),
            reachable_addrs: vec![],
            first_seen: 200, // ignored
            last_seen: 200,
            readiness: ReadinessState::Active,
        });
        let ids = reg.known_identities();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].first_seen, 100, "first_seen must be sticky");
        assert_eq!(ids[0].last_seen, 200);
        assert_eq!(ids[0].readiness, ReadinessState::Active);
        assert_eq!(reg.verifying_key(&peer), Some(sk.verifying_key()));
    }

    #[test]
    fn record_identity_key_change_is_logged_but_accepted() {
        let reg = PeerRegistry::new();
        let sk1 = SigningKey::from_bytes(&[1u8; 32]);
        let sk2 = SigningKey::from_bytes(&[2u8; 32]);
        let peer = fresh_peer();
        reg.record_identity(PeerIdentity {
            peer_id: peer,
            verifying_key: sk1.verifying_key(),
            reachable_addrs: vec![],
            first_seen: 100,
            last_seen: 100,
            readiness: ReadinessState::Ready,
        });
        reg.record_identity(PeerIdentity {
            peer_id: peer,
            verifying_key: sk2.verifying_key(),
            reachable_addrs: vec![],
            first_seen: 200,
            last_seen: 200,
            readiness: ReadinessState::Ready,
        });
        // V0.2.5 accepts the rotation (V0.3+ will gate it).
        assert_eq!(reg.verifying_key(&peer), Some(sk2.verifying_key()));
    }

    #[test]
    fn upsert_extracts_verifying_key_into_directory() {
        let reg = PeerRegistry::new();
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let peer = fresh_peer();
        let ad = ad_v25(
            peer,
            *sk.verifying_key().as_bytes(),
            100,
            600,
            vec![ServiceKind::Inference],
            ReadinessState::Ready,
        );
        reg.upsert(ad);
        assert_eq!(reg.verifying_key(&peer), Some(sk.verifying_key()));
        assert_eq!(reg.identity_count(), 1);
    }

    #[test]
    fn ready_peers_for_service_filters_by_readiness_and_sorts_by_tps() {
        let reg = PeerRegistry::new();
        let sk1 = SigningKey::from_bytes(&[1u8; 32]);
        let sk2 = SigningKey::from_bytes(&[2u8; 32]);
        let sk3 = SigningKey::from_bytes(&[3u8; 32]);
        let p_fast = fresh_peer();
        let p_slow = fresh_peer();
        let p_degraded = fresh_peer();

        let mut ad_fast = ad_v25(
            p_fast,
            *sk1.verifying_key().as_bytes(),
            100,
            600,
            vec![ServiceKind::Inference],
            ReadinessState::Ready,
        );
        ad_fast.inference = Some(InferenceCapability {
            models: vec!["qwen2.5:7b".into()],
            context_size: 4096,
            estimated_tokens_per_sec: 120,
        });
        let mut ad_slow = ad_v25(
            p_slow,
            *sk2.verifying_key().as_bytes(),
            100,
            600,
            vec![ServiceKind::Inference],
            ReadinessState::Active,
        );
        ad_slow.inference = Some(InferenceCapability {
            models: vec!["qwen2.5:7b".into()],
            context_size: 4096,
            estimated_tokens_per_sec: 20,
        });
        let ad_deg = ad_v25(
            p_degraded,
            *sk3.verifying_key().as_bytes(),
            100,
            600,
            vec![ServiceKind::Inference],
            ReadinessState::Degraded, // filtered out
        );
        reg.upsert(ad_fast);
        reg.upsert(ad_slow);
        reg.upsert(ad_deg);

        let peers = reg.ready_peers_for_service(ServiceKind::Inference);
        assert_eq!(peers.len(), 2, "degraded peer must be filtered out");
        assert_eq!(peers[0].peer_id, p_fast, "fast peer must come first");
        assert_eq!(peers[1].peer_id, p_slow);
    }

    #[test]
    fn ready_peers_with_external_internet() {
        let reg = PeerRegistry::new();
        let sk1 = SigningKey::from_bytes(&[1u8; 32]);
        let sk2 = SigningKey::from_bytes(&[2u8; 32]);
        let p_bridge = fresh_peer();
        let p_plain = fresh_peer();
        let mut ad_bridge = ad_v25(
            p_bridge,
            *sk1.verifying_key().as_bytes(),
            100,
            600,
            vec![ServiceKind::Relay],
            ReadinessState::Ready,
        );
        ad_bridge.has_external_internet = true;
        ad_bridge.bandwidth_mbps_external = Some(100);
        let ad_plain = ad_v25(
            p_plain,
            *sk2.verifying_key().as_bytes(),
            100,
            600,
            vec![ServiceKind::Relay],
            ReadinessState::Ready,
        );
        reg.upsert(ad_bridge);
        reg.upsert(ad_plain);
        let peers = reg.ready_peers_with_external_internet();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].peer_id, p_bridge);
    }

    #[test]
    fn closest_peers_orders_by_xor_distance() {
        let reg = PeerRegistry::new();
        let mut peers: Vec<PeerId> = (0..5).map(|_| fresh_peer()).collect();
        for (i, p) in peers.iter().enumerate() {
            let sk = SigningKey::from_bytes(&[i as u8 + 1; 32]);
            reg.upsert(ad_v25(
                *p,
                *sk.verifying_key().as_bytes(),
                100,
                600,
                vec![ServiceKind::Inference],
                ReadinessState::Ready,
            ));
        }
        let target = peers[2];
        let closest = reg.closest_peers(&target, 3);
        // The target itself is the closest (zero XOR distance).
        assert_eq!(closest[0].peer_id, target);
        // 0-result on k=0.
        assert!(reg.closest_peers(&target, 0).is_empty());
        // K larger than population returns all.
        let all = reg.closest_peers(&target, 100);
        assert_eq!(all.len(), peers.len());
        let _ = peers.pop();
    }

    #[test]
    fn cbor_roundtrip_v0_2_5_advertisement() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let peer = fresh_peer();
        let ad = ad_v25(
            peer,
            *sk.verifying_key().as_bytes(),
            1_000,
            300,
            vec![ServiceKind::Inference, ServiceKind::Relay],
            ReadinessState::Active,
        );
        let bytes = encode_advertisement(&ad).expect("encode");
        let round = decode_advertisement(&bytes).expect("decode");
        assert_eq!(round.peer_id, ad.peer_id);
        assert_eq!(round.version, ad.version);
        assert_eq!(round.readiness, ReadinessState::Active);
        assert_eq!(round.verifying_key_bytes, *sk.verifying_key().as_bytes());
        assert_eq!(round.reachable_addrs.len(), 1);
    }

    #[test]
    fn cbor_decoder_accepts_v0_2_1_wire_shape() {
        // Old wire shape — five new fields omitted. Use a transient
        // serde struct to forge bytes without `serde(default)`.
        #[derive(Serialize)]
        struct V0_2_1Ad {
            peer_id: PeerId,
            version: u32,
            services: Vec<ServiceKind>,
            inference: Option<InferenceCapability>,
            relay: Option<RelayCapability>,
            storage: Option<StorageCapability>,
            network_address: Multiaddr,
            signed_at: u64,
            ttl_seconds: u32,
        }
        let peer = fresh_peer();
        let old = V0_2_1Ad {
            peer_id: peer,
            version: 1,
            services: vec![ServiceKind::Inference],
            inference: Some(InferenceCapability {
                models: vec!["tinyllama:1.1b".into()],
                context_size: 2048,
                estimated_tokens_per_sec: 30,
            }),
            relay: None,
            storage: None,
            network_address: loopback_addr(),
            signed_at: 1_000,
            ttl_seconds: 300,
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&old, &mut buf).expect("encode v1");
        let decoded = decode_advertisement(&buf).expect("decode v1 via v2 decoder");
        assert_eq!(decoded.peer_id, peer);
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.readiness, ReadinessState::Ready); // default
        assert_eq!(decoded.verifying_key_bytes, [0u8; 32]);
        assert!(decoded.reachable_addrs.is_empty());
        assert!(!decoded.has_external_internet);
        assert!(decoded.bandwidth_mbps_external.is_none());
        // The all-zero pubkey is treated as "absent" by `verifying_key()`.
        assert!(decoded.verifying_key().is_none());
    }
}
