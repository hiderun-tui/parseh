# parseh-core

Shared types, configuration, and the V0.2 **peer-registry** (a.k.a.
peer-key directory) for every server-side PARSEH crate.

This crate intentionally does very little. Its job is to define the
data model (`NodeConfig`, `NodeId`, `NodeCapabilities`, plus the V0.2
peer-registry envelope shapes) once so the relay, inference host,
miner, and wallet crates can pass values around without depending on
each other.

## V0.2.5 surface (peer identity + readiness)

`server/parseh-core/src/peer_registry.rs` exposes the V0.2.5 peer-key
directory + capability registry. The key public types:

| Type | Purpose |
|---|---|
| `CapabilityAdvertisement` | Self-describing CBOR envelope gossiped on `parseh.caps.v1`. V0.2.5 wire version = `CAPS_WIRE_VERSION = 2`. Embeds the publisher's ed25519 pubkey, reachable multiaddrs, readiness state, and bridge-leg capability flag. |
| `ReadinessState` | Where a peer is in its lifecycle: `Initialised → Connected → Listening → Ready → Active → Degraded → Stopped`. Mirrors the project notes §3.4. |
| `ReputationBand` | Coarse classification of a peer's reputation score (`New` `0..9`, `Probationary` `10..99`, `Established` `100..999`, `Trusted` `1000..9999`, `Slashed`). Mirrors the project notes §1. |
| `PeerIdentity` | Cryptographic + network identity of a peer (peer_id, verifying_key, reachable_addrs, first_seen, last_seen, readiness). |
| `PeerRegistry` | Thread-safe cache of advertisements + peer-key directory. Cloneable handle backed by `Arc<RwLock<…>>`. |

Selected public methods on `PeerRegistry`:

- `upsert(ad: CapabilityAdvertisement) -> bool` — record an inbound ad, extract its verifying key into the identity directory.
- `verifying_key(&peer_id) -> Option<VerifyingKey>` — peer-key directory lookup, used by inbound `JobSpec` / `JobResult` / `JobVerification` signature checks.
- `record_identity(identity: PeerIdentity)` — explicitly register a peer's key. Subsequent same-key advertisements refresh `last_seen`; key rotations are logged at WARN but not rejected at V0.2.5.
- `ready_peers_for_service(kind) -> Vec<PeerIdentity>` — match-making filter (only `Ready` / `Active`).
- `ready_peers_with_external_internet() -> Vec<PeerIdentity>` — bridge-leg discovery for the V0.2.5 tunnel crate.
- `closest_peers(target, k) -> Vec<PeerIdentity>` — K closest peers by XOR distance (Kad metric).

## Wire-format compatibility

`CapabilityAdvertisement::version` bumped `1 → 2` in V0.2.5. The new
decoder `decode_advertisement` accepts both shapes — every field added
in V0.2.5 is `#[serde(default)]`, so V0.2.1 peers continue to interop
with V0.2.5 peers during a rolling upgrade. V0.2.1's missing
`verifying_key_bytes` defaults to all-zero, which `verifying_key()`
treats as "key not advertised".

## Encoding helpers

- `encode_advertisement(&ad) -> Result<Vec<u8>, …>` — emit CBOR.
- `decode_advertisement(bytes) -> Result<CapabilityAdvertisement, …>` — accept v1 or v2.

## Tests

Run `cargo test -p parseh-core --release`. The 17 unit tests + 10
doctests cover concurrent upserts, the v1 fallback decoder, the XOR
metric, and all reputation-band boundary cases.
