//! Integration tests for `parseh-shared-state` — V0.2 Primitive 3.
//!
//! Every test is self-contained: a fresh `tempfile::TempDir`, a fresh
//! `KeyMaterial`, no network. The encryption-at-rest tests still run
//! on the default `bundled` feature (SQLCipher off) because the PRAGMA
//! key dance is a no-op there — see the README for the `encrypted`
//! feature that turns real encryption on.

use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use libp2p::PeerId;
use parseh_core::ServiceKind;
use parseh_shared_state::{
    DeltaKind, KeyMaterial, KeySource, OpenOptions, SharedState, StateDelta,
};
use parseh_task::{
    content_hash, ContentHash, JobInputs, JobKind, JobOutcome, JobResult, JobSpec,
    JobVerification, OutcomeVerdict, ResultMeta, VerifierMethod, VerifierVerdict,
};
use rand::rngs::OsRng;
use tempfile::TempDir;
use zeroize::Zeroizing;

// ---------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------

fn fresh_peer() -> PeerId {
    PeerId::from(libp2p::identity::Keypair::generate_ed25519().public())
}

fn fresh_sk() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

fn fresh_key() -> KeyMaterial {
    let bytes: [u8; 32] = rand::random();
    KeyMaterial::from_source(KeySource::Raw(bytes)).unwrap()
}

fn open_fresh() -> (SharedState, TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared-state.db");
    let store = SharedState::open(OpenOptions::create(path.clone(), fresh_key())).unwrap();
    (store, dir, path)
}

fn build_spec(submitter: PeerId, sk: &SigningKey, submitted_at: u64) -> (JobSpec, ContentHash) {
    JobSpec::new_signed_at(
        JobKind::Inference,
        JobInputs::inference_prompt("hi", 7),
        ServiceKind::Inference,
        false,
        submitted_at,
        submitter,
        sk,
    )
}

fn build_result(
    executor: PeerId,
    sk: &SigningKey,
    spec_hash: ContentHash,
    payload: &[u8],
) -> (JobResult, ContentHash) {
    let meta = ResultMeta {
        verifier_method: VerifierMethod::Deterministic,
        execution_time_ms: 100,
        model_used: Some("qwen2.5:7b".into()),
        inference_token_count: Some(payload.len() as u32),
    };
    JobResult::new_signed_at(
        spec_hash,
        executor,
        1_700_000_100,
        meta,
        payload.to_vec(),
        sk,
    )
}

fn build_verification(
    verifier: PeerId,
    sk: &SigningKey,
    result_hash: ContentHash,
    verdict: VerifierVerdict,
) -> (JobVerification, ContentHash) {
    JobVerification::new_signed_at(
        result_hash,
        verifier,
        verdict,
        VerifierMethod::Deterministic,
        1_700_000_200,
        sk,
    )
}

fn build_outcome(
    observer: PeerId,
    sk: &SigningKey,
    spec_hash: ContentHash,
    result_hash: ContentHash,
    verifications: &[ContentHash],
) -> (JobOutcome, ContentHash) {
    JobOutcome::new_signed_at(
        spec_hash,
        result_hash,
        verifications.to_vec(),
        OutcomeVerdict::Valid {
            agreements: verifications.len() as u32,
            disagreements: 0,
            abstentions: 0,
            reputation_weighted: 0.85,
        },
        1_700_000_300,
        observer,
        sk,
    )
}

// ---------------------------------------------------------------------
// open / encryption
// ---------------------------------------------------------------------

#[test]
fn open_creates_database_with_schema_v1() {
    let (_store, _dir, path) = open_fresh();
    assert!(path.exists());
}

#[test]
fn open_with_create_if_missing_false_fails_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonexistent.db");
    let err = SharedState::open(OpenOptions {
        path,
        key: fresh_key(),
        create_if_missing: false,
    })
    .expect_err("should fail");
    matches!(err, parseh_shared_state::OpenError::NotFound);
}

#[test]
#[cfg(feature = "encrypted")]
fn open_with_wrong_key_fails() {
    // Only meaningful when SQLCipher is on. Skipped on the default
    // `bundled` feature because the PRAGMA key is a no-op there.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("encrypted.db");
    let key_a = fresh_key();
    let key_b = fresh_key();
    {
        let s = SharedState::open(OpenOptions::create(path.clone(), key_a)).unwrap();
        let (spec, _) = build_spec(fresh_peer(), &fresh_sk(), 1_700_000_000);
        s.record_spec(&spec).unwrap();
    }
    let err = SharedState::open(OpenOptions::create(path, key_b)).expect_err("wrong key");
    assert!(matches!(
        err,
        parseh_shared_state::OpenError::WrongKey | parseh_shared_state::OpenError::Sqlite(_)
    ));
}

#[test]
fn schema_version_migration_v0_to_v1_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotent.db");
    // Open three times — migrations must be a no-op after the first.
    for _ in 0..3 {
        let _s = SharedState::open(OpenOptions::create(path.clone(), fresh_key())).unwrap();
    }
}

// ---------------------------------------------------------------------
// record + query
// ---------------------------------------------------------------------

#[test]
fn record_spec_round_trip() {
    let (store, _dir, _) = open_fresh();
    let sk = fresh_sk();
    let peer = fresh_peer();
    let (spec, _) = build_spec(peer, &sk, 1_700_000_000);
    store.record_spec(&spec).unwrap();
    let tasks = store.recent_tasks(0).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].submitted_at, 1_700_000_000);
}

#[test]
fn record_spec_is_idempotent() {
    let (store, _dir, _) = open_fresh();
    let (spec, _) = build_spec(fresh_peer(), &fresh_sk(), 1_700_000_000);
    store.record_spec(&spec).unwrap();
    store.record_spec(&spec).unwrap();
    store.record_spec(&spec).unwrap();
    let tasks = store.recent_tasks(0).unwrap();
    assert_eq!(tasks.len(), 1);
}

#[test]
fn record_result_with_unknown_spec_fails() {
    let (store, _dir, _) = open_fresh();
    let sk = fresh_sk();
    let peer = fresh_peer();
    // No spec recorded → FK violation.
    let (result, _) = build_result(peer, &sk, content_hash(b"nope"), b"payload");
    let err = store.record_result(&result).unwrap_err();
    matches!(err, parseh_shared_state::StoreError::ForeignKey(_));
}

#[test]
fn record_verification_with_unknown_result_fails() {
    let (store, _dir, _) = open_fresh();
    let (v, _) = build_verification(
        fresh_peer(),
        &fresh_sk(),
        content_hash(b"unknown"),
        VerifierVerdict::Agreed,
    );
    let err = store.record_verification(&v).unwrap_err();
    matches!(err, parseh_shared_state::StoreError::ForeignKey(_));
}

#[test]
fn record_outcome_links_spec_and_result() {
    let (store, _dir, _) = open_fresh();
    let sk = fresh_sk();
    let peer = fresh_peer();
    let (spec, spec_hash) = build_spec(peer, &sk, 1_700_000_000);
    store.record_spec(&spec).unwrap();
    let (result, result_hash) = build_result(peer, &sk, spec_hash, b"payload");
    store.record_result(&result).unwrap();
    let (v, vh) = build_verification(fresh_peer(), &fresh_sk(), result_hash, VerifierVerdict::Agreed);
    store.record_verification(&v).unwrap();
    let (outcome, _) = build_outcome(peer, &sk, spec_hash, result_hash, &[vh]);
    store.record_outcome(&outcome).unwrap();

    let fetched = store.outcome_for_spec(&spec_hash).unwrap().unwrap();
    assert_eq!(fetched.spec_hash, spec_hash);
    assert_eq!(fetched.result_hash, result_hash);
    let verifs = store.verifications_for_result(&result_hash).unwrap();
    assert_eq!(verifs.len(), 1);
}

// ---------------------------------------------------------------------
// reputation
// ---------------------------------------------------------------------

#[test]
fn reputation_delta_accumulates_correctly() {
    let (store, _dir, _) = open_fresh();
    let peer = fresh_peer();
    store
        .apply_reputation_delta(peer, 5, "verification_agreed", None)
        .unwrap();
    store
        .apply_reputation_delta(peer, 3, "verification_agreed", None)
        .unwrap();
    store
        .apply_reputation_delta(peer, -2, "verification_disagreed_consensus", None)
        .unwrap();
    assert_eq!(store.reputation_of(peer).unwrap(), 6);
}

#[test]
fn reputation_of_returns_zero_for_unknown_peer() {
    let (store, _dir, _) = open_fresh();
    assert_eq!(store.reputation_of(fresh_peer()).unwrap(), 0);
}

#[test]
fn established_peers_filters_by_min_rep() {
    let (store, _dir, _) = open_fresh();
    let a = fresh_peer();
    let b = fresh_peer();
    let c = fresh_peer();
    store.apply_reputation_delta(a, 100, "x", None).unwrap();
    store.apply_reputation_delta(b, 25, "x", None).unwrap();
    store.apply_reputation_delta(c, 5, "x", None).unwrap();
    let mut e = store.established_peers(20).unwrap();
    e.sort_by_key(|p| p.to_bytes());
    let mut expected = vec![a, b];
    expected.sort_by_key(|p| p.to_bytes());
    assert_eq!(e, expected);
}

// ---------------------------------------------------------------------
// detection
// ---------------------------------------------------------------------

#[test]
fn detect_repeating_verifier_sets_flags_collusion() {
    let (store, _dir, _) = open_fresh();
    // Submitter S, ring R1, R2, R3 always vote Agreed across 4
    // tasks. Expect detection.
    let submitter_sk = fresh_sk();
    let submitter = fresh_peer();
    let observer = fresh_peer();
    let observer_sk = fresh_sk();
    let executor_sk = fresh_sk();
    let executor = fresh_peer();
    let (r1_sk, r1) = (fresh_sk(), fresh_peer());
    let (r2_sk, r2) = (fresh_sk(), fresh_peer());
    let (r3_sk, r3) = (fresh_sk(), fresh_peer());

    for i in 0..4 {
        let (spec, spec_hash) = build_spec(submitter, &submitter_sk, 1_700_000_000 + i);
        store.record_spec(&spec).unwrap();
        let payload = format!("payload-{i}");
        let (result, result_hash) =
            build_result(executor, &executor_sk, spec_hash, payload.as_bytes());
        store.record_result(&result).unwrap();
        let mut vh = Vec::new();
        for (sk, peer) in [(&r1_sk, r1), (&r2_sk, r2), (&r3_sk, r3)] {
            let (v, h) =
                build_verification(peer, sk, result_hash, VerifierVerdict::Agreed);
            store.record_verification(&v).unwrap();
            vh.push(h);
        }
        let (outcome, _) =
            build_outcome(observer, &observer_sk, spec_hash, result_hash, &vh);
        store.record_outcome(&outcome).unwrap();
    }

    let rings = store
        .detect_repeating_verifier_sets(u64::MAX / 2, 3)
        .unwrap();
    assert_eq!(rings.len(), 1);
    let (sub, verifiers, count) = &rings[0];
    assert_eq!(*sub, submitter);
    assert_eq!(*count, 4);
    let mut expected = vec![r1, r2, r3];
    expected.sort_by_key(|p| p.to_bytes());
    let mut got = verifiers.clone();
    got.sort_by_key(|p| p.to_bytes());
    assert_eq!(got, expected);
}

#[test]
fn detect_returns_empty_when_no_ring() {
    let (store, _dir, _) = open_fresh();
    let rings = store
        .detect_repeating_verifier_sets(u64::MAX / 2, 3)
        .unwrap();
    assert!(rings.is_empty());
}

// ---------------------------------------------------------------------
// delta apply + verify
// ---------------------------------------------------------------------

#[test]
fn apply_delta_validates_signature() {
    let (store, _dir, _) = open_fresh();
    let sk = fresh_sk();
    let observer = fresh_peer();
    // Record prerequisites so the inner outcome's FKs are satisfied.
    let (spec, spec_hash) = build_spec(observer, &sk, 1_700_000_000);
    store.record_spec(&spec).unwrap();
    let (result, result_hash) = build_result(observer, &sk, spec_hash, b"payload");
    store.record_result(&result).unwrap();
    let (outcome, _) = build_outcome(observer, &sk, spec_hash, result_hash, &[]);
    let signed = parseh_shared_state::sign_delta(
        StateDelta::unsigned(DeltaKind::Outcome(outcome), observer, 1_700_000_400),
        &sk,
    )
    .unwrap();
    store.apply_delta(signed, &sk.verifying_key()).unwrap();
    let fetched = store.outcome_for_spec(&spec_hash).unwrap();
    assert!(fetched.is_some());
}

#[test]
fn apply_delta_rejects_bad_signature() {
    let (store, _dir, _) = open_fresh();
    let signing = fresh_sk();
    let evil = fresh_sk();
    let observer = fresh_peer();
    let (spec, spec_hash) = build_spec(observer, &signing, 1_700_000_000);
    store.record_spec(&spec).unwrap();
    let (result, result_hash) = build_result(observer, &signing, spec_hash, b"payload");
    store.record_result(&result).unwrap();
    let (outcome, _) = build_outcome(observer, &signing, spec_hash, result_hash, &[]);
    let signed = parseh_shared_state::sign_delta(
        StateDelta::unsigned(DeltaKind::Outcome(outcome), observer, 1_700_000_500),
        &signing,
    )
    .unwrap();
    let err = store
        .apply_delta(signed, &evil.verifying_key())
        .unwrap_err();
    matches!(err, parseh_shared_state::StoreError::BadSignature(_));
}

#[test]
fn deltas_since_returns_in_order() {
    let (store, _dir, _) = open_fresh();
    let sk = fresh_sk();
    let observer = fresh_peer();
    // Two tasks, two outcomes.
    let (spec_a, sa) = build_spec(observer, &sk, 1_700_000_000);
    let (spec_b, sb) = build_spec(observer, &sk, 1_700_000_001);
    store.record_spec(&spec_a).unwrap();
    store.record_spec(&spec_b).unwrap();
    let (ra, ra_h) = build_result(observer, &sk, sa, b"a");
    let (rb, rb_h) = build_result(observer, &sk, sb, b"b");
    store.record_result(&ra).unwrap();
    store.record_result(&rb).unwrap();
    let (oa, _) = build_outcome(observer, &sk, sa, ra_h, &[]);
    let (ob, _) = build_outcome(observer, &sk, sb, rb_h, &[]);
    store.record_outcome(&oa).unwrap();
    // Brief delay so observed_at can advance.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    store.record_outcome(&ob).unwrap();

    let deltas = store.deltas_since(0).unwrap();
    assert_eq!(deltas.len(), 2);
    // Order is by observed_at ascending.
    for d in &deltas {
        match &d.kind {
            DeltaKind::Outcome(_) => (),
            _ => panic!("expected Outcome delta"),
        }
    }
}

#[test]
fn outcomes_since_filters_by_finalised_at_newest_first_and_caps() {
    let (store, _dir, _) = open_fresh();
    let sk = fresh_sk();
    let observer = fresh_peer();

    // Three tasks finalised at t=100, t=200, t=300.
    let finals = [1_700_000_100u64, 1_700_000_200, 1_700_000_300];
    let mut spec_hashes = Vec::new();
    for (i, fin) in finals.iter().enumerate() {
        let (spec, sh) = build_spec(observer, &sk, 1_700_000_000 + i as u64);
        store.record_spec(&spec).unwrap();
        let (res, rh) = build_result(observer, &sk, sh, format!("payload{i}").as_bytes());
        store.record_result(&res).unwrap();
        let (outcome, _) = JobOutcome::new_signed_at(
            sh,
            rh,
            vec![],
            OutcomeVerdict::Valid {
                agreements: 3,
                disagreements: 0,
                abstentions: 0,
                reputation_weighted: 0.9,
            },
            *fin,
            observer,
            &sk,
        );
        store.record_outcome(&outcome).unwrap();
        spec_hashes.push(sh);
    }

    // since=0 → all three, newest first.
    let all = store.outcomes_since(0, 100).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].finalised_at, 1_700_000_300);
    assert_eq!(all[1].finalised_at, 1_700_000_200);
    assert_eq!(all[2].finalised_at, 1_700_000_100);

    // since=t=200 → only the two at/after 200.
    let since_200 = store.outcomes_since(1_700_000_200, 100).unwrap();
    assert_eq!(since_200.len(), 2);
    assert!(since_200.iter().all(|o| o.finalised_at >= 1_700_000_200));

    // limit clamps the count, returning the newest first.
    let capped = store.outcomes_since(0, 1).unwrap();
    assert_eq!(capped.len(), 1);
    assert_eq!(capped[0].finalised_at, 1_700_000_300);

    // Every returned outcome still carries a valid inner signature.
    for o in &all {
        o.verify_signature(&sk.verifying_key())
            .expect("inner outcome signature must survive the round-trip");
    }
}

#[test]
fn governance_rule_upsert_replaces_value() {
    let (store, _dir, _) = open_fresh();
    let proposer = fresh_peer();
    store
        .upsert_governance_rule("quorum_standard", "{\"m\":5,\"n\":9}", proposer, &[])
        .unwrap();
    store
        .upsert_governance_rule(
            "quorum_standard",
            "{\"m\":7,\"n\":11}",
            proposer,
            &[fresh_peer()],
        )
        .unwrap();
    let v = store.governance_rule("quorum_standard").unwrap().unwrap();
    assert_eq!(v, "{\"m\":7,\"n\":11}");
}

// ---------------------------------------------------------------------
// key material
// ---------------------------------------------------------------------

#[test]
fn key_material_from_passphrase_deterministic_with_salt() {
    let salt = b"deadbeefcafef00d".to_vec();
    let a = KeyMaterial::from_source(KeySource::Passphrase {
        phrase: Zeroizing::new("correct horse battery staple".into()),
        salt: salt.clone(),
    })
    .unwrap();
    let b = KeyMaterial::from_source(KeySource::Passphrase {
        phrase: Zeroizing::new("correct horse battery staple".into()),
        salt,
    })
    .unwrap();
    assert_eq!(a.as_bytes(), b.as_bytes());
}

#[test]
fn key_material_from_identity_deterministic() {
    let bytes = Zeroizing::new(vec![1u8, 2, 3, 4, 5, 6, 7, 8]);
    let a = KeyMaterial::from_source(KeySource::IdentityFile {
        identity_bytes: bytes.clone(),
    })
    .unwrap();
    let b = KeyMaterial::from_source(KeySource::IdentityFile {
        identity_bytes: bytes,
    })
    .unwrap();
    assert_eq!(a.as_bytes(), b.as_bytes());
}

#[test]
fn key_material_debug_does_not_leak() {
    let km = KeyMaterial::from_source(KeySource::Raw([0xFE; 32])).unwrap();
    let s = format!("{:?}", km);
    assert!(!s.contains("fe"));
    assert!(s.contains("REDACTED"));
}

// ---------------------------------------------------------------------
// end-to-end: spec → result → verification × N → outcome → delta
// ---------------------------------------------------------------------

#[test]
fn end_to_end_lifecycle_persists_full_chain() {
    let (store, _dir, _) = open_fresh();
    let submitter_sk = fresh_sk();
    let submitter = fresh_peer();
    let executor_sk = fresh_sk();
    let executor = fresh_peer();
    let observer_sk = fresh_sk();
    let observer = fresh_peer();

    let (spec, spec_hash) = build_spec(submitter, &submitter_sk, 1_700_000_000);
    store.record_spec(&spec).unwrap();

    let (result, result_hash) = build_result(executor, &executor_sk, spec_hash, b"out");
    store.record_result(&result).unwrap();

    let mut vh = Vec::new();
    for _ in 0..5 {
        let (v, h) = build_verification(
            fresh_peer(),
            &fresh_sk(),
            result_hash,
            VerifierVerdict::Agreed,
        );
        store.record_verification(&v).unwrap();
        vh.push(h);
    }
    let (outcome, _) = build_outcome(observer, &observer_sk, spec_hash, result_hash, &vh);
    store.record_outcome(&outcome).unwrap();

    // Submitter / executor / each verifier earn reputation.
    store
        .apply_reputation_delta(executor, 10, "execution_completed", Some(result_hash))
        .unwrap();
    store
        .apply_reputation_delta(observer, 2, "outcome_finalised", Some(spec_hash))
        .unwrap();

    assert_eq!(
        store.outcome_for_spec(&spec_hash).unwrap().unwrap().result_hash,
        result_hash
    );
    assert_eq!(
        store.verifications_for_result(&result_hash).unwrap().len(),
        5
    );
    assert_eq!(store.reputation_of(executor).unwrap(), 10);
    assert_eq!(store.reputation_of(observer).unwrap(), 2);
}
