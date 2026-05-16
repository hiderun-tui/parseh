//! Fuzz the `parseh_task::JobSpec` CBOR decoder.
//!
//! Targets the deserialisation path the production miner takes on
//! every inbound `parseh.tasks.v1` envelope. The miner already enforces
//! a `MAX_MESSAGE_SIZE_BYTES` (1 MiB) cap upstream, but libFuzzer feeds
//! arbitrary-length inputs, so we keep the decoder under the same kind
//! of stress an in-spec adversary could produce by stitching multiple
//! near-1-MiB shapes.
//!
//! Outcome: a panic/abort here is release-blocking per the cultural
//! rule in the project notes.

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _: Result<parseh_task::JobSpec, _> = ciborium::from_reader(data);
});
