//! `parseh-integration-tests` — V0.2.5 multi-node integration harness.
//!
//! Spawns 5 in-process miners over `MemoryTransport`, wires the V0.2.5
//! peer-identity + readiness layer end-to-end, and asserts:
//!
//! 1. After mesh formation every node publishes a
//!    [`parseh_core::peer_registry::CapabilityAdvertisement`] carrying
//!    its ed25519 verifying key + readiness state.
//! 2. Deterministic executor self-selection (lowest `PeerId` bytes among
//!    eligible non-submitters) picks the right executor for every spec.
//! 3. M=2/N=3 reduced quorum (test-only) closes for each spec.
//! 4. All 5 nodes' SharedState shows all 10 outcomes within 5 seconds.
//! 5. Reputation deltas: executor +10×N_specs, each verifier +5×N_specs.
//!
//! ## Why a separate crate?
//!
//! The existing `parseh-testnet` harness is a 3-node primitive proof
//! with a documented Rule-3a relaxation. V0.2.5's contract — 5-node,
//! identity-routed signature verification, executor self-selection —
//! requires both more nodes and a different envelope shape, so we add
//! a sibling harness rather than overload the 3-node one.
//!
//! ## Scope
//!
//! This crate exposes the [`mesh::Mesh`] helper and the
//! [`mesh::MeshNode`] type. The actual scenario assertions live in
//! `tests/five_node_mesh.rs`. The harness re-uses
//! `parseh_core::peer_registry` for the live peer-key directory and the
//! readiness state; there is no duplicate identity store.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod mesh;

/// Crate version for diagnostics.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
