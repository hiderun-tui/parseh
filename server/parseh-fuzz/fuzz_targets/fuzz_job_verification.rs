//! Fuzz the `parseh_task::JobVerification` CBOR decoder.
//!
//! The verification envelope carries an enum (`VerifierVerdict`) with a
//! variant that itself holds a 32-byte `ContentHash`. Fuzzing tilts
//! toward the variant-discriminant-as-attack-surface pattern that has
//! historically broken hand-rolled CBOR decoders.

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _: Result<parseh_task::JobVerification, _> = ciborium::from_reader(data);
});
