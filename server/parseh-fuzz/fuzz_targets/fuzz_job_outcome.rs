//! Fuzz the `parseh_task::JobOutcome` CBOR decoder.
//!
//! The outcome is the deepest of the four wire types — it embeds
//! a `Vec<ContentHash>` plus an `OutcomeVerdict` whose `Disputed`
//! variant carries a `Vec<PeerId>`. Two nested arrays of variable
//! length is the highest amplification factor in the V0.2 wire
//! schema and the most likely to surface a length-decoding bug.

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _: Result<parseh_task::JobOutcome, _> = ciborium::from_reader(data);
});
