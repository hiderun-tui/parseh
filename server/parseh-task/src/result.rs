//! [`JobResult`] — a signed completion produced by an executor in
//! response to a [`crate::JobSpec`].
//!
//! The result references its spec by `spec_hash` (the spec's
//! [`ContentHash`]), so a result cannot be re-used to satisfy a
//! different spec.
//!
//! ## Signing scope (design decision)
//!
//! The signature covers the **entire CBOR encoding of the result**,
//! including the embedded `result_payload` bytes. We considered signing
//! `content_hash(result_payload)` only — which would let a peer pass
//! the payload by reference (out-of-band fetch) and still verify the
//! attestation — but that introduces a chicken-and-egg problem during
//! M-of-N verification: every verifier must fetch the same payload to
//! re-execute, and they cannot cross-check that they fetched the same
//! bytes without an extra round-trip. Keeping the payload in-band for
//! V0.2 keeps the verification path single-hop. The 1 MiB cap on
//! [`crate::MAX_MESSAGE_SIZE_BYTES`] is what makes this affordable;
//! V0.3+ will revisit when we accept larger completions.

use crate::{
    content_hash, from_cbor_bytes, sign_bytes, to_cbor_bytes, verify_bytes,
    verification::VerifierMethod, ContentHash, SignError, WIRE_VERSION,
};
use libp2p::PeerId;
use serde::{Deserialize, Serialize};

/// Executor-supplied metadata that describes how a result was produced
/// and how it should be verified.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultMeta {
    /// How the executor declares this result should be verified.
    /// Verifiers may refuse a result that asks for a method they cannot
    /// honour (e.g. they have no spot-check rig configured).
    pub verifier_method: VerifierMethod,
    /// Wall-clock execution time on the executor, milliseconds.
    pub execution_time_ms: u64,
    /// Identifier of the model used, if applicable (e.g. `"qwen2.5:7b"`).
    /// `None` for non-inference jobs.
    pub model_used: Option<String>,
    /// Number of inference tokens produced. Only meaningful for
    /// `JobKind::Inference`.
    pub inference_token_count: Option<u32>,
}

/// A signed completion to a [`crate::JobSpec`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResult {
    /// Wire-format version; always equal to [`crate::WIRE_VERSION`].
    pub wire_version: u32,
    /// `ContentHash` of the [`crate::JobSpec`] this result responds to.
    /// This is the task identifier that ties together all four
    /// lifecycle objects.
    pub spec_hash: ContentHash,
    /// libp2p `PeerId` of the executor that produced this result.
    pub executor: PeerId,
    /// Wall-clock UTC seconds at which the executor finalised the result.
    pub executed_at: u64,
    /// Result metadata (verifier method, timings, model id, ...).
    pub result_meta: ResultMeta,
    /// CBOR-encoded result body (e.g. a serialised inference completion).
    /// We keep this an opaque `Vec<u8>` so different `JobKind`s can carry
    /// different payload shapes without bloating this struct's schema.
    #[serde(with = "serde_bytes")]
    pub result_payload: Vec<u8>,
    /// Signature over the canonical CBOR of all preceding fields, by
    /// the executor's ed25519 keypair. Empty (`len = 0`) on the
    /// unsigned pre-sign form.
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
}

impl JobResult {
    /// Build a [`JobResult`], signing with the executor's keypair.
    /// Returns the signed object and its [`ContentHash`].
    pub fn new_signed(
        spec_hash: ContentHash,
        executor: PeerId,
        result_meta: ResultMeta,
        result_payload: Vec<u8>,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> (Self, ContentHash) {
        Self::new_signed_at(
            spec_hash,
            executor,
            now_unix(),
            result_meta,
            result_payload,
            signing_key,
        )
    }

    /// Like [`Self::new_signed`] but with an explicit `executed_at`.
    /// Useful for deterministic tests.
    pub fn new_signed_at(
        spec_hash: ContentHash,
        executor: PeerId,
        executed_at: u64,
        result_meta: ResultMeta,
        result_payload: Vec<u8>,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> (Self, ContentHash) {
        let mut r = Self {
            wire_version: WIRE_VERSION,
            spec_hash,
            executor,
            executed_at,
            result_meta,
            result_payload,
            signature: Vec::new(),
        };
        let unsigned = to_cbor_bytes(&r).expect("JobResult CBOR encode cannot fail");
        r.signature = sign_bytes(signing_key, &unsigned).to_vec();
        let hash = r.content_hash();
        (r, hash)
    }

    /// Verify the embedded signature against the executor's public key.
    pub fn verify_signature(
        &self,
        executor_pubkey: &ed25519_dalek::VerifyingKey,
    ) -> Result<(), SignError> {
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        let unsigned_cbor = to_cbor_bytes(&unsigned)
            .map_err(|e| SignError::Verify(format!("cbor encode: {e}")))?;
        verify_bytes(executor_pubkey, &unsigned_cbor, &self.signature)
    }

    /// Content hash of the signed result (over its full CBOR encoding).
    pub fn content_hash(&self) -> ContentHash {
        let cbor = to_cbor_bytes(self).expect("JobResult CBOR encode cannot fail");
        content_hash(&cbor)
    }

    /// Decode a `JobResult` from a CBOR byte slice.
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
    use crate::VerifierMethod;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn fresh() -> (SigningKey, PeerId) {
        let sk = SigningKey::generate(&mut OsRng);
        let id_kp = libp2p::identity::Keypair::generate_ed25519();
        (sk, PeerId::from(id_kp.public()))
    }

    #[test]
    fn sign_then_self_verify() {
        let (sk, peer) = fresh();
        let meta = ResultMeta {
            verifier_method: VerifierMethod::Deterministic,
            execution_time_ms: 1234,
            model_used: Some("qwen2.5:7b".into()),
            inference_token_count: Some(42),
        };
        let (r, h) = JobResult::new_signed_at(
            ContentHash::zero(),
            peer,
            1_700_000_100,
            meta,
            b"completion text".to_vec(),
            &sk,
        );
        r.verify_signature(&sk.verifying_key()).expect("sig");
        assert_eq!(h, r.content_hash());
    }

    #[test]
    fn tampering_payload_breaks_signature() {
        let (sk, peer) = fresh();
        let meta = ResultMeta {
            verifier_method: VerifierMethod::Deterministic,
            execution_time_ms: 1,
            model_used: None,
            inference_token_count: None,
        };
        let (mut r, _) = JobResult::new_signed_at(
            ContentHash::zero(),
            peer,
            0,
            meta,
            b"original".to_vec(),
            &sk,
        );
        r.result_payload = b"tampered".to_vec();
        r.verify_signature(&sk.verifying_key()).unwrap_err();
    }
}
