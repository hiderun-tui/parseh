//! End-to-end verifier — selection → re-execution → signed
//! attestation.
//!
//! The [`Verifier`] type owns one local node's verification state. It
//! wraps a [`crate::SelectionConfig`] (which itself holds a
//! [`crate::RateLimit`]) and the local signing key, and exposes
//! [`Verifier::verify_task`] which performs the full pipeline:
//!
//! 1. **Selection** — should we verify? (delegates to
//!    [`crate::decide_to_verify`])
//! 2. **Re-execution** — if so, run the supplied method
//!    ([`crate::VerifierMethodImpl`]) against the original result.
//! 3. **Sign** — build a [`parseh_task::JobVerification`] capturing
//!    the verdict, signed by the local key.
//!
//! Returns [`VerifyOutcome`] — one of `Agreed`, `Disagreed`,
//! `Abstained`, or `Skipped`. The first three each carry a signed
//! [`parseh_task::JobVerification`] the caller can publish on
//! `parseh.verify.v1`; `Skipped` carries no signed object (silent
//! branch).

use ed25519_dalek::SigningKey;
use libp2p::PeerId;
use parseh_task::{ContentHash, JobResult, JobSpec, JobVerification, VerifierMethod, VerifierVerdict};
use thiserror::Error;

use crate::{
    decide_to_verify, SelectionConfig, SelectionDecision, SkipReason, VerifierMethodImpl,
};

/// Outcome of one [`Verifier::verify_task`] call.
#[derive(Debug, Clone)]
pub enum VerifyOutcome {
    /// Local verifier ran the method and agrees with the executor.
    /// Contains the signed `JobVerification` ready for gossip.
    Agreed(JobVerification),
    /// Local verifier ran the method and disagrees. Contains the
    /// signed `JobVerification` with an evidence hash.
    Disagreed(JobVerification),
    /// Local verifier was selected but could not honour the declared
    /// method (e.g. SpotCheck in V0.2). Emits an `Abstained` signed
    /// envelope — this is the explicit-abstain branch we may need in
    /// V0.3 (per arch §3.2 footnote) and we already emit it now so
    /// downstream tooling has a stable shape.
    Abstained(JobVerification),
    /// Local verifier was not selected at all. No signed object
    /// emitted. The reason is included for metrics.
    Skipped(SkipReason),
}

/// Errors a [`crate::VerifierMethodImpl`] can return.
#[derive(Error, Debug)]
pub enum VerifyError {
    /// The local executor failed to run.
    #[error("re-execution failed: {0}")]
    Execution(String),
    /// The declared method is not implemented in V0.2.
    #[error("method not implemented in V0.2: only Deterministic is supported")]
    MethodNotImplementedInV0_2,
    /// The local node does not have the model required to re-execute
    /// this spec.
    #[error("model not locally available: {0}")]
    ModelUnavailable(String),
    /// [`parseh_task::JobInputs::seed`] is `None` but the declared
    /// method is `Deterministic`.
    #[error("seed missing — JobInputs.seed must be Some for Deterministic")]
    SeedMissing,
    /// Signing or hashing failure (CBOR encode error wrapped). Should
    /// be impossible for the owned types in this crate, but kept as a
    /// surfaced variant rather than a panic.
    #[error("internal encoding error: {0}")]
    Internal(String),
}

/// Per-node verifier state.
///
/// Construct one of these on miner startup, then call
/// [`Self::verify_task`] for every `JobResult` heard on the network.
pub struct Verifier {
    /// Local node's libp2p `PeerId`.
    pub local_peer_id: PeerId,
    /// Local node's ed25519 signing key. Used to sign
    /// [`JobVerification`] envelopes.
    pub local_signing_key: SigningKey,
    /// Selection configuration. Mutable so the verifier can update
    /// reputation / rate-limit state between tasks.
    pub selection_config: SelectionConfig,
}

impl Verifier {
    /// Construct a [`Verifier`] from its three constituents.
    pub fn new(
        local_peer_id: PeerId,
        local_signing_key: SigningKey,
        selection_config: SelectionConfig,
    ) -> Self {
        Self {
            local_peer_id,
            local_signing_key,
            selection_config,
        }
    }

    /// End-to-end verification of one observed `JobResult`.
    ///
    /// - `spec` and `result` are the wire objects.
    /// - `method` is the [`crate::VerifierMethodImpl`] to apply if
    ///   selection chooses us. The caller wires this — typically a
    ///   `DeterministicMethod` carrying a local-LLM executor in V0.2.
    /// - `seed` is the per-decision random seed for the selection
    ///   roll (see [`crate::decide_to_verify`]).
    pub fn verify_task<M: VerifierMethodImpl>(
        &self,
        spec: &JobSpec,
        result: &JobResult,
        method: &M,
        seed: u64,
    ) -> VerifyOutcome {
        // 1. Selection.
        match decide_to_verify(&self.selection_config, spec, result, seed) {
            SelectionDecision::Skip { reason } => return VerifyOutcome::Skipped(reason),
            SelectionDecision::Verify => {}
        }

        // 2. Re-execute.
        let outcome = match method.verify(spec, result) {
            Ok(outcome) => outcome,
            Err(VerifyError::MethodNotImplementedInV0_2) => {
                // Abstain — sign a JobVerification with verdict Abstained.
                return VerifyOutcome::Abstained(self.sign_abstained(result.content_hash()));
            }
            Err(VerifyError::SeedMissing) => {
                // Cannot verify deterministically without a seed. Abstain.
                tracing::debug!(
                    spec_hash = ?spec.content_hash(),
                    "abstaining: seed missing for deterministic re-execution"
                );
                return VerifyOutcome::Abstained(self.sign_abstained(result.content_hash()));
            }
            Err(VerifyError::ModelUnavailable(model)) => {
                tracing::debug!(
                    %model,
                    spec_hash = ?spec.content_hash(),
                    "abstaining: model not locally available"
                );
                return VerifyOutcome::Abstained(self.sign_abstained(result.content_hash()));
            }
            Err(err) => {
                tracing::warn!(error = %err, "verifier re-execution error");
                // Treat any other error as a skipped opportunity, not
                // a signed verification. The verifier did not actually
                // form an opinion.
                return VerifyOutcome::Skipped(SkipReason::RandomNotChosen);
            }
        };

        // 3. Build verdict.
        let verdict = if outcome.matched {
            VerifierVerdict::Agreed
        } else {
            VerifierVerdict::Disagreed {
                evidence_hash: outcome.evidence_hash.unwrap_or_default(),
            }
        };

        // 4. Sign.
        let (verification, _hash) = JobVerification::new_signed(
            result.content_hash(),
            self.local_peer_id,
            verdict,
            VerifierMethod::Deterministic,
            &self.local_signing_key,
        );

        if outcome.matched {
            VerifyOutcome::Agreed(verification)
        } else {
            VerifyOutcome::Disagreed(verification)
        }
    }

    /// Build a signed `Abstained` verification.
    fn sign_abstained(&self, result_hash: ContentHash) -> JobVerification {
        let (v, _) = JobVerification::new_signed(
            result_hash,
            self.local_peer_id,
            VerifierVerdict::Abstained,
            VerifierMethod::Deterministic,
            &self.local_signing_key,
        );
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeterministicMethod, LocalExecutor, RateLimit};
    use ed25519_dalek::SigningKey;
    use parseh_core::ServiceKind;
    use parseh_task::{JobInputs, JobKind, ResultMeta};
    use rand::rngs::OsRng;

    struct CannedExecutor(Vec<u8>);

    impl LocalExecutor for CannedExecutor {
        fn execute(&self, _spec: &JobSpec) -> Result<Vec<u8>, VerifyError> {
            Ok(self.0.clone())
        }
    }

    fn fresh_peer() -> PeerId {
        PeerId::from(libp2p::identity::Keypair::generate_ed25519().public())
    }

    /// Build a verifier with a self-selected seed (low p, but we set
    /// rep to 100× the average so `p_node = p_max = 0.5`; with seed
    /// `0` the roll lands inside).
    fn build_verifier() -> Verifier {
        let sk = SigningKey::generate(&mut OsRng);
        let local = fresh_peer();
        let cfg = SelectionConfig {
            local_peer_id: local,
            local_reputation: 1000,
            network_avg_reputation: 10,
            rate_limit: RateLimit::v0_2_defaults(),
            already_verified_this_task: false,
        };
        Verifier::new(local, sk, cfg)
    }

    fn build_spec_result(payload: Vec<u8>) -> (JobSpec, JobResult) {
        let sk = SigningKey::generate(&mut OsRng);
        let peer = fresh_peer();
        let (spec, _) = JobSpec::new_signed_at(
            JobKind::Inference,
            JobInputs::inference_prompt("hi", 7),
            ServiceKind::Inference,
            false,
            1_700_000_000,
            peer,
            &sk,
        );
        let meta = ResultMeta {
            verifier_method: VerifierMethod::Deterministic,
            execution_time_ms: 1,
            model_used: None,
            inference_token_count: None,
        };
        let (result, _) =
            JobResult::new_signed_at(spec.content_hash(), peer, 1_700_000_001, meta, payload, &sk);
        (spec, result)
    }

    /// Find a seed that makes the given verifier *not* skip due to the
    /// dice roll. With `p_max = 0.5` and a uniform digest, half of
    /// seeds work; we just scan.
    fn force_select_seed(v: &Verifier, spec: &JobSpec, result: &JobResult) -> u64 {
        for s in 0..10_000u64 {
            if matches!(
                decide_to_verify(&v.selection_config, spec, result, s),
                SelectionDecision::Verify
            ) {
                return s;
            }
        }
        panic!("could not find a selecting seed in 10000 tries");
    }

    #[test]
    fn skipped_when_selection_says_skip() {
        let v = build_verifier();
        let (spec, result) = build_spec_result(b"hello".to_vec());
        // Force rule-3 by setting submitter = local.
        let mut spec = spec;
        spec.submitter = v.local_peer_id;
        let method = DeterministicMethod::new(CannedExecutor(b"hello".to_vec()));
        match v.verify_task(&spec, &result, &method, 0) {
            VerifyOutcome::Skipped(SkipReason::OwnTask) => {}
            other => panic!("expected Skipped(OwnTask), got {other:?}"),
        }
    }

    #[test]
    fn agreed_on_byte_match() {
        let v = build_verifier();
        let (spec, result) = build_spec_result(b"hello".to_vec());
        let method = DeterministicMethod::new(CannedExecutor(b"hello".to_vec()));
        let seed = force_select_seed(&v, &spec, &result);
        match v.verify_task(&spec, &result, &method, seed) {
            VerifyOutcome::Agreed(jv) => {
                assert!(matches!(jv.verdict, VerifierVerdict::Agreed));
            }
            other => panic!("expected Agreed, got {other:?}"),
        }
    }

    #[test]
    fn disagreed_on_mismatch_with_evidence() {
        let v = build_verifier();
        let (spec, result) = build_spec_result(b"hello".to_vec());
        let method = DeterministicMethod::new(CannedExecutor(b"goodbye".to_vec()));
        let seed = force_select_seed(&v, &spec, &result);
        match v.verify_task(&spec, &result, &method, seed) {
            VerifyOutcome::Disagreed(jv) => match jv.verdict {
                VerifierVerdict::Disagreed { evidence_hash } => {
                    assert_eq!(evidence_hash, parseh_task::content_hash(b"goodbye"));
                }
                other => panic!("expected Disagreed verdict, got {other:?}"),
            },
            other => panic!("expected Disagreed, got {other:?}"),
        }
    }

    #[test]
    fn abstained_when_method_unimplemented() {
        let v = build_verifier();
        let (spec, result) = build_spec_result(b"hello".to_vec());
        let method = crate::SpotCheckMethod;
        let seed = force_select_seed(&v, &spec, &result);
        match v.verify_task(&spec, &result, &method, seed) {
            VerifyOutcome::Abstained(jv) => {
                assert!(matches!(jv.verdict, VerifierVerdict::Abstained));
                jv.verify_signature(&v.local_signing_key.verifying_key())
                    .expect("abstained signature should verify");
            }
            other => panic!("expected Abstained, got {other:?}"),
        }
    }
}
