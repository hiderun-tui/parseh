//! Library surface for the `parseh-relay` crate.
//!
//! The binary in `src/main.rs` is the primary artifact; this `lib.rs`
//! exists so feature-gated public modules (today: `reality`) can be
//! reached from `examples/` and `tests/`. Plain default builds remain
//! a no-op library — they expose nothing, change no behaviour.

#![allow(unused_imports)]

#[cfg(feature = "reality")]
pub mod reality;
