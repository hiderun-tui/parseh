//! `parseh-agent-runtime` — execute signed agent + workflow definitions.
//!
//! Pillar-1 runtime (see [the project notes](the project notes)):
//! turns an [`parseh_agent_spec::AgentDefinition`] into LLM-backed work
//! with schema-validated input + output and a signed
//! [`parseh_task::JobResult`]. Plus a minimal workflow chainer — the
//! n8n-analogue: a DAG of agent steps with output→input wiring.
//!
//! ## Honest scope (binding, per
//! [the project notes](the project notes))
//!
//! - LOCAL execution only in this crate — against a local Ollama
//!   ([`OllamaBackend`], endpoint discovered via `parseh-llm-detect`)
//!   or the deterministic [`MockBackend`]. The FREE DISTRIBUTED
//!   execution that pillar 1 promises (a builder priced out of
//!   commercial APIs submits a workflow and the volunteer network runs
//!   it for free) requires the network to exist: bootstrap servers,
//!   which are **not provisioned**. This crate is the executable
//!   primitive for pillar 1, *not* the operational free-AI service.
//! - Deterministic-mode only (seed-pinned, temperature 0) so
//!   `parseh-verify` can re-execute and get byte-identical output.
//!   Non-deterministic verification is V0.3+.
//! - Zero economic / payment / reward logic. Zero marketplace / agent
//!   discovery. Those are deferred per the maintainer note and the
//!   audits §5 refusal of "tradeable instructions".
//!
//! ## Pipeline
//!
//! ```text
//!   AgentDefinition + JobSpec + input JSON
//!     ├── validate input JSON against AgentDefinition.input_schema
//!     ├── resolve knowledge refs (local content-addressed cache)
//!     ├── render prompt_template with input + resolved knowledge
//!     ├── backend.complete(prompt, seed, max_tokens)   [seed from JobSpec]
//!     ├── parse + validate output against AgentDefinition.output_schema
//!     └── build + sign a JobResult (parseh-task)
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod backend;
mod executor;
mod knowledge;
mod render;
mod workflow;

pub use backend::{BackendError, LlmBackend, MockBackend, OllamaBackend};
pub use executor::{AgentExecutor, ExecError, ExecOutcome};
pub use knowledge::{resolve_knowledge, KnowledgeError, ResolvedKnowledge};
pub use render::{render_prompt, RenderError};
pub use workflow::{Workflow, WorkflowError, WorkflowResult, WorkflowStep};

/// Crate version surfaced via `parseh_agent_runtime::VERSION`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
