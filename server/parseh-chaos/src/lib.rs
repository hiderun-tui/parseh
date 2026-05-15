//! `parseh-chaos` — V0.2.5 adversarial test harness.
//!
//! ## Hard cultural boundary
//!
//! This crate is **adversarial testing**, not exploit development.
//!
//! - All scenarios run in-process over `libp2p::core::transport::MemoryTransport`.
//! - Faults are injected via test-only APIs we own.
//! - When an assertion FAILS, that signals a real V0.2 protocol bug.
//!   The remedy is to fix the protocol; the test is the spec.
//! - This crate is NOT a tool for attacking real PARSEH peers in
//!   production. There is no network-side adversarial code and no
//!   facility for targeting external IPs.
//! - This crate is NOT a public "attack toolkit". It lives behind a
//!   `publish = false` workspace member.
//!
//! ## Scope
//!
//! Four scenarios, covering security model attacks 3.1, 3.2, 3.3, 3.5,
//! 3.6, 3.9, 3.10 (see the project notes):
//!
//! - [`partition`] — **PRIORITY per maintainer direction 2026-05-14.**
//!   Splits a 6-node mesh, lets each side run independently, rejoins,
//!   and asserts state convergence. Tests whether conflicting
//!   shared-state histories merge correctly under V0.2 design.
//! - [`malicious_verifier`] — Honest nodes vs misbehaving verifiers
//!   (`RubberStamp`, `AlwaysDisagreed`, `Random`, `RaceToVoteFirst`,
//!   `AlwaysAgreed`). Documents the >50% threshold beyond which V0.2
//!   cannot defend.
//! - [`sybil`] — Empirical Sybil-cost measurement at P=50/100/500
//!   in-process identities, compared against the theoretical numbers
//!   in the project notes.
//! - [`corruption`] — Bit-flips / truncation / re-signing / row deletes
//!   in a 3-node mesh. Verifies corrupted signatures fail verification
//!   and reputation degrades.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod corruption;
pub mod malicious_verifier;
pub mod partition;
pub mod scenario;
pub mod sybil;

pub use corruption::{CorruptionMode, CorruptionScenario};
pub use malicious_verifier::{MaliciousMode, MaliciousVerifier};
pub use partition::{PartitionConfig, PartitionScenario};
pub use scenario::{ChaosNode, ChaosScenario};
pub use sybil::{SybilConfig, SybilScenario};

/// Crate version for diagnostics.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
