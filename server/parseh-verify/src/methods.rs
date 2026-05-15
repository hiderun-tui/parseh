//! Verifier methods — how a verifier actually decides whether a
//! [`parseh_task::JobResult`] is faithful to its
//! [`parseh_task::JobSpec`].
//!
//! V0.2 ships **one** working method: [`DeterministicMethod`] —
//! re-execute the spec deterministically (caller supplies the
//! executor) and byte-compare the payload.
//!
//! [`SpotCheckMethod`] and [`StatisticalMethod`] are **V0.3+** stubs:
//! they implement [`VerifierMethodImpl`] but every call returns
//! [`VerifyError::MethodNotImplementedInV0_2`]. The
//! [`crate::Verifier`] catches that error and emits a
//! [`parseh_task::VerifierVerdict::Abstained`] verification, which is
//! the silent branch in the §3.2 state machine — see
//! the project notes.

use parseh_task::{ContentHash, JobInputs, JobKind, JobResult, JobSpec};

use crate::VerifyError;

/// Outcome of a single re-execution attempt by a [`VerifierMethodImpl`].
#[derive(Debug, Clone)]
pub struct VerificationOutcome {
    /// Whether the re-execution result matched the original.
    pub matched: bool,
    /// `ContentHash` of the diff / alternative completion / evidence
    /// blob. `None` when [`Self::matched`] is `true` (no evidence
    /// needed).
    pub evidence_hash: Option<ContentHash>,
}

/// Trait every verifier method implements.
///
/// V0.2: only [`DeterministicMethod`] returns `Ok(_)`. The other two
/// variants return [`VerifyError::MethodNotImplementedInV0_2`], which
/// the upstream [`crate::Verifier`] interprets as "abstain".
pub trait VerifierMethodImpl {
    /// Re-execute or otherwise re-check the result.
    ///
    /// Returns:
    /// - `Ok(VerificationOutcome { matched: true, .. })` if the
    ///   re-check reproduces the original result.
    /// - `Ok(VerificationOutcome { matched: false, evidence_hash })`
    ///   if the re-check produces a different result.
    /// - `Err(VerifyError::_)` if the verifier cannot run the method
    ///   (e.g. seed missing, model unavailable, method not yet
    ///   implemented).
    fn verify(
        &self,
        spec: &JobSpec,
        result: &JobResult,
    ) -> Result<VerificationOutcome, VerifyError>;
}

/// Trait a [`DeterministicMethod`] uses to call into the local LLM
/// runtime.
///
/// Decoupling this from `parseh-inference` is intentional: this crate
/// is leaf-level, and depending on the inference crate would force a
/// heavyweight dependency tree onto downstream consumers that only
/// want the verification primitive (e.g. test harnesses, a future
/// audit tool, etc.). The miner crate wires in a real implementation.
pub trait LocalExecutor: Send + Sync {
    /// Execute `spec` deterministically and return the bytes that
    /// would have been put in [`JobResult::result_payload`].
    ///
    /// **Determinism contract:** the executor *must* honour
    /// [`JobInputs::seed`]. If [`JobInputs::seed`] is `None`, the
    /// caller should not be invoking this method — but we still
    /// return `Err(VerifyError::SeedMissing)` from
    /// [`DeterministicMethod::verify`] as a defensive guard.
    fn execute(&self, spec: &JobSpec) -> Result<Vec<u8>, VerifyError>;
}

/// V0.2 verifier method — deterministic re-execution.
///
/// Requires `JobInputs::seed = Some(_)` and a [`LocalExecutor`] that
/// can reproduce the executor's output byte-for-byte. Mismatch is
/// surfaced as `matched = false` with an evidence hash over the
/// alternate payload.
pub struct DeterministicMethod<E: LocalExecutor> {
    /// The local executor used for re-execution.
    pub executor: E,
}

impl<E: LocalExecutor> DeterministicMethod<E> {
    /// Construct a [`DeterministicMethod`] from a [`LocalExecutor`].
    pub fn new(executor: E) -> Self {
        Self { executor }
    }
}

impl<E: LocalExecutor> VerifierMethodImpl for DeterministicMethod<E> {
    fn verify(
        &self,
        spec: &JobSpec,
        result: &JobResult,
    ) -> Result<VerificationOutcome, VerifyError> {
        // 1. Determinism precondition: seed must be set for inference
        //    jobs. Other JobKinds will get their own dedicated method
        //    in V0.3+.
        if matches!(spec.kind, JobKind::Inference) {
            let JobInputs { seed, .. } = &spec.inputs;
            if seed.is_none() {
                return Err(VerifyError::SeedMissing);
            }
        }

        // 2. Re-execute through the local executor.
        let local_payload = self.executor.execute(spec)?;

        // 3. Byte-compare.
        if local_payload == result.result_payload {
            Ok(VerificationOutcome {
                matched: true,
                evidence_hash: None,
            })
        } else {
            // 4. Mismatch — emit content-addressed evidence pointing
            //    at the verifier's alternative payload. The evidence
            //    blob itself lives out-of-band (gossipsub envelope
            //    would exceed the 1 MiB cap on a long completion);
            //    the verifier publishes it on
            //    `parseh.verify.v1.evidence` and downstream consumers
            //    fetch by hash.
            Ok(VerificationOutcome {
                matched: false,
                evidence_hash: Some(parseh_task::content_hash(&local_payload)),
            })
        }
    }
}

/// V0.3+ stub: spot-check verifier.
///
/// Implementation will re-execute N short prefix chunks and compare
/// semantic similarity. Today the [`VerifierMethodImpl::verify`] call
/// returns [`VerifyError::MethodNotImplementedInV0_2`], which the
/// [`crate::Verifier`] converts into a silent
/// [`parseh_task::VerifierVerdict::Abstained`] verification.
pub struct SpotCheckMethod;

impl VerifierMethodImpl for SpotCheckMethod {
    fn verify(
        &self,
        _spec: &JobSpec,
        _result: &JobResult,
    ) -> Result<VerificationOutcome, VerifyError> {
        Err(VerifyError::MethodNotImplementedInV0_2)
    }
}

/// V0.3+ stub: statistical-rerun verifier.
///
/// Implementation will rerun across a small sample and check the
/// output distribution. Today behaves identically to
/// [`SpotCheckMethod`].
pub struct StatisticalMethod;

impl VerifierMethodImpl for StatisticalMethod {
    fn verify(
        &self,
        _spec: &JobSpec,
        _result: &JobResult,
    ) -> Result<VerificationOutcome, VerifyError> {
        Err(VerifyError::MethodNotImplementedInV0_2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use libp2p::PeerId;
    use parseh_core::ServiceKind;
    use parseh_task::{JobInputs, JobKind, ResultMeta, VerifierMethod};
    use rand::rngs::OsRng;

    /// A trivial executor that returns whatever bytes the test asks for.
    struct CannedExecutor(Vec<u8>);

    impl LocalExecutor for CannedExecutor {
        fn execute(&self, _spec: &JobSpec) -> Result<Vec<u8>, VerifyError> {
            Ok(self.0.clone())
        }
    }

    /// An executor that returns an error.
    struct BrokenExecutor;

    impl LocalExecutor for BrokenExecutor {
        fn execute(&self, _spec: &JobSpec) -> Result<Vec<u8>, VerifyError> {
            Err(VerifyError::Execution("simulated failure".into()))
        }
    }

    fn build_spec_result(payload: Vec<u8>) -> (JobSpec, JobResult) {
        let sk = SigningKey::generate(&mut OsRng);
        let peer = PeerId::from(libp2p::identity::Keypair::generate_ed25519().public());
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

    #[test]
    fn deterministic_matches_byte_equal() {
        let (spec, result) = build_spec_result(b"hello world".to_vec());
        let m = DeterministicMethod::new(CannedExecutor(b"hello world".to_vec()));
        let out = m.verify(&spec, &result).unwrap();
        assert!(out.matched);
        assert!(out.evidence_hash.is_none());
    }

    #[test]
    fn deterministic_mismatches_returns_evidence() {
        let (spec, result) = build_spec_result(b"hello world".to_vec());
        let m = DeterministicMethod::new(CannedExecutor(b"hello mars".to_vec()));
        let out = m.verify(&spec, &result).unwrap();
        assert!(!out.matched);
        let ev = out.evidence_hash.expect("evidence on mismatch");
        assert_eq!(ev, parseh_task::content_hash(b"hello mars"));
    }

    #[test]
    fn deterministic_seed_missing_errors() {
        let sk = SigningKey::generate(&mut OsRng);
        let peer = PeerId::from(libp2p::identity::Keypair::generate_ed25519().public());
        let inputs = JobInputs {
            prompt_text: Some("hi".into()),
            seed: None,
            max_tokens: None,
            content_refs: Vec::new(),
        };
        let (spec, _) = JobSpec::new_signed_at(
            JobKind::Inference,
            inputs,
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
            JobResult::new_signed_at(spec.content_hash(), peer, 0, meta, b"x".to_vec(), &sk);
        let m = DeterministicMethod::new(CannedExecutor(b"x".to_vec()));
        match m.verify(&spec, &result) {
            Err(VerifyError::SeedMissing) => {}
            other => panic!("expected SeedMissing, got {other:?}"),
        }
    }

    #[test]
    fn deterministic_executor_error_propagates() {
        let (spec, result) = build_spec_result(b"x".to_vec());
        let m = DeterministicMethod::new(BrokenExecutor);
        match m.verify(&spec, &result) {
            Err(VerifyError::Execution(_)) => {}
            other => panic!("expected Execution error, got {other:?}"),
        }
    }

    #[test]
    fn spot_check_returns_unimplemented() {
        let (spec, result) = build_spec_result(b"x".to_vec());
        let m = SpotCheckMethod;
        match m.verify(&spec, &result) {
            Err(VerifyError::MethodNotImplementedInV0_2) => {}
            other => panic!("expected MethodNotImplementedInV0_2, got {other:?}"),
        }
    }

    #[test]
    fn statistical_returns_unimplemented() {
        let (spec, result) = build_spec_result(b"x".to_vec());
        let m = StatisticalMethod;
        match m.verify(&spec, &result) {
            Err(VerifyError::MethodNotImplementedInV0_2) => {}
            other => panic!("expected MethodNotImplementedInV0_2, got {other:?}"),
        }
    }
}
