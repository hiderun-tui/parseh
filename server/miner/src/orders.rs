//! Wire types for the `/parseh/job/1.0.0` request-response protocol.
//!
//! These travel over libp2p, which runs Noise encryption + Yamux
//! multiplexing under the hood. Every byte on the wire is confidential
//! and authenticated end-to-end between the two libp2p peers.
//!
//! The payload format is CBOR (via the `request_response::cbor::Behaviour`
//! upstream), which is compact and self-describing without requiring
//! .proto codegen.

use serde::{Deserialize, Serialize};

/// A unit of work a peer asks this miner to execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOrder {
    /// Stable identifier so result attestations can reference this job.
    pub job_id: [u8; 32],

    /// Model tag, e.g. "qwen2.5:7b" or "relay". A miner that does not
    /// advertise this tag should respond with `JobResult::declined(...)`.
    pub model: String,

    /// SHA-256 of the user's prompt. The plaintext prompt also travels
    /// inside this enum for now — V0.1 splits prompts out so they can be
    /// streamed and the on-chain attestation only references the hash.
    pub prompt_hash: [u8; 32],

    /// Plaintext prompt body. V0.1 may move this to a streaming side-channel.
    pub prompt: String,

    /// Maximum tokens the requester is willing to pay for.
    pub max_tokens: u32,

    /// Bounty offered in micro-PARSEH (1 PARSEH = 1_000_000 micro-PARSEH).
    pub bounty_upar: u64,
}

/// Reply produced by the executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    /// Mirror of the request id so the requester can correlate.
    pub job_id: [u8; 32],

    /// Outcome — successful completion, declined, or error.
    pub outcome: JobOutcome,

    /// Token-or-byte-count actually used (depends on service).
    pub tokens_used: u32,

    /// Wall-clock execution time in milliseconds.
    pub wall_ms: u64,

    /// Plaintext completion (V0.1) or relay byte count.
    pub completion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobOutcome {
    Ok,
    Declined { reason: String },
    Error    { reason: String },
}

impl JobResult {
    pub fn declined(job_id: [u8; 32], reason: impl Into<String>) -> Self {
        Self {
            job_id,
            outcome: JobOutcome::Declined { reason: reason.into() },
            tokens_used: 0,
            wall_ms: 0,
            completion: None,
        }
    }

    pub fn ok(job_id: [u8; 32], completion: String, tokens_used: u32, wall_ms: u64) -> Self {
        Self {
            job_id,
            outcome: JobOutcome::Ok,
            tokens_used,
            wall_ms,
            completion: Some(completion),
        }
    }
}
