//! `parseh-agent-spec` — V0.2 agent definition format.
//!
//! Contributors design agents (prompt template + RAG knowledge refs +
//! exact JSON schemas for input + output), sign the definition with
//! their ed25519 key, and the signed definition becomes a
//! content-addressable artefact other peers can reference.
//!
//! NO economic logic in this crate. Agent execution, quality scoring,
//! and reward distribution are V0.3+ work blocked behind the
//! "adversarial testing must land first" gate from
//! [the project notes](the project notes).
//!
//! ## Scope boundary (binding)
//!
//! This crate ships:
//!
//! - Signed agent definitions (the "ownership" concept as a protocol
//!   property — see [the project notes](the project notes)
//!   Rule 8: whoever signed the definition IS the owner).
//! - Content-addressable agent identifiers (SHA-256 over CBOR, mirroring
//!   the `parseh-task` pattern).
//! - Schema for exact input + exact output (a JSON Schema document
//!   embedded as a string, validated with the `jsonschema` crate).
//! - Knowledge-base reference structure (content-hash addressable RAG).
//! - Version and lineage (parent agent IDs for forks / improvements).
//!
//! This crate does NOT ship:
//!
//! - Marketplace, investment, or trading mechanics.
//! - Economic emission tied to agent use.
//! - Quality-weighted Proof-of-Useful-Intelligence reward distribution.
//! - Any on-chain interaction.
//!
//! All of those are downstream (`parseh-shared-state` for replay,
//! `parseh-verify` for verification, future `parseh-economic-*` crates
//! for V0.3+ emission). If you find yourself adding any of them here:
//! stop, open a maintainer-team discussion, and add a paragraph to
//! [the project notes](the project notes)
//! explaining why the deferral was relaxed.
//!
//! ## Wire format
//!
//! CBOR throughout, matching `parseh-task`. The signature on
//! [`AgentDefinition`] is computed over the CBOR encoding of the
//! struct with the signature field cleared, the same convention used
//! by `parseh-task::JobSpec` (the project notes
//! §3.1).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod definition;
mod knowledge_ref;
mod lineage;
mod schema_validation;

pub use definition::{
    AgentDefinition, AgentId, AgentMetadata, AgentVersion, DefinitionError, ModelRequirements,
};
pub use knowledge_ref::{KnowledgeKind, KnowledgeRef};
pub use lineage::{ForkReason, ParentRef};
pub use schema_validation::{
    validate_against_schema, InputSchema, OutputSchema, SchemaError, ValidationError,
};

// Re-export the signature error from `parseh-task` so downstream crates
// do not need a separate dependency edge just to handle our verify
// failures.
pub use parseh_task::{ContentHash, SignError};

/// Crate version surfaced via `parseh_agent_spec::VERSION`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Agent-spec format version. Bumped on backwards-incompatible CBOR
/// shape changes. Peers that decode an unknown `spec_version` MUST
/// drop rather than mis-parse — same discipline as `parseh-task`'s
/// `WIRE_VERSION`.
pub const SPEC_VERSION: u32 = 1;

/// Maximum CBOR-encoded size of a single [`AgentDefinition`].
///
/// Mirrors `parseh-task::MAX_MESSAGE_SIZE_BYTES`. Larger agent
/// definitions (e.g., long prompt templates) use the same
/// content-addressed sidechannel pattern as oversized job payloads —
/// the gossipsub envelope carries only the [`AgentId`] hash and peers
/// fetch the body via request-response.
pub const MAX_DEFINITION_SIZE_BYTES: usize = 1024 * 1024; // 1 MiB

/// CBOR-encode a value into a freshly-allocated `Vec`. Re-exported
/// from `parseh-task`'s pattern so callers do not have to depend on
/// `ciborium` directly.
pub fn to_cbor_bytes<T: serde::Serialize>(
    value: &T,
) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf)?;
    Ok(buf)
}

/// CBOR-decode a value from a byte slice.
pub fn from_cbor_bytes<T: for<'de> serde::Deserialize<'de>>(
    bytes: &[u8],
) -> Result<T, ciborium::de::Error<std::io::Error>> {
    ciborium::from_reader(bytes)
}
