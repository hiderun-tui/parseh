//! The core single-agent executor.
//!
//! Turns a signed [`AgentDefinition`] + a [`JobSpec`] + an input JSON
//! value into a signed [`JobResult`]. Every failure mode is a typed
//! [`ExecError`] variant — no `unwrap()`, no `panic!()` on the
//! execution path.
//!
//! ## Determinism contract
//!
//! Given the *same agent*, the *same input*, the *same seed* (carried
//! by the `JobSpec`), and the *same model*, [`AgentExecutor::execute`]
//! produces a **byte-identical** `JobResult` (modulo the executor's
//! signature, which is deterministic for a fixed key + message under
//! ed25519). This is the property `parseh-verify` relies on to
//! re-execute and counter-sign. The `created_at`/`executed_at`
//! timestamp is taken from the `JobSpec.submitted_at` field (NOT
//! wall-clock) precisely so the result bytes do not drift between the
//! original execution and a verifier's re-execution.

use crate::backend::{BackendError, LlmBackend};
use crate::knowledge::{resolve_knowledge, KnowledgeError};
use crate::render::{render_prompt, RenderError};
use libp2p::PeerId;
use parseh_agent_spec::AgentDefinition;
use parseh_task::{
    JobResult, JobSpec, ResultMeta, VerifierMethod,
};
use serde_json::Value;
use std::path::PathBuf;
use thiserror::Error;

/// Every way executing an agent can fail. None of these panic.
#[derive(Error, Debug)]
pub enum ExecError {
    /// The input JSON did not validate against the agent's
    /// `input_schema`.
    #[error("input failed agent input_schema: {0}")]
    InputSchemaViolation(String),
    /// A knowledge ref could not be resolved.
    #[error("knowledge resolution failed: {0}")]
    KnowledgeUnavailable(#[from] KnowledgeError),
    /// The prompt template could not be rendered.
    #[error("prompt template render failed: {0}")]
    TemplateError(#[from] RenderError),
    /// The LLM backend failed.
    #[error("backend failed: {0}")]
    Backend(#[from] BackendError),
    /// The backend's completion was not parseable JSON. (Agents in
    /// V0.2.5 must produce JSON; the output_schema is a JSON Schema.)
    #[error("model output is not valid JSON: {0}")]
    OutputNotJson(String),
    /// The model output parsed as JSON but violated the agent's
    /// `output_schema`.
    #[error("model output failed agent output_schema: {0}")]
    OutputSchemaViolation(String),
    /// The `JobSpec` did not carry the seed required for
    /// deterministic-mode execution.
    #[error("JobSpec.inputs.seed is required for deterministic-mode execution but was absent")]
    MissingSeed,
    /// CBOR encoding the output payload failed (should be unreachable
    /// for owned `serde_json::Value`, surfaced rather than panicked).
    #[error("result payload CBOR encode failed: {0}")]
    PayloadEncode(String),
}

/// The successful outcome of executing an agent.
#[derive(Clone, Debug)]
pub struct ExecOutcome {
    /// The validated model output, parsed as JSON.
    pub output_json: Value,
    /// The signed [`JobResult`] — its payload is the CBOR of
    /// `output_json`, its `executor` is the runtime's `PeerId`.
    pub job_result: JobResult,
    /// Model identifier reported by the backend.
    pub model_id: String,
    /// Wall-clock execution time, milliseconds. NOTE: this lives in
    /// `ResultMeta` and is the one field that legitimately differs
    /// between runs — verifiers compare `result_payload`, not timing.
    pub execution_time_ms: u64,
}

/// Executes signed agent definitions against a pluggable backend.
///
/// Generic over the [`LlmBackend`] so tests use [`crate::MockBackend`]
/// and production uses [`crate::OllamaBackend`].
pub struct AgentExecutor<B: LlmBackend> {
    backend: B,
    signing_key: ed25519_dalek::SigningKey,
    local_peer_id: PeerId,
    knowledge_cache_root: Option<PathBuf>,
}

impl<B: LlmBackend> AgentExecutor<B> {
    /// Construct an executor.
    ///
    /// `signing_key` signs the produced [`JobResult`]; `local_peer_id`
    /// is recorded as the result's `executor`. The two are logically
    /// the same identity — the caller is responsible for passing a
    /// `PeerId` derived from the same ed25519 key (the wider codebase
    /// uses `libp2p::identity::Keypair` for the PeerId and a parallel
    /// `ed25519_dalek::SigningKey` for protocol signatures; see
    /// `parseh-task` tests for the established pattern).
    pub fn new(
        backend: B,
        signing_key: ed25519_dalek::SigningKey,
        local_peer_id: PeerId,
    ) -> Self {
        Self {
            backend,
            signing_key,
            local_peer_id,
            knowledge_cache_root: None,
        }
    }

    /// Override the knowledge cache root (default `~/.parseh/knowledge/`).
    /// Primarily for tests.
    pub fn with_knowledge_cache_root(mut self, root: PathBuf) -> Self {
        self.knowledge_cache_root = Some(root);
        self
    }

    /// The ed25519 verifying key results are signed under. Callers
    /// (and `parseh-verify`) check `JobResult` signatures against this.
    pub fn verifying_key(&self) -> ed25519_dalek::VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// Run the full pipeline. See module docs for the determinism
    /// contract.
    ///
    /// # Errors
    ///
    /// Returns the matching [`ExecError`] variant for every failure;
    /// never panics.
    pub async fn execute(
        &self,
        agent: &AgentDefinition,
        spec: &JobSpec,
        input: Value,
    ) -> Result<ExecOutcome, ExecError> {
        // 1. Validate input against the agent's input_schema.
        agent
            .input_schema
            .validate(&input)
            .map_err(|e| ExecError::InputSchemaViolation(e.to_string()))?;

        // 2. Resolve knowledge refs from the local cache.
        let resolved = resolve_knowledge(
            &agent.knowledge_refs,
            self.knowledge_cache_root.clone(),
        )?;

        // 3. Build the rendering context: input at top level, resolved
        //    knowledge under the reserved `knowledge` key (array of
        //    {hash, encoding, text}), then render the prompt template.
        let mut context = input.clone();
        if let Value::Object(ref mut map) = context {
            let knowledge: Vec<Value> = resolved
                .iter()
                .map(|k| {
                    serde_json::json!({
                        "content_hash": k.content_hash_hex,
                        "encoding": k.encoding,
                        "text": k.text,
                    })
                })
                .collect();
            map.insert("knowledge".to_string(), Value::Array(knowledge));
        }
        let prompt = render_prompt(&agent.prompt_template, &context)?;

        // 4. Deterministic completion. Seed comes from the JobSpec;
        //    deterministic-mode is mandatory in V0.2.5.
        let seed = spec
            .inputs
            .seed
            .ok_or(ExecError::MissingSeed)?;
        let max_tokens = spec.inputs.max_tokens.unwrap_or(2048);
        let start = std::time::Instant::now();
        let raw = self
            .backend
            .complete(&prompt, seed, max_tokens)
            .await?;
        let execution_time_ms = start.elapsed().as_millis() as u64;

        // 5. Parse + validate output against output_schema.
        let output_json: Value = serde_json::from_str(raw.trim())
            .map_err(|e| ExecError::OutputNotJson(e.to_string()))?;
        agent
            .output_schema
            .validate(&output_json)
            .map_err(|e| ExecError::OutputSchemaViolation(e.to_string()))?;

        // 6. Build + sign a JobResult. The payload is the CBOR of the
        //    validated output JSON. We anchor `executed_at` to the
        //    spec's `submitted_at` (NOT wall-clock) so the signed
        //    bytes are reproducible by a verifier — the determinism
        //    contract. Token count is not exposed by all backends, so
        //    it is `None` here (V0.3+ may surface it for billing-free
        //    telemetry).
        let model_id = self.backend.model_id();
        let payload = to_cbor(&output_json)
            .map_err(|e| ExecError::PayloadEncode(e.to_string()))?;
        let meta = ResultMeta {
            verifier_method: VerifierMethod::Deterministic,
            execution_time_ms,
            model_used: Some(model_id.clone()),
            inference_token_count: None,
        };
        let (job_result, _hash) = JobResult::new_signed_at(
            spec.content_hash(),
            self.local_peer_id,
            spec.submitted_at,
            meta,
            payload,
            &self.signing_key,
        );

        Ok(ExecOutcome {
            output_json,
            job_result,
            model_id,
            execution_time_ms,
        })
    }
}

/// CBOR-encode a value (local helper; mirrors `parseh-task`'s pattern).
fn to_cbor<T: serde::Serialize>(
    v: &T,
) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
    let mut buf = Vec::new();
    ciborium::into_writer(v, &mut buf)?;
    Ok(buf)
}
