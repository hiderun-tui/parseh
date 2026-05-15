//! M-of-N quorum aggregator.
//!
//! Collects [`parseh_task::JobVerification`] envelopes for one result,
//! applies the **V0.2 finalisation rules** from
//! the project notes §3, and produces a signed
//! [`parseh_task::JobOutcome`] when the window closes.
//!
//! ## Finalisation rules
//!
//! Let `agreed`, `disagreed`, `abstained` be the per-verdict counts.
//! Let `now - started_at` be the elapsed window.
//!
//! - If `elapsed < T_min` → [`QuorumDecision::StillOpen`].
//! - Else if `elapsed > T_max` and we have not reached M-of-N →
//!   [`QuorumDecision::Indeterminate`].
//! - Else if `agreed ≥ M` *and* `rep_weighted ≥ 0.6` →
//!   [`QuorumDecision::Agreed`].
//! - Else if `disagreed ≥ M` *and* `1 - rep_weighted ≥ 0.6` →
//!   [`QuorumDecision::Disagreed`].
//! - Else if both sides have ≥ ⌊M/2⌋ votes (split decision) →
//!   [`QuorumDecision::Disputed`].
//! - Else [`QuorumDecision::StillOpen`].
//!
//! Where `rep_weighted = Σ rep_i over Agreed verifiers / Σ rep_i over
//! (Agreed ∪ Disagreed)`. Abstentions are explicitly excluded — they
//! do not move the reputation-weighted tally either way, mirroring
//! the spec §3.2.
//!
//! ## Why the observer signs
//!
//! V0.2 has no chain. The [`JobOutcome`] is signed by the **node that
//! finalised the quorum locally**, so peers consuming the outcome via
//! a shared-state delta can verify the projection was authored by a
//! known peer. At V0.3+ this signature is replaced by a chain-validated
//! state transition (see
//! the project notes §3.2 and the
//! `parseh-task` `JobOutcome` rationale).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use ed25519_dalek::{SigningKey, VerifyingKey};
use libp2p::PeerId;
use parseh_task::{
    ContentHash, JobOutcome, JobVerification, OutcomeVerdict, VerifierVerdict,
};

use crate::{params, VerifyError};

/// Quorum tunables. Use [`QuorumConfig::standard`] (5-of-9) or
/// [`QuorumConfig::sensitive`] (9-of-15).
#[derive(Debug, Clone, Copy)]
pub struct QuorumConfig {
    /// M — agreement threshold (the number of `Agreed` or
    /// `Disagreed` verdicts needed to finalise).
    pub m: u32,
    /// N — target verifier count. Carried for diagnostics; the quorum
    /// does not need it to finalise.
    pub n: u32,
    /// Minimum elapsed time before finalisation is allowed.
    pub t_min: Duration,
    /// Maximum elapsed time before the quorum is declared
    /// `Indeterminate`.
    pub t_max: Duration,
    /// Reputation-weighted threshold for an `Agreed` (or
    /// `Disagreed`) finalisation. V0.2: **0.6**.
    pub rep_weighted_threshold: f64,
}

impl QuorumConfig {
    /// V0.2 **Standard** quorum (M=5, N=9).
    pub fn standard() -> Self {
        Self {
            m: params::M_STANDARD,
            n: params::N_STANDARD,
            t_min: Duration::from_secs(params::T_MIN_SECS),
            t_max: Duration::from_secs(params::T_MAX_SECS),
            rep_weighted_threshold: params::REP_WEIGHTED_THRESHOLD,
        }
    }

    /// V0.2 **Sensitive** quorum (M=9, N=15).
    pub fn sensitive() -> Self {
        Self {
            m: params::M_SENSITIVE,
            n: params::N_SENSITIVE,
            t_min: Duration::from_secs(params::T_MIN_SECS),
            t_max: Duration::from_secs(params::T_MAX_SECS),
            rep_weighted_threshold: params::REP_WEIGHTED_THRESHOLD,
        }
    }
}

/// Open / closed state of a quorum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuorumDecision {
    /// Not enough verdicts yet, **and** `T_min` not yet elapsed, OR
    /// the time-bound has elapsed but the M-of-N counting has not
    /// resolved.
    StillOpen,
    /// `Agreed` quorum reached (`agreed ≥ M` and
    /// `rep_weighted ≥ threshold`).
    Agreed,
    /// `Disagreed` quorum reached symmetrically.
    Disagreed,
    /// Split decision — both sides have substantial vote shares but
    /// neither side cleared the threshold. The `Disputed` outcome
    /// triggers the §3.3 escalation path.
    Disputed,
    /// `T_max` elapsed without reaching M-of-N. No reputation
    /// changes; requester may re-submit.
    Indeterminate,
}

/// The finalised outcome of a quorum window, plus the signed
/// [`JobOutcome`] envelope ready for shared-state propagation.
#[derive(Debug, Clone)]
pub struct FinalisedQuorum {
    /// The terminal decision.
    pub decision: QuorumDecision,
    /// Signed `JobOutcome` envelope.
    pub outcome: JobOutcome,
    /// Number of `Agreed` verdicts that contributed.
    pub agreements: u32,
    /// Number of `Disagreed` verdicts that contributed.
    pub disagreements: u32,
    /// Number of `Abstained` verdicts that contributed.
    pub abstentions: u32,
    /// Reputation-weighted score in `[0, 1]`. `0.0` means no
    /// reputation-weighted vote tally was computable (e.g. only
    /// abstentions present).
    pub reputation_weighted: f64,
}

/// One stored verification in the quorum.
#[derive(Debug, Clone)]
struct StoredVerification {
    verification: JobVerification,
    reputation: u32,
}

/// One open quorum.
#[derive(Debug, Clone)]
pub struct Quorum {
    /// Tunables.
    config: QuorumConfig,
    /// `ContentHash` of the [`parseh_task::JobResult`] this quorum
    /// covers.
    result_hash: ContentHash,
    /// `ContentHash` of the originating [`parseh_task::JobSpec`].
    spec_hash: ContentHash,
    /// When the window opened. The first observed verification is
    /// what starts the clock (§3.2), but we let the caller fix
    /// `started_at` at construction so tests can run on injected
    /// times.
    started_at: SystemTime,
    /// Accumulated verifications, keyed by verifier `PeerId` (we
    /// allow at most one per verifier).
    by_verifier: HashMap<PeerId, StoredVerification>,
}

impl Quorum {
    /// Construct a fresh, empty quorum.
    ///
    /// The `spec_hash` ties the eventual [`JobOutcome`] back to the
    /// task identifier; the `result_hash` is what every
    /// [`JobVerification::result_hash`] must match.
    pub fn new(
        config: QuorumConfig,
        spec_hash: ContentHash,
        result_hash: ContentHash,
        started_at: SystemTime,
    ) -> Self {
        Self {
            config,
            result_hash,
            spec_hash,
            started_at,
            by_verifier: HashMap::new(),
        }
    }

    /// Number of accepted verifications so far. Useful for tests and
    /// diagnostics; not part of the finalisation logic.
    pub fn len(&self) -> usize {
        self.by_verifier.len()
    }

    /// `true` iff no verifications have been added yet.
    pub fn is_empty(&self) -> bool {
        self.by_verifier.is_empty()
    }

    /// Add a [`JobVerification`] to the quorum.
    ///
    /// Validates:
    /// - `verification.result_hash` matches the quorum's `result_hash`,
    /// - signature is valid against the supplied verifier pubkey,
    /// - no prior verification from the same verifier (one-vote-per-
    ///   verifier rule, mitigates Sybil-on-aggregator).
    pub fn add_verification(
        &mut self,
        verification: JobVerification,
        verifier_reputation: u32,
        verifier_pubkey: &VerifyingKey,
    ) -> Result<(), VerifyError> {
        if verification.result_hash != self.result_hash {
            return Err(VerifyError::Internal(format!(
                "result_hash mismatch: quorum tracks {}, verification carries {}",
                self.result_hash, verification.result_hash
            )));
        }
        verification
            .verify_signature(verifier_pubkey)
            .map_err(|e| VerifyError::Internal(format!("bad signature on verification: {e}")))?;
        if self.by_verifier.contains_key(&verification.verifier) {
            return Err(VerifyError::Internal(format!(
                "duplicate verification from verifier {}",
                verification.verifier
            )));
        }
        self.by_verifier.insert(
            verification.verifier,
            StoredVerification {
                verification,
                reputation: verifier_reputation,
            },
        );
        Ok(())
    }

    /// Compute the current per-verdict counts and reputation-weighted
    /// tally without finalising.
    pub fn tally(&self) -> (u32, u32, u32, f64) {
        let mut agreed = 0u32;
        let mut disagreed = 0u32;
        let mut abstained = 0u32;
        let mut agreed_rep: u64 = 0;
        let mut disagreed_rep: u64 = 0;
        for s in self.by_verifier.values() {
            match s.verification.verdict {
                VerifierVerdict::Agreed => {
                    agreed += 1;
                    agreed_rep = agreed_rep.saturating_add(s.reputation as u64);
                }
                VerifierVerdict::Disagreed { .. } => {
                    disagreed += 1;
                    disagreed_rep = disagreed_rep.saturating_add(s.reputation as u64);
                }
                VerifierVerdict::Abstained => abstained += 1,
            }
        }
        let total_rep = agreed_rep + disagreed_rep;
        let rep_weighted = if total_rep == 0 {
            0.0
        } else {
            agreed_rep as f64 / total_rep as f64
        };
        (agreed, disagreed, abstained, rep_weighted)
    }

    /// Try to finalise the quorum as of `now`.
    ///
    /// Returns `Some(FinalisedQuorum)` if either:
    ///
    /// - M-of-N reached **and** `T_min` elapsed **and** the
    ///   reputation-weighted threshold passes one side, or
    /// - `T_max` elapsed (regardless of count) — finalises as
    ///   [`QuorumDecision::Indeterminate`] unless M-of-N happens to be
    ///   met in the same call.
    ///
    /// Returns `None` if the window is still open.
    pub fn try_finalise(
        &self,
        now: SystemTime,
        observer: PeerId,
        observer_signing_key: &SigningKey,
    ) -> Option<FinalisedQuorum> {
        let elapsed = now.duration_since(self.started_at).unwrap_or_default();
        let (agreed, disagreed, abstained, rep_weighted) = self.tally();

        // Pre-T_min: cannot close yet.
        if elapsed < self.config.t_min {
            return None;
        }

        let m = self.config.m;
        let thresh = self.config.rep_weighted_threshold;
        let agreed_quorum = agreed >= m && rep_weighted >= thresh;
        let disagreed_quorum = disagreed >= m && (1.0 - rep_weighted) >= thresh;

        // The split-decision threshold — both sides crossed half-M.
        // We only use this if T_max has elapsed without a clean win;
        // before T_max we keep collecting in case the deadlock breaks.
        let half_m = (m / 2).max(1);
        let split = agreed >= half_m && disagreed >= half_m;

        let decision = if agreed_quorum {
            QuorumDecision::Agreed
        } else if disagreed_quorum {
            QuorumDecision::Disagreed
        } else if elapsed >= self.config.t_max {
            if split {
                QuorumDecision::Disputed
            } else {
                QuorumDecision::Indeterminate
            }
        } else {
            return None;
        };

        // Build the JobOutcome.
        // Stable order so two observers produce byte-identical
        // outcomes (and therefore identical content hashes). The raw
        // 32-byte digest is the natural sort key; `ContentHash` does
        // not implement `Ord` so we sort by `as_bytes()`.
        let mut verification_hashes: Vec<ContentHash> = self
            .by_verifier
            .values()
            .map(|s| s.verification.content_hash())
            .collect();
        verification_hashes.sort_by_key(|h| *h.as_bytes());
        verification_hashes.dedup_by_key(|h| *h.as_bytes());

        let verdict = match decision {
            QuorumDecision::Agreed | QuorumDecision::Disagreed => OutcomeVerdict::Valid {
                agreements: agreed,
                disagreements: disagreed,
                abstentions: abstained,
                reputation_weighted: rep_weighted,
            },
            QuorumDecision::Disputed => {
                let disputers = self.collect_disputers();
                OutcomeVerdict::Disputed { disputers }
            }
            QuorumDecision::Indeterminate => OutcomeVerdict::Indeterminate,
            QuorumDecision::StillOpen => return None,
        };

        let (outcome, _hash) = JobOutcome::new_signed(
            self.spec_hash,
            self.result_hash,
            verification_hashes,
            verdict,
            observer,
            observer_signing_key,
        );

        Some(FinalisedQuorum {
            decision,
            outcome,
            agreements: agreed,
            disagreements: disagreed,
            abstentions: abstained,
            reputation_weighted: rep_weighted,
        })
    }

    /// Return the minority-side verifier `PeerId`s.
    fn collect_disputers(&self) -> Vec<PeerId> {
        let (agreed, disagreed, _, _) = self.tally();
        // The smaller side files dispute. On exact tie we treat
        // `Disagreed` as the dissenting minority by convention (the
        // executor's claim is the "yes" baseline).
        let minority_is_disagreed = disagreed <= agreed;
        let mut set: HashSet<PeerId> = HashSet::new();
        for s in self.by_verifier.values() {
            let is_minority = match s.verification.verdict {
                VerifierVerdict::Agreed => !minority_is_disagreed,
                VerifierVerdict::Disagreed { .. } => minority_is_disagreed,
                VerifierVerdict::Abstained => false,
            };
            if is_minority {
                set.insert(s.verification.verifier);
            }
        }
        let mut out: Vec<PeerId> = set.into_iter().collect();
        // Stable order; PeerId implements Ord via its bytes form.
        out.sort_by_key(|p| p.to_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params;
    use ed25519_dalek::SigningKey;
    use parseh_task::{JobVerification, VerifierMethod};
    use rand::rngs::OsRng;
    use std::time::{Duration, UNIX_EPOCH};

    fn fresh_peer() -> PeerId {
        PeerId::from(libp2p::identity::Keypair::generate_ed25519().public())
    }

    fn make_verification(verdict: VerifierVerdict, result_hash: ContentHash) -> (JobVerification, SigningKey, PeerId) {
        let sk = SigningKey::generate(&mut OsRng);
        let peer = fresh_peer();
        let (v, _) = JobVerification::new_signed_at(
            result_hash,
            peer,
            verdict,
            VerifierMethod::Deterministic,
            1_700_000_500,
            &sk,
        );
        (v, sk, peer)
    }

    #[test]
    fn empty_quorum_is_still_open_before_tmin() {
        let observer_sk = SigningKey::generate(&mut OsRng);
        let observer = fresh_peer();
        let q = Quorum::new(
            QuorumConfig::standard(),
            ContentHash::zero(),
            ContentHash::zero(),
            UNIX_EPOCH,
        );
        assert!(q
            .try_finalise(UNIX_EPOCH + Duration::from_secs(1), observer, &observer_sk)
            .is_none());
    }

    #[test]
    fn tally_counts_each_verdict_kind() {
        let result_hash = parseh_task::content_hash(b"r");
        let mut q = Quorum::new(
            QuorumConfig::standard(),
            ContentHash::zero(),
            result_hash,
            UNIX_EPOCH,
        );
        // Three agreed.
        for _ in 0..3 {
            let (v, sk, _) = make_verification(VerifierVerdict::Agreed, result_hash);
            q.add_verification(v, 100, &sk.verifying_key()).unwrap();
        }
        // One disagreed.
        let (v, sk, _) = make_verification(
            VerifierVerdict::Disagreed {
                evidence_hash: parseh_task::content_hash(b"diff"),
            },
            result_hash,
        );
        q.add_verification(v, 50, &sk.verifying_key()).unwrap();
        // One abstained.
        let (v, sk, _) = make_verification(VerifierVerdict::Abstained, result_hash);
        q.add_verification(v, 200, &sk.verifying_key()).unwrap();
        let (a, d, ab, w) = q.tally();
        assert_eq!((a, d, ab), (3, 1, 1));
        assert!((w - 300.0 / 350.0).abs() < 1e-9);
    }

    #[test]
    fn duplicate_verifier_rejected() {
        let result_hash = parseh_task::content_hash(b"r");
        let mut q = Quorum::new(
            QuorumConfig::standard(),
            ContentHash::zero(),
            result_hash,
            UNIX_EPOCH,
        );
        let sk = SigningKey::generate(&mut OsRng);
        let peer = fresh_peer();
        let (v1, _) = JobVerification::new_signed_at(
            result_hash,
            peer,
            VerifierVerdict::Agreed,
            VerifierMethod::Deterministic,
            1,
            &sk,
        );
        let (v2, _) = JobVerification::new_signed_at(
            result_hash,
            peer,
            VerifierVerdict::Agreed,
            VerifierMethod::Deterministic,
            2,
            &sk,
        );
        q.add_verification(v1, 100, &sk.verifying_key()).unwrap();
        let err = q.add_verification(v2, 100, &sk.verifying_key()).unwrap_err();
        assert!(matches!(err, VerifyError::Internal(_)));
    }

    #[test]
    fn wrong_result_hash_rejected() {
        let result_hash = parseh_task::content_hash(b"r");
        let other_hash = parseh_task::content_hash(b"other");
        let mut q = Quorum::new(
            QuorumConfig::standard(),
            ContentHash::zero(),
            result_hash,
            UNIX_EPOCH,
        );
        let (v, sk, _) = make_verification(VerifierVerdict::Agreed, other_hash);
        let err = q.add_verification(v, 100, &sk.verifying_key()).unwrap_err();
        assert!(matches!(err, VerifyError::Internal(_)));
    }

    #[test]
    fn standard_config_uses_v0_2_params() {
        let c = QuorumConfig::standard();
        assert_eq!(c.m, params::M_STANDARD);
        assert_eq!(c.n, params::N_STANDARD);
        assert_eq!(c.t_min, Duration::from_secs(params::T_MIN_SECS));
        assert_eq!(c.t_max, Duration::from_secs(params::T_MAX_SECS));
        assert!((c.rep_weighted_threshold - params::REP_WEIGHTED_THRESHOLD).abs() < 1e-12);
    }
}
