//! Job executors — the part that actually does the work.
//!
//! Today: an `EchoExecutor` that returns a deterministic completion so
//! the wire end-to-end can be verified without GPU hardware.
//!
//! V0.1 work: a `LlamaExecutor` backed by `llama-cpp-2` or `candle-core`
//! that loads a quantised model from disk and runs inference.
//!
//! A `CanaryExecutor` lives alongside `EchoExecutor` and is selected at
//! compile time via the `canary-executor` cargo feature. It exists for
//! two reasons:
//!   1. Prove the trait-based executor-swap pattern works with two
//!      distinct implementations before the real LLM lands.
//!   2. Give integration tests a deterministic, weight-free executor
//!      whose output (SHA-256 of prompt) changes with input.

use std::time::Instant;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::orders::{JobOrder, JobResult};
use parseh_core::NodeCapabilities;

/// Pluggable executor interface so multiple back-ends can coexist.
#[async_trait]
pub trait Executor: Send + Sync {
    async fn execute(&self, order: &JobOrder, caps: &NodeCapabilities) -> JobResult;
}

/// Default executor wired today. Real GPU-backed executors slot in here.
///
/// Build with `--features canary-executor` to swap in the deterministic
/// `CanaryExecutor` instead of `EchoExecutor`. The selection happens at
/// compile time so we ship a single binary per build profile.
#[cfg(feature = "canary-executor")]
pub fn default_executor() -> Box<dyn Executor> {
    Box::new(CanaryExecutor)
}

#[cfg(not(feature = "canary-executor"))]
pub fn default_executor() -> Box<dyn Executor> {
    Box::new(EchoExecutor)
}

/// Returns a synthetic completion that proves the entire job pipeline
/// works without requiring any model files. The completion text shows
/// the miner's capability advertisement and the original prompt hash —
/// useful for end-to-end testing.
pub struct EchoExecutor;

#[async_trait]
impl Executor for EchoExecutor {
    async fn execute(&self, order: &JobOrder, caps: &NodeCapabilities) -> JobResult {
        let started = Instant::now();

        // Decline jobs we are not configured to serve.
        if order.model == "relay" && !caps.relay {
            return JobResult::declined(order.job_id, "this miner does not advertise relay service");
        }
        if order.model != "relay" && !caps.inference {
            return JobResult::declined(
                order.job_id,
                "this miner does not advertise inference service (set capabilities.inference = true)",
            );
        }
        if order.model != "relay"
            && !caps.model_tags.is_empty()
            && !caps.model_tags.iter().any(|t| t == &order.model)
        {
            return JobResult::declined(
                order.job_id,
                format!("model {:?} not in advertised tags", order.model),
            );
        }

        // Synthetic completion — used to verify end-to-end wiring.
        let prompt_preview: String = order.prompt.chars().take(40).collect();
        let completion = format!(
            "parseh-miner v{ver} · echo response\n\
             received model='{model}', max_tokens={tokens}, prompt_hash={hash}\n\
             prompt preview: {preview}\n\
             (this miner runs the echo executor — replace with llama-cpp-2 to serve real inference)",
            ver = env!("CARGO_PKG_VERSION"),
            model = order.model,
            tokens = order.max_tokens,
            hash = hex::encode(&order.prompt_hash[..8]),
            preview = prompt_preview,
        );
        let wall_ms = started.elapsed().as_millis() as u64;
        JobResult::ok(order.job_id, completion, /* tokens */ 0, wall_ms)
    }
}

/// Deterministic canary executor: returns the SHA-256 of the prompt as the
/// "completion", with token_count estimated as prompt.bytes().count() / 4.
///
/// Purpose: end-to-end integration tests + smoke-testing the executor swap
/// pattern without requiring any LLM weights on disk.
///
/// Enable with `cargo build --features canary-executor`.
pub struct CanaryExecutor;

#[async_trait]
impl Executor for CanaryExecutor {
    async fn execute(&self, order: &JobOrder, caps: &NodeCapabilities) -> JobResult {
        let started = Instant::now();

        // Decline jobs we are not configured to serve. Mirrors EchoExecutor's
        // contract so swapping at the trait boundary is observably equivalent
        // for the decline paths.
        if order.model == "relay" && !caps.relay {
            return JobResult::declined(order.job_id, "this miner does not advertise relay service");
        }
        if order.model != "relay" && !caps.inference {
            return JobResult::declined(
                order.job_id,
                "this miner does not advertise inference service (set capabilities.inference = true)",
            );
        }
        if order.model != "relay"
            && !caps.model_tags.is_empty()
            && !caps.model_tags.iter().any(|t| t == &order.model)
        {
            return JobResult::declined(
                order.job_id,
                format!("model {:?} not in advertised tags", order.model),
            );
        }

        // Deterministic hash of the prompt — same prompt always yields the
        // same completion, different prompts always differ. Useful for
        // smoke-testing the request/response pipe without an LLM.
        let full_hash = Sha256::digest(order.prompt.as_bytes());
        let hex_hash_full = hex::encode(full_hash);
        // Suffix-truncated to 16 chars so the line stays short on the wire.
        let hex_hash = &hex_hash_full[hex_hash_full.len() - 16..];

        let tokens_used = (order.prompt.len() / 4) as u32;

        let completion = format!(
            "CanaryExecutor v{} · prompt_hash={} · tokens~{}",
            env!("CARGO_PKG_VERSION"),
            hex_hash,
            tokens_used,
        );
        let wall_ms = started.elapsed().as_millis() as u64;
        JobResult::ok(order.job_id, completion, tokens_used, wall_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orders::JobOutcome;

    fn caps_inference_on() -> NodeCapabilities {
        NodeCapabilities {
            relay: false,
            inference: true,
            gpu_memory_mb: 0,
            model_tags: vec![],
            uplink_mbps: 0,
        }
    }

    fn caps_inference_off() -> NodeCapabilities {
        NodeCapabilities {
            relay: false,
            inference: false,
            gpu_memory_mb: 0,
            model_tags: vec![],
            uplink_mbps: 0,
        }
    }

    fn order_with_prompt(prompt: &str) -> JobOrder {
        JobOrder {
            job_id: [0u8; 32],
            model: "qwen2.5:7b".to_string(),
            prompt_hash: [0u8; 32],
            prompt: prompt.to_string(),
            max_tokens: 256,
            bounty_upar: 1_000,
        }
    }

    #[tokio::test]
    async fn canary_returns_deterministic_completion_for_same_prompt() {
        let exec = CanaryExecutor;
        let caps = caps_inference_on();
        let order = order_with_prompt("hello parseh");

        let r1 = exec.execute(&order, &caps).await;
        let r2 = exec.execute(&order, &caps).await;

        assert!(matches!(r1.outcome, JobOutcome::Ok));
        assert!(matches!(r2.outcome, JobOutcome::Ok));
        assert_eq!(
            r1.completion, r2.completion,
            "CanaryExecutor must be deterministic for identical prompts"
        );
    }

    #[tokio::test]
    async fn canary_differs_for_different_prompts() {
        let exec = CanaryExecutor;
        let caps = caps_inference_on();

        let r1 = exec
            .execute(&order_with_prompt("prompt-one"), &caps)
            .await;
        let r2 = exec
            .execute(&order_with_prompt("prompt-two-different"), &caps)
            .await;

        let c1 = r1.completion.expect("ok result has completion");
        let c2 = r2.completion.expect("ok result has completion");
        assert_ne!(
            c1, c2,
            "different prompts must produce different hex hashes"
        );
    }

    #[tokio::test]
    async fn canary_declines_inference_when_caps_disable_it() {
        let exec = CanaryExecutor;
        let caps = caps_inference_off();
        let order = order_with_prompt("anything");

        let result = exec.execute(&order, &caps).await;

        match result.outcome {
            JobOutcome::Declined { reason } => {
                assert!(
                    reason.contains("does not advertise inference service"),
                    "decline reason should mention missing inference capability, got: {reason}"
                );
            }
            other => panic!("expected Declined outcome, got {other:?}"),
        }
        assert!(result.completion.is_none());
    }

    #[tokio::test]
    async fn canary_token_count_is_proportional_to_prompt_length() {
        let exec = CanaryExecutor;
        let caps = caps_inference_on();
        // 40 ASCII bytes → tokens_used == 10
        let prompt = "0123456789012345678901234567890123456789";
        assert_eq!(prompt.len(), 40);

        let result = exec.execute(&order_with_prompt(prompt), &caps).await;

        assert!(matches!(result.outcome, JobOutcome::Ok));
        assert_eq!(
            result.tokens_used,
            (prompt.len() / 4) as u32,
            "tokens_used must equal prompt.len() / 4"
        );
        assert_eq!(result.tokens_used, 10);
    }
}
