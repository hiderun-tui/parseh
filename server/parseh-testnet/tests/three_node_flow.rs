//! V0.2 minimum success condition — the 3-node acceptance test.
//!
//! Per the project notes:
//!
//! > If the following works on a local 3-node testnet:
//! > - one node submits a task
//! > - another node processes it
//! > - multiple nodes verify it
//! > - shared memory updates
//! > - Parseh reward is issued ONLY after consensus (V0.2: reputation only)
//! >
//! > …then the project becomes REAL infrastructure, not merely theory.
//!
//! This test asserts each of those bullets in turn against an
//! in-process 3-node [`parseh_testnet::Scenario`].
//!
//! ## Quorum reduction (documented test-only)
//!
//! The scenario uses a reduced **M=2/N=3** quorum (see
//! `parseh_testnet::scenario::reduced_quorum_for_test`). Production V0.2
//! uses **M=5/N=9** per the project notes §3.1; that is
//! unsatisfiable with 3 nodes. The minimum success condition stated in
//! the engineering summary is about the **flow primitive**, not the
//! parameter sweep — load testing at production parameters needs at
//! least 9 nodes and is post-V0.2 work.

use std::time::Duration;

use libp2p::PeerId;
use parseh_core::ServiceKind;
use parseh_task::{
    JobInputs, JobKind, JobOutcome, JobSpec, OutcomeVerdict,
};
use parseh_testnet::Scenario;

/// Build a deterministic, signed `JobSpec` for the test.
fn build_test_spec(
    signing_key: &ed25519_dalek::SigningKey,
    submitter: PeerId,
) -> JobSpec {
    let (spec, _hash) = JobSpec::new_signed_at(
        JobKind::Inference,
        JobInputs::inference_prompt(
            "PARSEH V0.2 minimum success condition · 3-node acceptance.",
            1_701_234_567,
        ),
        ServiceKind::Inference,
        false,
        1_715_000_000,
        submitter,
        signing_key,
    );
    spec
}

fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "parseh_testnet=info,parseh_verify=info".into()),
        )
        .with_test_writer()
        .try_init();
}

/// The acceptance test itself. Runs the full flow:
///
/// 1. Spawn 3 nodes.
/// 2. Node 0 submits a `JobSpec`. Spec propagates to nodes 1 and 2.
/// 3. Nodes 1 and 2 each execute (the first published `JobResult` wins).
/// 4. The remaining two nodes verify; M=2 verifications close the
///    quorum.
/// 5. A signed `JobOutcome` propagates as a `StateDelta`; each node
///    persists it.
/// 6. A reputation delta carrying `+REPUTATION_AWARD_EXECUTOR` for the
///    executor propagates and every node applies it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_acceptance() {
    init_logging();

    let start = std::time::Instant::now();

    // 1. Spawn 3 nodes.
    let scenario = Scenario::new(3).await.expect("spawn 3-node scenario");

    // 2. Build + submit a spec from node 0.
    let submitter = scenario.node(0);
    let spec = build_test_spec(&submitter.signing_key, submitter.peer_id);
    let spec_hash = spec.content_hash();
    let submit_start = std::time::Instant::now();
    scenario
        .submit_from(0, spec.clone())
        .await
        .expect("submit spec");

    // 3. Wait for the spec to propagate to nodes 1 and 2.
    scenario
        .await_state_predicate(
            move |snap| snap.has_task(&spec_hash),
            Duration::from_secs(10),
            "spec propagation",
        )
        .await
        .expect("all 3 nodes should observe the spec");
    let spec_propagated_ms = submit_start.elapsed().as_millis();
    eprintln!(
        "[latency] spec_propagation = {} ms (all 3 nodes observe JobSpec)",
        spec_propagated_ms
    );

    assert!(
        scenario.dump_state(1).await.has_task(&spec_hash),
        "node 1 missing spec"
    );
    assert!(
        scenario.dump_state(2).await.has_task(&spec_hash),
        "node 2 missing spec"
    );

    // 4. Await outcome — covers result emission, verification, and
    //    quorum finalisation. With reduced M=2/N=3 + t_min=200ms +
    //    200ms gossipsub heartbeats, this typically finalises in well
    //    under 5 seconds. We give it 60 seconds as a generous ceiling
    //    so debug-mode CI doesn't flake.
    let outcome: JobOutcome = scenario
        .await_outcome(spec_hash, Duration::from_secs(60))
        .await
        .expect("M-of-N quorum should finalise an outcome");
    let outcome_ms = submit_start.elapsed().as_millis();
    eprintln!(
        "[latency] outcome_finalised = {} ms (first node sees finalised outcome)",
        outcome_ms
    );

    // Quick verdict sanity check.
    let executor_peer = match &outcome.verdict {
        OutcomeVerdict::Valid {
            agreements,
            disagreements,
            abstentions,
            reputation_weighted,
        } => {
            assert!(
                *agreements >= 2,
                "expected ≥2 agreements (M=2), got {agreements}"
            );
            assert_eq!(
                *disagreements, 0,
                "expected 0 disagreements in deterministic harness"
            );
            // Abstentions may legitimately be 0 in the M=2 case.
            let _ = abstentions;
            assert!(
                *reputation_weighted >= 0.6,
                "rep_weighted should clear the 0.6 threshold, got {reputation_weighted}"
            );
            // Resolve executor from the outcome's referenced result hash —
            // we look it up in any node's local shared state via its
            // observed result. Simpler path: every node persisted the
            // result, and the outcome's `result_hash` is unique per
            // executor. The executor PeerId is the one that *isn't*
            // the submitter (peer 0) and *isn't* the consensus
            // observer for the outcome (any of the verifiers).
            //
            // We pull the executor PeerId out of any node's
            // shared-state by reading back the result the outcome
            // references. For test simplicity we just iterate the
            // three nodes and find whichever one is NOT the submitter
            // and IS not the observer — that's the executor.
            outcome.observed_by // placeholder, fixed below
        }
        OutcomeVerdict::Disputed { disputers } => panic!(
            "deterministic harness should never dispute; disputers = {:?}",
            disputers
        ),
        OutcomeVerdict::Indeterminate => panic!(
            "quorum did not finalise inside the {:?} window",
            Duration::from_secs(60)
        ),
    };

    // 5. Every node should have the outcome.
    let outcome_prop_start = std::time::Instant::now();
    let spec_hash_for_pred = spec_hash;
    scenario
        .await_state_predicate(
            move |snap| snap.has_outcome_for_spec(&spec_hash_for_pred),
            Duration::from_secs(30),
            "outcome propagation",
        )
        .await
        .expect("outcome should propagate to all 3 nodes");
    eprintln!(
        "[latency] outcome_propagated = {} ms (all 3 nodes have outcome)",
        outcome_prop_start.elapsed().as_millis()
    );
    for i in 0..3 {
        let snap = scenario.dump_state(i).await;
        assert!(
            snap.has_outcome_for_spec(&spec_hash),
            "node {i} missing outcome"
        );
    }

    // 6. Identify the executor — the one peer that is neither the
    //    submitter nor self. We look at the node's local result row
    //    via a snapshot. The simplest cross-check: ask every node to
    //    expose which peer it logged a reputation increment for; that
    //    peer is the executor.
    let mut executor_peer_id: Option<PeerId> = None;
    let _ = executor_peer; // placeholder from match above
    for i in 0..3 {
        let snap = scenario.dump_state(i).await;
        if let Some((peer, rep)) = snap
            .reputation
            .iter()
            .max_by_key(|(_, v)| **v)
        {
            if *rep > 0 {
                executor_peer_id = Some(*peer);
                break;
            }
        }
    }
    let executor_peer_id = executor_peer_id
        .expect("at least one node should have applied a positive reputation delta");
    let submitter_peer = scenario.node(0).peer_id;
    assert_ne!(
        executor_peer_id, submitter_peer,
        "Rule 3a: submitter must not execute its own task"
    );

    // 7. Reputation propagation — wait until every node has applied
    //    the executor's +10. (The publisher of the reputation delta
    //    applies it locally synchronously; the other two apply on
    //    receive.)
    let rep_prop_start = std::time::Instant::now();
    let exec_for_pred = executor_peer_id;
    scenario
        .await_state_predicate(
            move |snap| snap.reputation_of(exec_for_pred) >= 10,
            Duration::from_secs(30),
            "reputation propagation",
        )
        .await
        .expect("executor reward should propagate to all 3 nodes");
    eprintln!(
        "[latency] reputation_propagated = {} ms (all 3 nodes log +10 for executor)",
        rep_prop_start.elapsed().as_millis()
    );
    for i in 0..3 {
        let snap = scenario.dump_state(i).await;
        let rep = snap.reputation_of(executor_peer_id);
        assert!(
            rep >= 10,
            "node {i} executor rep too low: {rep} (expected ≥ 10)"
        );
    }

    let total_ms = start.elapsed().as_millis();
    eprintln!(
        "[latency] total_test_runtime = {} ms (scenario setup → all assertions pass)",
        total_ms
    );

    scenario.shutdown().await;
}
