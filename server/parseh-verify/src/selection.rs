//! Verifier self-selection.
//!
//! Per-task, every eligible verifier independently decides whether to
//! re-execute. The decision is **pure** — given the same inputs and
//! the same seed, the answer is identical — which makes the algorithm
//! trivially testable and replay-debuggable.
//!
//! The function intentionally encodes the four spec rules in this order:
//!
//! 1. **Rule 3a** — never verify a task you submitted yourself.
//! 2. **Rule 3b** — never verify a result you produced yourself.
//! 3. **Probationary gate** — reputation below
//!    [`params::PROBATIONARY_REP_FLOOR`] is not eligible.
//! 4. **Rate cap** — 10% of observed tasks per rolling hour.
//! 5. **Reputation-weighted dice roll** —
//!    `p_node = clamp(p_base · rep / rep_avg, p_min, p_max)`.
//!
//! See the project notes §2.1.

use libp2p::PeerId;
use parseh_task::{JobResult, JobSpec};
use sha2::{Digest, Sha256};

use crate::{params, RateLimit};

/// Inputs to [`decide_to_verify`]. Pass by reference; all fields are
/// immutable for the duration of one decision.
#[derive(Debug, Clone)]
pub struct SelectionConfig {
    /// The local node's libp2p `PeerId`. Used for Rule 3 (anti-self)
    /// and as a tie-break entropy source for the dice roll.
    pub local_peer_id: PeerId,
    /// The local node's current reputation. Below
    /// [`params::PROBATIONARY_REP_FLOOR`] disqualifies.
    pub local_reputation: u32,
    /// The network's rolling average reputation among Established
    /// peers. Used as the denominator in the reputation-weighted
    /// probability calculation. Caller must clamp 0 → 1 before
    /// passing in; we guard internally regardless.
    pub network_avg_reputation: u32,
    /// Rate-limit state. Read-only at decision time; the caller
    /// is responsible for `record_*` calls around the verify
    /// pipeline.
    pub rate_limit: RateLimit,
    /// Already-verified set check. When `true`, [`decide_to_verify`]
    /// short-circuits with [`SkipReason::AlreadyVerified`]. The caller
    /// owns the dedup map (typically `HashSet<ContentHash>` keyed by
    /// `result.content_hash()`).
    pub already_verified_this_task: bool,
}

/// The decision produced by [`decide_to_verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionDecision {
    /// Local node has been chosen to verify this task.
    Verify,
    /// Local node skips this task.
    Skip {
        /// Why it was skipped — useful for metrics and for tests.
        reason: SkipReason,
    },
}

/// Why a [`decide_to_verify`] call returned [`SelectionDecision::Skip`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Local reputation is below the probationary floor.
    BelowProbationary,
    /// The local node has already verified its quota in the rolling
    /// 10%-per-hour window.
    RateLimited,
    /// The probability roll did not land in the local node's window.
    /// This is the "normal" skip reason for almost every task.
    RandomNotChosen,
    /// Rule 3: local node is either the task submitter or the result
    /// executor.
    OwnTask,
    /// Local node has already produced a `JobVerification` for this
    /// task; verifying twice would invite signed-contradiction
    /// penalties.
    AlreadyVerified,
}

/// Decide whether the local node should verify this task.
///
/// Pure function: given `cfg`, `task`, `result` and `seed`, returns
/// the same [`SelectionDecision`]. Reuses the V0.2 parameters from
/// [`params`].
///
/// `seed` is a per-decision random source. In production, callers
/// derive it from `(peer_secret, result.content_hash())` so each peer
/// gets a different roll and the result is unpredictable to outsiders;
/// in tests, callers pass a fixed value to assert deterministic
/// behaviour.
pub fn decide_to_verify(
    cfg: &SelectionConfig,
    task: &JobSpec,
    result: &JobResult,
    seed: u64,
) -> SelectionDecision {
    // 1. Rule 3a — never verify a task we submitted.
    if task.submitter == cfg.local_peer_id {
        return SelectionDecision::Skip {
            reason: SkipReason::OwnTask,
        };
    }
    // 2. Rule 3b — never verify a result we produced.
    if result.executor == cfg.local_peer_id {
        return SelectionDecision::Skip {
            reason: SkipReason::OwnTask,
        };
    }
    // 3. Already-verified dedup.
    if cfg.already_verified_this_task {
        return SelectionDecision::Skip {
            reason: SkipReason::AlreadyVerified,
        };
    }
    // 4. Probationary gate.
    if cfg.local_reputation < params::PROBATIONARY_REP_FLOOR {
        return SelectionDecision::Skip {
            reason: SkipReason::BelowProbationary,
        };
    }
    // 5. Rate cap.
    if cfg.rate_limit.exceeded_at_now() {
        return SelectionDecision::Skip {
            reason: SkipReason::RateLimited,
        };
    }
    // 6. Reputation-weighted probability.
    let rep_ratio =
        cfg.local_reputation as f64 / (cfg.network_avg_reputation.max(1) as f64);
    let p_node = (params::P_BASE * rep_ratio).clamp(params::P_MIN, params::P_MAX);

    // 7. Roll. Combine seed + local_peer_id so two peers with the
    //    same seed reach different conclusions.
    let roll = roll_from_seed(seed, cfg.local_peer_id);
    if roll < p_node {
        SelectionDecision::Verify
    } else {
        SelectionDecision::Skip {
            reason: SkipReason::RandomNotChosen,
        }
    }
}

/// Map a `(seed, peer_id)` pair to a uniform `f64 ∈ [0, 1)`.
///
/// SHA-256 over the concatenation gives us a high-quality digest;
/// we take the first 8 bytes as a `u64` and divide by `u64::MAX + 1`
/// (as `2^64` in `f64`).
///
/// Not a CSPRNG — but the security property we need here is
/// *unpredictability to a non-local observer who doesn't know the
/// local peer's contribution*, which SHA-256 trivially gives us.
fn roll_from_seed(seed: u64, local_peer_id: PeerId) -> f64 {
    let mut hasher = Sha256::new();
    hasher.update(seed.to_le_bytes());
    hasher.update(local_peer_id.to_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let n = u64::from_le_bytes(bytes);
    // `2^64` in `f64` is exactly representable; division produces
    // values in `[0, 1)` as required.
    (n as f64) / (u64::MAX as f64 + 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use parseh_core::ServiceKind;
    use parseh_task::{JobInputs, JobKind, JobResult, JobSpec, ResultMeta, VerifierMethod};
    use rand::rngs::OsRng;

    fn fresh_peer() -> PeerId {
        PeerId::from(libp2p::identity::Keypair::generate_ed25519().public())
    }

    fn fresh_spec(submitter: PeerId, sk: &SigningKey) -> JobSpec {
        let (spec, _) = JobSpec::new_signed_at(
            JobKind::Inference,
            JobInputs::inference_prompt("hello", 42),
            ServiceKind::Inference,
            false,
            1_700_000_000,
            submitter,
            sk,
        );
        spec
    }

    fn fresh_result(executor: PeerId, sk: &SigningKey, spec_hash: parseh_task::ContentHash) -> JobResult {
        let meta = ResultMeta {
            verifier_method: VerifierMethod::Deterministic,
            execution_time_ms: 100,
            model_used: Some("test-model".into()),
            inference_token_count: Some(1),
        };
        let (r, _) = JobResult::new_signed_at(
            spec_hash,
            executor,
            1_700_000_001,
            meta,
            b"payload".to_vec(),
            sk,
        );
        r
    }

    fn base_cfg(local: PeerId) -> SelectionConfig {
        SelectionConfig {
            local_peer_id: local,
            local_reputation: 100,
            network_avg_reputation: 50,
            rate_limit: RateLimit::v0_2_defaults(),
            already_verified_this_task: false,
        }
    }

    #[test]
    fn rule_3a_skips_own_submitted_task() {
        let sk = SigningKey::generate(&mut OsRng);
        let local = fresh_peer();
        let spec = fresh_spec(local, &sk);
        let result = fresh_result(fresh_peer(), &sk, spec.content_hash());
        let cfg = base_cfg(local);
        assert_eq!(
            decide_to_verify(&cfg, &spec, &result, 1),
            SelectionDecision::Skip {
                reason: SkipReason::OwnTask
            }
        );
    }

    #[test]
    fn rule_3b_skips_own_executed_result() {
        let sk = SigningKey::generate(&mut OsRng);
        let local = fresh_peer();
        let spec = fresh_spec(fresh_peer(), &sk);
        let result = fresh_result(local, &sk, spec.content_hash());
        let cfg = base_cfg(local);
        assert_eq!(
            decide_to_verify(&cfg, &spec, &result, 1),
            SelectionDecision::Skip {
                reason: SkipReason::OwnTask
            }
        );
    }

    #[test]
    fn probationary_skipped() {
        let sk = SigningKey::generate(&mut OsRng);
        let local = fresh_peer();
        let spec = fresh_spec(fresh_peer(), &sk);
        let result = fresh_result(fresh_peer(), &sk, spec.content_hash());
        let mut cfg = base_cfg(local);
        cfg.local_reputation = 9;
        assert_eq!(
            decide_to_verify(&cfg, &spec, &result, 1),
            SelectionDecision::Skip {
                reason: SkipReason::BelowProbationary
            }
        );
    }

    #[test]
    fn roll_is_uniform_ish() {
        // 10_000 rolls — empirical mean should be close to 0.5.
        let peer = fresh_peer();
        let mut sum = 0.0;
        for s in 0..10_000u64 {
            sum += roll_from_seed(s, peer);
        }
        let mean = sum / 10_000.0;
        assert!(
            (0.45..0.55).contains(&mean),
            "empirical mean was {mean}, expected ~0.5"
        );
    }
}
