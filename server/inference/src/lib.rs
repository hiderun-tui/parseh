//! `parseh-inference` library surface.
//!
//! The inference crate ships both a binary (`parseh-inference`) and a thin
//! library facade so other crates (or future integration tests) can reuse the
//! Candle-based runtime helpers without re-invoking the CLI.
//!
//! Modules are gated behind feature flags so the default build stays small:
//!
//! * `candle` — pulls in the pure-Rust Candle stack and exposes
//!   [`candle_runtime`] for load-only model verification (V0.1).

#![deny(rust_2018_idioms)]

#[cfg(feature = "candle")]
pub mod candle_runtime;
