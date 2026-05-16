//! Integration tests for `parseh-agent-spec`.
//!
//! These exercise the public API the way a downstream crate would:
//! build an [`AgentDefinition`], sign it, CBOR-encode it, ship it,
//! decode it, verify the signature, validate inputs and outputs
//! against the embedded schemas, fork it with parent lineage.

use ed25519_dalek::SigningKey;
use libp2p::identity::Keypair;
use libp2p::PeerId;
use parseh_agent_spec::{
    from_cbor_bytes, to_cbor_bytes, validate_against_schema, AgentDefinition, AgentId,
    AgentMetadata, AgentVersion, ContentHash, DefinitionError, ForkReason, InputSchema,
    KnowledgeKind, KnowledgeRef, ModelRequirements, OutputSchema, ParentRef, SignError,
    ValidationError, MAX_DEFINITION_SIZE_BYTES, SPEC_VERSION,
};
use rand::rngs::OsRng;

// ── helpers ──────────────────────────────────────────────────────────

fn fresh_actor() -> (SigningKey, PeerId) {
    let sk = SigningKey::generate(&mut OsRng);
    let kp = Keypair::generate_ed25519();
    (sk, PeerId::from(kp.public()))
}

fn input_schema_strict() -> InputSchema {
    InputSchema {
        schema_json: r#"{
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "article_url": {"type": "string", "format": "uri"},
                "language": {"type": "string", "enum": ["fa", "en"]}
            },
            "required": ["article_url", "language"],
            "additionalProperties": false
        }"#
        .to_string(),
        required_keys: vec!["article_url".into(), "language".into()],
        max_size_bytes: 4096,
    }
}

fn output_schema_strict() -> OutputSchema {
    OutputSchema {
        schema_json: r#"{
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "summary": {"type": "string", "maxLength": 1000},
                "key_points": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 1,
                    "maxItems": 10
                }
            },
            "required": ["summary", "key_points"],
            "additionalProperties": false
        }"#
        .to_string(),
        required_keys: vec!["summary".into(), "key_points".into()],
        max_size_bytes: 16384,
    }
}

fn sample_metadata() -> AgentMetadata {
    AgentMetadata {
        name: "summarize_persian_news".into(),
        description: "Summarises a Persian-language news article into a short \
                      paragraph plus key points. Honours the a national filter / sanctions \
                      security model — knowledge refs are content-hashed so a \
                      Persian-friendly mirror can host the corpus without \
                      changing the agent id."
            .into(),
        languages: vec!["fa".into(), "fa-IR".into()],
        tags: vec!["news".into(), "summarisation".into(), "persian".into()],
        license: "Apache-2.0".into(),
    }
}

fn sample_requirements() -> ModelRequirements {
    ModelRequirements {
        min_parameters: Some(7_000_000_000),
        preferred_model_names: vec!["qwen2.5:7b".into(), "llama3.1:8b".into()],
        context_window_tokens: 8192,
        deterministic_mode_required: true,
    }
}

fn sample_agent() -> (AgentDefinition, AgentId, SigningKey) {
    let (sk, peer) = fresh_actor();
    let (def, id) = AgentDefinition::new_signed_at(
        AgentVersion::new(0, 1, 0),
        peer,
        sample_metadata(),
        input_schema_strict(),
        output_schema_strict(),
        "You are a Persian news summariser. Article: {{article_url}}".into(),
        vec![KnowledgeRef::from_text_bytes(
            "Persian news style guide v1\n...".as_bytes(),
            "utf-8",
        )],
        sample_requirements(),
        vec![], // original work, no parents
        1_700_000_000,
        &sk,
    )
    .expect("build");
    (def, id, sk)
}

// ── 1 ────────────────────────────────────────────────────────────────

#[test]
fn build_sign_verify_roundtrip() {
    let (def, _id, sk) = sample_agent();
    def.verify_signature(&sk.verifying_key())
        .expect("self-verify");
    assert_eq!(def.spec_version, SPEC_VERSION);
}

// ── 2 ────────────────────────────────────────────────────────────────

#[test]
fn identical_fields_produce_identical_agent_id() {
    // Same signing key + same inputs + same timestamp ⇒ same hash.
    let (sk, peer) = fresh_actor();
    let common = |sk: &SigningKey, peer: PeerId| {
        AgentDefinition::new_signed_at(
            AgentVersion::new(0, 1, 0),
            peer,
            sample_metadata(),
            input_schema_strict(),
            output_schema_strict(),
            "Hello world.".into(),
            vec![],
            sample_requirements(),
            vec![],
            1_700_000_000,
            sk,
        )
        .unwrap()
    };
    let (a, ida) = common(&sk, peer);
    let (b, idb) = common(&sk, peer);
    assert_eq!(ida, idb);
    assert_eq!(a.content_hash(), b.content_hash());
}

// ── 3 ────────────────────────────────────────────────────────────────

#[test]
fn one_bit_change_produces_different_agent_id() {
    let (def_a, id_a, _) = sample_agent();
    let mut def_b = def_a.clone();
    def_b.prompt_template.push('.');
    // Recompute the id from the mutated form.
    let id_b = def_b.recompute_id();
    assert_ne!(id_a, id_b);
}

// ── 4 ────────────────────────────────────────────────────────────────

#[test]
fn signature_fails_when_tampered() {
    let (mut def, _id, sk) = sample_agent();
    def.prompt_template = "evil prompt".into();
    let err = def.verify_signature(&sk.verifying_key()).unwrap_err();
    assert!(matches!(err, SignError::Verify(_)));
}

// ── 5 ────────────────────────────────────────────────────────────────

#[test]
fn signature_fails_with_wrong_pubkey() {
    let (def, _id, _sk) = sample_agent();
    let stranger = SigningKey::generate(&mut OsRng);
    def.verify_signature(&stranger.verifying_key())
        .expect_err("stranger must not verify");
}

// ── 6 ────────────────────────────────────────────────────────────────

#[test]
fn input_schema_accepts_valid_input() {
    let s = input_schema_strict();
    let v = serde_json::json!({
        "article_url": "https://example.com/article",
        "language": "fa"
    });
    s.validate(&v).expect("valid");
}

// ── 7 ────────────────────────────────────────────────────────────────

#[test]
fn input_schema_rejects_missing_required_key() {
    let s = input_schema_strict();
    // Missing `language`.
    let v = serde_json::json!({"article_url": "https://example.com/x"});
    let err = s.validate(&v).unwrap_err();
    assert!(matches!(err, ValidationError::InvalidValue(_)));
}

// ── 8 ────────────────────────────────────────────────────────────────

#[test]
fn output_schema_enforces_strict_shape() {
    let s = output_schema_strict();
    // additionalProperties: false ⇒ unknown keys rejected.
    let v = serde_json::json!({
        "summary": "x",
        "key_points": ["a"],
        "spurious": "should reject"
    });
    s.validate(&v).expect_err("strict schema must reject extras");
}

// ── 9 ────────────────────────────────────────────────────────────────

#[test]
fn knowledge_ref_cbor_roundtrip_preserves_all_variants() {
    let refs = vec![
        KnowledgeRef::from_text_bytes(b"fake corpus", "utf-8"),
        KnowledgeRef {
            kind: KnowledgeKind::EmbeddingIndex {
                content_hash: parseh_task::content_hash(b"fake index"),
                model_name: "bge-small-en-v1.5".into(),
                dimension: 384,
            },
            fetch_hint: Some("ipfs://bafy...example".into()),
            size_bytes: 1024,
        },
        KnowledgeRef {
            kind: KnowledgeKind::StructuredDataset {
                content_hash: parseh_task::content_hash(b"dataset"),
                schema_hash: parseh_task::content_hash(b"schema"),
            },
            fetch_hint: None,
            size_bytes: 2048,
        },
        KnowledgeRef {
            kind: KnowledgeKind::UpstreamAgentOutput {
                agent_id: AgentId(ContentHash::zero()),
                version: AgentVersion::new(1, 0, 0),
            },
            fetch_hint: None,
            size_bytes: 0,
        },
    ];
    for r in refs {
        let bytes = to_cbor_bytes(&r).expect("encode");
        let back: KnowledgeRef = from_cbor_bytes(&bytes).expect("decode");
        assert_eq!(r, back);
    }
}

// ── 10 ───────────────────────────────────────────────────────────────

#[test]
fn parent_lineage_supports_multiple_parents() {
    // A "merge" fork: two parents, both pinned.
    let (parent_a, id_a, _) = sample_agent();
    let (parent_b, id_b, _) = sample_agent();
    assert_ne!(id_a, id_b, "different signers ⇒ different ids");

    let (sk, peer) = fresh_actor();
    let parents = vec![
        ParentRef {
            agent_id: id_a,
            version: parent_a.version,
            fork_reason: ForkReason::Improvement,
        },
        ParentRef {
            agent_id: id_b,
            version: parent_b.version,
            fork_reason: ForkReason::Specialisation,
        },
    ];
    let (child, _id) = AgentDefinition::new_signed_at(
        AgentVersion::new(0, 2, 0),
        peer,
        sample_metadata(),
        input_schema_strict(),
        output_schema_strict(),
        "Merged child prompt.".into(),
        vec![],
        sample_requirements(),
        parents.clone(),
        1_700_000_100,
        &sk,
    )
    .expect("build child");
    assert_eq!(child.parents.len(), 2);
    assert_eq!(child.parents, parents);

    // CBOR roundtrip preserves the lineage edges.
    let bytes = to_cbor_bytes(&child).unwrap();
    let back: AgentDefinition = from_cbor_bytes(&bytes).unwrap();
    assert_eq!(back.parents, parents);
}

// ── 11 ───────────────────────────────────────────────────────────────

#[test]
fn agent_version_lexicographic_ordering() {
    assert!(AgentVersion::new(0, 1, 0) < AgentVersion::new(0, 2, 0));
    assert!(AgentVersion::new(1, 0, 0) > AgentVersion::new(0, 99, 99));
    assert_eq!(AgentVersion::new(1, 2, 3), AgentVersion::new(1, 2, 3));
}

// ── 12 ───────────────────────────────────────────────────────────────

#[test]
fn deterministic_mode_flag_survives_cbor() {
    let (def, _id, _sk) = sample_agent();
    assert!(def.model_requirements.deterministic_mode_required);
    let bytes = to_cbor_bytes(&def).unwrap();
    let back: AgentDefinition = from_cbor_bytes(&bytes).unwrap();
    assert!(back.model_requirements.deterministic_mode_required);
}

// ── 13 ───────────────────────────────────────────────────────────────

#[test]
fn empty_knowledge_refs_are_allowed() {
    let (sk, peer) = fresh_actor();
    let (def, _id) = AgentDefinition::new_signed_at(
        AgentVersion::new(0, 1, 0),
        peer,
        sample_metadata(),
        input_schema_strict(),
        output_schema_strict(),
        "Prompt only — no RAG.".into(),
        vec![], // empty
        sample_requirements(),
        vec![],
        1_700_000_000,
        &sk,
    )
    .expect("build");
    assert!(def.knowledge_refs.is_empty());
}

// ── 14 ───────────────────────────────────────────────────────────────

#[test]
fn empty_prompt_template_is_rejected() {
    let (sk, peer) = fresh_actor();
    let err = AgentDefinition::new_signed_at(
        AgentVersion::new(0, 1, 0),
        peer,
        sample_metadata(),
        input_schema_strict(),
        output_schema_strict(),
        "   ".into(), // whitespace-only, still empty
        vec![],
        sample_requirements(),
        vec![],
        1_700_000_000,
        &sk,
    )
    .unwrap_err();
    assert!(matches!(err, DefinitionError::EmptyPromptTemplate));
}

// ── 15 ───────────────────────────────────────────────────────────────

#[test]
fn empty_metadata_name_is_rejected() {
    let (sk, peer) = fresh_actor();
    let mut md = sample_metadata();
    md.name = "  ".into();
    let err = AgentDefinition::new_signed_at(
        AgentVersion::new(0, 1, 0),
        peer,
        md,
        input_schema_strict(),
        output_schema_strict(),
        "Prompt.".into(),
        vec![],
        sample_requirements(),
        vec![],
        1_700_000_000,
        &sk,
    )
    .unwrap_err();
    assert!(matches!(err, DefinitionError::EmptyName));
}

// ── 16 ───────────────────────────────────────────────────────────────

#[test]
fn invalid_license_is_rejected() {
    let (sk, peer) = fresh_actor();
    let mut md = sample_metadata();
    md.license = "not a license!".into();
    let err = AgentDefinition::new_signed_at(
        AgentVersion::new(0, 1, 0),
        peer,
        md,
        input_schema_strict(),
        output_schema_strict(),
        "Prompt.".into(),
        vec![],
        sample_requirements(),
        vec![],
        1_700_000_000,
        &sk,
    )
    .unwrap_err();
    assert!(matches!(err, DefinitionError::InvalidLicense(_)));
}

// ── 17 ───────────────────────────────────────────────────────────────

#[test]
fn max_size_bytes_enforced_on_input_validation() {
    let s = InputSchema {
        schema_json: r#"{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}"#
            .to_string(),
        required_keys: vec!["q".into()],
        max_size_bytes: 8,
    };
    let v = serde_json::json!({"q": "way too long, certainly more than eight bytes"});
    let err = s.validate(&v).unwrap_err();
    assert!(matches!(err, ValidationError::OversizeValue { .. }));
}

// ── 18 ───────────────────────────────────────────────────────────────

#[test]
fn max_size_bytes_enforced_on_output_validation() {
    let s = OutputSchema {
        schema_json: r#"{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}"#
            .to_string(),
        required_keys: vec!["summary".into()],
        max_size_bytes: 16,
    };
    let big = "x".repeat(1024);
    let v = serde_json::json!({"summary": big});
    let err = s.validate(&v).unwrap_err();
    assert!(matches!(err, ValidationError::OversizeValue { .. }));
}

// ── 19 ───────────────────────────────────────────────────────────────

#[test]
fn license_field_accepts_spdx_identifiers() {
    for license in [
        "Apache-2.0",
        "MIT",
        "GPL-3.0-only",
        "BSD-3-Clause",
        "CC0-1.0",
        "Apache-2.0 WITH LLVM-exception",
    ] {
        let (sk, peer) = fresh_actor();
        let mut md = sample_metadata();
        md.license = license.into();
        AgentDefinition::new_signed_at(
            AgentVersion::new(0, 1, 0),
            peer,
            md,
            input_schema_strict(),
            output_schema_strict(),
            "Prompt.".into(),
            vec![],
            sample_requirements(),
            vec![],
            1_700_000_000,
            &sk,
        )
        .unwrap_or_else(|e| panic!("license `{license}` should be accepted but errored: {e}"));
    }
}

// ── 20 ───────────────────────────────────────────────────────────────

#[test]
fn agent_definition_cbor_roundtrip_byte_identical() {
    let (def, _id, _sk) = sample_agent();
    let bytes_a = to_cbor_bytes(&def).expect("encode");
    let back: AgentDefinition = from_cbor_bytes(&bytes_a).expect("decode");
    let bytes_b = to_cbor_bytes(&back).expect("re-encode");
    assert_eq!(bytes_a, bytes_b);
    assert_eq!(def, back);
}

// ── 21 ───────────────────────────────────────────────────────────────

#[test]
fn small_definition_well_under_size_cap() {
    let (def, _, _) = sample_agent();
    let bytes = to_cbor_bytes(&def).unwrap();
    assert!(
        bytes.len() < MAX_DEFINITION_SIZE_BYTES,
        "a small sample agent must fit under the 1 MiB cap; got {}",
        bytes.len()
    );
}

// ── 22 ───────────────────────────────────────────────────────────────

#[test]
fn recompute_id_matches_stored_id_for_freshly_signed_definition() {
    let (def, id, _) = sample_agent();
    assert_eq!(def.id, id);
    assert_eq!(def.recompute_id(), id);
}

// ── 23 ───────────────────────────────────────────────────────────────

#[test]
fn module_level_validate_against_schema_helper_works() {
    let schema = r#"{"type":"integer","minimum":0}"#;
    validate_against_schema(&serde_json::json!(42), schema).expect("ok");
    validate_against_schema(&serde_json::json!(-1), schema).expect_err("negative");
}

// ── 24 ───────────────────────────────────────────────────────────────

#[test]
fn malformed_schema_at_construction_time_is_rejected() {
    let bad = InputSchema {
        // Missing closing brace — not valid JSON.
        schema_json: r#"{"type":"object""#.to_string(),
        required_keys: vec![],
        max_size_bytes: 0,
    };
    let (sk, peer) = fresh_actor();
    let err = AgentDefinition::new_signed_at(
        AgentVersion::new(0, 1, 0),
        peer,
        sample_metadata(),
        bad,
        output_schema_strict(),
        "Prompt.".into(),
        vec![],
        sample_requirements(),
        vec![],
        1_700_000_000,
        &sk,
    )
    .unwrap_err();
    assert!(matches!(err, DefinitionError::Schema(_)));
}
