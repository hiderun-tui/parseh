//! Integration tests for `parseh-agent-runtime`.
//!
//! Covers the full single-agent pipeline, the workflow chainer, the
//! deterministic mock backend, the wiremock-backed Ollama backend
//! (asserts seed + temperature=0 are sent), and — the load-bearing
//! property for `parseh-verify` — byte-identical `JobResult` across
//! two runs with the same agent + input + seed.

use parseh_agent_runtime::{
    AgentExecutor, ExecError, LlmBackend, MockBackend, OllamaBackend, Workflow, WorkflowError,
    WorkflowStep,
};
use parseh_agent_spec::{
    AgentDefinition, AgentId, AgentMetadata, AgentVersion, InputSchema, KnowledgeRef,
    ModelRequirements, OutputSchema,
};
use parseh_task::{to_cbor_bytes, JobInputs, JobKind, JobSpec};
use parseh_core::ServiceKind;
use ed25519_dalek::SigningKey;
use libp2p::PeerId;
use serde_json::json;
use std::collections::HashMap;

// ---- helpers ---------------------------------------------------------

fn keypair() -> (SigningKey, PeerId) {
    // Deterministic key so signatures are reproducible across runs.
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let id_kp = libp2p::identity::Keypair::generate_ed25519();
    (sk, PeerId::from(id_kp.public()))
}

fn obj_schema(props: &str, required: &str) -> String {
    format!(
        r#"{{"$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object","properties":{{{props}}},"required":[{required}]}}"#
    )
}

/// Build a signed agent whose output_schema requires `answer` (string)
/// and `seed` (integer) — exactly what `MockBackend` emits.
fn mock_agent(prompt_template: &str, knowledge: Vec<KnowledgeRef>) -> AgentDefinition {
    let sk = SigningKey::from_bytes(&[9u8; 32]);
    let author = PeerId::from(libp2p::identity::Keypair::generate_ed25519().public());
    let input_schema = InputSchema {
        schema_json: obj_schema(r#""topic":{"type":"string"}"#, r#""topic""#),
        required_keys: vec!["topic".into()],
        max_size_bytes: 0,
    };
    let output_schema = OutputSchema {
        schema_json: obj_schema(
            r#""answer":{"type":"string"},"seed":{"type":"integer"}"#,
            r#""answer","seed""#,
        ),
        required_keys: vec!["answer".into(), "seed".into()],
        max_size_bytes: 0,
    };
    let (def, _id) = AgentDefinition::new_signed_at(
        AgentVersion::new(0, 1, 0),
        author,
        AgentMetadata {
            name: "mock-agent".into(),
            description: "test".into(),
            languages: vec!["en".into()],
            tags: vec![],
            license: "Apache-2.0".into(),
        },
        input_schema,
        output_schema,
        prompt_template.to_string(),
        knowledge,
        ModelRequirements {
            min_parameters: None,
            preferred_model_names: vec![],
            context_window_tokens: 4096,
            deterministic_mode_required: true,
        },
        vec![],
        1_700_000_000,
        &sk,
    )
    .expect("agent builds");
    def
}

fn spec_with_seed(seed: u64) -> JobSpec {
    let sk = SigningKey::from_bytes(&[3u8; 32]);
    let submitter = PeerId::from(libp2p::identity::Keypair::generate_ed25519().public());
    let (spec, _h) = JobSpec::new_signed_at(
        JobKind::Inference,
        JobInputs::inference_prompt("ignored — agent supplies its own prompt", seed),
        ServiceKind::Inference,
        false,
        1_700_000_000,
        submitter,
        &sk,
    );
    spec
}

// ---- 1. input schema validation -------------------------------------

#[tokio::test]
async fn input_schema_accepts_valid_input() {
    let (sk, peer) = keypair();
    let agent = mock_agent("Topic: {{topic}}", vec![]);
    let exec = AgentExecutor::new(MockBackend::default(), sk, peer);
    let out = exec
        .execute(&agent, &spec_with_seed(1), json!({"topic": "rust"}))
        .await
        .expect("valid input executes");
    assert!(out.output_json.get("answer").is_some());
}

#[tokio::test]
async fn input_schema_rejects_invalid_input() {
    let (sk, peer) = keypair();
    let agent = mock_agent("Topic: {{topic}}", vec![]);
    let exec = AgentExecutor::new(MockBackend::default(), sk, peer);
    // `topic` missing → InputSchemaViolation.
    let err = exec
        .execute(&agent, &spec_with_seed(1), json!({"wrong": "field"}))
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::InputSchemaViolation(_)));
}

// ---- 2. prompt rendering --------------------------------------------

#[tokio::test]
async fn template_undefined_key_is_typed_error() {
    let (sk, peer) = keypair();
    let agent = mock_agent("Topic: {{does_not_exist}}", vec![]);
    let exec = AgentExecutor::new(MockBackend::default(), sk, peer);
    let err = exec
        .execute(&agent, &spec_with_seed(1), json!({"topic": "x"}))
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::TemplateError(_)));
}

#[tokio::test]
async fn template_substitutes_input_key() {
    use parseh_agent_runtime::render_prompt;
    let r = render_prompt("Q: {{topic}}", &json!({"topic": "free AI"})).unwrap();
    assert_eq!(r, "Q: free AI");
}

// ---- 3. MockBackend determinism -------------------------------------

#[tokio::test]
async fn mock_backend_same_seed_same_output() {
    let b = MockBackend::default();
    let a = b.complete("prompt", 42, 256).await.unwrap();
    let c = b.complete("prompt", 42, 256).await.unwrap();
    assert_eq!(a, c);
    let d = b.complete("prompt", 43, 256).await.unwrap();
    assert_ne!(a, d, "different seed must change output");
}

// ---- 4. full pipeline → signed JobResult ----------------------------

#[tokio::test]
async fn full_pipeline_produces_signed_job_result() {
    let (sk, peer) = keypair();
    let agent = mock_agent("Topic: {{topic}}", vec![]);
    let exec = AgentExecutor::new(MockBackend::default(), sk, peer);
    let out = exec
        .execute(&agent, &spec_with_seed(99), json!({"topic": "p2p"}))
        .await
        .expect("executes");
    // JobResult signature verifies against the executor pubkey.
    out.job_result
        .verify_signature(&exec.verifying_key())
        .expect("result signature verifies");
    assert_eq!(out.model_id, "mock-deterministic-v1");
}

#[tokio::test]
async fn job_result_signature_fails_under_wrong_key() {
    let (sk, peer) = keypair();
    let agent = mock_agent("Topic: {{topic}}", vec![]);
    let exec = AgentExecutor::new(MockBackend::default(), sk, peer);
    let out = exec
        .execute(&agent, &spec_with_seed(1), json!({"topic": "x"}))
        .await
        .unwrap();
    let imposter = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
    assert!(out.job_result.verify_signature(&imposter).is_err());
}

// ---- 5. output schema violation is typed, not a panic ---------------

#[tokio::test]
async fn output_schema_violation_is_typed_error() {
    let (sk, peer) = keypair();
    // Output schema demands a `must_have` key the MockBackend never
    // produces → OutputSchemaViolation (NOT a panic).
    let sk_author = SigningKey::from_bytes(&[9u8; 32]);
    let author = PeerId::from(libp2p::identity::Keypair::generate_ed25519().public());
    let (agent, _id) = AgentDefinition::new_signed_at(
        AgentVersion::new(0, 1, 0),
        author,
        AgentMetadata {
            name: "strict".into(),
            description: "t".into(),
            languages: vec![],
            tags: vec![],
            license: "Apache-2.0".into(),
        },
        InputSchema {
            schema_json: obj_schema(r#""topic":{"type":"string"}"#, r#""topic""#),
            required_keys: vec!["topic".into()],
            max_size_bytes: 0,
        },
        OutputSchema {
            schema_json: obj_schema(
                r#""must_have":{"type":"string"}"#,
                r#""must_have""#,
            ),
            required_keys: vec!["must_have".into()],
            max_size_bytes: 0,
        },
        "T: {{topic}}".into(),
        vec![],
        ModelRequirements {
            min_parameters: None,
            preferred_model_names: vec![],
            context_window_tokens: 4096,
            deterministic_mode_required: true,
        },
        vec![],
        1_700_000_000,
        &sk_author,
    )
    .unwrap();
    let exec = AgentExecutor::new(MockBackend::default(), sk, peer);
    let err = exec
        .execute(&agent, &spec_with_seed(1), json!({"topic": "x"}))
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::OutputSchemaViolation(_)));
}

#[tokio::test]
async fn missing_seed_is_typed_error() {
    let (sk, peer) = keypair();
    let agent = mock_agent("T: {{topic}}", vec![]);
    let exec = AgentExecutor::new(MockBackend::default(), sk, peer);
    // Build a spec with seed = None.
    let sk_sub = SigningKey::from_bytes(&[3u8; 32]);
    let submitter = PeerId::from(libp2p::identity::Keypair::generate_ed25519().public());
    let (spec, _h) = JobSpec::new_signed_at(
        JobKind::Inference,
        JobInputs {
            prompt_text: None,
            seed: None,
            max_tokens: None,
            content_refs: vec![],
        },
        ServiceKind::Inference,
        false,
        1_700_000_000,
        submitter,
        &sk_sub,
    );
    let err = exec
        .execute(&agent, &spec, json!({"topic": "x"}))
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::MissingSeed));
}

// ---- 6. knowledge ref resolution from local cache -------------------

#[tokio::test]
async fn knowledge_resolved_from_cache_and_injected() {
    let (sk, peer) = keypair();
    let dir = tempfile::tempdir().unwrap();
    let body = b"PARSEH gives builders free distributed AI.";
    let kref = KnowledgeRef::from_text_bytes(body, "utf-8");
    let hex = match &kref.kind {
        parseh_agent_spec::KnowledgeKind::TextCorpus { content_hash, .. } => content_hash.as_hex(),
        _ => unreachable!(),
    };
    std::fs::write(dir.path().join(&hex), body).unwrap();

    // Template embeds the resolved corpus text via the reserved key.
    let agent = mock_agent(
        "Context: {{knowledge.0.text}} | Topic: {{topic}}",
        vec![kref],
    );
    let exec = AgentExecutor::new(MockBackend::default(), sk, peer)
        .with_knowledge_cache_root(dir.path().to_path_buf());
    let out = exec
        .execute(&agent, &spec_with_seed(5), json!({"topic": "rag"}))
        .await
        .expect("knowledge resolves and executes");
    assert!(out.output_json.get("answer").is_some());
}

#[tokio::test]
async fn knowledge_missing_is_clear_error() {
    let (sk, peer) = keypair();
    let dir = tempfile::tempdir().unwrap();
    let kref = KnowledgeRef::from_text_bytes(b"absent corpus", "utf-8");
    let agent = mock_agent("{{knowledge.0.text}} {{topic}}", vec![kref]);
    let exec = AgentExecutor::new(MockBackend::default(), sk, peer)
        .with_knowledge_cache_root(dir.path().to_path_buf());
    let err = exec
        .execute(&agent, &spec_with_seed(1), json!({"topic": "x"}))
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::KnowledgeUnavailable(_)));
}

// ---- 7. workflow: linear 3-step DAG ---------------------------------

fn registry(agents: &[&AgentDefinition]) -> HashMap<AgentId, AgentDefinition> {
    agents.iter().map(|a| (a.id, (*a).clone())).collect()
}

#[tokio::test]
async fn workflow_linear_three_steps_wires_outputs() {
    let (sk, peer) = keypair();
    let agent = mock_agent("In: {{topic}}", vec![]);
    let reg = registry(&[&agent]);
    let exec = AgentExecutor::new(MockBackend::default(), sk, peer);

    let wf = Workflow::new(vec![
        WorkflowStep {
            step_id: "s1".into(),
            agent_id: agent.id,
            input_mapping: json!({"topic": "{{initial.topic}}"}),
            depends_on: vec![],
        },
        WorkflowStep {
            step_id: "s2".into(),
            agent_id: agent.id,
            input_mapping: json!({"topic": "{{steps.s1.output.answer}}"}),
            depends_on: vec!["s1".into()],
        },
        WorkflowStep {
            step_id: "s3".into(),
            agent_id: agent.id,
            input_mapping: json!({"topic": "{{steps.s2.output.answer}}"}),
            depends_on: vec!["s2".into()],
        },
    ])
    .unwrap();

    let res = wf
        .run(&exec, &reg, &spec_with_seed(11), json!({"topic": "start"}))
        .await
        .expect("workflow runs");
    assert_eq!(res.final_step_id, "s3");
    assert_eq!(res.step_outputs.len(), 3);
    // s2's input was s1's output; the chained answers differ from s1.
    assert_ne!(res.step_outputs["s1"], res.step_outputs["s2"]);
}

// ---- 8. workflow: diamond DAG ---------------------------------------

#[tokio::test]
async fn workflow_diamond_dag_runs_correctly() {
    let (sk, peer) = keypair();
    // D consumes B and C; B and C both consume A.
    let agent = mock_agent("In: {{topic}}", vec![]);
    let reg = registry(&[&agent]);
    let exec = AgentExecutor::new(MockBackend::default(), sk, peer);

    let wf = Workflow::new(vec![
        WorkflowStep {
            step_id: "A".into(),
            agent_id: agent.id,
            input_mapping: json!({"topic": "{{initial.topic}}"}),
            depends_on: vec![],
        },
        WorkflowStep {
            step_id: "B".into(),
            agent_id: agent.id,
            input_mapping: json!({"topic": "{{steps.A.output.answer}}"}),
            depends_on: vec!["A".into()],
        },
        WorkflowStep {
            step_id: "C".into(),
            agent_id: agent.id,
            input_mapping: json!({"topic": "{{steps.A.output.answer}}"}),
            depends_on: vec!["A".into()],
        },
        WorkflowStep {
            step_id: "D".into(),
            agent_id: agent.id,
            // Merge: take B's answer (a single string the schema accepts).
            input_mapping: json!({"topic": "{{steps.B.output.answer}}"}),
            depends_on: vec!["B".into(), "C".into()],
        },
    ])
    .unwrap();

    let res = wf
        .run(&exec, &reg, &spec_with_seed(7), json!({"topic": "seed"}))
        .await
        .expect("diamond runs");
    assert_eq!(res.final_step_id, "D");
    assert_eq!(res.step_outputs.len(), 4);
    // B and C had identical input (A's output) + same seed → identical.
    assert_eq!(res.step_outputs["B"], res.step_outputs["C"]);
}

// ---- 9. workflow: cycle rejected ------------------------------------

#[tokio::test]
async fn workflow_cycle_is_rejected() {
    let (sk, peer) = keypair();
    let agent = mock_agent("In: {{topic}}", vec![]);
    let reg = registry(&[&agent]);
    let exec = AgentExecutor::new(MockBackend::default(), sk, peer);
    let wf = Workflow::new(vec![
        WorkflowStep {
            step_id: "x".into(),
            agent_id: agent.id,
            input_mapping: json!({"topic": "{{steps.y.output.answer}}"}),
            depends_on: vec!["y".into()],
        },
        WorkflowStep {
            step_id: "y".into(),
            agent_id: agent.id,
            input_mapping: json!({"topic": "{{steps.x.output.answer}}"}),
            depends_on: vec!["x".into()],
        },
    ])
    .unwrap();
    let err = wf
        .run(&exec, &reg, &spec_with_seed(1), json!({"topic": "t"}))
        .await
        .unwrap_err();
    assert!(matches!(err, WorkflowError::CycleDetected(_)));
}

// ---- 10. determinism: byte-identical JobResult ----------------------

#[tokio::test]
async fn same_agent_input_seed_yields_byte_identical_job_result() {
    let agent = mock_agent("Topic: {{topic}}", vec![]);
    let spec = spec_with_seed(424242);
    let input = json!({"topic": "deterministic"});

    let run = |()| {
        let agent = agent.clone();
        let spec = spec.clone();
        let input = input.clone();
        async move {
            // Same signing key both runs (deterministic ed25519).
            let sk = SigningKey::from_bytes(&[7u8; 32]);
            let id_kp = libp2p::identity::Keypair::ed25519_from_bytes([5u8; 32]).unwrap();
            let peer = PeerId::from(id_kp.public());
            let exec = AgentExecutor::new(MockBackend::default(), sk, peer);
            let out = exec.execute(&agent, &spec, input).await.unwrap();
            to_cbor_bytes(&out.job_result).unwrap()
        }
    };

    let a = run(()).await;
    let b = run(()).await;
    assert_eq!(
        a, b,
        "same agent + input + seed + model must yield byte-identical JobResult \
         (this is what lets parseh-verify re-execute)"
    );
}

// ---- 11. wiremock-backed Ollama: seed + temperature=0 sent ----------

#[tokio::test]
async fn ollama_backend_sends_seed_and_temperature_zero() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            // Assert the deterministic-mode options were sent.
            assert_eq!(body["options"]["seed"], json!(777));
            assert_eq!(body["options"]["temperature"], json!(0));
            assert_eq!(body["stream"], json!(false));
            ResponseTemplate::new(200)
                .set_body_json(json!({"response": "{\"ok\":true}"}))
        })
        .mount(&server)
        .await;

    let backend = OllamaBackend::new(server.uri(), "qwen2.5:7b");
    let out = backend.complete("hello", 777, 128).await.unwrap();
    assert_eq!(out, r#"{"ok":true}"#);
    assert_eq!(backend.model_id(), "qwen2.5:7b");
}

#[tokio::test]
async fn ollama_backend_http_500_is_typed_error() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let backend = OllamaBackend::new(server.uri(), "m");
    let err = backend.complete("x", 1, 8).await.unwrap_err();
    assert!(matches!(
        err,
        parseh_agent_runtime::BackendError::HttpStatus { status: 500, .. }
    ));
}

// ---- 12. output-not-JSON is typed -----------------------------------

#[tokio::test]
async fn non_json_model_output_is_typed_error() {
    // A custom backend that returns prose, not JSON.
    struct ProseBackend;
    #[async_trait::async_trait]
    impl LlmBackend for ProseBackend {
        async fn complete(
            &self,
            _p: &str,
            _s: u64,
            _m: u32,
        ) -> Result<String, parseh_agent_runtime::BackendError> {
            Ok("I am not JSON at all.".into())
        }
        fn model_id(&self) -> String {
            "prose".into()
        }
    }
    let (sk, peer) = keypair();
    let agent = mock_agent("T: {{topic}}", vec![]);
    let exec = AgentExecutor::new(ProseBackend, sk, peer);
    let err = exec
        .execute(&agent, &spec_with_seed(1), json!({"topic": "x"}))
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::OutputNotJson(_)));
}
