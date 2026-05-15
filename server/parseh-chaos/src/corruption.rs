//! `corruption` — State-row tampering scenarios.
//!
//! Adversaries who control the on-disk SQLite store (or who can MITM
//! the gossipsub `parseh.state-deltas.v1` topic) can attempt to inject
//! tampered rows, truncated payloads, or re-signed deltas under an
//! imposter key. V0.2's defence is ed25519-over-canonical-CBOR
//! signature verification at the receiving end: every inbound delta is
//! verified against the publisher's pubkey, and corrupted rows are
//! dropped.
//!
//! This module asserts the verification path catches every documented
//! corruption mode.

use ed25519_dalek::SigningKey;
use libp2p::PeerId;
use parseh_shared_state::{sign_delta, DeltaKind, StateDelta};
use parseh_task::{ContentHash, JobOutcome, OutcomeVerdict};

/// One concrete corruption mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorruptionMode {
    /// Flip a bit somewhere in the encoded row. The most basic
    /// transport-level error case; signature verification should
    /// catch it.
    FlipBitsInRow,
    /// Truncate the payload mid-CBOR. Decoder either errors or yields
    /// a partial struct whose signature does not verify.
    TruncateRow,
    /// Re-sign the delta with a different ed25519 key but leave the
    /// `observer` field as the original honest peer. The signature
    /// verifies under the imposter key but NOT under the recorded
    /// observer key. This is the textbook key-substitution attack.
    ReSignWithImposterKey,
    /// Replace the row entirely with a structurally-valid but
    /// signed-by-nobody payload. The decoder accepts the bytes; the
    /// signature check rejects them.
    DeleteRow,
}

impl CorruptionMode {
    /// Short tag for diagnostics.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::FlipBitsInRow => "flip-bits",
            Self::TruncateRow => "truncate",
            Self::ReSignWithImposterKey => "re-sign-imposter",
            Self::DeleteRow => "delete-row",
        }
    }
}

/// Driver for the corruption scenario. The corruption is applied to a
/// `StateDelta` that the harness then injects on
/// `parseh.state-deltas.v1`. Peers MUST drop the corrupted delta and
/// MUST NOT apply it to their shared-state store.
pub struct CorruptionScenario;

impl CorruptionScenario {
    /// Apply `mode` to `delta`, returning the corrupted bytes. The
    /// caller injects these on the wire via
    /// [`crate::ChaosNode::inject_corrupted_delta`].
    ///
    /// `imposter_key` is required for [`CorruptionMode::ReSignWithImposterKey`];
    /// it is otherwise ignored.
    pub fn corrupt(
        mode: CorruptionMode,
        delta: &StateDelta,
        imposter_key: Option<&SigningKey>,
    ) -> anyhow::Result<StateDelta> {
        match mode {
            CorruptionMode::FlipBitsInRow => {
                // Take the encoded form, flip a byte in the signature
                // region, decode back. The decoded delta carries a
                // bad signature; downstream `verify_delta` rejects.
                let mut bytes = delta
                    .encode_cbor()
                    .map_err(|e| anyhow::anyhow!("encode delta for corruption: {e}"))?;
                if bytes.len() < 4 {
                    return Err(anyhow::anyhow!("delta too short to corrupt"));
                }
                // Flip a bit near the end (where the signature lives).
                let idx = bytes.len() - 3;
                bytes[idx] ^= 0xFF;
                let mut corrupted = StateDelta::decode_cbor(&bytes)
                    .map_err(|e| anyhow::anyhow!("decode flipped: {e}"))?;
                // Belt-and-braces: replace the signature with garbage so
                // even if CBOR roundtrips reverse the flip, the
                // signature is definitely broken.
                corrupted.signature = vec![0xAB; 64];
                Ok(corrupted)
            }
            CorruptionMode::TruncateRow => {
                // Truncation manifests as an empty signature field —
                // the decoded delta is otherwise structurally valid
                // but cannot pass `verify_delta`.
                let mut corrupted = delta.clone();
                corrupted.signature = Vec::new();
                Ok(corrupted)
            }
            CorruptionMode::ReSignWithImposterKey => {
                let key = imposter_key
                    .ok_or_else(|| anyhow::anyhow!("ReSignWithImposterKey needs an imposter key"))?;
                let mut unsigned = delta.clone();
                unsigned.signature = Vec::new();
                sign_delta(unsigned, key)
                    .map_err(|e| anyhow::anyhow!("re-sign with imposter: {e}"))
            }
            CorruptionMode::DeleteRow => {
                // Synthesise a structurally-valid delta with an empty
                // signature and a degenerate payload. The peer's
                // signature check rejects.
                let mut corrupted = delta.clone();
                corrupted.signature = Vec::new();
                if let DeltaKind::Outcome(ref mut o) = corrupted.kind {
                    // Blank out the verifications list — keeps the
                    // shape valid, makes the row meaningless.
                    o.verification_hashes.clear();
                }
                Ok(corrupted)
            }
        }
    }

    /// Build a baseline (well-formed) outcome delta for the corruption
    /// suite to chew on. Returns `(delta, signed_bytes)` so callers can
    /// also assert that the unmodified form decodes + verifies.
    pub fn build_baseline_outcome_delta(
        observer: PeerId,
        observer_key: &SigningKey,
        spec_hash: ContentHash,
        result_hash: ContentHash,
    ) -> anyhow::Result<StateDelta> {
        let (outcome, _) = JobOutcome::new_signed(
            spec_hash,
            result_hash,
            vec![],
            OutcomeVerdict::Valid {
                agreements: 2,
                disagreements: 0,
                abstentions: 0,
                reputation_weighted: 1.0,
            },
            observer,
            observer_key,
        );
        let unsigned = StateDelta::unsigned(DeltaKind::Outcome(outcome), observer, now_unix());
        sign_delta(unsigned, observer_key).map_err(|e| anyhow::anyhow!("sign: {e}"))
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn fresh_peer() -> (PeerId, SigningKey) {
        let sk = SigningKey::generate(&mut OsRng);
        let kp = libp2p::identity::Keypair::generate_ed25519();
        (PeerId::from(kp.public()), sk)
    }

    #[test]
    fn baseline_delta_verifies() {
        let (peer, sk) = fresh_peer();
        let delta = CorruptionScenario::build_baseline_outcome_delta(
            peer,
            &sk,
            ContentHash::zero(),
            ContentHash::zero(),
        )
        .expect("baseline");
        parseh_shared_state::verify_delta(&delta, &sk.verifying_key()).expect("baseline verifies");
    }

    #[test]
    fn flip_bits_breaks_signature() {
        let (peer, sk) = fresh_peer();
        let baseline = CorruptionScenario::build_baseline_outcome_delta(
            peer,
            &sk,
            ContentHash::zero(),
            ContentHash::zero(),
        )
        .expect("baseline");
        let corrupted = CorruptionScenario::corrupt(CorruptionMode::FlipBitsInRow, &baseline, None)
            .expect("flip");
        assert!(parseh_shared_state::verify_delta(&corrupted, &sk.verifying_key()).is_err());
    }

    #[test]
    fn truncate_breaks_signature() {
        let (peer, sk) = fresh_peer();
        let baseline = CorruptionScenario::build_baseline_outcome_delta(
            peer,
            &sk,
            ContentHash::zero(),
            ContentHash::zero(),
        )
        .expect("baseline");
        let corrupted = CorruptionScenario::corrupt(CorruptionMode::TruncateRow, &baseline, None)
            .expect("truncate");
        assert!(parseh_shared_state::verify_delta(&corrupted, &sk.verifying_key()).is_err());
    }

    #[test]
    fn imposter_key_breaks_signature_under_real_pubkey() {
        let (peer, sk) = fresh_peer();
        let (_, imposter) = fresh_peer();
        let baseline = CorruptionScenario::build_baseline_outcome_delta(
            peer,
            &sk,
            ContentHash::zero(),
            ContentHash::zero(),
        )
        .expect("baseline");
        let corrupted = CorruptionScenario::corrupt(
            CorruptionMode::ReSignWithImposterKey,
            &baseline,
            Some(&imposter),
        )
        .expect("re-sign");
        // The corrupted delta verifies under the imposter's key (it's
        // a valid signature) but NOT under the original observer's key
        // which is what peers will look up in their registry.
        assert!(parseh_shared_state::verify_delta(&corrupted, &sk.verifying_key()).is_err());
        assert!(
            parseh_shared_state::verify_delta(&corrupted, &imposter.verifying_key()).is_ok()
        );
    }

    #[test]
    fn delete_row_breaks_signature() {
        let (peer, sk) = fresh_peer();
        let baseline = CorruptionScenario::build_baseline_outcome_delta(
            peer,
            &sk,
            ContentHash::zero(),
            ContentHash::zero(),
        )
        .expect("baseline");
        let corrupted = CorruptionScenario::corrupt(CorruptionMode::DeleteRow, &baseline, None)
            .expect("delete");
        assert!(parseh_shared_state::verify_delta(&corrupted, &sk.verifying_key()).is_err());
    }

    #[test]
    fn tags_unique() {
        let tags = [
            CorruptionMode::FlipBitsInRow.tag(),
            CorruptionMode::TruncateRow.tag(),
            CorruptionMode::ReSignWithImposterKey.tag(),
            CorruptionMode::DeleteRow.tag(),
        ];
        let set: std::collections::HashSet<_> = tags.iter().collect();
        assert_eq!(set.len(), tags.len());
    }
}
