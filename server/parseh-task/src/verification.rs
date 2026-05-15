//! [`JobVerification`] — a signed attestation by a verifier peer about
//! a particular [`crate::JobResult`].
//!
//! The full M-of-N protocol lives in `parseh-verify`. This crate only
//! defines the wire object the protocol exchanges.

use crate::{
    content_hash, from_cbor_bytes, sign_bytes, to_cbor_bytes, verify_bytes, ContentHash,
    SignError, WIRE_VERSION,
};
use libp2p::PeerId;
use serde::{Deserialize, Serialize};

/// How a verifier checks (or claims to check) a result.
///
/// Only `Deterministic` is wired through end-to-end at V0.2. The other
/// variants are listed so the wire format does not need a breaking
/// change when `parseh-verify` ships them in V0.3+.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifierMethod {
    /// Re-execute with the same seed; byte-equal output expected.
    /// Requires `JobInputs::seed = Some(_)` and a pinned runtime.
    Deterministic,
    /// Re-execute N short prefix chunks and check semantic similarity.
    /// **V0.3+** — variant reserved.
    SpotCheck,
    /// Statistical re-execution: rerun across a sample, check output
    /// distribution. **V0.3+** — variant reserved.
    Statistical,
}

/// The verdict a verifier publishes after applying its
/// [`VerifierMethod`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifierVerdict {
    /// The verifier reproduced the result (or otherwise agrees).
    Agreed,
    /// The verifier disagrees. The full evidence (diff, alternative
    /// completion, hardware fingerprint) is stored out-of-band and
    /// referenced by hash; gossiping the evidence inline would breach
    /// the 1 MiB cap and produce too-large gossipsub envelopes.
    Disagreed {
        /// `ContentHash` of the out-of-band evidence blob.
        evidence_hash: ContentHash,
    },
    /// The verifier refused or was unable to verify (e.g. it could not
    /// honour the declared `VerifierMethod`). Counts against neither
    /// agree nor disagree quorum.
    Abstained,
}

/// A signed verification attestation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobVerification {
    /// Wire-format version; always equal to [`crate::WIRE_VERSION`].
    pub wire_version: u32,
    /// `ContentHash` of the [`crate::JobResult`] being verified.
    pub result_hash: ContentHash,
    /// libp2p `PeerId` of the verifier.
    pub verifier: PeerId,
    /// The verifier's verdict.
    pub verdict: VerifierVerdict,
    /// The method the verifier actually applied (may differ from the
    /// method the executor *declared* in `ResultMeta.verifier_method` —
    /// downstream code decides whether to count a mismatched method).
    pub method_used: VerifierMethod,
    /// Wall-clock UTC seconds at which the verifier finalised the
    /// verdict.
    pub verified_at: u64,
    /// Signature over the canonical CBOR of all preceding fields, by
    /// the verifier's ed25519 keypair.
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
}

impl JobVerification {
    /// Build a [`JobVerification`], signing with the verifier's keypair.
    /// Returns the signed object and its [`ContentHash`].
    pub fn new_signed(
        result_hash: ContentHash,
        verifier: PeerId,
        verdict: VerifierVerdict,
        method_used: VerifierMethod,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> (Self, ContentHash) {
        Self::new_signed_at(
            result_hash,
            verifier,
            verdict,
            method_used,
            now_unix(),
            signing_key,
        )
    }

    /// Like [`Self::new_signed`] but with an explicit `verified_at`.
    pub fn new_signed_at(
        result_hash: ContentHash,
        verifier: PeerId,
        verdict: VerifierVerdict,
        method_used: VerifierMethod,
        verified_at: u64,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> (Self, ContentHash) {
        let mut v = Self {
            wire_version: WIRE_VERSION,
            result_hash,
            verifier,
            verdict,
            method_used,
            verified_at,
            signature: Vec::new(),
        };
        let unsigned = to_cbor_bytes(&v).expect("JobVerification CBOR encode cannot fail");
        v.signature = sign_bytes(signing_key, &unsigned).to_vec();
        let hash = v.content_hash();
        (v, hash)
    }

    /// Verify the embedded signature against the verifier's public key.
    pub fn verify_signature(
        &self,
        verifier_pubkey: &ed25519_dalek::VerifyingKey,
    ) -> Result<(), SignError> {
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        let unsigned_cbor = to_cbor_bytes(&unsigned)
            .map_err(|e| SignError::Verify(format!("cbor encode: {e}")))?;
        verify_bytes(verifier_pubkey, &unsigned_cbor, &self.signature)
    }

    /// Content hash of the signed verification.
    pub fn content_hash(&self) -> ContentHash {
        let cbor = to_cbor_bytes(self).expect("JobVerification CBOR encode cannot fail");
        content_hash(&cbor)
    }

    /// Decode a `JobVerification` from a CBOR byte slice.
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
        let id_kp = libp2p::identity::Keypair::generate_ed25519();
        (sk, PeerId::from(id_kp.public()))
    }

    #[test]
    fn agreed_verdict_signs_and_verifies() {
        let (sk, peer) = fresh();
        let (v, _h) = JobVerification::new_signed_at(
            ContentHash::zero(),
            peer,
            VerifierVerdict::Agreed,
            VerifierMethod::Deterministic,
            1_700_000_200,
            &sk,
        );
        v.verify_signature(&sk.verifying_key()).expect("sig");
    }

    #[test]
    fn disagreed_carries_evidence_hash() {
        let (sk, peer) = fresh();
        let ev = content_hash(b"diff blob");
        let (v, _h) = JobVerification::new_signed_at(
            ContentHash::zero(),
            peer,
            VerifierVerdict::Disagreed { evidence_hash: ev },
            VerifierMethod::Deterministic,
            0,
            &sk,
        );
        v.verify_signature(&sk.verifying_key()).expect("sig");
        match v.verdict {
            VerifierVerdict::Disagreed { evidence_hash } => {
                assert_eq!(evidence_hash, ev)
            }
            _ => panic!("expected Disagreed"),
        }
    }

    #[test]
    fn abstain_signs_too() {
        let (sk, peer) = fresh();
        let (v, _) = JobVerification::new_signed_at(
            ContentHash::zero(),
            peer,
            VerifierVerdict::Abstained,
            VerifierMethod::SpotCheck,
            0,
            &sk,
        );
        v.verify_signature(&sk.verifying_key()).expect("sig");
    }
}
