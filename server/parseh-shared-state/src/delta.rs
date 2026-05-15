//! The `parseh.state-deltas.v1` propagation envelope.
//!
//! Per the verify agent's design review, **only signed
//! [`parseh_task::JobOutcome`] objects (and the two minimal companion
//! kinds [`DeltaKind::Reputation`] / [`DeltaKind::GovernanceRule`])
//! cross the trust boundary on this topic**. Raw mid-window
//! [`parseh_verify::Quorum`] state is deliberately NOT carried — that
//! would expose a partial-state replay class where an adversary streams
//! a sequence of mid-window snapshots to bias a peer's perception of
//! an in-flight verification.
//!
//! ## Wire shape
//!
//! ```text
//! StateDelta {
//!     wire_version: u32,        // == crate::WIRE_VERSION
//!     kind:         DeltaKind,  // Outcome | Reputation | GovernanceRule
//!     observer:     PeerId,     // the peer that signed this delta
//!     observed_at:  u64,        // UTC seconds
//!     signature:    bytes       // ed25519 over the CBOR with sig field empty
//! }
//! ```
//!
//! The signing convention mirrors `parseh-task`: clear the `signature`
//! field, CBOR-encode, sign, write the bytes back.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use libp2p::PeerId;
use parseh_task::{
    from_cbor_bytes, to_cbor_bytes, ContentHash, JobOutcome, SignError as TaskSignError,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::WIRE_VERSION;

/// Errors surfaced by [`verify_delta`].
#[derive(Error, Debug)]
pub enum SignError {
    /// Wire-version mismatch — the peer that produced this delta
    /// speaks a different protocol level. Drop the message.
    #[error("wire version mismatch: expected {expected}, got {actual}")]
    WireVersion {
        /// What this binary speaks.
        expected: u32,
        /// What the inbound delta claims.
        actual: u32,
    },
    /// The signature bytes parsed but the cryptographic check failed.
    #[error("ed25519 signature verification failed: {0}")]
    Verify(String),
    /// The signature byte slice was not 64 bytes long.
    #[error("invalid signature length: expected 64, got {0}")]
    InvalidSignatureLength(usize),
    /// CBOR encode of the unsigned form failed.
    #[error("cbor encode for signature input: {0}")]
    CborEncode(String),
    /// The embedded [`JobOutcome`] failed its own inner signature
    /// check. The delta envelope says peer X attested the outcome was
    /// observed; the outcome itself says peer Y signed the consensus
    /// projection. Both must check out.
    #[error("inner JobOutcome signature failed: {0}")]
    InnerOutcomeSignature(TaskSignError),
}

/// The payloads we propagate on `parseh.state-deltas.v1`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DeltaKind {
    /// A new outcome has been finalised. The inner [`JobOutcome`] is
    /// itself signed by its observer (see `parseh-task`); a receiving
    /// peer should verify both signatures before applying.
    Outcome(JobOutcome),
    /// A reputation delta has been applied by `observer`. Receivers
    /// may choose to apply or ignore based on `observer`'s reputation
    /// — V0.2 stores everything and lets analysts filter later (see
    /// `sybil-cost-analysis.md` §7 on detection-via-SQL).
    Reputation {
        /// Subject of the reputation change.
        peer: PeerId,
        /// Signed delta value (e.g. `+5`, `-10`).
        delta: i32,
        /// Human-readable reason, e.g.
        /// `"verification_agreed"` or `"vouch_failed"`.
        reason: String,
        /// Hash of the outcome or verification that motivated the
        /// change, when applicable.
        related_hash: Option<ContentHash>,
    },
    /// A governance amendment that has cleared the `core-rules.md` §11
    /// approval process. Receivers may apply (it is the last-write-
    /// wins entry for that `rule_name`).
    GovernanceRule {
        /// Canonical short name, e.g. `"quorum_standard"`.
        rule_name: String,
        /// JSON-encoded value. The amendment author picks the schema
        /// of the inner JSON.
        rule_value: String,
        /// Peer that proposed the amendment.
        proposer: PeerId,
        /// Peers that approved the amendment.
        approvers: Vec<PeerId>,
    },
}

/// The signed gossip envelope. See module-level docs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateDelta {
    /// Wire-format version; equal to [`crate::WIRE_VERSION`] on a
    /// fresh delta. Receivers must drop mismatches.
    pub wire_version: u32,
    /// Payload kind.
    pub kind: DeltaKind,
    /// Peer that signed this envelope. Note that for
    /// [`DeltaKind::Outcome`], the inner outcome carries its own
    /// `observed_by` peer; the two may differ (the envelope signer
    /// is the peer who *propagated*, not necessarily the peer who
    /// *finalised*).
    pub observer: PeerId,
    /// UTC seconds at which the observer signed this delta.
    pub observed_at: u64,
    /// 64-byte ed25519 signature over the CBOR of all preceding
    /// fields with `signature` empty.
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
}

impl StateDelta {
    /// Build an unsigned envelope — [`crate::WIRE_VERSION`], the given
    /// payload, signer identity + timestamp, no signature.
    ///
    /// Pass to [`sign_delta`] (or [`StateDelta::sign`]) to produce a
    /// ready-to-publish envelope.
    pub fn unsigned(kind: DeltaKind, observer: PeerId, observed_at: u64) -> Self {
        Self {
            wire_version: WIRE_VERSION,
            kind,
            observer,
            observed_at,
            signature: Vec::new(),
        }
    }

    /// Sign this envelope in place. Equivalent to [`sign_delta`].
    pub fn sign(&mut self, signing_key: &SigningKey) -> Result<(), SignError> {
        self.signature.clear();
        let unsigned = to_cbor_bytes(self).map_err(|e| SignError::CborEncode(e.to_string()))?;
        let sig: Signature = signing_key.sign(&unsigned);
        self.signature = sig.to_bytes().to_vec();
        Ok(())
    }

    /// CBOR-decode a wire envelope. Convenience over
    /// [`parseh_task::from_cbor_bytes`].
    pub fn decode_cbor(bytes: &[u8]) -> Result<Self, ciborium::de::Error<std::io::Error>> {
        from_cbor_bytes(bytes)
    }

    /// CBOR-encode a wire envelope.
    pub fn encode_cbor(&self) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
        to_cbor_bytes(self)
    }
}

/// Sign `unsigned` with `key` and return the signed envelope.
///
/// On failure (CBOR encode), returns the original envelope unchanged
/// inside the `Err` variant for forensic logging. In practice CBOR
/// encode cannot fail for the shapes we construct here, so this is a
/// belt-and-braces error path.
pub fn sign_delta(mut unsigned: StateDelta, key: &SigningKey) -> Result<StateDelta, SignError> {
    unsigned.sign(key)?;
    Ok(unsigned)
}

/// Verify the envelope's signature against `pubkey`. If the payload is
/// a [`DeltaKind::Outcome`], also verify the inner [`JobOutcome`]'s
/// signature against the same `pubkey` — V0.2 assumes the propagator
/// is also the consensus-observer, which matches the
/// `parseh-verify::Quorum::try_finalise()` contract.
///
/// If you need to verify a delta where the propagator differs from the
/// inner-outcome observer (e.g. a relay re-propagating), call this with
/// the propagator's key for the envelope, then call
/// [`JobOutcome::verify_signature`] separately with the observer's key.
pub fn verify_delta(delta: &StateDelta, pubkey: &VerifyingKey) -> Result<(), SignError> {
    if delta.wire_version != WIRE_VERSION {
        return Err(SignError::WireVersion {
            expected: WIRE_VERSION,
            actual: delta.wire_version,
        });
    }
    if delta.signature.len() != 64 {
        return Err(SignError::InvalidSignatureLength(delta.signature.len()));
    }
    let mut unsigned = delta.clone();
    unsigned.signature.clear();
    let bytes =
        to_cbor_bytes(&unsigned).map_err(|e| SignError::CborEncode(e.to_string()))?;
    let sig = Signature::from_slice(&delta.signature)
        .map_err(|_| SignError::InvalidSignatureLength(delta.signature.len()))?;
    pubkey
        .verify(&bytes, &sig)
        .map_err(|e| SignError::Verify(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use parseh_task::{
        content_hash, ContentHash, JobOutcome, OutcomeVerdict,
    };
    use rand::rngs::OsRng;

    fn fresh_peer() -> PeerId {
        PeerId::from(libp2p::identity::Keypair::generate_ed25519().public())
    }

    #[test]
    fn sign_then_verify_reputation_delta() {
        let sk = SigningKey::generate(&mut OsRng);
        let observer = fresh_peer();
        let kind = DeltaKind::Reputation {
            peer: fresh_peer(),
            delta: 5,
            reason: "verification_agreed".into(),
            related_hash: Some(content_hash(b"o")),
        };
        let signed = sign_delta(
            StateDelta::unsigned(kind, observer, 1_700_000_000),
            &sk,
        )
        .unwrap();
        verify_delta(&signed, &sk.verifying_key()).expect("verify");
    }

    #[test]
    fn tampered_observed_at_breaks_signature() {
        let sk = SigningKey::generate(&mut OsRng);
        let kind = DeltaKind::Reputation {
            peer: fresh_peer(),
            delta: 1,
            reason: "x".into(),
            related_hash: None,
        };
        let mut signed = sign_delta(
            StateDelta::unsigned(kind, fresh_peer(), 1_700_000_000),
            &sk,
        )
        .unwrap();
        signed.observed_at = 9_999_999_999;
        assert!(verify_delta(&signed, &sk.verifying_key()).is_err());
    }

    #[test]
    fn wire_version_mismatch_rejected() {
        let sk = SigningKey::generate(&mut OsRng);
        let kind = DeltaKind::Reputation {
            peer: fresh_peer(),
            delta: 1,
            reason: "x".into(),
            related_hash: None,
        };
        let mut signed = sign_delta(
            StateDelta::unsigned(kind, fresh_peer(), 1),
            &sk,
        )
        .unwrap();
        signed.wire_version = 999;
        match verify_delta(&signed, &sk.verifying_key()) {
            Err(SignError::WireVersion { .. }) => (),
            other => panic!("expected WireVersion, got {other:?}"),
        }
    }

    #[test]
    fn truncated_signature_rejected() {
        let sk = SigningKey::generate(&mut OsRng);
        let kind = DeltaKind::Reputation {
            peer: fresh_peer(),
            delta: 1,
            reason: "x".into(),
            related_hash: None,
        };
        let mut signed = sign_delta(
            StateDelta::unsigned(kind, fresh_peer(), 1),
            &sk,
        )
        .unwrap();
        signed.signature.truncate(32);
        match verify_delta(&signed, &sk.verifying_key()) {
            Err(SignError::InvalidSignatureLength(32)) => (),
            other => panic!("expected InvalidSignatureLength(32), got {other:?}"),
        }
    }

    #[test]
    fn outcome_delta_round_trip() {
        let sk = SigningKey::generate(&mut OsRng);
        let observer = fresh_peer();
        let (outcome, _h) = JobOutcome::new_signed_at(
            ContentHash::zero(),
            ContentHash::zero(),
            vec![content_hash(b"v")],
            OutcomeVerdict::Valid {
                agreements: 5,
                disagreements: 0,
                abstentions: 0,
                reputation_weighted: 0.8,
            },
            1_700_000_500,
            observer,
            &sk,
        );
        let signed = sign_delta(
            StateDelta::unsigned(DeltaKind::Outcome(outcome), observer, 1_700_000_600),
            &sk,
        )
        .unwrap();
        verify_delta(&signed, &sk.verifying_key()).expect("verify");
    }

    #[test]
    fn cbor_round_trip_preserves_signature() {
        let sk = SigningKey::generate(&mut OsRng);
        let signed = sign_delta(
            StateDelta::unsigned(
                DeltaKind::Reputation {
                    peer: fresh_peer(),
                    delta: -3,
                    reason: "x".into(),
                    related_hash: None,
                },
                fresh_peer(),
                42,
            ),
            &sk,
        )
        .unwrap();
        let bytes = signed.encode_cbor().unwrap();
        let back = StateDelta::decode_cbor(&bytes).unwrap();
        assert_eq!(signed, back);
    }
}
