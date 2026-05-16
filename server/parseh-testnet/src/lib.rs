//! `parseh-testnet` — V0.2 acceptance harness.
//!
//! A **test-only** crate that spins up 3 in-process libp2p nodes (over
//! `MemoryTransport`, no real sockets) and exercises the full V0.2
//! coordination plane end-to-end: a [`parseh_task::JobSpec`] flows
//! from a submitter, an executor produces a [`parseh_task::JobResult`],
//! verifiers attest, the [`parseh_verify::Quorum`] finalises into a
//! signed [`parseh_task::JobOutcome`], the outcome propagates as a
//! [`parseh_shared_state::StateDelta`] on `parseh.state-deltas.v1`, and
//! every node converges on a reputation increment for the executor.
//!
//! This crate exists to prove the V0.2 minimum success condition stated
//! in the project notes: if a 3-node testnet can route
//! a task through `task→verify→outcome→shared-state→reputation`, V0.2
//! transitions from "architecture concept" to "verified protocol
//! primitive."
//!
//! ## What this harness is NOT
//!
//! - It is **not** a load test. It uses an unrealistically small quorum
//!   (M=2/N=3) so the flow can close with 3 nodes; production V0.2 uses
//!   M=5/N=9 per the project notes §3.1. Demonstrating
//!   the protocol primitive does not depend on production parameters.
//! - It is **not** a libp2p stress test. `MemoryTransport` gives us
//!   perfect-loss-free, in-process channels — the test asserts that the
//!   crates wire up correctly, not that the underlying network is fast.
//! - It is **not** wired into the miner binary. The [`TestNode`] mirrors
//!   the miner's libp2p swarm shape but lives in its own loop because
//!   the miner's V0.1 swarm does not yet carry the V0.2 topics; the
//!   miner-side integration is a follow-up PR.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod node;
mod scenario;

pub use node::{NodeRole, StateSnapshot, TestNode};
pub use scenario::{Scenario, ScenarioError};
