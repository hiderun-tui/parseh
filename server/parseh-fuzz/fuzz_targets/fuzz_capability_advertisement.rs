//! Fuzz the `parseh_core::peer_registry::decode_advertisement` decoder.
//!
//! This is the V0.2.5 capability-advertisement decoder — wire-format-
//! version bumped 1 → 2 with a `serde(default)` v1 fallback. The
//! fallback path is the one most likely to surface a decoder bug
//! because it accepts CBOR objects that legitimately *lack* fields the
//! deserialiser would otherwise consider mandatory.
//!
//! Particular shapes of interest libFuzzer will explore:
//!   - `verifying_key_bytes` of length ≠ 32 (custom adapter)
//!   - oversized `reachable_addrs` (`Vec<Multiaddr>`)
//!   - malformed Multiaddr bytes (libp2p decoder)
//!   - any/all readiness-state discriminants out of range

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parseh_core::peer_registry::decode_advertisement(data);
});
