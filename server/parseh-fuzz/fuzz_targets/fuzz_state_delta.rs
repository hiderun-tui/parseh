//! Fuzz the `parseh_shared_state::StateDelta` CBOR decoder.
//!
//! `StateDelta` is the gossipsub envelope on `parseh.state-deltas.v1`.
//! Its `DeltaKind` enum has three variants, one of which (`Outcome`)
//! embeds a fully-formed `parseh_task::JobOutcome` — so a single
//! attacker input drives BOTH the `StateDelta` and the inner outcome
//! decoder. That nested fan-out is exactly what we want covered.
//!
//! Note: this target only exercises decoding, NOT signature
//! verification — see `fuzz_signature_verify` for that path.

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parseh_shared_state::StateDelta::decode_cbor(data);
});
