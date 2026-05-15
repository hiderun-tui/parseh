//! `parseh-verify` — V0.2 Primitive 2: multi-peer verification.
//!
//! Given a [`parseh_task::JobResult`] heard on the network, this crate
//! provides:
//!
//! - the **selection algorithm** deciding whether to re-execute
//!   ([`selection`]),
//! - the **re-execution + signed-attestation** flow ([`verifier`]),
//! - the **M-of-N aggregator** producing a finalised
//!   [`parseh_task::JobOutcome`] ([`quorum`]).
//!
//! ## Scope
//!
//! The crate has **no chain dependency**. Reputation is in-memory only
//! (callers feed numbers in; this crate counts them up). At V0.3+ the
//! same primitive plugs into the chain emission flow without schema
//! changes — the wire types (defined in [`parseh_task`]) are stable.
//!
//! ## What this crate intentionally does NOT do
//!
//! - It does not move bytes on the network (that is `parseh-miner`'s
//!   gossipsub + request-response wiring).
//! - It does not persist anything (that is `parseh-shared-state`, the
//!   next primitive).
//! - It does not re-implement the wire types (those are in
//!   [`parseh_task`]).
//! - It does not implement the chain-side reputation projection — it
//!   reads reputation as an opaque `u32` and returns weighted floats.
//!
//! ## V0.2 parameter pinning
//!
//! See [`params`] for the canonical V0.2 values, cross-referenced with
//! the project notes §3.1.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod methods;
mod quorum;
mod rate_limit;
mod selection;
mod verifier;

pub use methods::{
    DeterministicMethod, LocalExecutor, SpotCheckMethod, StatisticalMethod, VerificationOutcome,
    VerifierMethodImpl,
};
pub use parseh_task::VerifierMethod;
pub use quorum::{FinalisedQuorum, Quorum, QuorumConfig, QuorumDecision};
pub use rate_limit::RateLimit;
pub use selection::{decide_to_verify, SelectionConfig, SelectionDecision, SkipReason};
pub use verifier::{Verifier, VerifyError, VerifyOutcome};

/// V0.2 parameter pins. These are the single source of truth — match
/// the project notes §3.1.
pub mod params {
    /// Base self-selection probability before reputation weighting.
    pub const P_BASE: f64 = 0.05;
    /// Lower clamp on per-node selection probability.
    pub const P_MIN: f64 = 0.001;
    /// Upper clamp on per-node selection probability — even the
    /// highest-reputation peer cannot self-select more than this often
    /// before the per-hour rate cap kicks in.
    pub const P_MAX: f64 = 0.5;
    /// Per-node rate cap, expressed as the fraction of *observed* tasks
    /// in a rolling 1-hour window that a single node is allowed to
    /// verify.
    pub const RATE_CAP_PER_HOUR: f64 = 0.10;
    /// Reputation floor below which a peer is not eligible to verify.
    /// Mirrors the `Probationary` tier boundary in
    /// the project notes §1.
    pub const PROBATIONARY_REP_FLOOR: u32 = 10;
    /// M for the **Standard** quorum (M-of-N agreement threshold).
    pub const M_STANDARD: u32 = 5;
    /// N for the **Standard** quorum (target verifier count).
    pub const N_STANDARD: u32 = 9;
    /// M for the **Sensitive** quorum.
    pub const M_SENSITIVE: u32 = 9;
    /// N for the **Sensitive** quorum.
    pub const N_SENSITIVE: u32 = 15;
    /// Minimum quorum window before a finalisation is allowed (seconds).
    pub const T_MIN_SECS: u64 = 5;
    /// Maximum quorum window before a quorum is declared
    /// `Indeterminate` (seconds).
    pub const T_MAX_SECS: u64 = 30;
    /// Reputation-weighted threshold for an `Agreed` finalisation.
    pub const REP_WEIGHTED_THRESHOLD: f64 = 0.6;
    /// Sliding-window length for the per-hour rate cap (seconds).
    pub const RATE_WINDOW_SECS: u64 = 3600;
}

/// Crate version surfaced via `parseh_verify::VERSION`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
