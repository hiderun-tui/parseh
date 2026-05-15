//! Fuzz the `parseh_task::verify_bytes` ed25519-verification path with
//! attacker-controlled pubkey, message, and signature bytes.
//!
//! `verify_bytes` is the leaf primitive every
//! `JobSpec::verify_signature` / `JobResult::verify_signature` /
//! `JobVerification::verify_signature` / `JobOutcome::verify_signature`
//! ultimately calls. It is the only place in the codebase where
//! attacker bytes flow into the `ed25519-dalek` crate, so fuzzing it
//! directly is the highest-leverage way to exercise the dependency
//! under hostile input.
//!
//! Carve-up of `data`:
//!   - first 32 bytes  → candidate verifying-key bytes
//!   - next 64 bytes   → candidate signature bytes
//!   - remainder       → message
//!
//! If `data` is too short, we early-return — there is no signal from
//! "decoder bailed on a too-small slice" because that path is exercised
//! identically by every other libFuzzer iteration.

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 32 pubkey + 64 sig = 96 minimum. Below that, no useful fuzzing
    // surface: dalek's length-check would reject before any decoding
    // happens, which is the trivial path we don't need to amplify.
    if data.len() < 96 {
        return;
    }
    let (pk_bytes, rest) = data.split_at(32);
    let (sig_bytes, msg) = rest.split_at(64);

    // Try to parse the pubkey. An invalid edwards point is a normal
    // outcome — we just want to ensure that path doesn't panic.
    let pk_arr: &[u8; 32] = pk_bytes.try_into().expect("32 bytes by construction");
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(pk_arr) else {
        return;
    };
    // verify_bytes is the production hot path; any panic here is a
    // bug. We deliberately discard the `Result`.
    let _ = parseh_task::verify_bytes(&vk, msg, sig_bytes);
});
