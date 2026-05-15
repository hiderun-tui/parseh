//! Fuzz the `parseh_task::JobResult` CBOR decoder.
//!
//! The result envelope embeds the executor's `result_payload` as an
//! opaque `Vec<u8>`; the decoder MUST accept any byte sequence in that
//! field without crashing or unbounded growth. libFuzzer will steer
//! toward shapes that pathologically size the embedded payload.

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _: Result<parseh_task::JobResult, _> = ciborium::from_reader(data);
});
