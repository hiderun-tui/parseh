//! [`JobOutcome`] — the aggregated result of one full task lifecycle.
//!
//! Aggregates a [`crate::JobSpec`], its [`crate::JobResult`], and N
//! [`crate::JobVerification`]s into a single signed envelope.
//!
//! ## Why is the outcome signed at V0.2?
//!
//! The architecture doc (`distributed-coordination-architecture.md`
//! §3.1) says the outcome is "not signed by a single peer" — it is a
//! deterministic function of the underlying verifications, so any peer
//! can reconstruct it. That argument holds for the consensus *value*
//! but not for the *projection* a particular node persists to its
//! local `parseh-shared-state` (`docs/v0-2/architecture-and-state-
//! machines.md` §4 lists `parseh.state-deltas.v1` envelopes as signed).
//! At V0.2 we sign the outcome with the **observing** node's key so
//! shared-state replicas can reject forged deltas. At V0.3+ the chain
//! validates state transitions and this per-node signature becomes
//! redundant — but adding it now is forward-compatible and lets V0.2
//! reuse the same `verify_signature` machinery as the other three
//! types.

use crate::{
    content_hash, from_cbor_bytes, sign_bytes, to_cbor_bytes, verify_bytes, ContentHash,
    SignError, WIRE_VERSION,
};
use libp2p::PeerId;
use serde::{Deserialize, Serialize};

/// The consensus verdict reached at the end of one task lifecycle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum OutcomeVerdict {
    /// M-of-N quorum reached. The counts let downstream code recompute
    /// the reputation-weighted score and audit against the listed
    /// verification hashes.
    Valid {
        /// Number of `Agreed` verdicts that contributed.
        agreements: u32,
        /// Number of `Disagreed` verdicts that contributed.
        disagreements: u32,
        /// Number of `Abstained` verdicts that contributed.
        abstentions: u32,
        /// Reputation-weighted score in `[0, 1]`. Reputation comes from
        /// `parseh-shared-state` and is opaque to this crate — we
        /// just carry the float.
        reputation_weighted: f64,
    },
    /// Verification ended in dispute. The listed peers raised conflicting
    /// verdicts; the next step is the `parseh.tasks.v1.dispute` topic.
    Disputed {
        /// PeerIds of verifiers whose verdicts conflict.
        disputers: Vec<PeerId>,
    },
    /// M-of-N never reached before `T_max`. No slashing, no settlement;
    /// the task is shelved and a fresh `JobSpec` (different `task_id`)
    /// is the only path forward.
    Indeterminate,
}

/// The consensus result aggregating the JobSpec + JobResult + N
/// JobVerifications. Signed by the node that observed quorum and is
/// writing the outcome to local `parseh-shared-state`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobOutcome {
    /// Wire-format version; always equal to [`crate::WIRE_VERSION`].
    pub wire_version: u32,
    /// `ContentHash` of the originating `JobSpec` (the task identifier).
    pub spec_hash: ContentHash,
    /// `ContentHash` of the `JobResult` that quorum verified.
    pub result_hash: ContentHash,
    /// Hashes of every `JobVerification` that contributed. Order is
    /// not significant but is preserved across re-encodes.
    pub verification_hashes: Vec<ContentHash>,
    /// The verdict.
    pub verdict: OutcomeVerdict,
    /// Wall-clock UTC seconds at which the observing node finalised
    /// this outcome.
    pub finalised_at: u64,
    /// At V0.2 nobody is "consensus authority" — the `JobOutcome` is
    /// signed by the consensus-observing node that wrote it to local
    /// `parseh-shared-state`. That node is identified here. At V0.3+
    /// this becomes a chain-validated state transition and `observed_by`
    /// becomes a block-producer attestation.
    pub observed_by: PeerId,
    /// Signature over the canonical CBOR of all preceding fields, by
    /// the `observed_by` peer's ed25519 keypair.
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
}

impl JobOutcome {
    /// Build a [`JobOutcome`], signing with the observing node's keypair.
    /// Returns the signed object and its [`ContentHash`].
    pub fn new_signed(
        spec_hash: ContentHash,
        result_hash: ContentHash,
        verification_hashes: Vec<ContentHash>,
        verdict: OutcomeVerdict,
        observed_by: PeerId,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> (Self, ContentHash) {
        Self::new_signed_at(
            spec_hash,
            result_hash,
            verification_hashes,
            verdict,
            now_unix(),
            observed_by,
            signing_key,
        )
    }

    /// Like [`Self::new_signed`] but with an explicit `finalised_at`.
    pub fn new_signed_at(
        spec_hash: ContentHash,
        result_hash: ContentHash,
        verification_hashes: Vec<ContentHash>,
        verdict: OutcomeVerdict,
        finalised_at: u64,
        observed_by: PeerId,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> (Self, ContentHash) {
        let mut o = Self {
            wire_version: WIRE_VERSION,
            spec_hash,
            result_hash,
            verification_hashes,
            verdict,
            finalised_at,
            observed_by,
            signature: Vec::new(),
        };
        let unsigned = to_cbor_bytes(&o).expect("JobOutcome CBOR encode cannot fail");
        o.signature = sign_bytes(signing_key, &unsigned).to_vec();
        let hash = o.content_hash();
        (o, hash)
    }

    /// Verify the embedded signature against the observing node's
    /// public key.
    pub fn verify_signature(
        &self,
        observer_pubkey: &ed25519_dalek::VerifyingKey,
    ) -> Result<(), SignError> {
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        let unsigned_cbor = to_cbor_bytes(&unsigned)
            .map_err(|e| SignError::Verify(format!("cbor encode: {e}")))?;
        verify_bytes(observer_pubkey, &unsigned_cbor, &self.signature)
    }

    /// Content hash of the signed outcome.
    pub fn content_hash(&self) -> ContentHash {
        let cbor = to_cbor_bytes(self).expect("JobOutcome CBOR encode cannot fail");
        content_hash(&cbor)
    }

    /// Decode a `JobOutcome` from a CBOR byte slice.
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
    fn valid_verdict_signs_and_verifies() {
        let (sk, peer) = fresh();
        let (o, _h) = JobOutcome::new_signed_at(
            ContentHash::zero(),
            ContentHash::zero(),
            vec![content_hash(b"v1"), content_hash(b"v2"), content_hash(b"v3")],
            OutcomeVerdict::Valid {
                agreements: 3,
                disagreements: 0,
                abstentions: 0,
                reputation_weighted: 0.9876,
            },
            1_700_000_300,
            peer,
            &sk,
        );
        o.verify_signature(&sk.verifying_key()).expect("sig");
    }

    #[test]
    fn disputed_verdict_carries_peer_list() {
        let (sk, peer) = fresh();
        let id_kp = libp2p::identity::Keypair::generate_ed25519();
        let other = PeerId::from(id_kp.public());
        let (o, _) = JobOutcome::new_signed_at(
            ContentHash::zero(),
            ContentHash::zero(),
            vec![],
            OutcomeVerdict::Disputed {
                disputers: vec![other],
            },
            0,
            peer,
            &sk,
        );
        o.verify_signature(&sk.verifying_key()).expect("sig");
    }

    #[test]
    fn indeterminate_signs_too() {
        let (sk, peer) = fresh();
        let (o, _) = JobOutcome::new_signed_at(
            ContentHash::zero(),
            ContentHash::zero(),
            vec![],
            OutcomeVerdict::Indeterminate,
            0,
            peer,
            &sk,
        );
        o.verify_signature(&sk.verifying_key()).expect("sig");
    }
}
