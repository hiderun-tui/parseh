//! `/parseh/state-sync/1.0.0` — request-response wire types.
//!
//! ## Why this protocol exists
//!
//! The chaos harness (`server/parseh-chaos/`) proved empirically that
//! V0.2 has **no anti-entropy / state-sync mechanism**. After a network
//! partition heals, the formerly-isolated minority does not catch up to
//! outcomes finalised during the partition window: gossipsub's IHAVE
//! cache is too short (200 ms heartbeat × `mcache_len` ≈ a few seconds)
//! to cover meaningful partition durations, and there is no out-of-band
//! state-delta resend. Safety is preserved (the minority correctly
//! stalls during the partition); the gap is in **liveness recovery**.
//!
//! `/parseh/state-sync/1.0.0` closes that gap with a pragmatic
//! "since-timestamp pull": a peer that detects it may have missed
//! outcomes asks a better-connected peer for every outcome finalised at
//! or after a conservative cutoff. The full Merkle-root anti-entropy
//! design is V0.3+ work — see the project notes.
//!
//! ## Trust model
//!
//! Each [`JobOutcome`] carried in a [`StateSyncResponse`] is **already
//! individually signed by its observing peer**. The requester
//! re-verifies every inner signature before persisting (the application
//! layer owns the observer-key directory; this crate only carries the
//! bytes). A malicious responder can therefore *withhold* or *reorder*
//! outcomes, but it **cannot forge** one: a fabricated outcome fails the
//! inner ed25519 check against the claimed `observed_by` peer's key.
//!
//! The request itself is signed by the requester so a responder can
//! reject a malformed/forged request before doing any disk work (a cheap
//! DoS guard) and so the responder can rate-limit per requester identity.

use crate::{
    from_cbor_bytes, sign_bytes, to_cbor_bytes, verify_bytes, JobOutcome, SignError,
    WIRE_VERSION,
};
use libp2p::PeerId;
use serde::{Deserialize, Serialize};

/// Hard ceiling the responder clamps `max_outcomes` to, regardless of
/// what the requester asked for. A malicious requester cannot ask for
/// "everything"; the worst it can do is pull this many outcomes per
/// answered request (and it is additionally rate-limited per identity by
/// the miner).
pub const STATE_SYNC_HARD_CEILING: u32 = 500;

/// Request: "give me outcomes I might have missed."
///
/// Signed by the requester so the responder can (1) reject a forged
/// request before touching disk and (2) rate-limit per requester
/// identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateSyncRequest {
    /// Wire-format version; always equal to [`crate::WIRE_VERSION`].
    /// Mismatching peers drop the request rather than mis-parse it.
    pub wire_version: u32,
    /// Only outcomes finalised at or after this Unix-UTC second.
    ///
    /// The requester computes this as `(now - max_partition_window)` or
    /// the timestamp of its most-recent locally-known outcome,
    /// whichever is **older** — being generous is cheap (the responder
    /// clamps the count anyway), missing outcomes is not.
    pub since_unix: u64,
    /// Cap the response so a malicious requester can't ask for
    /// everything. The responder additionally clamps this to
    /// [`STATE_SYNC_HARD_CEILING`].
    pub max_outcomes: u32,
    /// The requesting peer. The `signature` below must verify against
    /// this peer's ed25519 key.
    pub requester: PeerId,
    /// Wall-clock UTC seconds at which the requester signed this.
    pub signed_at: u64,
    /// Requester ed25519 signature over the CBOR of all preceding
    /// fields with `signature` empty (same convention as the other
    /// wire types — see crate docs).
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
}

impl StateSyncRequest {
    /// Build and sign a request with the requester's keypair.
    pub fn new_signed(
        since_unix: u64,
        max_outcomes: u32,
        requester: PeerId,
        signed_at: u64,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> Self {
        let mut r = Self {
            wire_version: WIRE_VERSION,
            since_unix,
            max_outcomes,
            requester,
            signed_at,
            signature: Vec::new(),
        };
        let unsigned =
            to_cbor_bytes(&r).expect("StateSyncRequest CBOR encode cannot fail");
        r.signature = sign_bytes(signing_key, &unsigned).to_vec();
        r
    }

    /// Verify the requester signature. Callers MUST do this before
    /// doing any work on behalf of the request (cheap DoS guard).
    pub fn verify_signature(
        &self,
        requester_pubkey: &ed25519_dalek::VerifyingKey,
    ) -> Result<(), SignError> {
        if self.wire_version != WIRE_VERSION {
            return Err(SignError::Verify(format!(
                "wire version mismatch: expected {WIRE_VERSION}, got {}",
                self.wire_version
            )));
        }
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        let unsigned_cbor = to_cbor_bytes(&unsigned)
            .map_err(|e| SignError::Verify(format!("cbor encode: {e}")))?;
        verify_bytes(requester_pubkey, &unsigned_cbor, &self.signature)
    }

    /// CBOR-decode a request from a byte slice.
    pub fn decode_cbor(bytes: &[u8]) -> Result<Self, ciborium::de::Error<std::io::Error>> {
        from_cbor_bytes(bytes)
    }

    /// CBOR-encode this request.
    pub fn encode_cbor(&self) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
        to_cbor_bytes(self)
    }
}

/// Response: signed outcomes the responder has that the requester might
/// not. Each [`JobOutcome`] is already individually signed by its
/// observer; the requester re-verifies every inner signature before
/// persisting (it does **not** trust this envelope's framing).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateSyncResponse {
    /// Wire-format version; always equal to [`crate::WIRE_VERSION`].
    pub wire_version: u32,
    /// The outcomes. Order is not significant and is NOT trusted — the
    /// requester re-verifies each inner signature regardless.
    pub outcomes: Vec<JobOutcome>,
    /// `true` iff more outcomes exist past `max_outcomes` (or past the
    /// hard ceiling). The requester may issue a follow-up request with
    /// a later `since_unix` to page forward.
    pub truncated: bool,
    /// The responding peer. The `signature` must verify against this
    /// peer's ed25519 key. NOTE: a passing envelope signature only
    /// proves *who answered*, not that the outcomes are genuine — the
    /// inner per-outcome signatures carry that proof.
    pub responder: PeerId,
    /// Wall-clock UTC seconds at which the responder signed this.
    pub signed_at: u64,
    /// Responder ed25519 signature over the CBOR of all preceding
    /// fields with `signature` empty.
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
}

impl StateSyncResponse {
    /// Build and sign a response with the responder's keypair.
    pub fn new_signed(
        outcomes: Vec<JobOutcome>,
        truncated: bool,
        responder: PeerId,
        signed_at: u64,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> Self {
        let mut r = Self {
            wire_version: WIRE_VERSION,
            outcomes,
            truncated,
            responder,
            signed_at,
            signature: Vec::new(),
        };
        let unsigned =
            to_cbor_bytes(&r).expect("StateSyncResponse CBOR encode cannot fail");
        r.signature = sign_bytes(signing_key, &unsigned).to_vec();
        r
    }

    /// Verify the responder envelope signature. This proves *who*
    /// answered; it does NOT prove the outcomes are genuine — the
    /// caller MUST additionally call [`JobOutcome::verify_signature`]
    /// on every element of [`Self::outcomes`] against the respective
    /// observer's key before persisting.
    pub fn verify_signature(
        &self,
        responder_pubkey: &ed25519_dalek::VerifyingKey,
    ) -> Result<(), SignError> {
        if self.wire_version != WIRE_VERSION {
            return Err(SignError::Verify(format!(
                "wire version mismatch: expected {WIRE_VERSION}, got {}",
                self.wire_version
            )));
        }
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        let unsigned_cbor = to_cbor_bytes(&unsigned)
            .map_err(|e| SignError::Verify(format!("cbor encode: {e}")))?;
        verify_bytes(responder_pubkey, &unsigned_cbor, &self.signature)
    }

    /// CBOR-decode a response from a byte slice.
    pub fn decode_cbor(bytes: &[u8]) -> Result<Self, ciborium::de::Error<std::io::Error>> {
        from_cbor_bytes(bytes)
    }

    /// CBOR-encode this response.
    pub fn encode_cbor(&self) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
        to_cbor_bytes(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{content_hash, ContentHash, OutcomeVerdict};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn fresh() -> (SigningKey, PeerId) {
        let sk = SigningKey::generate(&mut OsRng);
        let id_kp = libp2p::identity::Keypair::generate_ed25519();
        (sk, PeerId::from(id_kp.public()))
    }

    #[test]
    fn request_signs_and_verifies() {
        let (sk, peer) = fresh();
        let req = StateSyncRequest::new_signed(1_700_000_000, 100, peer, 1_700_000_500, &sk);
        req.verify_signature(&sk.verifying_key()).expect("sig");
    }

    #[test]
    fn request_tamper_breaks_signature() {
        let (sk, peer) = fresh();
        let mut req =
            StateSyncRequest::new_signed(1_700_000_000, 100, peer, 1_700_000_500, &sk);
        req.since_unix = 0; // attacker widens the window
        assert!(req.verify_signature(&sk.verifying_key()).is_err());
    }

    #[test]
    fn request_wrong_key_rejected() {
        let (sk, peer) = fresh();
        let imposter = SigningKey::generate(&mut OsRng);
        let req = StateSyncRequest::new_signed(1, 1, peer, 1, &sk);
        assert!(req.verify_signature(&imposter.verifying_key()).is_err());
    }

    #[test]
    fn request_wire_version_mismatch_rejected() {
        let (sk, peer) = fresh();
        let mut req = StateSyncRequest::new_signed(1, 1, peer, 1, &sk);
        req.wire_version = 999;
        assert!(req.verify_signature(&sk.verifying_key()).is_err());
    }

    fn sample_outcome(sk: &SigningKey, observer: PeerId, n: u64) -> JobOutcome {
        let (o, _h) = JobOutcome::new_signed_at(
            content_hash(format!("spec{n}").as_bytes()),
            ContentHash::zero(),
            vec![content_hash(b"v1")],
            OutcomeVerdict::Valid {
                agreements: 3,
                disagreements: 0,
                abstentions: 0,
                reputation_weighted: 0.9,
            },
            1_700_000_000 + n,
            observer,
            sk,
        );
        o
    }

    #[test]
    fn response_signs_and_verifies_and_inner_sigs_hold() {
        let (sk, peer) = fresh();
        let outcomes = vec![
            sample_outcome(&sk, peer, 1),
            sample_outcome(&sk, peer, 2),
        ];
        let resp = StateSyncResponse::new_signed(
            outcomes.clone(),
            false,
            peer,
            1_700_000_900,
            &sk,
        );
        resp.verify_signature(&sk.verifying_key()).expect("envelope sig");
        for o in &resp.outcomes {
            o.verify_signature(&sk.verifying_key())
                .expect("inner outcome sig");
        }
    }

    #[test]
    fn response_round_trips_through_cbor() {
        let (sk, peer) = fresh();
        let resp =
            StateSyncResponse::new_signed(vec![sample_outcome(&sk, peer, 7)], true, peer, 9, &sk);
        let bytes = resp.encode_cbor().expect("encode");
        let back = StateSyncResponse::decode_cbor(&bytes).expect("decode");
        assert_eq!(resp, back);
    }

    #[test]
    fn malicious_responder_cannot_forge_an_outcome() {
        // The honest observer signs a genuine outcome.
        let (honest_sk, honest_peer) = fresh();
        let genuine = sample_outcome(&honest_sk, honest_peer, 42);

        // A malicious responder forges an outcome that CLAIMS to be
        // observed by the honest peer, but signs it with its OWN key.
        let (mal_sk, mal_peer) = fresh();
        let (forged, _h) = JobOutcome::new_signed_at(
            content_hash(b"forged-spec"),
            ContentHash::zero(),
            vec![],
            OutcomeVerdict::Valid {
                agreements: 99,
                disagreements: 0,
                abstentions: 0,
                reputation_weighted: 1.0,
            },
            1_700_009_999,
            honest_peer, // lies about who observed it
            &mal_sk,     // but cannot sign as the honest peer
        );

        let resp = StateSyncResponse::new_signed(
            vec![genuine.clone(), forged.clone()],
            false,
            mal_peer,
            1,
            &mal_sk,
        );
        // The envelope verifies (the responder really did answer)…
        resp.verify_signature(&mal_sk.verifying_key())
            .expect("envelope ok");
        // …the genuine outcome verifies against the honest observer key…
        genuine
            .verify_signature(&honest_sk.verifying_key())
            .expect("genuine inner sig ok");
        // …but the forged outcome FAILS the inner check against the
        // observer key it claims. This is the security argument: the
        // responder framing is untrusted; only inner signatures count.
        assert!(
            forged
                .verify_signature(&honest_sk.verifying_key())
                .is_err(),
            "forged outcome must NOT verify against the observer key it claims"
        );
    }
}
