//! `malicious_verifier` — verifier-side misbehaviour scenarios.
//!
//! Per the project notes §3.2 + §3.6, V0.2
//! assumes a minority of verifiers may rubber-stamp results without
//! re-executing, vote randomly, or vote in the dissenting direction to
//! attack the executor's reputation. This module models five concrete
//! misbehaviours.
//!
//! ## What "MaliciousVerifier" means here
//!
//! The chaos mesh installs the [`MaliciousMode`] on a subset of nodes;
//! when that node receives a `JobResult` it produces a verdict via
//! [`malicious_verdict`] instead of re-executing. The resulting
//! `JobVerification` is still signed by the malicious node's real
//! ed25519 key and carries a real `PeerId` — so the protocol layer
//! sees a perfectly-valid envelope with a dishonest payload.
//!
//! ## V0.2 design boundary
//!
//! V0.2 protects against minority-malicious quorums via M-of-N
//! agreement and reputation-weighted thresholds. It does **not**
//! defend against >50% rubber-stamp adversaries; that's a Sybil-cost
//! problem solved by `sybil-cost-analysis.md`. The tests in this
//! module pin the exact threshold where the protocol fails — i.e.
//! they are the empirical record of what V0.2 cannot defend against.

use parseh_task::{JobResult, VerifierVerdict};
use rand::Rng;

use crate::scenario::{standard_quorum_for_test, ChaosScenario};

/// One of five concrete verifier misbehaviours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaliciousMode {
    /// Always signs `Agreed`, never re-executes. The textbook lazy
    /// rubber-stamp adversary.
    AlwaysAgreed,
    /// Always signs `Disagreed`. The reputation-griefing adversary
    /// — tries to drag an honest executor's reputation down by
    /// disputing every result.
    AlwaysDisagreed,
    /// 50/50 verdict — uniformly random. Models the "verifier that
    /// flipped a coin" scenario.
    Random,
    /// Same as `AlwaysAgreed` but emphasises the timing channel: this
    /// verifier races to publish its `Agreed` verdict within 50 ms,
    /// before any honest peer has time to re-execute and disagree.
    /// (The 50 ms claim is documented; this module returns the verdict
    /// immediately — the timing-attack property is asserted at
    /// integration-test level by clocking arrival latencies.)
    RaceToVoteFirst,
    /// Signs `Agreed` iff the result envelope parses, regardless of
    /// content. Distinct from `AlwaysAgreed` in that a structurally-
    /// broken result still gets a `Disagreed` (sentinel
    /// `evidence_hash`). Models the "I only check the envelope"
    /// adversary.
    RubberStamp,
}

impl MaliciousMode {
    /// Short tag for logging.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::AlwaysAgreed => "always-agreed",
            Self::AlwaysDisagreed => "always-disagreed",
            Self::Random => "random",
            Self::RaceToVoteFirst => "race-to-vote-first",
            Self::RubberStamp => "rubber-stamp",
        }
    }
}

/// Replace the honest verifier verdict with a misbehaving one.
///
/// Called from the chaos scenario's dispatch path when the node has
/// a non-`None` [`MaliciousMode`]. The returned `VerifierVerdict` is
/// signed normally by the malicious node's real ed25519 key.
pub fn malicious_verdict(mode: MaliciousMode, result: &JobResult) -> VerifierVerdict {
    match mode {
        MaliciousMode::AlwaysAgreed => VerifierVerdict::Agreed,
        MaliciousMode::AlwaysDisagreed => VerifierVerdict::Disagreed {
            evidence_hash: result.content_hash(),
        },
        MaliciousMode::Random => {
            if rand::thread_rng().gen_bool(0.5) {
                VerifierVerdict::Agreed
            } else {
                VerifierVerdict::Disagreed {
                    evidence_hash: result.content_hash(),
                }
            }
        }
        MaliciousMode::RaceToVoteFirst => VerifierVerdict::Agreed,
        // For RubberStamp we always Agreed — the malicious-verdict
        // path only fires after the result envelope has parsed and
        // its signature has verified upstream, so the "envelope
        // unparseable → Disagreed" branch is unreachable here. Kept
        // distinct from `AlwaysAgreed` for log clarity.
        MaliciousMode::RubberStamp => VerifierVerdict::Agreed,
    }
}

/// High-level harness for the malicious-verifier scenarios. Constructs
/// a mesh with `honest_count` honest nodes plus `malicious_count` nodes
/// in the given [`MaliciousMode`], waits for mesh formation, and
/// returns the underlying [`ChaosScenario`] for the test to drive.
pub struct MaliciousVerifier;

impl MaliciousVerifier {
    /// Construct a mesh with N nodes; the first `honest_count` are
    /// honest, the remainder run `mode`. Uses the V0.2 standard
    /// quorum (M=5/N=9) — these tests need the realistic 9-node
    /// quorum spread to exercise the malicious threshold properly.
    pub async fn build(
        honest_count: usize,
        malicious_count: usize,
        mode: MaliciousMode,
    ) -> anyhow::Result<ChaosScenario> {
        let mut modes = Vec::with_capacity(honest_count + malicious_count);
        for _ in 0..honest_count {
            modes.push(None);
        }
        for _ in 0..malicious_count {
            modes.push(Some(mode));
        }
        ChaosScenario::with_quorum_and_modes(
            honest_count + malicious_count,
            standard_quorum_for_test(),
            modes,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use parseh_task::{ContentHash, ResultMeta};

    fn fake_result() -> JobResult {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let peer = libp2p::PeerId::from(
            libp2p::identity::Keypair::ed25519_from_bytes(&mut [7u8; 32])
                .expect("seed valid")
                .public(),
        );
        let (r, _) = JobResult::new_signed(
            ContentHash::zero(),
            peer,
            ResultMeta {
                verifier_method: parseh_task::VerifierMethod::Deterministic,
                execution_time_ms: 0,
                model_used: None,
                inference_token_count: None,
            },
            b"payload".to_vec(),
            &sk,
        );
        r
    }

    #[test]
    fn always_agreed_returns_agreed() {
        let r = fake_result();
        assert!(matches!(
            malicious_verdict(MaliciousMode::AlwaysAgreed, &r),
            VerifierVerdict::Agreed
        ));
    }

    #[test]
    fn always_disagreed_returns_disagreed() {
        let r = fake_result();
        assert!(matches!(
            malicious_verdict(MaliciousMode::AlwaysDisagreed, &r),
            VerifierVerdict::Disagreed { .. }
        ));
    }

    #[test]
    fn rubber_stamp_returns_agreed() {
        let r = fake_result();
        assert!(matches!(
            malicious_verdict(MaliciousMode::RubberStamp, &r),
            VerifierVerdict::Agreed
        ));
    }

    #[test]
    fn tags_are_unique() {
        let tags = [
            MaliciousMode::AlwaysAgreed.tag(),
            MaliciousMode::AlwaysDisagreed.tag(),
            MaliciousMode::Random.tag(),
            MaliciousMode::RaceToVoteFirst.tag(),
            MaliciousMode::RubberStamp.tag(),
        ];
        let unique: std::collections::HashSet<_> = tags.iter().collect();
        assert_eq!(unique.len(), tags.len());
    }
}
