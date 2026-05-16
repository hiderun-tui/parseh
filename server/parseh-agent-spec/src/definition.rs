//! [`AgentDefinition`] — the signed, content-addressable agent
//! specification a contributor publishes to the network.
//!
//! Authoring flow mirrors `parseh-task::JobSpec`:
//!
//! ```text
//!     AgentDefinition::new_signed(...)
//!         ├── builds struct with empty signature
//!         ├── CBOR-encodes
//!         ├── ed25519-signs the encoding
//!         ├── writes the signature back into the struct
//!         ├── re-encodes the signed form
//!         ├── SHA-256 → AgentId
//!         └── returns (definition, agent_id)
//! ```
//!
//! Verification clears the signature, re-encodes, and checks the bytes.
//!
//! ## Ownership semantics
//!
//! Per [the project notes](the project notes) Rule 8 and
//! footnote 4: the `author` field is the signing PeerId, and **that
//! signature IS the protocol-level ownership claim**. No marketplace,
//! no royalty flow, no marketing copy about "tradeable agents" — just
//! the signed-author fact. V0.3+ economic layers MAY read this field
//! to attribute rewards; V0.2 does not.

use crate::{
    knowledge_ref::KnowledgeRef,
    lineage::ParentRef,
    schema_validation::{InputSchema, OutputSchema, SchemaError},
    to_cbor_bytes, ContentHash, SignError, SPEC_VERSION,
};
use libp2p::PeerId;
use parseh_task::{content_hash, sign_bytes, verify_bytes};
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors raised while constructing an [`AgentDefinition`].
#[derive(Error, Debug)]
pub enum DefinitionError {
    /// `prompt_template` was empty — an agent must have something to do.
    #[error("prompt_template must not be empty")]
    EmptyPromptTemplate,
    /// `metadata.name` was empty or all whitespace.
    #[error("metadata.name must not be empty")]
    EmptyName,
    /// `metadata.license` did not match the SPDX-identifier shape
    /// (`[A-Za-z0-9.+\-]+`, with optional `WITH ...` clause). Empty
    /// licences are rejected — pick `Apache-2.0` if unsure.
    #[error("metadata.license `{0}` is not a plausible SPDX identifier")]
    InvalidLicense(String),
    /// One of the embedded JSON Schemas failed to compile or contained
    /// a structural error.
    #[error("schema error: {0}")]
    Schema(#[from] SchemaError),
    /// The CBOR encode step failed. Should be unreachable for owned
    /// types but surfaced rather than panicked so downstream callers
    /// can decide whether to abort or retry.
    #[error("cbor encode failed: {0}")]
    CborEncode(String),
}

/// A signed, content-addressable agent definition.
///
/// **Identity:** [`Self::content_hash`] of the signed CBOR form is the
/// [`AgentId`] returned by [`Self::new_signed`]. All downstream
/// references (verification records, lineage `ParentRef`s, future
/// agent-execution records) use this hash as the canonical agent
/// identifier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// Spec-format version. Always [`crate::SPEC_VERSION`] for freshly
    /// built definitions; mismatching peers drop rather than parse.
    pub spec_version: u32,
    /// Self-referential content hash, computed AFTER signing. Stored
    /// here for convenience (peers can verify it independently by
    /// recomputing). Wire-decoded definitions whose stored `id` does
    /// not match the recomputed hash MUST be rejected.
    pub id: AgentId,
    /// Semver-like version. Authors increment this; nothing in this
    /// crate enforces monotonicity (lineage tracking in
    /// [`ParentRef`] is the structural mechanism for "improvement
    /// claim" — there is no central registry).
    pub version: AgentVersion,
    /// libp2p PeerId of the signer. Per Rule 8: this PeerId IS the
    /// ownership claim. No marketplace; no transfer mechanic in V0.2.
    pub author: PeerId,
    /// Human-readable metadata. NOT used for identity.
    pub metadata: AgentMetadata,
    /// Exact-input JSON Schema (Draft 2020-12). Callers MUST validate
    /// inputs against this before invoking the agent.
    pub input_schema: InputSchema,
    /// Exact-output JSON Schema. The agent's response is rejected by
    /// downstream verifiers if it does not validate against this
    /// schema.
    pub output_schema: OutputSchema,
    /// Prompt template. Templating semantics (e.g., `{{var}}`
    /// substitution) are NOT defined by this crate — the executor
    /// chooses. The string is verbatim agent-definition data; the
    /// only constraint is non-emptiness.
    pub prompt_template: String,
    /// Zero or more RAG knowledge references. May be empty (the agent
    /// then operates only from its prompt template).
    pub knowledge_refs: Vec<KnowledgeRef>,
    /// Model requirements the executor must honour.
    pub model_requirements: ModelRequirements,
    /// Lineage. Zero parents = original work; one or more parents =
    /// fork / translation / improvement. Multiple parents are allowed
    /// (a merge of two specialised forks) but the receiving network
    /// rejects parent cycles when it traverses the lineage DAG —
    /// this crate stores the edges only.
    pub parents: Vec<ParentRef>,
    /// Wall-clock UTC seconds at signing time. Embedded so peers can
    /// detect stale / replayed definitions.
    pub created_at: u64,
    /// Ed25519 signature over the canonical CBOR of all preceding
    /// fields, by `author`'s key. Empty (`len = 0`) on the unsigned
    /// pre-sign form.
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
}

/// Content-hash-based agent identifier. Newtype around
/// [`ContentHash`] so the type system distinguishes "task identity"
/// from "agent identity" even though both are SHA-256 of CBOR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub ContentHash);

impl AgentId {
    /// Lower-case hex encoding of the underlying hash (64 chars).
    pub fn as_hex(&self) -> String {
        self.0.as_hex()
    }

    /// Zero-valued agent id (placeholder for unsigned drafts).
    pub const fn zero() -> Self {
        Self(ContentHash::zero())
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Semver-like agent version. Comparison is field-lexicographic
/// (major then minor then patch). NOT full semver — no pre-release
/// or build-metadata semantics — because lineage is the structural
/// "improvement" signal in this crate, not the version tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AgentVersion {
    /// Breaking changes to input/output schema or prompt semantics.
    pub major: u32,
    /// Backwards-compatible feature additions.
    pub minor: u32,
    /// Bug fixes / clarifications with no semantic change.
    pub patch: u32,
}

impl AgentVersion {
    /// Construct a version from three components.
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl std::fmt::Display for AgentVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Human-readable metadata. None of these fields participate in
/// identity comparisons — two agents may share a name but they will
/// have different [`AgentId`]s because their signatures differ.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMetadata {
    /// Display name. Not unique. Persian or English encouraged;
    /// any UTF-8 string is accepted.
    pub name: String,
    /// Short paragraph describing what the agent does. Plain-text;
    /// no markdown rendering implied by this crate.
    pub description: String,
    /// BCP-47 language codes the agent operates in (e.g., `fa`,
    /// `en`, `fa-IR`). Empty is permitted (language-agnostic agents,
    /// e.g., numerical-only). NOT validated against an authoritative
    /// BCP-47 registry — that's an executor-side concern.
    pub languages: Vec<String>,
    /// Free-form tags for discovery. NOT a marketplace primitive —
    /// just a hint string for offline tooling. No search mechanic
    /// ships in V0.2.
    pub tags: Vec<String>,
    /// SPDX license identifier. `Apache-2.0` recommended to match
    /// the project's licence; any SPDX-shaped string is accepted.
    pub license: String,
}

/// Runtime requirements the executor must satisfy to invoke this
/// agent. Honest declaration here lets the network skip agents that
/// no available node can run, avoiding wasted task dispatches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequirements {
    /// Minimum parameter count (e.g., 7_000_000_000 for a 7B model).
    /// `None` = any size acceptable.
    pub min_parameters: Option<u64>,
    /// Hint list of model names the author tested with (e.g.,
    /// `["qwen2.5:7b", "llama3.1:8b"]`). Order = preference. Empty =
    /// no preference; executor picks anything matching
    /// `min_parameters`.
    pub preferred_model_names: Vec<String>,
    /// Minimum context window in tokens. `0` is permitted but
    /// nonsensical; gossipsub validators reject `0` at the wire
    /// layer (V0.3 work; this crate stores raw values).
    pub context_window_tokens: u32,
    /// `true` ⇒ the agent requires deterministic-mode execution
    /// (seed-pinned, temperature 0, fixed beam width). Matches the
    /// V0.2 verifier default per
    /// the project notes §3.2. Setting
    /// this to `false` allows stochastic execution; downstream
    /// verifiers will then need a non-(a) `VerifierMethod`.
    pub deterministic_mode_required: bool,
}

impl AgentDefinition {
    /// Build, sign, and content-address an [`AgentDefinition`].
    ///
    /// Returns the signed object and its [`AgentId`]. Use the id
    /// everywhere downstream — it is the canonical identifier.
    ///
    /// # Errors
    ///
    /// Returns [`DefinitionError`] if any inline validation fails:
    /// empty prompt template, empty name, malformed SPDX license, or
    /// a structurally-invalid JSON Schema document.
    #[allow(clippy::too_many_arguments)]
    pub fn new_signed(
        version: AgentVersion,
        author: PeerId,
        metadata: AgentMetadata,
        input_schema: InputSchema,
        output_schema: OutputSchema,
        prompt_template: String,
        knowledge_refs: Vec<KnowledgeRef>,
        model_requirements: ModelRequirements,
        parents: Vec<ParentRef>,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> Result<(Self, AgentId), DefinitionError> {
        Self::new_signed_at(
            version,
            author,
            metadata,
            input_schema,
            output_schema,
            prompt_template,
            knowledge_refs,
            model_requirements,
            parents,
            now_unix(),
            signing_key,
        )
    }

    /// Like [`Self::new_signed`] but with an explicit `created_at`
    /// timestamp — useful for deterministic tests that assert content
    /// hashes.
    #[allow(clippy::too_many_arguments)]
    pub fn new_signed_at(
        version: AgentVersion,
        author: PeerId,
        metadata: AgentMetadata,
        input_schema: InputSchema,
        output_schema: OutputSchema,
        prompt_template: String,
        knowledge_refs: Vec<KnowledgeRef>,
        model_requirements: ModelRequirements,
        parents: Vec<ParentRef>,
        created_at: u64,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> Result<(Self, AgentId), DefinitionError> {
        // Inline validation — fail fast on obviously-malformed input.
        if prompt_template.trim().is_empty() {
            return Err(DefinitionError::EmptyPromptTemplate);
        }
        if metadata.name.trim().is_empty() {
            return Err(DefinitionError::EmptyName);
        }
        if !is_plausible_spdx_identifier(&metadata.license) {
            return Err(DefinitionError::InvalidLicense(metadata.license.clone()));
        }
        // Compile schemas once at construction time so authors learn
        // about bad schema documents immediately, not later when a
        // verifier rejects everything.
        input_schema.compile()?;
        output_schema.compile()?;

        let mut def = Self {
            spec_version: SPEC_VERSION,
            id: AgentId::zero(), // placeholder — overwritten after signing
            version,
            author,
            metadata,
            input_schema,
            output_schema,
            prompt_template,
            knowledge_refs,
            model_requirements,
            parents,
            created_at,
            signature: Vec::new(),
        };
        // Sign the unsigned + zero-id form.
        let unsigned = to_cbor_bytes(&def).map_err(|e| DefinitionError::CborEncode(e.to_string()))?;
        def.signature = sign_bytes(signing_key, &unsigned).to_vec();
        // Now that the signature is embedded, compute the canonical
        // identity. We hash the *signed* CBOR — same convention as
        // `parseh-task::JobSpec` — and write it back into `id` so the
        // wire form carries its own hash. Peers verify by recomputing.
        let signed_cbor =
            to_cbor_bytes(&def).map_err(|e| DefinitionError::CborEncode(e.to_string()))?;
        let id = AgentId(content_hash(&signed_cbor));
        def.id = id;
        Ok((def, id))
    }

    /// Verify the embedded ed25519 signature against the author's
    /// public key.
    pub fn verify_signature(
        &self,
        author_pubkey: &ed25519_dalek::VerifyingKey,
    ) -> Result<(), SignError> {
        // Rebuild the unsigned + zero-id form (clear `id`, clear
        // `signature`) and verify.
        let mut bare = self.clone();
        bare.id = AgentId::zero();
        bare.signature.clear();
        let unsigned_cbor =
            to_cbor_bytes(&bare).map_err(|e| SignError::Verify(format!("cbor encode: {e}")))?;
        verify_bytes(author_pubkey, &unsigned_cbor, &self.signature)
    }

    /// Recompute the content hash of the signed form.
    ///
    /// To check that a wire-decoded definition is internally consistent:
    /// compare `def.id` against `def.recompute_id()` — they must match.
    pub fn recompute_id(&self) -> AgentId {
        // We hash the struct with `id` cleared to zero so the result
        // matches the value computed at signing time (see
        // `new_signed_at`).
        let mut snapshot = self.clone();
        snapshot.id = AgentId::zero();
        let cbor = to_cbor_bytes(&snapshot)
            .expect("AgentDefinition CBOR encode cannot fail for owned types");
        AgentId(content_hash(&cbor))
    }

    /// Content hash of the signed-and-id-bearing CBOR encoding. Used
    /// when other types need to reference this definition by its full
    /// wire bytes (vs. the cached `id` field).
    pub fn content_hash(&self) -> ContentHash {
        let cbor = to_cbor_bytes(self)
            .expect("AgentDefinition CBOR encode cannot fail for owned types");
        content_hash(&cbor)
    }
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Coarse SPDX-identifier shape check.
///
/// We do NOT pull in the full SPDX expression grammar — that's a
/// 200+-line dependency for a leaf schema crate. Instead we accept
/// the alphanumeric + `.`, `+`, `-` character class that covers every
/// SPDX identifier currently in the registry, with an optional
/// ` WITH <exception-id>` clause. Detailed conformance is left to
/// downstream linters.
fn is_plausible_spdx_identifier(s: &str) -> bool {
    if s.trim().is_empty() {
        return false;
    }
    // Static regex compile is safe — pattern is known good.
    let re = Regex::new(r"^[A-Za-z0-9.+\-]+( WITH [A-Za-z0-9.+\-]+)?$").expect("static regex");
    re.is_match(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spdx_shape_accepts_common_identifiers() {
        for id in [
            "Apache-2.0",
            "MIT",
            "GPL-3.0-only",
            "GPL-3.0-or-later",
            "BSD-3-Clause",
            "Apache-2.0 WITH LLVM-exception",
            "CC0-1.0",
        ] {
            assert!(is_plausible_spdx_identifier(id), "should accept {id}");
        }
    }

    #[test]
    fn spdx_shape_rejects_obvious_nonsense() {
        for id in ["", "   ", "not a license!", "GPL 3.0", "MIT/Apache"] {
            assert!(
                !is_plausible_spdx_identifier(id),
                "should reject {id:?}"
            );
        }
    }
}
