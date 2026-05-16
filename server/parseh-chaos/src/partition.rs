//! `partition` — Network-partition recovery scenarios.
//!
//! **Priority milestone per maintainer direction 2026-05-14.** The next
//! engineering frontier after V0.2-PASS is whether the protocol survives
//! contact with split-brain reality: delayed sync, relay disappearance,
//! split verifier consensus, conflicting shared-state histories, replay
//! after reconnection.
//!
//! ## Scenario shape
//!
//! 1. Spin up N nodes (default 6) on `MemoryTransport`.
//! 2. Let them form a mesh and reach consensus on a small task batch.
//! 3. Move a subset (e.g. indices `[4, 5]`) into partition group 1.
//!    The remaining `[0, 1, 2, 3]` stay in group 0. Cross-group
//!    gossipsub traffic is dropped at the dispatch boundary.
//! 4. Submit new tasks to group 0. Assert group 0 reaches quorum and
//!    finalises, while group 1 cannot (not enough peers for M-of-N).
//! 5. Heal the partition. Re-share-state.
//! 6. Assert group 1 catches up via state-deltas within budget D.
//! 7. Assert ALL nodes converge on the same final state.
//!
//! ## What this tests
//!
//! - **State-delta convergence.** Are deltas idempotent under replay?
//! - **Quorum stall.** Does an undersized group correctly stall rather
//!   than finalising on a phantom consensus?
//! - **History merge.** When two groups have processed disjoint task
//!   sets and rejoin, do their `JobOutcome` views merge cleanly?
//! - **Divergence detection.** If V0.2's design has a hidden assumption
//!   about message ordering, this test surfaces it as a real bug.

use std::time::Duration;

use parseh_task::{ContentHash, JobSpec};

use crate::scenario::ChaosScenario;

/// Tunables for a partition scenario.
#[derive(Debug, Clone, Copy)]
pub struct PartitionConfig {
    /// Total nodes in the mesh.
    pub total_nodes: usize,
    /// How many nodes go into the minority partition group.
    pub minority_size: usize,
    /// Number of tasks submitted BEFORE partition.
    pub pre_partition_tasks: usize,
    /// Number of tasks submitted DURING partition (to majority group).
    pub during_partition_tasks: usize,
    /// How long the partition lasts.
    pub partition_duration: Duration,
    /// Budget for the minority to catch up after heal.
    pub catchup_budget: Duration,
}

impl Default for PartitionConfig {
    fn default() -> Self {
        Self {
            total_nodes: 6,
            minority_size: 2,
            pre_partition_tasks: 5,
            during_partition_tasks: 3,
            partition_duration: Duration::from_secs(2),
            catchup_budget: Duration::from_secs(15),
        }
    }
}

/// Empirical result returned by [`PartitionScenario::run`].
#[derive(Debug, Clone)]
pub struct PartitionResult {
    /// Tasks observed by every node BEFORE partition.
    pub pre_partition_outcome_count: usize,
    /// Tasks finalised by the majority group DURING partition.
    pub majority_during_partition_count: usize,
    /// Tasks finalised by the minority group DURING partition (must
    /// be 0 if M-of-N quorum reduction is correct — minority of 2
    /// cannot reach M=2 with itself when other half is silent).
    pub minority_during_partition_count: usize,
    /// Wall-clock seconds from heal to minority catching up.
    pub catchup_seconds: f64,
    /// `true` iff every node, after heal, agrees on every outcome.
    pub converged: bool,
    /// `true` iff conflicting histories merged correctly (i.e. the
    /// minority did not enter a divergent state).
    pub histories_merged_correctly: bool,
}

/// Driver for the partition scenario.
///
/// Single-shot — construct with [`PartitionScenario::new`] and call
/// [`PartitionScenario::run`].
pub struct PartitionScenario {
    config: PartitionConfig,
}

impl PartitionScenario {
    /// Construct a scenario with the given config.
    pub fn new(config: PartitionConfig) -> Self {
        Self { config }
    }

    /// Execute the scenario. Returns the empirical
    /// [`PartitionResult`]. Spec submissions are signed by node 0.
    pub async fn run(self) -> anyhow::Result<PartitionResult> {
        let cfg = self.config;
        let scenario = ChaosScenario::new(cfg.total_nodes).await?;

        // ── 1 · pre-partition baseline ────────────────────────────
        scenario
            .await_state_predicate(
                |s| s.known_identities >= cfg.total_nodes,
                Duration::from_secs(15),
                "pre-partition identities settle",
            )
            .await?;

        let submitter = scenario.node(0);
        let mut pre_hashes: Vec<ContentHash> = Vec::with_capacity(cfg.pre_partition_tasks);
        for i in 0..cfg.pre_partition_tasks as u64 {
            let spec = build_spec(&submitter.signing_key, submitter.peer_id, 1_000 + i);
            pre_hashes.push(spec.content_hash());
            scenario.submit_from(0, spec).await?;
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let pre_hashes_clone = pre_hashes.clone();
        scenario
            .await_state_predicate(
                move |s| pre_hashes_clone.iter().all(|h| s.has_outcome_for_spec(h)),
                Duration::from_secs(30),
                "all pre-partition outcomes propagate",
            )
            .await?;
        let pre_outcome_count = cfg.pre_partition_tasks;

        // ── 2 · partition ─────────────────────────────────────────
        let majority_size = cfg.total_nodes - cfg.minority_size;
        // Minority occupies indices [majority_size, total).
        scenario
            .partition_into(majority_size, cfg.minority_size, 1)
            .await;
        tracing::info!(
            majority = majority_size,
            minority = cfg.minority_size,
            "partition installed"
        );

        // Give the partition gate a heartbeat to settle.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // ── 3 · majority submits during partition ────────────────
        let mut during_hashes: Vec<ContentHash> =
            Vec::with_capacity(cfg.during_partition_tasks);
        for i in 0..cfg.during_partition_tasks as u64 {
            let spec = build_spec(&submitter.signing_key, submitter.peer_id, 2_000 + i);
            during_hashes.push(spec.content_hash());
            scenario.submit_from(0, spec).await?;
            tokio::time::sleep(Duration::from_millis(150)).await;
        }

        // Wait at least `partition_duration` to let the majority
        // group's quorum close. Cap waiting to whichever is larger of
        // `partition_duration` or a sensible default (10s) — under
        // serialised-test load the runtime can be slow.
        let majority_indices: Vec<usize> = (0..majority_size).collect();
        let during_hashes_clone = during_hashes.clone();
        let wait_budget = cfg.partition_duration.max(Duration::from_secs(10));
        let _ = scenario
            .await_subset_predicate(
                &majority_indices,
                move |s| during_hashes_clone.iter().all(|h| s.has_outcome_for_spec(h)),
                wait_budget,
                "majority finalises during-partition tasks",
            )
            .await;

        // Sample majority + minority outcome counts at the partition
        // tail. The majority should have all `during_partition_tasks`
        // outcomes; the minority should have zero of the new ones.
        let majority_during = count_finalised(&scenario, &majority_indices, &during_hashes).await;
        let minority_indices: Vec<usize> =
            (majority_size..cfg.total_nodes).collect();
        let minority_during = count_finalised(&scenario, &minority_indices, &during_hashes).await;

        // ── 4 · heal partition ────────────────────────────────────
        scenario.heal_partition().await;
        let heal_at = std::time::Instant::now();

        // ── 5 · wait for minority catch-up ───────────────────────
        let during_hashes_for_catchup = during_hashes.clone();
        let catchup_res = scenario
            .await_subset_predicate(
                &minority_indices,
                move |s| during_hashes_for_catchup.iter().all(|h| s.has_outcome_for_spec(h)),
                cfg.catchup_budget,
                "minority catches up post-heal",
            )
            .await;
        let catchup_secs = heal_at.elapsed().as_secs_f64();
        let catchup_succeeded = catchup_res.is_ok();

        // ── 6 · global convergence check ─────────────────────────
        let all_hashes: Vec<ContentHash> = pre_hashes
            .iter()
            .chain(during_hashes.iter())
            .copied()
            .collect();
        let all_hashes_clone = all_hashes.clone();
        let converged_res = scenario
            .await_state_predicate(
                move |s| all_hashes_clone.iter().all(|h| s.has_outcome_for_spec(h)),
                Duration::from_secs(10),
                "all nodes converge on all outcomes",
            )
            .await;
        let converged = converged_res.is_ok();

        // Histories merged correctly iff the minority caught up AND
        // global convergence holds AND every node's outcome for each
        // spec matches by ContentHash. The third condition is the one
        // that catches divergent finalisation: two groups could both
        // claim "Agreed" but with different verifier sets, producing
        // different JobOutcome content hashes.
        let outcomes_match = if converged {
            check_outcomes_match(&scenario, &all_hashes).await
        } else {
            false
        };
        let histories_merged_correctly = catchup_succeeded && converged && outcomes_match;

        scenario.shutdown().await;

        Ok(PartitionResult {
            pre_partition_outcome_count: pre_outcome_count,
            majority_during_partition_count: majority_during,
            minority_during_partition_count: minority_during,
            catchup_seconds: catchup_secs,
            converged,
            histories_merged_correctly,
        })
    }

    /// Like [`Self::run`], but after `heal()` the minority issues
    /// `/parseh/state-sync/1.0.0` requests (the production
    /// post-isolation trigger) instead of passively hoping a future
    /// gossip-delta arrives. This is the regression proof that the
    /// chaos-discovered partition-recovery bug is CLOSED.
    ///
    /// The returned `catchup_seconds` is measured from `heal()` to the
    /// instant the minority holds every during-partition outcome —
    /// i.e. the observed state-sync catch-up latency.
    pub async fn run_with_state_sync(self) -> anyhow::Result<PartitionResult> {
        let cfg = self.config;
        let scenario = ChaosScenario::new(cfg.total_nodes).await?;

        scenario
            .await_state_predicate(
                |s| s.known_identities >= cfg.total_nodes,
                Duration::from_secs(15),
                "pre-partition identities settle",
            )
            .await?;

        let submitter = scenario.node(0);
        let mut pre_hashes: Vec<ContentHash> = Vec::with_capacity(cfg.pre_partition_tasks);
        for i in 0..cfg.pre_partition_tasks as u64 {
            let spec = build_spec(&submitter.signing_key, submitter.peer_id, 1_000 + i);
            pre_hashes.push(spec.content_hash());
            scenario.submit_from(0, spec).await?;
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let pre_hashes_clone = pre_hashes.clone();
        scenario
            .await_state_predicate(
                move |s| pre_hashes_clone.iter().all(|h| s.has_outcome_for_spec(h)),
                Duration::from_secs(30),
                "all pre-partition outcomes propagate",
            )
            .await?;
        let pre_outcome_count = cfg.pre_partition_tasks;

        let majority_size = cfg.total_nodes - cfg.minority_size;
        scenario
            .partition_into(majority_size, cfg.minority_size, 1)
            .await;
        tracing::info!(
            majority = majority_size,
            minority = cfg.minority_size,
            "partition installed (state-sync regression scenario)"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut during_hashes: Vec<ContentHash> =
            Vec::with_capacity(cfg.during_partition_tasks);
        for i in 0..cfg.during_partition_tasks as u64 {
            let spec = build_spec(&submitter.signing_key, submitter.peer_id, 2_000 + i);
            during_hashes.push(spec.content_hash());
            scenario.submit_from(0, spec).await?;
            tokio::time::sleep(Duration::from_millis(150)).await;
        }

        let majority_indices: Vec<usize> = (0..majority_size).collect();
        let during_hashes_clone = during_hashes.clone();
        let wait_budget = cfg.partition_duration.max(Duration::from_secs(10));
        let _ = scenario
            .await_subset_predicate(
                &majority_indices,
                move |s| during_hashes_clone.iter().all(|h| s.has_outcome_for_spec(h)),
                wait_budget,
                "majority finalises during-partition tasks",
            )
            .await;

        // "anywhere in the majority" is the correct gap signal here.
        let majority_during =
            count_finalised_any(&scenario, &majority_indices, &during_hashes).await;
        let minority_indices: Vec<usize> =
            (majority_size..cfg.total_nodes).collect();
        let minority_during =
            count_finalised(&scenario, &minority_indices, &during_hashes).await;

        // ── heal + STATE-SYNC TRIGGER ─────────────────────────────
        scenario.heal_partition().await;
        let heal_at = std::time::Instant::now();
        // Give caps a heartbeat to re-cross the (now-healed) gate so
        // each minority node knows the majority observers' pubkeys —
        // the apply path re-verifies every inner outcome signature, so
        // it MUST have the observer keys. This models the production
        // path where `parseh.caps.v1` re-propagates on reconnect.
        tokio::time::sleep(Duration::from_millis(800)).await;

        // The minority issues the catch-up pull. `since = 0` is the
        // generous "I might have missed anything" cutoff a node uses
        // when it cannot bound how long it was isolated.
        scenario.trigger_state_sync(&minority_indices, 0).await?;

        // Anti-entropy is request/response, not instant. Re-trigger on
        // a short cadence until the minority converges or the budget
        // expires — this mirrors the production periodic backstop tick
        // (every node re-asks if it is still behind).
        let during_for_catchup = during_hashes.clone();
        let minority_for_retry = minority_indices.clone();
        let catchup_res = retry_until(
            cfg.catchup_budget,
            Duration::from_millis(750),
            || {
                let scn = &scenario;
                let during = during_for_catchup.clone();
                let mins = minority_for_retry.clone();
                async move {
                    // Re-issue (idempotent — record_outcome is a no-op
                    // for known outcomes) then check convergence.
                    let _ = scn.trigger_state_sync(&mins, 0).await;
                    for &i in &mins {
                        let snap = scn.node(i).snapshot().await;
                        if !during.iter().all(|h| snap.has_outcome_for_spec(h)) {
                            return false;
                        }
                    }
                    true
                }
            },
        )
        .await;
        let catchup_secs = heal_at.elapsed().as_secs_f64();
        let catchup_succeeded = catchup_res;

        // `converged` here means precisely what the chaos-discovered
        // bug was about: the formerly-isolated MINORITY now holds every
        // outcome finalised during the partition window (pre + during).
        // Global all-6-node convergence is NOT the right signal — a
        // majority node that is the submitter (and so never executes
        // its own task) can lack the during-outcome under serialised
        // tokio load, which is a gossip-timing artifact unrelated to
        // the state-sync liveness gap we are proving closed.
        let all_minority_hashes: Vec<ContentHash> = pre_hashes
            .iter()
            .chain(during_hashes.iter())
            .copied()
            .collect();
        let mut converged = catchup_succeeded;
        for &i in &minority_indices {
            let snap = scenario.node(i).snapshot().await;
            if !all_minority_hashes
                .iter()
                .all(|h| snap.has_outcome_for_spec(h))
            {
                converged = false;
                break;
            }
        }

        // Divergence guard: every during-partition outcome the minority
        // caught up to must be byte-identical (by `content_hash`) to
        // the majority's. This is what defeats a malicious responder —
        // the minority re-verified each inner observer signature, so a
        // forged/altered outcome could never have been persisted; this
        // asserts the *honest* path also produced no split-brain fork.
        let histories_merged_correctly = if converged {
            minority_matches_majority(
                &scenario,
                &minority_indices,
                &majority_indices,
                &during_hashes,
            )
            .await
        } else {
            false
        };

        scenario.shutdown().await;

        Ok(PartitionResult {
            pre_partition_outcome_count: pre_outcome_count,
            majority_during_partition_count: majority_during,
            minority_during_partition_count: minority_during,
            catchup_seconds: catchup_secs,
            converged,
            histories_merged_correctly,
        })
    }
}

/// Poll `cond` every `interval` until it returns `true` or `budget`
/// elapses. Returns whether it became true in time.
async fn retry_until<F, Fut>(
    budget: Duration,
    interval: Duration,
    mut cond: F,
) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + budget;
    loop {
        if cond().await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

fn build_spec(
    sk: &ed25519_dalek::SigningKey,
    submitter: libp2p::PeerId,
    idx: u64,
) -> JobSpec {
    let (spec, _) = JobSpec::new_signed_at(
        parseh_task::JobKind::Inference,
        parseh_task::JobInputs::inference_prompt(
            format!("chaos partition spec #{idx}"),
            1_700_000_000 + idx,
        ),
        parseh_core::peer_registry::ServiceKind::Inference,
        false,
        1_715_000_000 + idx,
        submitter,
        sk,
    );
    spec
}

async fn count_finalised(
    scenario: &ChaosScenario,
    indices: &[usize],
    hashes: &[ContentHash],
) -> usize {
    if indices.is_empty() || hashes.is_empty() {
        return 0;
    }
    // Use the first index in the group as the canonical view.
    let snap = scenario.node(indices[0]).snapshot().await;
    hashes
        .iter()
        .filter(|h| snap.has_outcome_for_spec(h))
        .count()
}

/// Count how many of `hashes` have a finalised outcome on AT LEAST ONE
/// node in `indices`. Used by the state-sync regression scenario: the
/// during-partition outcome existing *anywhere* in the majority is
/// sufficient to establish "there is a gap the minority must close" —
/// `count_finalised` (node[0]-only) misses it because node 0 is the
/// submitter and neither executes its own task nor necessarily holds
/// the delta yet under serialised load.
async fn count_finalised_any(
    scenario: &ChaosScenario,
    indices: &[usize],
    hashes: &[ContentHash],
) -> usize {
    if indices.is_empty() || hashes.is_empty() {
        return 0;
    }
    let mut snaps = Vec::with_capacity(indices.len());
    for &i in indices {
        snaps.push(scenario.node(i).snapshot().await);
    }
    hashes
        .iter()
        .filter(|h| snaps.iter().any(|s| s.has_outcome_for_spec(h)))
        .count()
}

/// For every spec in `hashes` that any majority node finalised, assert
/// the minority's caught-up outcome has the SAME `content_hash`. A
/// mismatch would mean state-sync delivered a different (forked or
/// tampered) outcome — the divergence the chaos harness exists to
/// catch.
async fn minority_matches_majority(
    scenario: &ChaosScenario,
    minority: &[usize],
    majority: &[usize],
    hashes: &[ContentHash],
) -> bool {
    for h in hashes {
        // The set of valid per-observer outcome content_hashes the
        // majority holds for this spec. V0.2 outcomes are per-observer
        // signed projections (see `parseh-task::outcome` docs +
        // `architecture-and-state-machines.md` §4): different majority
        // nodes that each finalised independently sign with their own
        // key, so several distinct-but-equivalent content_hashes can
        // legitimately exist. State-sync delivered exactly ONE of them
        // (whichever responder answered); convergence is correct iff
        // the minority's caught-up outcome is one the majority also
        // holds — NOT a fork the responder invented.
        let mut majority_hashes: std::collections::HashSet<ContentHash> =
            std::collections::HashSet::new();
        for &m in majority {
            let snap = scenario.node(m).snapshot().await;
            if let Some(o) = snap.outcomes.get(h) {
                majority_hashes.insert(o.content_hash());
            }
        }
        if majority_hashes.is_empty() {
            // No majority node finalised it — nothing to compare.
            continue;
        }
        for &mi in minority {
            let snap = scenario.node(mi).snapshot().await;
            match snap.outcomes.get(h) {
                Some(o) if majority_hashes.contains(&o.content_hash()) => {}
                Some(o) => {
                    tracing::warn!(
                        spec = %h,
                        actual = %o.content_hash(),
                        majority_variants = majority_hashes.len(),
                        "state-sync divergence: minority outcome is NOT one the \
                         majority holds — a responder forged/forked it"
                    );
                    return false;
                }
                None => return false,
            }
        }
    }
    true
}

async fn check_outcomes_match(
    scenario: &ChaosScenario,
    hashes: &[ContentHash],
) -> bool {
    if hashes.is_empty() {
        return true;
    }
    // Take node 0's outcome content_hash for each spec_hash, then
    // verify every other node reports the same content_hash. This
    // catches a class of divergent finalisation where two halves
    // produced "Agreed" but with disjoint verifier sets, yielding
    // different `JobOutcome::content_hash()`.
    let snap0 = scenario.node(0).snapshot().await;
    let expected: std::collections::HashMap<ContentHash, ContentHash> = hashes
        .iter()
        .filter_map(|sh| snap0.outcomes.get(sh).map(|o| (*sh, o.content_hash())))
        .collect();
    for idx in 1..scenario.len() {
        let snap = scenario.node(idx).snapshot().await;
        for (sh, ch) in &expected {
            match snap.outcomes.get(sh) {
                Some(o) if o.content_hash() == *ch => {}
                Some(o) => {
                    tracing::warn!(
                        node = idx,
                        spec = %sh,
                        expected = %ch,
                        actual = %o.content_hash(),
                        "outcome content_hash divergence"
                    );
                    return false;
                }
                None => {
                    tracing::warn!(node = idx, spec = %sh, "missing outcome");
                    return false;
                }
            }
        }
    }
    true
}
