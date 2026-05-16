//! [`JobSpec`] — the signed task specification a client submits to the
//! network to request work.
//!
//! Authoring flow:
//!
//! ```text
//!     JobSpec::new_signed(kind, inputs, service, sensitive, submitter, &sk)
//!         └── builds struct with empty signature
//!         └── CBOR-encodes
//!         └── ed25519-signs the encoding
//!         └── writes signature into the struct
//!         └── re-encodes + SHA-256 → ContentHash
//!         └── returns (spec, content_hash)
//! ```
//!
//! Verification mirrors this: clear `signature`, re-encode, `verify_bytes`.

use crate::{
    content_hash, from_cbor_bytes, sign_bytes, to_cbor_bytes, verify_bytes, ContentHash,
    SignError, WIRE_VERSION,
};
use libp2p::PeerId;
use parseh_core::ServiceKind;
use serde::{Deserialize, Serialize};

/// What kind of work is being requested.
///
/// Kept coarse-grained at V0.2 — each variant gates a different
/// [`crate::VerifierMethod`] requirement downstream in `parseh-verify`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobKind {
    /// LLM inference. [`JobInputs::prompt_text`] must be set; the
    /// matching verifier honours [`JobInputs::seed`].
    Inference,
    /// Bandwidth relay. V0.3+ Verifier semantics TBD.
    Relay,
    /// Storage of a small content-addressed blob.
    Storage,
}

/// Inputs carried by a [`JobSpec`].
///
/// Field optionality is by `JobKind` — see each field's doc comment.
/// All `Option<T>` defaults to `None` so older clients can omit fields
/// they do not understand without breaking the CBOR decode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobInputs {
    /// Plain-text prompt for `Inference` jobs. `None` for other kinds.
    pub prompt_text: Option<String>,
    /// Deterministic seed for inference reproducibility. Required when
    /// `JobKind = Inference`; the matching verifier **must** honour
    /// this seed so deterministic-mode verification (the V0.2 default)
    /// produces byte-equal completions.
    pub seed: Option<u64>,
    /// Max token budget for inference responses. `None` ⇒ service default.
    pub max_tokens: Option<u32>,
    /// Out-of-band inputs by content hash, e.g. large prompts shipped
    /// over IPFS or `/parseh/job/2.0.0`'s `JobBodyFetch`.
    pub content_refs: Vec<ContentHash>,
}

impl JobInputs {
    /// Convenience: a bare inference-prompt input with a seed and no
    /// out-of-band refs. Mostly for tests and short examples.
    pub fn inference_prompt(prompt: impl Into<String>, seed: u64) -> Self {
        Self {
            prompt_text: Some(prompt.into()),
            seed: Some(seed),
            max_tokens: None,
            content_refs: Vec::new(),
        }
    }
}

/// A signed task specification submitted by a client to the network.
///
/// **Identity:** the spec's [`ContentHash`] (over the fully-signed CBOR)
/// is the `task_id` referenced by [`crate::JobResult::spec_hash`] and
/// [`crate::JobOutcome::spec_hash`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// Wire-format version; always equal to [`crate::WIRE_VERSION`] for
    /// freshly-built specs. Mismatching peers drop rather than parse.
    pub wire_version: u32,
    /// Kind of work being requested.
    pub kind: JobKind,
    /// Inputs to the job.
    pub inputs: JobInputs,
    /// Service-kind hint for capability matching on the `parseh.caps.v1`
    /// topic (executors filter by this).
    pub service: ServiceKind,
    /// `sensitive = true` requests use the 9-of-15 quorum per
    /// the project notes §3.1. Insensitive requests use
    /// the M-of-N default (3-of-5).
    pub sensitive: bool,
    /// Wall-clock UTC seconds at submission time. Embedded so peers can
    /// detect stale or replayed specs.
    pub submitted_at: u64,
    /// libp2p `PeerId` of the submitter. The crate-wide `libp2p` feature
    /// `serde` (enabled at the workspace level) makes `PeerId` round-trip
    /// through CBOR.
    pub submitter: PeerId,
    /// Signature over the canonical CBOR of all preceding fields, by the
    /// submitter's ed25519 keypair. Empty (`len = 0`) on the unsigned
    /// pre-sign form.
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
}

impl JobSpec {
    /// Build a [`JobSpec`], signing with the submitter's keypair.
    ///
    /// Returns the signed object **and** its [`ContentHash`]. Use the
    /// hash as the `task_id` everywhere downstream — it is uniquely
    /// determined by the signed bytes.
    pub fn new_signed(
        kind: JobKind,
        inputs: JobInputs,
        service: ServiceKind,
        sensitive: bool,
        submitter: PeerId,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> (Self, ContentHash) {
        Self::new_signed_at(
            kind,
            inputs,
            service,
            sensitive,
            now_unix(),
            submitter,
            signing_key,
        )
    }

    /// Like [`Self::new_signed`] but with an explicit `submitted_at` —
    /// useful for deterministic tests that need to assert content hashes.
    pub fn new_signed_at(
        kind: JobKind,
        inputs: JobInputs,
        service: ServiceKind,
        sensitive: bool,
        submitted_at: u64,
        submitter: PeerId,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> (Self, ContentHash) {
        let mut spec = Self {
            wire_version: WIRE_VERSION,
            kind,
            inputs,
            service,
            sensitive,
            submitted_at,
            submitter,
            signature: Vec::new(),
        };
        // Sign the unsigned form (signature field is empty `Vec<u8>`).
        let unsigned = to_cbor_bytes(&spec).expect("JobSpec CBOR encode cannot fail for owned types");
        spec.signature = sign_bytes(signing_key, &unsigned).to_vec();
        let hash = spec.content_hash();
        (spec, hash)
    }

    /// Verify the embedded signature against the submitter's public key.
    pub fn verify_signature(
        &self,
        submitter_pubkey: &ed25519_dalek::VerifyingKey,
    ) -> Result<(), SignError> {
        // Rebuild the unsigned form (zero out the signature field).
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        let unsigned_cbor = to_cbor_bytes(&unsigned)
            .map_err(|e| SignError::Verify(format!("cbor encode: {e}")))?;
        verify_bytes(submitter_pubkey, &unsigned_cbor, &self.signature)
    }

    /// Content hash of the **signed** spec (over its full CBOR
    /// encoding). This is the canonical task identifier.
    pub fn content_hash(&self) -> ContentHash {
        let cbor = to_cbor_bytes(self).expect("JobSpec CBOR encode cannot fail for owned types");
        content_hash(&cbor)
    }

    /// Decode a `JobSpec` from a CBOR byte slice. Convenience around
    /// [`crate::from_cbor_bytes`].
    pub fn decode_cbor(bytes: &[u8]) -> Result<Self, ciborium::de::Error<std::io::Error>> {
        from_cbor_bytes(bytes)
    }
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn fresh() -> (SigningKey, PeerId) {
        let sk = SigningKey::generate(&mut OsRng);
        // Generate a separate libp2p identity for the PeerId. The spec
        // signs with `sk` (the dalek key); the PeerId is only an opaque
        // identifier in the wire types.
        let id_kp = libp2p::identity::Keypair::generate_ed25519();
        (sk, PeerId::from(id_kp.public()))
    }

    #[test]
    fn sign_then_self_verify() {
        let (sk, peer) = fresh();
        let (spec, hash) = JobSpec::new_signed_at(
            JobKind::Inference,
            JobInputs::inference_prompt("hello", 42),
            ServiceKind::Inference,
            false,
            1_700_000_000,
            peer,
            &sk,
        );
        spec.verify_signature(&sk.verifying_key()).expect("sig");
        assert_eq!(hash, spec.content_hash());
    }

    #[test]
    fn tampering_changes_hash_and_breaks_signature() {
        let (sk, peer) = fresh();
        let (mut spec, original_hash) = JobSpec::new_signed_at(
            JobKind::Inference,
            JobInputs::inference_prompt("hello", 42),
            ServiceKind::Inference,
            false,
            1_700_000_000,
            peer,
            &sk,
        );
        spec.inputs.prompt_text = Some("goodbye".into());
        assert_ne!(original_hash, spec.content_hash());
        spec.verify_signature(&sk.verifying_key()).unwrap_err();
    }

    #[test]
    fn inputs_default_optional_fields_are_none() {
        let i = JobInputs::inference_prompt("x", 1);
        assert!(i.max_tokens.is_none());
        assert!(i.content_refs.is_empty());
    }
}
