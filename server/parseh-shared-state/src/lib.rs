//! `parseh-shared-state` — V0.2 Primitive 3 (Shared Memory Layer).
//!
//! Persisted, eventually-consistent shared state across the PARSEH
//! network. Backed by SQLite (optionally SQLCipher-encrypted via the
//! `encrypted` feature) with signed deltas propagated over gossipsub on
//! `parseh.state-deltas.v1`.
//!
//! ## Scope
//!
//! This crate provides:
//!
//! - The local store API ([`SharedState`]) — typed CRUD over five
//!   tables (`tasks`, `results`, `verifications`, `outcomes`,
//!   `reputation_log`, `governance_rules`).
//! - The wire envelope for propagation ([`StateDelta`]) wrapping signed
//!   [`parseh_task::JobOutcome`] / reputation / governance updates.
//! - Detection queries (cross-table SQL) that maintainers run to spot
//!   coordinated-verifier rings, per
//!   the project notes §7.
//!
//! ## What this crate intentionally does NOT do
//!
//! - It does not move bytes on the network. The miner crate owns
//!   gossipsub publish + subscribe; this crate produces and consumes
//!   `StateDelta`s.
//! - It does not implement Sybil-resistance — only Sybil-detection.
//!   Reaction to detected rings is V0.3+ work (chain slashing).
//! - It does not depend on the PARSEH chain. Reputation here is
//!   strictly local + gossiped; the chain (deferred to V0.3+) becomes
//!   the canonical reputation projection only after the V0.2
//!   coordination primitives prove robust.
//!
//! ## Trust boundary
//!
//! Per the design review surfaced in the verify-agent's report, **only
//! signed [`parseh_task::JobOutcome`] objects cross the trust boundary
//! on the `state-deltas` topic** — the mid-window [`parseh_verify::Quorum`]
//! is never gossiped. This avoids the partial-state replay attack class
//! (where an adversary streams a sequence of mid-window quorum snapshots
//! to bias a peer's perception of an in-flight verification).
//!
//! See the project notes §3.3,
//! the project notes §5, and
//! the project notes Rule 4.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod cipher;
mod delta;
mod migrations;
mod schema;
mod store;

pub use cipher::{KeyMaterial, KeySource};
pub use delta::{sign_delta, verify_delta, DeltaKind, SignError, StateDelta};
pub use store::{OpenError, OpenOptions, SharedState, StoreError};

/// Local on-disk schema version. Bump whenever
/// [`crate::migrations`] gains a new step.
pub const SCHEMA_VERSION: u32 = 1;

/// Gossipsub topic the miner publishes [`StateDelta`] envelopes on.
pub const GOSSIPSUB_TOPIC: &str = "parseh.state-deltas.v1";

/// Wire-format version embedded in every [`StateDelta`].
///
/// Peers receiving a delta with a different `wire_version` must drop
/// the message rather than mis-parse — same convention as
/// [`parseh_task::WIRE_VERSION`].
pub const WIRE_VERSION: u32 = 1;

/// Crate version surfaced via `parseh_shared_state::VERSION`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
