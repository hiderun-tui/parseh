//! Integration tests for `parseh-task`.
//!
//! These exercise the public API exactly as a downstream crate
//! (`parseh-verify`, `parseh-shared-state`) will use it: build, sign,
//! CBOR-encode, ship, decode, verify.

use ed25519_dalek::SigningKey;
use libp2p::identity::Keypair;
use libp2p::PeerId;
use parseh_core::ServiceKind;
use parseh_task::{
    content_hash, from_cbor_bytes, to_cbor_bytes, ContentHash, JobInputs, JobKind, JobOutcome,
    JobResult, JobSpec, JobVerification, OutcomeVerdict, ResultMeta, VerifierMethod,
    VerifierVerdict, MAX_MESSAGE_SIZE_BYTES, WIRE_VERSION,
};
use rand::rngs::OsRng;

fn fresh_actor() -> (SigningKey, PeerId) {
    let sk = SigningKey::generate(&mut OsRng);
    let kp = Keypair::generate_ed25519();
    (sk, PeerId::from(kp.public()))
}

fn sample_spec() -> (JobSpec, ContentHash, SigningKey) {
    let (sk, peer) = fresh_actor();
    let (spec, h) = JobSpec::new_signed_at(
        JobKind::Inference,
        JobInputs::inference_prompt("translate the README to fa", 7),
        ServiceKind::Inference,
        false,
        1_700_000_000,
        peer,
        &sk,
    );
    (spec, h, sk)
}

fn sample_result(spec_hash: ContentHash) -> (JobResult, ContentHash, SigningKey) {
    let (sk, peer) = fresh_actor();
    let meta = ResultMeta {
        verifier_method: VerifierMethod::Deterministic,
        execution_time_ms: 2500,
        model_used: Some("qwen2.5:7b".into()),
        inference_token_count: Some(128),
    };
    let (r, h) = JobResult::new_signed_at(
        spec_hash,
        peer,
        1_700_000_010,
        meta,
        b"completion bytes".to_vec(),
        &sk,
    );
    (r, h, sk)
}

fn sample_verification(result_hash: ContentHash) -> (JobVerification, ContentHash, SigningKey) {
    let (sk, peer) = fresh_actor();
    let (v, h) = JobVerification::new_signed_at(
        result_hash,
        peer,
        VerifierVerdict::Agreed,
        VerifierMethod::Deterministic,
        1_700_000_020,
        &sk,
    );
    (v, h, sk)
}

// ── 1 ─────────────────────────────────────────────────────────────────

#[test]
fn spec_roundtrip_through_cbor_is_byte_identical() {
    let (spec, _h, _sk) = sample_spec();
    let bytes_a = to_cbor_bytes(&spec).expect("encode");
    let decoded: JobSpec = from_cbor_bytes(&bytes_a).expect("decode");
    let bytes_b = to_cbor_bytes(&decoded).expect("re-encode");
    assert_eq!(bytes_a, bytes_b, "CBOR round-trip must be byte-identical");
    assert_eq!(spec, decoded);
}

// ── 2 ─────────────────────────────────────────────────────────────────

#[test]
fn spec_signature_verifies_when_correct() {
    let (spec, _h, sk) = sample_spec();
    spec.verify_signature(&sk.verifying_key()).expect("sig");
}

// ── 3 ─────────────────────────────────────────────────────────────────

#[test]
fn spec_signature_fails_when_tampered() {
    let (mut spec, original_hash, sk) = sample_spec();
    // Flip a single byte in the prompt — the content hash must change
    // and the embedded signature must fail.
    spec.inputs.prompt_text = spec.inputs.prompt_text.map(|s| s + "!");
    assert_ne!(spec.content_hash(), original_hash);
    let err = spec.verify_signature(&sk.verifying_key()).unwrap_err();
    assert!(matches!(err, parseh_task::SignError::Verify(_)));
}

// ── 4 ─────────────────────────────────────────────────────────────────

#[test]
fn spec_signature_fails_when_signer_pubkey_does_not_match() {
    let (spec, _h, _sk) = sample_spec();
    let stranger = SigningKey::generate(&mut OsRng);
    spec.verify_signature(&stranger.verifying_key())
        .expect_err("stranger must not verify");
}

// ── 5 ─────────────────────────────────────────────────────────────────

#[test]
fn result_roundtrip_through_cbor_is_byte_identical() {
    let (spec, spec_hash, _) = sample_spec();
    let (result, _h, _sk) = sample_result(spec_hash);
    // Sanity: result's spec_hash reference matches.
    assert_eq!(result.spec_hash, spec.content_hash());

    let bytes_a = to_cbor_bytes(&result).expect("encode");
    let decoded: JobResult = from_cbor_bytes(&bytes_a).expect("decode");
    let bytes_b = to_cbor_bytes(&decoded).expect("re-encode");
    assert_eq!(bytes_a, bytes_b);
    assert_eq!(result, decoded);
}

// ── 6 ─────────────────────────────────────────────────────────────────

#[test]
fn result_signature_breaks_when_payload_tampered() {
    let (_spec, sh, _) = sample_spec();
    let (mut result, _h, sk) = sample_result(sh);
    result.result_payload.push(0xff);
    result
        .verify_signature(&sk.verifying_key())
        .expect_err("tampered payload must fail");
}

// ── 7 ─────────────────────────────────────────────────────────────────

#[test]
fn verification_roundtrip_through_cbor() {
    let (_spec, sh, _) = sample_spec();
    let (result, result_hash, _) = sample_result(sh);
    let (verif, _h, sk) = sample_verification(result_hash);
    assert_eq!(verif.result_hash, result.content_hash());

    let bytes_a = to_cbor_bytes(&verif).expect("encode");
    let decoded: JobVerification = from_cbor_bytes(&bytes_a).expect("decode");
    let bytes_b = to_cbor_bytes(&decoded).expect("re-encode");
    assert_eq!(bytes_a, bytes_b);
    assert_eq!(verif, decoded);
    decoded
        .verify_signature(&sk.verifying_key())
        .expect("decoded sig still verifies");
}

// ── 8 ─────────────────────────────────────────────────────────────────

#[test]
fn verification_with_disagreed_verdict_carries_evidence_hash() {
    let (_, sh, _) = sample_spec();
    let (_result, rh, _) = sample_result(sh);
    let (sk, peer) = fresh_actor();
    let evidence = content_hash(b"diff against reference completion");
    let (v, _h) = JobVerification::new_signed_at(
        rh,
        peer,
        VerifierVerdict::Disagreed {
            evidence_hash: evidence,
        },
        VerifierMethod::Deterministic,
        1_700_000_030,
        &sk,
    );
    let bytes = to_cbor_bytes(&v).unwrap();
    let decoded: JobVerification = from_cbor_bytes(&bytes).unwrap();
    match decoded.verdict {
        VerifierVerdict::Disagreed { evidence_hash } => assert_eq!(evidence_hash, evidence),
        other => panic!("expected Disagreed, got {other:?}"),
    }
}

// ── 9 ─────────────────────────────────────────────────────────────────

#[test]
fn outcome_roundtrip_through_cbor() {
    let (_, sh, _) = sample_spec();
    let (result, rh, _) = sample_result(sh);
    let (v1, vh1, _) = sample_verification(rh);
    let (v2, vh2, _) = sample_verification(result.content_hash());
    let (v3, vh3, _) = sample_verification(rh);

    let (sk, observer) = fresh_actor();
    let (outcome, _h) = JobOutcome::new_signed_at(
        sh,
        rh,
        vec![vh1, vh2, vh3],
        OutcomeVerdict::Valid {
            agreements: 3,
            disagreements: 0,
            abstentions: 0,
            reputation_weighted: 0.95,
        },
        1_700_000_040,
        observer,
        &sk,
    );
    // Touch v1, v2, v3 to silence "unused" if anything trims them later.
    let _ = (v1.wire_version, v2.wire_version, v3.wire_version);

    let bytes_a = to_cbor_bytes(&outcome).expect("encode");
    let decoded: JobOutcome = from_cbor_bytes(&bytes_a).expect("decode");
    let bytes_b = to_cbor_bytes(&decoded).expect("re-encode");
    assert_eq!(bytes_a, bytes_b);
    assert_eq!(outcome, decoded);
    decoded
        .verify_signature(&sk.verifying_key())
        .expect("decoded outcome sig still verifies");
}

// ── 10 ────────────────────────────────────────────────────────────────

#[test]
fn outcome_disputed_variant_roundtrips() {
    let (sk, observer) = fresh_actor();
    let (_, disputer) = fresh_actor();
    let (o, _) = JobOutcome::new_signed_at(
        ContentHash::zero(),
        ContentHash::zero(),
        vec![],
        OutcomeVerdict::Disputed {
            disputers: vec![disputer],
        },
        0,
        observer,
        &sk,
    );
    let bytes = to_cbor_bytes(&o).unwrap();
    let back: JobOutcome = from_cbor_bytes(&bytes).unwrap();
    assert_eq!(o, back);
}

// ── 11 ────────────────────────────────────────────────────────────────

#[test]
fn content_hash_is_deterministic_across_encodes() {
    let (spec, h1, _) = sample_spec();
    let h2 = spec.content_hash();
    // Re-encode/decode and rehash — must match.
    let bytes = to_cbor_bytes(&spec).unwrap();
    let decoded: JobSpec = from_cbor_bytes(&bytes).unwrap();
    let h3 = decoded.content_hash();
    assert_eq!(h1, h2);
    assert_eq!(h2, h3);
}

// ── 12 ────────────────────────────────────────────────────────────────

#[test]
fn content_hash_differs_for_one_bit_change() {
    let (spec_a, _, _) = sample_spec();
    let mut spec_b = spec_a.clone();
    // Flip exactly one bit in a payload field.
    spec_b.sensitive = !spec_b.sensitive;
    let ha = spec_a.content_hash();
    let hb = spec_b.content_hash();
    assert_ne!(ha, hb, "any payload change must produce a different hash");
}

// ── 13 ────────────────────────────────────────────────────────────────

#[test]
fn max_message_size_constant_is_respected_for_small_specs() {
    // A "real-world small spec" (~256-byte prompt) must encode well
    // under the 1 MiB cap.
    let (sk, peer) = fresh_actor();
    let prompt = "x".repeat(256);
    let (spec, _) = JobSpec::new_signed_at(
        JobKind::Inference,
        JobInputs::inference_prompt(prompt, 1),
        ServiceKind::Inference,
        false,
        1_700_000_000,
        peer,
        &sk,
    );
    let bytes = to_cbor_bytes(&spec).unwrap();
    assert!(bytes.len() < MAX_MESSAGE_SIZE_BYTES);
}

// ── 14 ────────────────────────────────────────────────────────────────

#[test]
fn oversize_payload_encodes_but_exceeds_message_cap() {
    // Build a spec whose embedded prompt deliberately exceeds the 1 MiB
    // cap. The CBOR encode itself does **not** enforce the cap (that is
    // a transport-layer concern), but the resulting bytes are larger
    // than `MAX_MESSAGE_SIZE_BYTES`, which is the property downstream
    // gossipsub validators check.
    let (sk, peer) = fresh_actor();
    let huge_prompt = "z".repeat(MAX_MESSAGE_SIZE_BYTES + 1024);
    let (spec, _) = JobSpec::new_signed_at(
        JobKind::Inference,
        JobInputs::inference_prompt(huge_prompt, 1),
        ServiceKind::Inference,
        false,
        1_700_000_000,
        peer,
        &sk,
    );
    let bytes = to_cbor_bytes(&spec).unwrap();
    assert!(
        bytes.len() > MAX_MESSAGE_SIZE_BYTES,
        "oversize spec must exceed the documented cap so transport rejects it"
    );
}

// ── 15 ────────────────────────────────────────────────────────────────

#[test]
fn wire_version_mismatch_does_not_panic_on_deserialise() {
    // A peer fields a message claiming a future wire_version. Decoding
    // must still succeed — the WIRE_VERSION check is the *consumer's*
    // responsibility, and reading the field is how the consumer learns
    // to drop. Specifically: no panic, no error, just a parsed struct
    // with an unexpected version number.
    let (mut spec, _, _) = sample_spec();
    spec.wire_version = u32::MAX;
    let bytes = to_cbor_bytes(&spec).unwrap();
    let back: JobSpec = from_cbor_bytes(&bytes).expect("decode tolerates unknown version");
    assert_eq!(back.wire_version, u32::MAX);
    assert_ne!(back.wire_version, WIRE_VERSION);
}

// ── 16 ────────────────────────────────────────────────────────────────

#[test]
fn end_to_end_lifecycle_one_spec_one_result_three_verifs_one_outcome() {
    // Walks the full V0.2 happy path. Every signature on every object
    // must verify against the publishing peer's pubkey, and every hash
    // reference must resolve.
    let (spec, spec_hash, spec_sk) = sample_spec();
    spec.verify_signature(&spec_sk.verifying_key()).unwrap();

    let (result, result_hash, exec_sk) = sample_result(spec_hash);
    result.verify_signature(&exec_sk.verifying_key()).unwrap();
    assert_eq!(result.spec_hash, spec_hash);

    let (v1, v1h, sk1) = sample_verification(result_hash);
    let (v2, v2h, sk2) = sample_verification(result_hash);
    let (v3, v3h, sk3) = sample_verification(result_hash);
    v1.verify_signature(&sk1.verifying_key()).unwrap();
    v2.verify_signature(&sk2.verifying_key()).unwrap();
    v3.verify_signature(&sk3.verifying_key()).unwrap();

    let (sk_obs, observer) = fresh_actor();
    let (outcome, _) = JobOutcome::new_signed_at(
        spec_hash,
        result_hash,
        vec![v1h, v2h, v3h],
        OutcomeVerdict::Valid {
            agreements: 3,
            disagreements: 0,
            abstentions: 0,
            reputation_weighted: 1.0,
        },
        1_700_000_050,
        observer,
        &sk_obs,
    );
    outcome.verify_signature(&sk_obs.verifying_key()).unwrap();
    assert_eq!(outcome.spec_hash, spec_hash);
    assert_eq!(outcome.result_hash, result_hash);
    assert_eq!(outcome.verification_hashes.len(), 3);
}

// ── 17 ────────────────────────────────────────────────────────────────

#[test]
fn decode_cbor_methods_match_module_level_helper() {
    let (spec, _, _) = sample_spec();
    let bytes = to_cbor_bytes(&spec).unwrap();
    let via_method = JobSpec::decode_cbor(&bytes).unwrap();
    let via_helper: JobSpec = from_cbor_bytes(&bytes).unwrap();
    assert_eq!(via_method, via_helper);
}

// ── 18 ────────────────────────────────────────────────────────────────

#[test]
fn signature_clearing_recovers_unsigned_form_for_verify() {
    // Whitebox check that the documented sign/verify convention works:
    // re-encoding the struct with the signature field cleared must
    // produce the exact bytes the signer signed.
    let (spec, _, sk) = sample_spec();
    let mut bare = spec.clone();
    bare.signature.clear();
    let bytes = to_cbor_bytes(&bare).unwrap();
    parseh_task::verify_bytes(&sk.verifying_key(), &bytes, &spec.signature)
        .expect("recovered unsigned bytes verify against the embedded signature");
}
