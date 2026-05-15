//! Integration tests for `parseh-verify` — V0.2 Primitive 2.
//!
//! These tests exercise the full pipeline: selection → method →
//! signed verification → quorum aggregation → signed outcome. Every
//! test is self-contained; we never touch the network or the file
//! system.

use std::time::{Duration, UNIX_EPOCH};

use ed25519_dalek::SigningKey;
use libp2p::PeerId;
use parseh_core::ServiceKind;
use parseh_task::{
    content_hash, ContentHash, JobInputs, JobKind, JobResult, JobSpec, JobVerification,
    OutcomeVerdict, ResultMeta, VerifierMethod, VerifierVerdict,
};
use parseh_verify::{
    decide_to_verify, params, DeterministicMethod, LocalExecutor, Quorum, QuorumConfig,
    QuorumDecision, RateLimit, SelectionConfig, SelectionDecision, SkipReason, SpotCheckMethod,
    StatisticalMethod, VerificationOutcome, Verifier, VerifierMethodImpl, VerifyError, VerifyOutcome,
};
use rand::rngs::OsRng;

// ---------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------

fn fresh_peer() -> PeerId {
    PeerId::from(libp2p::identity::Keypair::generate_ed25519().public())
}

fn fresh_sk() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

fn build_spec(submitter: PeerId, sk: &SigningKey) -> JobSpec {
    let (spec, _) = JobSpec::new_signed_at(
        JobKind::Inference,
        JobInputs::inference_prompt("hi", 7),
        ServiceKind::Inference,
        false,
        1_700_000_000,
        submitter,
        sk,
    );
    spec
}

fn build_result(executor: PeerId, sk: &SigningKey, spec_hash: ContentHash, payload: &[u8]) -> JobResult {
    let meta = ResultMeta {
        verifier_method: VerifierMethod::Deterministic,
        execution_time_ms: 1,
        model_used: None,
        inference_token_count: None,
    };
    let (r, _) = JobResult::new_signed_at(
        spec_hash,
        executor,
        1_700_000_001,
        meta,
        payload.to_vec(),
        sk,
    );
    r
}

struct CannedExecutor(Vec<u8>);

impl LocalExecutor for CannedExecutor {
    fn execute(&self, _spec: &JobSpec) -> Result<Vec<u8>, VerifyError> {
        Ok(self.0.clone())
    }
}

/// Find a seed for the dice-roll selection that picks this verifier.
fn force_select_seed(v: &Verifier, spec: &JobSpec, result: &JobResult) -> u64 {
    for s in 0..10_000u64 {
        if matches!(
            decide_to_verify(&v.selection_config, spec, result, s),
            SelectionDecision::Verify
        ) {
            return s;
        }
    }
    panic!("could not find a selecting seed");
}

fn build_verifier_with_rep(rep: u32, avg: u32) -> Verifier {
    let sk = fresh_sk();
    let local = fresh_peer();
    let cfg = SelectionConfig {
        local_peer_id: local,
        local_reputation: rep,
        network_avg_reputation: avg,
        rate_limit: RateLimit::v0_2_defaults(),
        already_verified_this_task: false,
    };
    Verifier::new(local, sk, cfg)
}

// ---------------------------------------------------------------------
// 1 — Rule 3 (own task / own result)
// ---------------------------------------------------------------------

#[test]
fn rule_3_blocks_verifying_own_submitted_task() {
    let v = build_verifier_with_rep(1000, 10);
    let result_sk = fresh_sk();
    let spec = build_spec(v.local_peer_id, &result_sk);
    let result = build_result(fresh_peer(), &result_sk, spec.content_hash(), b"x");
    let method = DeterministicMethod::new(CannedExecutor(b"x".to_vec()));
    match v.verify_task(&spec, &result, &method, 0) {
        VerifyOutcome::Skipped(SkipReason::OwnTask) => {}
        other => panic!("expected OwnTask skip, got {other:?}"),
    }
}

#[test]
fn rule_3_blocks_verifying_own_executed_result() {
    let v = build_verifier_with_rep(1000, 10);
    let sk = fresh_sk();
    let spec = build_spec(fresh_peer(), &sk);
    let result = build_result(v.local_peer_id, &sk, spec.content_hash(), b"x");
    let method = DeterministicMethod::new(CannedExecutor(b"x".to_vec()));
    match v.verify_task(&spec, &result, &method, 0) {
        VerifyOutcome::Skipped(SkipReason::OwnTask) => {}
        other => panic!("expected OwnTask skip, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// 2 — Probationary gate
// ---------------------------------------------------------------------

#[test]
fn probationary_gate_blocks_low_reputation() {
    let v = build_verifier_with_rep(9, 50);
    let sk = fresh_sk();
    let spec = build_spec(fresh_peer(), &sk);
    let result = build_result(fresh_peer(), &sk, spec.content_hash(), b"x");
    let method = DeterministicMethod::new(CannedExecutor(b"x".to_vec()));
    match v.verify_task(&spec, &result, &method, 0) {
        VerifyOutcome::Skipped(SkipReason::BelowProbationary) => {}
        other => panic!("expected BelowProbationary skip, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// 3 — Rate limit
// ---------------------------------------------------------------------

#[test]
fn rate_limit_blocks_excess_verification() {
    // Saturate the limiter: 100 observed, 11 own → over 10% cap.
    let mut rl = RateLimit::v0_2_defaults();
    let now = std::time::SystemTime::now();
    for i in 0..100 {
        rl.record_observed_task_at(now - Duration::from_secs(60 + i));
    }
    for i in 0..11 {
        rl.record_own_verification_at(now - Duration::from_secs(30 + i));
    }
    assert!(rl.exceeded_at_now());

    let local = fresh_peer();
    let cfg = SelectionConfig {
        local_peer_id: local,
        local_reputation: 1000,
        network_avg_reputation: 10,
        rate_limit: rl,
        already_verified_this_task: false,
    };
    let v = Verifier::new(local, fresh_sk(), cfg);
    let sk = fresh_sk();
    let spec = build_spec(fresh_peer(), &sk);
    let result = build_result(fresh_peer(), &sk, spec.content_hash(), b"x");
    let method = DeterministicMethod::new(CannedExecutor(b"x".to_vec()));
    match v.verify_task(&spec, &result, &method, 0) {
        VerifyOutcome::Skipped(SkipReason::RateLimited) => {}
        other => panic!("expected RateLimited skip, got {other:?}"),
    }
}

#[test]
fn already_verified_dedup_blocks_double_verification() {
    let mut v = build_verifier_with_rep(1000, 10);
    v.selection_config.already_verified_this_task = true;
    let sk = fresh_sk();
    let spec = build_spec(fresh_peer(), &sk);
    let result = build_result(fresh_peer(), &sk, spec.content_hash(), b"x");
    let method = DeterministicMethod::new(CannedExecutor(b"x".to_vec()));
    match v.verify_task(&spec, &result, &method, 0) {
        VerifyOutcome::Skipped(SkipReason::AlreadyVerified) => {}
        other => panic!("expected AlreadyVerified skip, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// 4 — Random selection distribution
// ---------------------------------------------------------------------

#[test]
fn random_selection_distribution_is_within_expected_range() {
    // With rep_node = network_avg, p_node = p_base = 0.05. Across
    // 5000 trials we expect ~5% selection — accept 3.5% - 6.5%.
    let sk = fresh_sk();
    let spec = build_spec(fresh_peer(), &sk);
    let result = build_result(fresh_peer(), &sk, spec.content_hash(), b"x");
    let local = fresh_peer();
    let cfg = SelectionConfig {
        local_peer_id: local,
        local_reputation: 100,
        network_avg_reputation: 100,
        rate_limit: RateLimit::v0_2_defaults(),
        already_verified_this_task: false,
    };
    let n_trials = 5_000u64;
    let mut hits = 0u64;
    for s in 0..n_trials {
        if matches!(
            decide_to_verify(&cfg, &spec, &result, s),
            SelectionDecision::Verify
        ) {
            hits += 1;
        }
    }
    let rate = hits as f64 / n_trials as f64;
    assert!(
        (0.035..0.065).contains(&rate),
        "empirical selection rate {rate} should be near p_base={}",
        params::P_BASE
    );
}

// ---------------------------------------------------------------------
// 5 — Methods
// ---------------------------------------------------------------------

#[test]
fn deterministic_method_byte_equal_match_returns_agreed() {
    let v = build_verifier_with_rep(1000, 10);
    let sk = fresh_sk();
    let spec = build_spec(fresh_peer(), &sk);
    let result = build_result(fresh_peer(), &sk, spec.content_hash(), b"hello world");
    let method = DeterministicMethod::new(CannedExecutor(b"hello world".to_vec()));
    let seed = force_select_seed(&v, &spec, &result);
    match v.verify_task(&spec, &result, &method, seed) {
        VerifyOutcome::Agreed(jv) => {
            assert!(matches!(jv.verdict, VerifierVerdict::Agreed));
            jv.verify_signature(&v.local_signing_key.verifying_key())
                .expect("signature");
        }
        other => panic!("expected Agreed, got {other:?}"),
    }
}

#[test]
fn deterministic_method_byte_different_returns_disagreed_with_evidence() {
    let v = build_verifier_with_rep(1000, 10);
    let sk = fresh_sk();
    let spec = build_spec(fresh_peer(), &sk);
    let result = build_result(fresh_peer(), &sk, spec.content_hash(), b"hello world");
    let method = DeterministicMethod::new(CannedExecutor(b"hello mars".to_vec()));
    let seed = force_select_seed(&v, &spec, &result);
    match v.verify_task(&spec, &result, &method, seed) {
        VerifyOutcome::Disagreed(jv) => match jv.verdict {
            VerifierVerdict::Disagreed { evidence_hash } => {
                assert_eq!(evidence_hash, content_hash(b"hello mars"));
            }
            other => panic!("expected Disagreed verdict, got {other:?}"),
        },
        other => panic!("expected Disagreed, got {other:?}"),
    }
}

#[test]
fn spot_check_method_returns_abstained_in_v0_2() {
    let v = build_verifier_with_rep(1000, 10);
    let sk = fresh_sk();
    let spec = build_spec(fresh_peer(), &sk);
    let result = build_result(fresh_peer(), &sk, spec.content_hash(), b"x");
    let seed = force_select_seed(&v, &spec, &result);
    match v.verify_task(&spec, &result, &SpotCheckMethod, seed) {
        VerifyOutcome::Abstained(jv) => {
            assert!(matches!(jv.verdict, VerifierVerdict::Abstained));
        }
        other => panic!("expected Abstained, got {other:?}"),
    }
}

#[test]
fn statistical_method_returns_abstained_in_v0_2() {
    let v = build_verifier_with_rep(1000, 10);
    let sk = fresh_sk();
    let spec = build_spec(fresh_peer(), &sk);
    let result = build_result(fresh_peer(), &sk, spec.content_hash(), b"x");
    let seed = force_select_seed(&v, &spec, &result);
    match v.verify_task(&spec, &result, &StatisticalMethod, seed) {
        VerifyOutcome::Abstained(jv) => {
            assert!(matches!(jv.verdict, VerifierVerdict::Abstained));
        }
        other => panic!("expected Abstained, got {other:?}"),
    }
}

#[test]
fn seed_missing_for_deterministic_returns_error() {
    // Directly exercise the method trait, since the verifier
    // converts SeedMissing into Abstained at the pipeline level.
    let sk = fresh_sk();
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
        fresh_peer(),
        &sk,
    );
    let result = build_result(fresh_peer(), &sk, spec.content_hash(), b"x");
    let method = DeterministicMethod::new(CannedExecutor(b"x".to_vec()));
    match VerifierMethodImpl::verify(&method, &spec, &result) {
        Err(VerifyError::SeedMissing) => {}
        other => panic!("expected SeedMissing, got {other:?}"),
    }
}

#[test]
fn seed_missing_at_pipeline_level_yields_abstained_envelope() {
    // At the Verifier level the same condition becomes a signed
    // Abstained verification, not an error. This is the protocol
    // contract — the network sees a signed abstention rather than
    // silence.
    let v = build_verifier_with_rep(1000, 10);
    let sk = fresh_sk();
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
        fresh_peer(),
        &sk,
    );
    let result = build_result(fresh_peer(), &sk, spec.content_hash(), b"x");
    let method = DeterministicMethod::new(CannedExecutor(b"x".to_vec()));
    let seed = force_select_seed(&v, &spec, &result);
    match v.verify_task(&spec, &result, &method, seed) {
        VerifyOutcome::Abstained(_) => {}
        other => panic!("expected Abstained envelope, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// 6 — Quorum (standard 5-of-9)
// ---------------------------------------------------------------------

/// Build N signed `Agreed` verifications.
fn n_agreed_verifications(n: u32, result_hash: ContentHash, rep: u32) -> Vec<(JobVerification, SigningKey, u32)> {
    (0..n)
        .map(|i| {
            let sk = fresh_sk();
            let peer = fresh_peer();
            let (v, _) = JobVerification::new_signed_at(
                result_hash,
                peer,
                VerifierVerdict::Agreed,
                VerifierMethod::Deterministic,
                1_700_000_500 + i as u64,
                &sk,
            );
            (v, sk, rep)
        })
        .collect()
}

#[test]
fn quorum_standard_5_of_9_agreed_finalises_agreed() {
    let result_hash = content_hash(b"r");
    let mut q = Quorum::new(
        QuorumConfig::standard(),
        content_hash(b"spec"),
        result_hash,
        UNIX_EPOCH,
    );
    for (v, sk, rep) in n_agreed_verifications(5, result_hash, 100) {
        q.add_verification(v, rep, &sk.verifying_key()).unwrap();
    }
    let observer_sk = fresh_sk();
    let observer = fresh_peer();
    let now = UNIX_EPOCH + Duration::from_secs(params::T_MIN_SECS + 1);
    let final_ = q.try_finalise(now, observer, &observer_sk).expect("finalised");
    assert!(matches!(final_.decision, QuorumDecision::Agreed));
    assert_eq!(final_.agreements, 5);
    assert!(matches!(
        final_.outcome.verdict,
        OutcomeVerdict::Valid { .. }
    ));
}

#[test]
fn quorum_5_of_9_with_4_agreed_3_disagreed_finalises_disputed() {
    // 4 agreed, 3 disagreed, T_max elapsed → Disputed.
    let result_hash = content_hash(b"r");
    let mut q = Quorum::new(
        QuorumConfig::standard(),
        content_hash(b"spec"),
        result_hash,
        UNIX_EPOCH,
    );
    for (v, sk, rep) in n_agreed_verifications(4, result_hash, 100) {
        q.add_verification(v, rep, &sk.verifying_key()).unwrap();
    }
    for i in 0..3 {
        let sk = fresh_sk();
        let peer = fresh_peer();
        let (v, _) = JobVerification::new_signed_at(
            result_hash,
            peer,
            VerifierVerdict::Disagreed {
                evidence_hash: content_hash(b"diff"),
            },
            VerifierMethod::Deterministic,
            1_700_000_700 + i,
            &sk,
        );
        q.add_verification(v, 100, &sk.verifying_key()).unwrap();
    }
    let observer_sk = fresh_sk();
    let observer = fresh_peer();
    // T_max elapsed.
    let now = UNIX_EPOCH + Duration::from_secs(params::T_MAX_SECS + 1);
    let final_ = q.try_finalise(now, observer, &observer_sk).expect("finalised");
    assert!(matches!(final_.decision, QuorumDecision::Disputed));
    if let OutcomeVerdict::Disputed { ref disputers } = final_.outcome.verdict {
        assert_eq!(disputers.len(), 3, "minority should be the 3 Disagreed verifiers");
    } else {
        panic!("expected Disputed verdict");
    }
}

#[test]
fn quorum_cannot_close_before_t_min() {
    let result_hash = content_hash(b"r");
    let mut q = Quorum::new(
        QuorumConfig::standard(),
        content_hash(b"spec"),
        result_hash,
        UNIX_EPOCH,
    );
    for (v, sk, rep) in n_agreed_verifications(9, result_hash, 100) {
        q.add_verification(v, rep, &sk.verifying_key()).unwrap();
    }
    let observer_sk = fresh_sk();
    let observer = fresh_peer();
    // Only 1 second elapsed — below T_min.
    let now = UNIX_EPOCH + Duration::from_secs(1);
    assert!(q.try_finalise(now, observer, &observer_sk).is_none());
    // After T_min — finalises.
    let now = UNIX_EPOCH + Duration::from_secs(params::T_MIN_SECS + 1);
    let final_ = q.try_finalise(now, observer, &observer_sk).expect("finalised");
    assert!(matches!(final_.decision, QuorumDecision::Agreed));
}

#[test]
fn quorum_indeterminate_after_t_max() {
    // Only 1 agreed (below M=5). After T_max, finalises as
    // Indeterminate.
    let result_hash = content_hash(b"r");
    let mut q = Quorum::new(
        QuorumConfig::standard(),
        content_hash(b"spec"),
        result_hash,
        UNIX_EPOCH,
    );
    for (v, sk, rep) in n_agreed_verifications(1, result_hash, 100) {
        q.add_verification(v, rep, &sk.verifying_key()).unwrap();
    }
    let observer_sk = fresh_sk();
    let observer = fresh_peer();
    let now = UNIX_EPOCH + Duration::from_secs(params::T_MAX_SECS + 1);
    let final_ = q.try_finalise(now, observer, &observer_sk).expect("finalised");
    assert!(matches!(final_.decision, QuorumDecision::Indeterminate));
    assert!(matches!(
        final_.outcome.verdict,
        OutcomeVerdict::Indeterminate
    ));
}

#[test]
fn reputation_weighted_threshold_blocks_low_rep_majority() {
    // 5 agreed at rep=1 (raw count passes), 1 disagreed at rep=1000
    // (60% rep-weighted threshold fails). The quorum should NOT
    // finalise Agreed before T_max, and should finalise Disputed at
    // T_max.
    let result_hash = content_hash(b"r");
    let mut q = Quorum::new(
        QuorumConfig::standard(),
        content_hash(b"spec"),
        result_hash,
        UNIX_EPOCH,
    );
    for (v, sk, _) in n_agreed_verifications(5, result_hash, 1) {
        q.add_verification(v, 1, &sk.verifying_key()).unwrap();
    }
    let sk = fresh_sk();
    let peer = fresh_peer();
    let (v, _) = JobVerification::new_signed_at(
        result_hash,
        peer,
        VerifierVerdict::Disagreed {
            evidence_hash: content_hash(b"diff"),
        },
        VerifierMethod::Deterministic,
        1_700_001_000,
        &sk,
    );
    q.add_verification(v, 1000, &sk.verifying_key()).unwrap();

    let (a, d, _, w) = q.tally();
    assert_eq!((a, d), (5, 1));
    assert!(w < params::REP_WEIGHTED_THRESHOLD, "rep-weighted is {w}, should be below 0.6");

    let observer_sk = fresh_sk();
    let observer = fresh_peer();
    // T_min elapsed but T_max not yet — should still be open (or at
    // most finalise as something other than Agreed). Verify it is
    // NOT Agreed.
    let now = UNIX_EPOCH + Duration::from_secs(params::T_MIN_SECS + 1);
    if let Some(f) = q.try_finalise(now, observer, &observer_sk) {
        assert!(!matches!(f.decision, QuorumDecision::Agreed));
    }
    // After T_max, must finalise Disputed (both sides have ≥ half-M).
    let now = UNIX_EPOCH + Duration::from_secs(params::T_MAX_SECS + 1);
    let final_ = q.try_finalise(now, observer, &observer_sk).expect("finalised");
    assert!(
        matches!(final_.decision, QuorumDecision::Disputed | QuorumDecision::Indeterminate),
        "expected Disputed or Indeterminate, got {:?}",
        final_.decision
    );
}

#[test]
fn signed_verification_with_bad_signature_rejected_by_quorum() {
    let result_hash = content_hash(b"r");
    let mut q = Quorum::new(
        QuorumConfig::standard(),
        content_hash(b"spec"),
        result_hash,
        UNIX_EPOCH,
    );
    let real_sk = fresh_sk();
    let imposter_sk = fresh_sk();
    let peer = fresh_peer();
    let (v, _) = JobVerification::new_signed_at(
        result_hash,
        peer,
        VerifierVerdict::Agreed,
        VerifierMethod::Deterministic,
        1_700_000_500,
        &real_sk,
    );
    // Verify against imposter's pubkey — must fail.
    let err = q.add_verification(v, 100, &imposter_sk.verifying_key()).unwrap_err();
    assert!(matches!(err, VerifyError::Internal(_)));
}

#[test]
fn sensitive_quorum_uses_9_of_15() {
    let result_hash = content_hash(b"r");
    let cfg = QuorumConfig::sensitive();
    assert_eq!(cfg.m, params::M_SENSITIVE);
    assert_eq!(cfg.n, params::N_SENSITIVE);
    let mut q = Quorum::new(cfg, content_hash(b"spec"), result_hash, UNIX_EPOCH);

    // 8 agreed = below M=9 → cannot finalise Agreed.
    for (v, sk, _) in n_agreed_verifications(8, result_hash, 100) {
        q.add_verification(v, 100, &sk.verifying_key()).unwrap();
    }
    let observer_sk = fresh_sk();
    let observer = fresh_peer();
    let now = UNIX_EPOCH + Duration::from_secs(params::T_MIN_SECS + 1);
    assert!(q.try_finalise(now, observer, &observer_sk).is_none());

    // Adding the 9th tips it over.
    let (v, sk, _) = &n_agreed_verifications(1, result_hash, 100)[0];
    q.add_verification(v.clone(), 100, &sk.verifying_key())
        .unwrap();
    let final_ = q.try_finalise(now, observer, &observer_sk).expect("finalised");
    assert!(matches!(final_.decision, QuorumDecision::Agreed));
}

// ---------------------------------------------------------------------
// 7 — Additional integration coverage
// ---------------------------------------------------------------------

#[test]
fn outcome_signature_verifies_under_observer_key() {
    let result_hash = content_hash(b"r");
    let mut q = Quorum::new(
        QuorumConfig::standard(),
        content_hash(b"spec"),
        result_hash,
        UNIX_EPOCH,
    );
    for (v, sk, _) in n_agreed_verifications(5, result_hash, 100) {
        q.add_verification(v, 100, &sk.verifying_key()).unwrap();
    }
    let observer_sk = fresh_sk();
    let observer = fresh_peer();
    let now = UNIX_EPOCH + Duration::from_secs(params::T_MIN_SECS + 1);
    let final_ = q.try_finalise(now, observer, &observer_sk).expect("finalised");
    final_
        .outcome
        .verify_signature(&observer_sk.verifying_key())
        .expect("outcome signature must verify under observer's key");
    assert_eq!(final_.outcome.observed_by, observer);
}

#[test]
fn custom_method_can_be_swapped_in() {
    // Demonstrates the trait is open for V0.3+ extensions.
    struct AlwaysAgreeMethod;
    impl VerifierMethodImpl for AlwaysAgreeMethod {
        fn verify(
            &self,
            _spec: &JobSpec,
            _result: &JobResult,
        ) -> Result<VerificationOutcome, VerifyError> {
            Ok(VerificationOutcome {
                matched: true,
                evidence_hash: None,
            })
        }
    }
    let v = build_verifier_with_rep(1000, 10);
    let sk = fresh_sk();
    let spec = build_spec(fresh_peer(), &sk);
    let result = build_result(fresh_peer(), &sk, spec.content_hash(), b"x");
    let seed = force_select_seed(&v, &spec, &result);
    match v.verify_task(&spec, &result, &AlwaysAgreeMethod, seed) {
        VerifyOutcome::Agreed(_) => {}
        other => panic!("expected Agreed, got {other:?}"),
    }
}
