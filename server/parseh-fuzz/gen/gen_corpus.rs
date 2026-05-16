//! Seed-corpus generator for the `parseh-fuzz` targets.
//!
//! Produces 3–5 byte-identical CBOR fixtures for each of the seven
//! fuzz targets. Committed alongside the targets so a reviewer can
//! regenerate the corpus from source and `diff` against the on-disk
//! `corpus/<target>/` directory.
//!
//! Run from the crate root:
//!
//! ```sh
//! cd server/parseh-fuzz
//! cargo run --bin gen_corpus
//! ```
//!
//! Determinism: every signing key is constructed from a fixed
//! 32-byte seed and every wall-clock timestamp is hard-coded. The
//! output bytes are therefore stable across machines and Rust
//! versions (modulo serde / ciborium minor-version-bump behaviour,
//! which we accept — `[workspace.dependencies] ciborium = "0.2"`
//! pins the encoder version).

use std::fs;
use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use libp2p::{Multiaddr, PeerId};
use parseh_core::peer_registry::{
    encode_advertisement, CapabilityAdvertisement, InferenceCapability, ReadinessState,
    RelayCapability, ServiceKind, StorageCapability, CAPS_WIRE_VERSION,
};
use parseh_shared_state::{sign_delta, DeltaKind, StateDelta};
use parseh_task::{
    content_hash, to_cbor_bytes, ContentHash, JobInputs, JobKind, JobOutcome, JobResult, JobSpec,
    JobVerification, OutcomeVerdict, ResultMeta, VerifierMethod, VerifierVerdict,
};

fn corpus_dir(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("corpus");
    p.push(name);
    fs::create_dir_all(&p).expect("create corpus dir");
    p
}

fn write_seed(dir: &PathBuf, name: &str, bytes: &[u8]) {
    let mut path = dir.clone();
    path.push(name);
    fs::write(&path, bytes).unwrap_or_else(|e| panic!("write {}: {}", path.display(), e));
    println!("  wrote {} ({} bytes)", path.display(), bytes.len());
}

/// Deterministic signing key from a single-byte fill (`SigningKey::from_bytes`
/// accepts any 32 bytes; that's a private scalar pre-clamp).
fn sk_from(fill: u8) -> SigningKey {
    SigningKey::from_bytes(&[fill; 32])
}

/// Deterministic `PeerId` from a single-byte fill. We can't derive a
/// peer-id directly from a dalek `SigningKey` without a roundtrip
/// through `libp2p::identity`; the seed-bytes path is private to that
/// crate so we use `Keypair::ed25519_from_bytes` instead.
fn peer_from(fill: u8) -> PeerId {
    let kp = libp2p::identity::Keypair::ed25519_from_bytes([fill; 32])
        .expect("ed25519_from_bytes accepts any 32 bytes");
    PeerId::from(kp.public())
}

fn loopback() -> Multiaddr {
    "/ip4/127.0.0.1/tcp/8421"
        .parse()
        .expect("static multiaddr parses")
}

// ─── JobSpec ─────────────────────────────────────────────────────────────
fn gen_job_spec() {
    let dir = corpus_dir("fuzz_job_spec");
    println!("→ fuzz_job_spec");
    let cases: &[(&str, JobSpec)] = &[
        ("inference_minimal", {
            let (s, _) = JobSpec::new_signed_at(
                JobKind::Inference,
                JobInputs::inference_prompt("hello", 42),
                ServiceKind::Inference,
                false,
                1_700_000_000,
                peer_from(1),
                &sk_from(1),
            );
            s
        }),
        ("relay_sensitive", {
            let (s, _) = JobSpec::new_signed_at(
                JobKind::Relay,
                JobInputs {
                    prompt_text: None,
                    seed: None,
                    max_tokens: None,
                    content_refs: vec![ContentHash::zero()],
                },
                ServiceKind::Relay,
                true,
                1_700_000_100,
                peer_from(2),
                &sk_from(2),
            );
            s
        }),
        ("storage_with_refs", {
            let (s, _) = JobSpec::new_signed_at(
                JobKind::Storage,
                JobInputs {
                    prompt_text: None,
                    seed: None,
                    max_tokens: Some(0),
                    content_refs: vec![
                        content_hash(b"blob-a"),
                        content_hash(b"blob-b"),
                        content_hash(b"blob-c"),
                    ],
                },
                ServiceKind::Storage,
                false,
                1_700_000_200,
                peer_from(3),
                &sk_from(3),
            );
            s
        }),
        ("inference_long_prompt", {
            let prompt = "x".repeat(1024);
            let (s, _) = JobSpec::new_signed_at(
                JobKind::Inference,
                JobInputs::inference_prompt(prompt, u64::MAX),
                ServiceKind::Inference,
                true,
                1_700_000_300,
                peer_from(4),
                &sk_from(4),
            );
            s
        }),
    ];
    for (n, s) in cases {
        write_seed(&dir, n, &to_cbor_bytes(s).unwrap());
    }
}

// ─── JobResult ──────────────────────────────────────────────────────────
fn gen_job_result() {
    let dir = corpus_dir("fuzz_job_result");
    println!("→ fuzz_job_result");
    let meta_inf = ResultMeta {
        verifier_method: VerifierMethod::Deterministic,
        execution_time_ms: 1234,
        model_used: Some("qwen2.5:7b".into()),
        inference_token_count: Some(42),
    };
    let meta_min = ResultMeta {
        verifier_method: VerifierMethod::SpotCheck,
        execution_time_ms: 0,
        model_used: None,
        inference_token_count: None,
    };
    let cases: Vec<(&str, JobResult)> = vec![
        ("inference_short", {
            let (r, _) = JobResult::new_signed_at(
                ContentHash::zero(),
                peer_from(1),
                1_700_000_100,
                meta_inf.clone(),
                b"completion text".to_vec(),
                &sk_from(1),
            );
            r
        }),
        ("empty_payload", {
            let (r, _) = JobResult::new_signed_at(
                ContentHash::zero(),
                peer_from(2),
                0,
                meta_min.clone(),
                Vec::new(),
                &sk_from(2),
            );
            r
        }),
        ("large_payload", {
            let (r, _) = JobResult::new_signed_at(
                content_hash(b"spec-3"),
                peer_from(3),
                1_700_000_200,
                meta_inf.clone(),
                vec![0xAAu8; 4096],
                &sk_from(3),
            );
            r
        }),
    ];
    for (n, r) in &cases {
        write_seed(&dir, n, &to_cbor_bytes(r).unwrap());
    }
}

// ─── JobVerification ────────────────────────────────────────────────────
fn gen_job_verification() {
    let dir = corpus_dir("fuzz_job_verification");
    println!("→ fuzz_job_verification");
    let cases: Vec<(&str, JobVerification)> = vec![
        ("agreed", {
            let (v, _) = JobVerification::new_signed_at(
                ContentHash::zero(),
                peer_from(1),
                VerifierVerdict::Agreed,
                VerifierMethod::Deterministic,
                1_700_000_200,
                &sk_from(1),
            );
            v
        }),
        ("disagreed_with_evidence", {
            let (v, _) = JobVerification::new_signed_at(
                content_hash(b"result-x"),
                peer_from(2),
                VerifierVerdict::Disagreed {
                    evidence_hash: content_hash(b"evidence-blob"),
                },
                VerifierMethod::SpotCheck,
                1_700_000_210,
                &sk_from(2),
            );
            v
        }),
        ("abstained_statistical", {
            let (v, _) = JobVerification::new_signed_at(
                content_hash(b"result-y"),
                peer_from(3),
                VerifierVerdict::Abstained,
                VerifierMethod::Statistical,
                1_700_000_220,
                &sk_from(3),
            );
            v
        }),
    ];
    for (n, v) in &cases {
        write_seed(&dir, n, &to_cbor_bytes(v).unwrap());
    }
}

// ─── JobOutcome ─────────────────────────────────────────────────────────
fn gen_job_outcome() {
    let dir = corpus_dir("fuzz_job_outcome");
    println!("→ fuzz_job_outcome");
    let cases: Vec<(&str, JobOutcome)> = vec![
        ("valid_unanimous", {
            let (o, _) = JobOutcome::new_signed_at(
                ContentHash::zero(),
                ContentHash::zero(),
                vec![
                    content_hash(b"v1"),
                    content_hash(b"v2"),
                    content_hash(b"v3"),
                ],
                OutcomeVerdict::Valid {
                    agreements: 3,
                    disagreements: 0,
                    abstentions: 0,
                    reputation_weighted: 0.987,
                },
                1_700_000_300,
                peer_from(1),
                &sk_from(1),
            );
            o
        }),
        ("disputed_two_peers", {
            let (o, _) = JobOutcome::new_signed_at(
                content_hash(b"spec-disputed"),
                content_hash(b"result-disputed"),
                vec![content_hash(b"v1"), content_hash(b"v2")],
                OutcomeVerdict::Disputed {
                    disputers: vec![peer_from(10), peer_from(11)],
                },
                1_700_000_310,
                peer_from(2),
                &sk_from(2),
            );
            o
        }),
        ("indeterminate", {
            let (o, _) = JobOutcome::new_signed_at(
                content_hash(b"spec-i"),
                content_hash(b"result-i"),
                vec![],
                OutcomeVerdict::Indeterminate,
                1_700_000_320,
                peer_from(3),
                &sk_from(3),
            );
            o
        }),
    ];
    for (n, o) in &cases {
        write_seed(&dir, n, &to_cbor_bytes(o).unwrap());
    }
}

// ─── CapabilityAdvertisement ────────────────────────────────────────────
fn gen_capability_advertisement() {
    let dir = corpus_dir("fuzz_capability_advertisement");
    println!("→ fuzz_capability_advertisement");
    let vk_bytes = *sk_from(1).verifying_key().as_bytes();
    let cases: Vec<(&str, CapabilityAdvertisement)> = vec![
        ("v2_inference_ready", CapabilityAdvertisement {
            peer_id: peer_from(1),
            version: CAPS_WIRE_VERSION,
            services: vec![ServiceKind::Inference],
            inference: Some(InferenceCapability {
                models: vec!["qwen2.5:7b".into()],
                context_size: 4096,
                estimated_tokens_per_sec: 50,
            }),
            relay: None,
            storage: None,
            network_address: loopback(),
            signed_at: 1_700_000_000,
            ttl_seconds: 300,
            verifying_key_bytes: vk_bytes,
            reachable_addrs: vec![loopback()],
            readiness: ReadinessState::Ready,
            has_external_internet: false,
            bandwidth_mbps_external: None,
        }),
        ("v2_relay_bridge", CapabilityAdvertisement {
            peer_id: peer_from(2),
            version: CAPS_WIRE_VERSION,
            services: vec![ServiceKind::Relay],
            inference: None,
            relay: Some(RelayCapability {
                bandwidth_mbps: 100,
                transport_kinds: vec!["REALITY".into(), "SOCKS5".into()],
            }),
            storage: None,
            network_address: loopback(),
            signed_at: 1_700_000_100,
            ttl_seconds: 300,
            verifying_key_bytes: vk_bytes,
            reachable_addrs: vec![loopback()],
            readiness: ReadinessState::Active,
            has_external_internet: true,
            bandwidth_mbps_external: Some(50),
        }),
        ("v2_storage_degraded", CapabilityAdvertisement {
            peer_id: peer_from(3),
            version: CAPS_WIRE_VERSION,
            services: vec![ServiceKind::Storage],
            inference: None,
            relay: None,
            storage: Some(StorageCapability {
                free_mb: 1024,
                persistence_guarantee: "best-effort".into(),
            }),
            network_address: loopback(),
            signed_at: 1_700_000_200,
            ttl_seconds: 60,
            verifying_key_bytes: [0u8; 32],
            reachable_addrs: vec![],
            readiness: ReadinessState::Degraded,
            has_external_internet: false,
            bandwidth_mbps_external: None,
        }),
    ];
    for (n, ad) in &cases {
        write_seed(&dir, n, &encode_advertisement(ad).unwrap());
    }

    // Additionally: a v1 wire shape (omits the five V0.2.5 fields) to
    // give libFuzzer a seed that exercises the `serde(default)` path.
    #[derive(serde::Serialize)]
    struct V0_2_1Ad {
        peer_id: PeerId,
        version: u32,
        services: Vec<ServiceKind>,
        inference: Option<InferenceCapability>,
        relay: Option<RelayCapability>,
        storage: Option<StorageCapability>,
        network_address: Multiaddr,
        signed_at: u64,
        ttl_seconds: u32,
    }
    let old = V0_2_1Ad {
        peer_id: peer_from(4),
        version: 1,
        services: vec![ServiceKind::Inference],
        inference: Some(InferenceCapability {
            models: vec!["tinyllama:1.1b".into()],
            context_size: 2048,
            estimated_tokens_per_sec: 30,
        }),
        relay: None,
        storage: None,
        network_address: loopback(),
        signed_at: 1_700_000_000,
        ttl_seconds: 300,
    };
    let mut buf = Vec::new();
    ciborium::into_writer(&old, &mut buf).expect("encode v1 fallback shape");
    write_seed(&dir, "v1_legacy_shape", &buf);
}

// ─── StateDelta ─────────────────────────────────────────────────────────
fn gen_state_delta() {
    let dir = corpus_dir("fuzz_state_delta");
    println!("→ fuzz_state_delta");

    let signer = sk_from(1);
    let observer = peer_from(1);

    let reputation_kind = DeltaKind::Reputation {
        peer: peer_from(2),
        delta: 5,
        reason: "verification_agreed".into(),
        related_hash: Some(content_hash(b"outcome-rep")),
    };
    let signed_rep = sign_delta(
        StateDelta::unsigned(reputation_kind, observer, 1_700_000_400),
        &signer,
    )
    .expect("sign reputation");
    write_seed(&dir, "reputation", &signed_rep.encode_cbor().unwrap());

    let gov_kind = DeltaKind::GovernanceRule {
        rule_name: "quorum_standard".into(),
        rule_value: "{\"m\":3,\"n\":5}".into(),
        proposer: peer_from(3),
        approvers: vec![peer_from(4), peer_from(5), peer_from(6)],
    };
    let signed_gov = sign_delta(
        StateDelta::unsigned(gov_kind, observer, 1_700_000_500),
        &signer,
    )
    .expect("sign governance");
    write_seed(&dir, "governance_rule", &signed_gov.encode_cbor().unwrap());

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
        1_700_000_600,
        observer,
        &signer,
    );
    let signed_outcome = sign_delta(
        StateDelta::unsigned(DeltaKind::Outcome(outcome), observer, 1_700_000_700),
        &signer,
    )
    .expect("sign outcome");
    write_seed(&dir, "outcome", &signed_outcome.encode_cbor().unwrap());
}

// ─── signature_verify ───────────────────────────────────────────────────
fn gen_signature_verify() {
    let dir = corpus_dir("fuzz_signature_verify");
    println!("→ fuzz_signature_verify");

    // Layout: 32 pubkey · 64 sig · N message bytes.
    fn bundle(pk: &[u8; 32], sig: &[u8; 64], msg: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(32 + 64 + msg.len());
        v.extend_from_slice(pk);
        v.extend_from_slice(sig);
        v.extend_from_slice(msg);
        v
    }

    let sk = sk_from(7);
    let vk = sk.verifying_key();

    // 1. Valid signature → exercises the success path.
    let msg_a: &[u8] = b"PARSEH fuzz seed";
    let sig_a = parseh_task::sign_bytes(&sk, msg_a);
    write_seed(
        &dir,
        "valid_pk_sig_msg",
        &bundle(vk.as_bytes(), &sig_a, msg_a),
    );

    // 2. Valid pubkey, sig over different message → exercises the
    //    "cryptographic-verify-fails" path.
    let sig_b = parseh_task::sign_bytes(&sk, b"originally signed this");
    write_seed(
        &dir,
        "wrong_message",
        &bundle(vk.as_bytes(), &sig_b, b"but here's something else"),
    );

    // 3. Garbage pubkey bytes (not a valid edwards point) → early-return
    //    inside the fuzz target.
    write_seed(
        &dir,
        "garbage_pubkey",
        &bundle(&[0xFFu8; 32], &[0u8; 64], b"msg"),
    );

    // 4. Sig with wrong second-half (R good, S bogus) → another
    //    distinct dalek error path.
    let mut mangled = sig_a;
    mangled[63] ^= 0xAA;
    write_seed(
        &dir,
        "mangled_sig_low",
        &bundle(vk.as_bytes(), &mangled, msg_a),
    );

    // 5. Minimum-length input (96 bytes, all zero) → trivial seed
    //    libFuzzer can mutate from quickly.
    let zero = vec![0u8; 96];
    write_seed(&dir, "all_zero_96", &zero);
}

fn main() {
    println!("parseh-fuzz · regenerating seed corpus");
    gen_job_spec();
    gen_job_result();
    gen_job_verification();
    gen_job_outcome();
    gen_capability_advertisement();
    gen_state_delta();
    gen_signature_verify();
    println!("✓ done");
}
