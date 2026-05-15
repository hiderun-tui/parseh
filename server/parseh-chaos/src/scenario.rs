//! `scenario` — Chaos-aware in-process miner.
//!
//! Spawns N libp2p nodes over `MemoryTransport`, identical to
//! `parseh-integration-tests::mesh` but with two test-only knobs:
//!
//! 1. **Partition group.** Each node carries an `Arc<AtomicU8>` group
//!    tag. Inbound messages whose publisher's group does not match the
//!    receiver's are dropped at the dispatch boundary — emulating a
//!    network partition without changing libp2p's transport.
//! 2. **Malicious mode.** Each node optionally carries a
//!    [`crate::MaliciousMode`] that replaces the honest deterministic
//!    verifier with a misbehaving stand-in.
//!
//! Everything else mirrors production V0.2.5: signed envelopes, M-of-N
//! quorum, periodic finalise tick, state-deltas gossip, reputation
//! application.
//!
//! ## Why not call into the integration-tests crate?
//!
//! `parseh-integration-tests::mesh` is purpose-built for the happy
//! path; pulling adversarial knobs into it would compromise its
//! readability. Instead we copy the load-bearing scaffolding here and
//! pin the malicious surface inside this crate — keeps the cultural
//! rule (this crate = adversarial testing) intact.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::SigningKey;
use futures::StreamExt;
use libp2p::core::transport::{MemoryTransport, Transport};
use libp2p::{
    core::upgrade,
    gossipsub,
    identity::Keypair,
    noise, request_response,
    swarm::{NetworkBehaviour, Swarm, SwarmEvent},
    yamux, Multiaddr, PeerId, StreamProtocol,
};
use parking_lot::Mutex;
use parseh_core::peer_registry::{
    encode_advertisement, CapabilityAdvertisement, InferenceCapability, PeerIdentity, PeerRegistry,
    ReadinessState, ServiceKind, CAPS_WIRE_VERSION,
};
use parseh_shared_state::{
    sign_delta, DeltaKind, KeyMaterial, KeySource, OpenOptions, SharedState, StateDelta,
};
use parseh_task::{
    ContentHash, JobOutcome, JobResult, JobSpec, JobVerification, OutcomeVerdict, ResultMeta,
    StateSyncRequest, StateSyncResponse, VerifierMethod, VerifierVerdict,
    STATE_SYNC_HARD_CEILING,
};
use parseh_verify::{
    DeterministicMethod, LocalExecutor, Quorum, QuorumConfig, VerifierMethodImpl, VerifyError,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, trace, warn};

use crate::MaliciousMode;

// ─── topic + tag constants ─────────────────────────────────────────────

/// `parseh.caps.v1` — capability advertisements.
pub const TOPIC_CAPS: &str = "parseh.caps.v1";
/// `parseh.tasks.v1` — `JobSpec` envelopes.
pub const TOPIC_TASKS: &str = "parseh.tasks.v1";
/// `parseh.verify.v1` — `JobResult` + `JobVerification` envelopes.
pub const TOPIC_VERIFY: &str = "parseh.verify.v1";
/// `parseh.state-deltas.v1` — `StateDelta` envelopes.
pub const TOPIC_STATE_DELTAS: &str = parseh_shared_state::GOSSIPSUB_TOPIC;

/// Tag byte on `parseh.verify.v1` for a `JobResult`.
pub const TAG_JOB_RESULT: u8 = 0x02;
/// Tag byte on `parseh.verify.v1` for a `JobVerification`.
pub const TAG_JOB_VERIFICATION: u8 = 0x03;

/// `/parseh/state-sync/1.0.0` — anti-entropy pull. Mirrors the
/// production miner's protocol so the chaos harness can prove the
/// partition-recovery gap is closed. (Identical wire types from
/// `parseh-task`; the dispatch/responder/apply logic is copied here for
/// the same reason the rest of the node loop is — the cultural rule
/// keeps adversarial scaffolding in this crate.)
pub const PARSEH_STATE_SYNC_PROTOCOL: &str = "/parseh/state-sync/1.0.0";

/// Reputation increment for the executor on `Agreed`.
pub const REPUTATION_AWARD_EXECUTOR: i32 = 10;
/// Reputation increment for each agreeing verifier.
pub const REPUTATION_AWARD_VERIFIER: i32 = 5;
/// Reputation penalty for verifiers who disagree with the consensus
/// (i.e. cast a `Disagreed` when the quorum closes `Agreed`, or vice
/// versa). Matches the V0.2 placeholder in `verifier-economics.md` §3.4.
pub const REPUTATION_PENALTY_FALSE_DISPUTE: i32 = -5;

// ─── partition group ───────────────────────────────────────────────────

/// Default partition group — every node starts in group 0. Partition
/// flips a subset to group 1 (or higher).
pub const DEFAULT_GROUP: u8 = 0;

/// Shared, in-process map `PeerId → group tag`. Nodes read this during
/// message dispatch; if the inbound publisher's group differs from the
/// local group, the message is dropped. This emulates a network
/// partition without modifying libp2p internals.
#[derive(Clone, Default)]
pub struct PartitionTable {
    inner: Arc<Mutex<HashMap<PeerId, Arc<AtomicU8>>>>,
}

impl PartitionTable {
    /// Construct an empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a node's group tag. Nodes start in [`DEFAULT_GROUP`].
    pub fn register(&self, peer: PeerId) -> Arc<AtomicU8> {
        let mut g = self.inner.lock();
        g.entry(peer)
            .or_insert_with(|| Arc::new(AtomicU8::new(DEFAULT_GROUP)))
            .clone()
    }

    /// Move a node into a specific group. Lookup creates the entry if
    /// the peer was not yet registered (defensive, should not happen).
    pub fn set_group(&self, peer: PeerId, group: u8) {
        let g = {
            let mut tbl = self.inner.lock();
            tbl.entry(peer)
                .or_insert_with(|| Arc::new(AtomicU8::new(DEFAULT_GROUP)))
                .clone()
        };
        g.store(group, Ordering::SeqCst);
    }

    /// Read a peer's current group.
    pub fn group_of(&self, peer: &PeerId) -> u8 {
        self.inner
            .lock()
            .get(peer)
            .map(|a| a.load(Ordering::SeqCst))
            .unwrap_or(DEFAULT_GROUP)
    }
}

// ─── public types ──────────────────────────────────────────────────────

/// Snapshot of one chaos-node's state.
#[derive(Debug, Clone, Default)]
pub struct NodeSnapshot {
    /// Tasks observed.
    pub task_hashes: HashSet<ContentHash>,
    /// Outcomes per spec_hash.
    pub outcomes: HashMap<ContentHash, JobOutcome>,
    /// Reputation per peer (this node's local accumulator).
    pub reputation: HashMap<PeerId, i64>,
    /// Identities known to this node.
    pub known_identities: usize,
    /// Current group.
    pub group: u8,
    /// Verifications observed locally, by result_hash.
    pub verifications_by_result: HashMap<ContentHash, u32>,
}

impl NodeSnapshot {
    /// `true` iff a finalised outcome exists for `spec_hash`.
    pub fn has_outcome_for_spec(&self, spec_hash: &ContentHash) -> bool {
        self.outcomes.contains_key(spec_hash)
    }
    /// Reputation tally for `peer`. `0` when unseen.
    pub fn reputation_of(&self, peer: PeerId) -> i64 {
        self.reputation.get(&peer).copied().unwrap_or(0)
    }
}

/// Command channel between the scenario driver and a node task.
enum NodeCmd {
    Submit(JobSpec, oneshot::Sender<Result<()>>),
    Snapshot(oneshot::Sender<NodeSnapshot>),
    Dial(Multiaddr, oneshot::Sender<Result<()>>),
    ListenAddr(oneshot::Sender<Option<Multiaddr>>),
    ConnectedCount(oneshot::Sender<usize>),
    SetGroup(u8, oneshot::Sender<()>),
    InjectCorruptedDelta(StateDelta, oneshot::Sender<Result<()>>),
    /// Issue a `/parseh/state-sync/1.0.0` request to every currently-
    /// connected peer asking for outcomes finalised at/after
    /// `since_unix`. Mirrors the production miner's post-isolation
    /// trigger (the chaos partition is dispatch-layer, so there is no
    /// real `ConnectionEstablished` to hang the trigger off — the
    /// scenario driver invokes it explicitly after `heal()`).
    RequestStateSync(u64, oneshot::Sender<Result<usize>>),
    Shutdown(oneshot::Sender<()>),
}

/// Handle to one in-process chaos node.
#[derive(Clone)]
pub struct ChaosNode {
    /// libp2p PeerId.
    pub peer_id: PeerId,
    /// ed25519 signing key (test-only).
    pub signing_key: SigningKey,
    /// Malicious mode (`None` = honest).
    pub malicious: Option<MaliciousMode>,
    cmd_tx: mpsc::Sender<NodeCmd>,
    #[allow(dead_code)]
    tempdir: Arc<TempDir>,
}

impl ChaosNode {
    /// Submit a signed `JobSpec` from this node.
    pub async fn submit(&self, spec: JobSpec) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(NodeCmd::Submit(spec, tx))
            .await
            .map_err(|_| anyhow!("node channel closed"))?;
        rx.await.map_err(|_| anyhow!("node dropped reply"))?
    }

    /// Snapshot the node.
    pub async fn snapshot(&self) -> NodeSnapshot {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(NodeCmd::Snapshot(tx)).await.is_err() {
            return NodeSnapshot::default();
        }
        rx.await.unwrap_or_default()
    }

    /// Move this node into `group`.
    pub async fn set_group(&self, group: u8) {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(NodeCmd::SetGroup(group, tx)).await.is_err() {
            return;
        }
        let _ = rx.await;
    }

    /// Inject a tampered/forged `StateDelta` over the wire. Used by
    /// [`crate::corruption`] to assert peers reject it.
    pub async fn inject_corrupted_delta(&self, delta: StateDelta) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(NodeCmd::InjectCorruptedDelta(delta, tx))
            .await
            .map_err(|_| anyhow!("node channel closed"))?;
        rx.await.map_err(|_| anyhow!("node dropped reply"))?
    }

    /// Trigger a state-sync round from this node: ask every connected
    /// peer for outcomes finalised at/after `since_unix`. Returns the
    /// number of peers the request was sent to.
    pub async fn request_state_sync(&self, since_unix: u64) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(NodeCmd::RequestStateSync(since_unix, tx))
            .await
            .map_err(|_| anyhow!("node channel closed"))?;
        rx.await.map_err(|_| anyhow!("node dropped reply"))?
    }

    async fn dial(&self, addr: Multiaddr) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(NodeCmd::Dial(addr, tx))
            .await
            .map_err(|_| anyhow!("node channel closed"))?;
        rx.await.map_err(|_| anyhow!("node dropped reply"))?
    }

    async fn listen_addr(&self) -> Option<Multiaddr> {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(NodeCmd::ListenAddr(tx)).await.is_err() {
            return None;
        }
        rx.await.unwrap_or(None)
    }

    async fn connected_count(&self) -> usize {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(NodeCmd::ConnectedCount(tx)).await.is_err() {
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    /// Cleanly shut the node down.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(NodeCmd::Shutdown(tx)).await.is_err() {
            return;
        }
        let _ = rx.await;
    }
}

// ─── ChaosScenario ─────────────────────────────────────────────────────

/// Top-level handle to a fully-connected, partition-aware N-node mesh.
pub struct ChaosScenario {
    nodes: Vec<ChaosNode>,
    partition: PartitionTable,
}

impl ChaosScenario {
    /// Construct a chaos mesh with `n` honest nodes (default quorum).
    pub async fn new(n: usize) -> Result<Self> {
        Self::with_quorum_and_modes(n, reduced_quorum_for_test(), vec![None; n]).await
    }

    /// Construct a chaos mesh with mixed honest + malicious nodes. The
    /// `modes` slice has one entry per node (`None` = honest).
    pub async fn with_modes(n: usize, modes: Vec<Option<MaliciousMode>>) -> Result<Self> {
        Self::with_quorum_and_modes(n, reduced_quorum_for_test(), modes).await
    }

    /// Full construction with custom quorum + per-node modes.
    pub async fn with_quorum_and_modes(
        n: usize,
        quorum: QuorumConfig,
        modes: Vec<Option<MaliciousMode>>,
    ) -> Result<Self> {
        if n < 2 {
            return Err(anyhow!("ChaosScenario requires ≥2 nodes (got {n})"));
        }
        if modes.len() != n {
            return Err(anyhow!(
                "modes len {} != node count {}",
                modes.len(),
                n
            ));
        }
        let partition = PartitionTable::new();
        let mut nodes = Vec::with_capacity(n);
        for mode in modes.iter().take(n) {
            nodes.push(spawn_node(quorum, *mode, partition.clone()).await?);
        }
        let addrs = collect_listen_addrs(&nodes, Duration::from_secs(5)).await?;
        for (i, node) in nodes.iter().enumerate() {
            for (j, addr) in addrs.iter().enumerate() {
                if i == j {
                    continue;
                }
                node.dial(addr.clone()).await.context("dial peer")?;
            }
        }
        // Wait for every node to see (n-1) peers, with mesh formation
        // budget of 10s. In a chaos mesh on shared runtime this can be
        // slower than `parseh-integration-tests::Mesh`, so we are more
        // patient here.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut ok = true;
            for node in &nodes {
                if node.connected_count().await < n - 1 {
                    ok = false;
                    break;
                }
            }
            if ok {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err(anyhow!(
                    "chaos mesh did not form (each node needs {} peers)",
                    n - 1
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // GRAFT heartbeats + caps round-trip.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        info!(node_count = n, "chaos mesh ready");
        Ok(Self { nodes, partition })
    }

    /// Borrow a node by index.
    pub fn node(&self, idx: usize) -> &ChaosNode {
        &self.nodes[idx]
    }

    /// Number of nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// `true` iff there are no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Submit a spec from node `idx`.
    pub async fn submit_from(&self, idx: usize, spec: JobSpec) -> Result<()> {
        self.nodes[idx].submit(spec).await
    }

    /// Borrow the shared partition table.
    pub fn partition_table(&self) -> &PartitionTable {
        &self.partition
    }

    /// Move a contiguous prefix `[start_idx, start_idx+count)` into
    /// `group`. The remaining nodes stay in `DEFAULT_GROUP`.
    pub async fn partition_into(&self, start_idx: usize, count: usize, group: u8) {
        for i in start_idx..start_idx + count {
            self.partition
                .set_group(self.nodes[i].peer_id, group);
            self.nodes[i].set_group(group).await;
        }
    }

    /// Heal a partition — move every node back to [`DEFAULT_GROUP`].
    pub async fn heal_partition(&self) {
        for node in &self.nodes {
            self.partition.set_group(node.peer_id, DEFAULT_GROUP);
            node.set_group(DEFAULT_GROUP).await;
        }
    }

    /// Trigger a `/parseh/state-sync/1.0.0` round on every node in
    /// `indices`, asking for outcomes finalised at/after `since_unix`.
    /// This is what a reconnecting peer does in production on
    /// `ConnectionEstablished` after an isolation window; here the
    /// scenario invokes it explicitly post-heal (the chaos partition is
    /// dispatch-layer, so libp2p never sees a reconnect event).
    pub async fn trigger_state_sync(
        &self,
        indices: &[usize],
        since_unix: u64,
    ) -> Result<()> {
        for &i in indices {
            let sent = self.nodes[i].request_state_sync(since_unix).await?;
            tracing::info!(node = i, peers = sent, "state-sync round issued");
        }
        Ok(())
    }

    /// Block until every node's snapshot satisfies `predicate`.
    pub async fn await_state_predicate<F>(
        &self,
        predicate: F,
        timeout: Duration,
        reason: &str,
    ) -> Result<()>
    where
        F: Fn(&NodeSnapshot) -> bool,
    {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let mut all_ok = true;
            for node in &self.nodes {
                let snap = node.snapshot().await;
                if !predicate(&snap) {
                    all_ok = false;
                    break;
                }
            }
            if all_ok {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                return Err(anyhow!("timed out: {reason}"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Block until every node in `indices` satisfies `predicate`.
    pub async fn await_subset_predicate<F>(
        &self,
        indices: &[usize],
        predicate: F,
        timeout: Duration,
        reason: &str,
    ) -> Result<()>
    where
        F: Fn(&NodeSnapshot) -> bool,
    {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let mut all_ok = true;
            for &i in indices {
                let snap = self.nodes[i].snapshot().await;
                if !predicate(&snap) {
                    all_ok = false;
                    break;
                }
            }
            if all_ok {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                return Err(anyhow!("timed out (subset): {reason}"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Shut every node down cleanly.
    pub async fn shutdown(self) {
        for node in &self.nodes {
            node.shutdown().await;
        }
    }
}

/// Test-only reduced quorum (M=2/N=3, t_min = 200 ms). Same shape as
/// `parseh-integration-tests::mesh::reduced_quorum_for_test`.
pub fn reduced_quorum_for_test() -> QuorumConfig {
    QuorumConfig {
        m: 2,
        n: 3,
        t_min: Duration::from_millis(200),
        t_max: Duration::from_secs(30),
        rep_weighted_threshold: 0.6,
    }
}

/// A larger quorum (M=5/N=9) for malicious-verifier scenarios that need
/// to exercise the realistic 9-node spread. `t_max` is set wide
/// (60s) so the test harness does not bail to `Disputed` under tokio
/// scheduler pressure when multiple 9-node meshes share a runtime.
pub fn standard_quorum_for_test() -> QuorumConfig {
    QuorumConfig {
        m: 5,
        n: 9,
        t_min: Duration::from_millis(200),
        t_max: Duration::from_secs(60),
        rep_weighted_threshold: 0.6,
    }
}

// ─── node-task internals ───────────────────────────────────────────────

#[derive(NetworkBehaviour)]
struct ChaosBehaviour {
    gossipsub: gossipsub::Behaviour,
    /// `/parseh/state-sync/1.0.0` — the anti-entropy pull that closes
    /// the partition-recovery gap. NOTE: request-response is NOT
    /// dropped by the partition gate (the gate only filters gossipsub
    /// by `propagation_source`). That is realistic — the harness only
    /// triggers state-sync AFTER `heal()`, when connectivity is back.
    state_sync:
        request_response::cbor::Behaviour<StateSyncRequest, StateSyncResponse>,
}

struct TaskState {
    spec: JobSpec,
    observed_result: Option<JobResult>,
    quorum: Option<Quorum>,
    finalised: bool,
    self_executed: bool,
    self_verified: bool,
}

struct NodeLoop {
    peer_id: PeerId,
    signing_key: SigningKey,
    shared: SharedState,
    quorum_config: QuorumConfig,
    registry: PeerRegistry,
    tasks: HashMap<ContentHash, TaskState>,
    reputation_local: HashMap<PeerId, i64>,
    applied_rep_keys: HashSet<(PeerId, Option<ContentHash>, String)>,
    listen_addr: Option<Multiaddr>,
    readiness: ReadinessState,
    malicious: Option<MaliciousMode>,
    partition: PartitionTable,
    group: Arc<AtomicU8>,
    verifications_by_result: HashMap<ContentHash, u32>,
    /// Spec-hashes whose finalised outcome arrived via
    /// `/parseh/state-sync/1.0.0` (not via the normal gossip→quorum
    /// path). Surfaced in [`NodeSnapshot::outcomes`] so a post-heal
    /// catch-up is observable exactly like a gossip-delivered one.
    synced_outcomes: HashSet<ContentHash>,
}

impl NodeLoop {
    fn record_spec(&mut self, spec: &JobSpec) -> Result<()> {
        self.shared
            .record_spec(spec)
            .map_err(|e| anyhow!("record_spec: {e}"))?;
        let hash = spec.content_hash();
        self.tasks.entry(hash).or_insert(TaskState {
            spec: spec.clone(),
            observed_result: None,
            quorum: None,
            finalised: false,
            self_executed: false,
            self_verified: false,
        });
        Ok(())
    }

    fn record_result(&mut self, result: &JobResult) -> Result<()> {
        // Same FK race shape as `record_verification`: if the result
        // arrives before the spec was recorded (gossipsub ordering),
        // drop it and let the redelivery retry close the gap.
        if let Err(e) = self.shared.record_result(result) {
            let msg = format!("{e}");
            if msg.contains("FOREIGN KEY") || msg.contains("foreign-key") {
                debug!(error = %msg, "early result · dropping (gossipsub will redeliver)");
                return Ok(());
            }
            return Err(anyhow!("record_result: {e}"));
        }
        if let Some(t) = self.tasks.get_mut(&result.spec_hash) {
            if t.observed_result.is_none() {
                t.observed_result = Some(result.clone());
                t.quorum = Some(Quorum::new(
                    self.quorum_config,
                    result.spec_hash,
                    result.content_hash(),
                    SystemTime::now(),
                ));
            }
        }
        Ok(())
    }

    fn record_verification(&mut self, v: &JobVerification) -> Result<()> {
        // FK race: when a JobVerification arrives via gossipsub BEFORE
        // the corresponding JobResult was recorded locally, the
        // `verifications.result_hash` FK is unsatisfied. The
        // production miner has the same race; the documented mitigation
        // is to retry on the 100 ms tick once the result arrives. Here
        // we drop the early verification — gossipsub will redeliver
        // via IHAVE within a heartbeat or two.
        if let Err(e) = self.shared.record_verification(v) {
            let msg = format!("{e}");
            if msg.contains("FOREIGN KEY") || msg.contains("foreign-key") {
                debug!(error = %msg, "early verification · dropping (gossipsub will redeliver)");
                return Ok(());
            }
            return Err(anyhow!("record_verification: {e}"));
        }
        *self
            .verifications_by_result
            .entry(v.result_hash)
            .or_insert(0) += 1;
        let result_hash = v.result_hash;
        let mut to_finalise: Option<ContentHash> = None;
        for (sh, task) in self.tasks.iter_mut() {
            let Some(observed) = &task.observed_result else {
                continue;
            };
            if observed.content_hash() != result_hash {
                continue;
            }
            let Some(quorum) = task.quorum.as_mut() else {
                continue;
            };
            let key = match self.registry.verifying_key(&v.verifier) {
                Some(k) => k,
                None => {
                    debug!(verifier = %v.verifier, "no verifier pubkey · dropping");
                    return Ok(());
                }
            };
            match quorum.add_verification(v.clone(), 100, &key) {
                Ok(()) => trace!(%sh, "added verification"),
                Err(VerifyError::Internal(msg))
                    if msg.contains("duplicate") || msg.contains("result_hash") =>
                {
                    debug!(error = %msg, "ignoring verification");
                    return Ok(());
                }
                Err(e) => return Err(anyhow!("add_verification: {e}")),
            }
            if !task.finalised {
                to_finalise = Some(*sh);
            }
            break;
        }
        if let Some(sh) = to_finalise {
            self.try_finalise_quorum(sh)?;
        }
        Ok(())
    }

    fn try_finalise_quorum(&mut self, spec_hash: ContentHash) -> Result<()> {
        let Some(task) = self.tasks.get_mut(&spec_hash) else {
            return Ok(());
        };
        if task.finalised {
            return Ok(());
        }
        let Some(quorum) = task.quorum.as_ref() else {
            return Ok(());
        };
        let Some(finalised) =
            quorum.try_finalise(SystemTime::now(), self.peer_id, &self.signing_key)
        else {
            return Ok(());
        };
        task.finalised = true;
        info!(peer = %self.peer_id, decision = ?finalised.decision, "quorum finalised");
        self.shared
            .record_outcome(&finalised.outcome)
            .map_err(|e| anyhow!("record_outcome: {e}"))?;
        Ok(())
    }

    fn record_outcome(&mut self, outcome: &JobOutcome) -> Result<()> {
        self.shared
            .record_outcome(outcome)
            .map_err(|e| anyhow!("record_outcome: {e}"))?;
        Ok(())
    }

    fn apply_reputation(
        &mut self,
        peer: PeerId,
        delta: i32,
        reason: &str,
        related_hash: Option<ContentHash>,
    ) -> Result<()> {
        let key = (peer, related_hash, reason.to_string());
        if self.applied_rep_keys.contains(&key) {
            return Ok(());
        }
        self.shared
            .apply_reputation_delta(peer, delta, reason, related_hash)
            .map_err(|e| anyhow!("apply_reputation_delta: {e}"))?;
        self.applied_rep_keys.insert(key);
        *self.reputation_local.entry(peer).or_insert(0) += delta as i64;
        Ok(())
    }

    fn snapshot(&self) -> NodeSnapshot {
        let mut outcomes: HashMap<ContentHash, JobOutcome> = self
            .tasks
            .iter()
            .filter_map(|(hash, t)| {
                if !t.finalised {
                    return None;
                }
                self.shared
                    .outcome_for_spec(hash)
                    .ok()
                    .flatten()
                    .map(|o| (*hash, o))
            })
            .collect();
        // Outcomes that arrived via `/parseh/state-sync/1.0.0` —
        // surface them too, otherwise a post-heal catch-up that did not
        // go through the local quorum path would be invisible to the
        // convergence assertion.
        for sh in &self.synced_outcomes {
            if let Ok(Some(o)) = self.shared.outcome_for_spec(sh) {
                outcomes.entry(*sh).or_insert(o);
            }
        }
        NodeSnapshot {
            task_hashes: self
                .tasks
                .keys()
                .copied()
                .chain(self.synced_outcomes.iter().copied())
                .collect(),
            outcomes,
            reputation: self.reputation_local.clone(),
            known_identities: self.registry.identity_count(),
            group: self.group.load(Ordering::SeqCst),
            verifications_by_result: self.verifications_by_result.clone(),
        }
    }
}

/// Deterministic local executor for the harness — SHA-256(prompt + seed).
struct ChaosExecutor;
impl LocalExecutor for ChaosExecutor {
    fn execute(&self, spec: &JobSpec) -> std::result::Result<Vec<u8>, VerifyError> {
        let prompt = spec.inputs.prompt_text.as_deref().unwrap_or("");
        let seed = spec.inputs.seed.unwrap_or(0);
        let mut h = Sha256::new();
        h.update(prompt.as_bytes());
        h.update(seed.to_le_bytes());
        Ok(h.finalize().to_vec())
    }
}

/// Deterministic lowest-PeerId executor self-selection. Identical to
/// `parseh-integration-tests::mesh::pick_executor` semantics.
fn pick_executor(spec: &JobSpec, registry: &PeerRegistry, local: &PeerId) -> Option<PeerId> {
    let mut eligible: Vec<PeerId> = registry
        .ready_peers_for_service(spec.service.clone())
        .into_iter()
        .map(|p| p.peer_id)
        .filter(|p| *p != spec.submitter)
        .collect();
    if !eligible.contains(local) && *local != spec.submitter {
        eligible.push(*local);
    }
    eligible.sort_by_key(|p| p.to_bytes());
    eligible.into_iter().next()
}

async fn spawn_node(
    quorum_config: QuorumConfig,
    malicious: Option<MaliciousMode>,
    partition: PartitionTable,
) -> Result<ChaosNode> {
    let (signing_key, libp2p_kp, peer_id) = fresh_identity();
    let group = partition.register(peer_id);

    let tempdir = Arc::new(TempDir::new().context("create tempdir")?);
    let db_path = tempdir.path().join("shared-state.sqlite3");
    let key = KeyMaterial::from_source(KeySource::Raw([0xCE; 32])).context("derive key")?;
    let shared =
        SharedState::open(OpenOptions::create(db_path, key)).context("open shared")?;

    let (cmd_tx, cmd_rx) = mpsc::channel::<NodeCmd>(64);

    let registry = PeerRegistry::new();
    registry.record_identity(PeerIdentity {
        peer_id,
        verifying_key: signing_key.verifying_key(),
        reachable_addrs: vec![],
        first_seen: now_unix(),
        last_seen: now_unix(),
        readiness: ReadinessState::Initialised,
    });

    let signing_key_inner = signing_key.clone();
    let partition_inner = partition.clone();
    let group_inner = group.clone();
    tokio::spawn(async move {
        if let Err(e) = run_node_loop(
            peer_id,
            libp2p_kp,
            signing_key_inner,
            shared,
            quorum_config,
            registry,
            cmd_rx,
            malicious,
            partition_inner,
            group_inner,
        )
        .await
        {
            warn!(error = %e, %peer_id, "chaos node loop exited with error");
        }
    });

    Ok(ChaosNode {
        peer_id,
        signing_key,
        malicious,
        cmd_tx,
        tempdir,
    })
}

fn fresh_identity() -> (SigningKey, Keypair, PeerId) {
    let mut seed = [0u8; 32];
    rand::Rng::fill(&mut rand::thread_rng(), &mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let mut seed_clone = seed;
    let kp = Keypair::ed25519_from_bytes(&mut seed_clone).expect("32-byte seed valid");
    let peer_id = PeerId::from(kp.public());
    for b in seed.iter_mut() {
        *b = 0;
    }
    for b in seed_clone.iter_mut() {
        *b = 0;
    }
    (signing_key, kp, peer_id)
}

fn build_swarm(kp: Keypair) -> Result<Swarm<ChaosBehaviour>> {
    let transport = MemoryTransport::default()
        .upgrade(upgrade::Version::V1)
        .authenticate(noise::Config::new(&kp).map_err(|e| anyhow!("noise: {e}"))?)
        .multiplex(yamux::Config::default())
        .boxed();
    let cfg = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_millis(200))
        .heartbeat_initial_delay(Duration::from_millis(50))
        .mesh_n(3)
        .mesh_n_low(2)
        .mesh_n_high(4)
        .mesh_outbound_min(1)
        .validation_mode(gossipsub::ValidationMode::Strict)
        .allow_self_origin(true)
        .build()
        .map_err(|e| anyhow!("gossipsub config: {e}"))?;
    let gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(kp.clone()),
        cfg,
    )
    .map_err(|e| anyhow!("gossipsub: {e}"))?;
    let state_sync =
        request_response::cbor::Behaviour::<StateSyncRequest, StateSyncResponse>::new(
            [(
                StreamProtocol::new(PARSEH_STATE_SYNC_PROTOCOL),
                request_response::ProtocolSupport::Full,
            )],
            request_response::Config::default(),
        );
    let behaviour = ChaosBehaviour {
        gossipsub,
        state_sync,
    };
    let peer_id = PeerId::from(kp.public());
    let swarm = Swarm::new(
        transport,
        behaviour,
        peer_id,
        libp2p::swarm::Config::with_tokio_executor()
            .with_idle_connection_timeout(Duration::from_secs(60)),
    );
    Ok(swarm)
}

#[allow(clippy::too_many_arguments)]
async fn run_node_loop(
    peer_id: PeerId,
    libp2p_keypair: Keypair,
    signing_key: SigningKey,
    shared: SharedState,
    quorum_config: QuorumConfig,
    registry: PeerRegistry,
    mut cmd_rx: mpsc::Receiver<NodeCmd>,
    malicious: Option<MaliciousMode>,
    partition: PartitionTable,
    group: Arc<AtomicU8>,
) -> Result<()> {
    let mut swarm = build_swarm(libp2p_keypair).context("build swarm")?;
    let listen: Multiaddr = "/memory/0".parse().expect("valid multiaddr");
    swarm.listen_on(listen).context("listen /memory/0")?;

    for topic in [TOPIC_CAPS, TOPIC_TASKS, TOPIC_VERIFY, TOPIC_STATE_DELTAS] {
        let t = gossipsub::IdentTopic::new(topic);
        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&t)
            .with_context(|| format!("subscribe {topic}"))?;
    }

    let mut node = NodeLoop {
        peer_id,
        signing_key,
        shared,
        quorum_config,
        registry,
        tasks: HashMap::new(),
        reputation_local: HashMap::new(),
        applied_rep_keys: HashSet::new(),
        listen_addr: None,
        readiness: ReadinessState::Connected,
        malicious,
        partition,
        group,
        verifications_by_result: HashMap::new(),
        synced_outcomes: HashSet::new(),
    };

    let mut caps_tick = tokio::time::interval(Duration::from_millis(500));
    let mut finalise_tick = tokio::time::interval(Duration::from_millis(100));
    finalise_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;

            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                match cmd {
                    NodeCmd::Submit(spec, reply) => {
                        let r = submit_spec(&mut node, &mut swarm, spec);
                        let _ = reply.send(r);
                    }
                    NodeCmd::Snapshot(reply) => { let _ = reply.send(node.snapshot()); }
                    NodeCmd::Dial(addr, reply) => {
                        let r = swarm.dial(addr.clone()).map_err(|e| anyhow!("dial {addr}: {e}"));
                        let _ = reply.send(r);
                    }
                    NodeCmd::ListenAddr(reply) => {
                        let _ = reply.send(node.listen_addr.clone());
                    }
                    NodeCmd::ConnectedCount(reply) => {
                        let _ = reply.send(swarm.connected_peers().count());
                    }
                    NodeCmd::SetGroup(g, reply) => {
                        node.group.store(g, Ordering::SeqCst);
                        let _ = reply.send(());
                    }
                    NodeCmd::InjectCorruptedDelta(delta, reply) => {
                        let r = inject_delta_raw(&mut swarm, delta);
                        let _ = reply.send(r);
                    }
                    NodeCmd::RequestStateSync(since, reply) => {
                        let r = issue_state_sync(&mut node, &mut swarm, since);
                        let _ = reply.send(r);
                    }
                    NodeCmd::Shutdown(reply) => { let _ = reply.send(()); break; }
                }
            }

            _ = caps_tick.tick() => {
                publish_caps(&mut node, &mut swarm);
            }

            _ = finalise_tick.tick() => {
                let pending: Vec<ContentHash> = node.tasks.iter()
                    .filter(|(_, t)| !t.finalised && t.quorum.is_some())
                    .map(|(h, _)| *h).collect();
                let mut to_publish = Vec::new();
                for sh in pending {
                    let was = node.tasks.get(&sh).map(|t| t.finalised).unwrap_or(false);
                    if was { continue; }
                    if let Err(e) = node.try_finalise_quorum(sh) {
                        warn!(error = %e, "try_finalise_quorum");
                        continue;
                    }
                    if node.tasks.get(&sh).map(|t| t.finalised).unwrap_or(false) && !was {
                        to_publish.push(sh);
                    }
                }
                for sh in to_publish {
                    if let Err(e) = publish_outcome_and_rep(&mut node, &mut swarm, sh) {
                        warn!(error = %e, "publish_outcome_and_rep");
                    }
                }
            }

            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        node.listen_addr = Some(address);
                        node.readiness = ReadinessState::Listening;
                    }
                    SwarmEvent::ConnectionEstablished { .. }
                        if node.readiness == ReadinessState::Connected =>
                    {
                        node.readiness = ReadinessState::Listening;
                    }
                    SwarmEvent::Behaviour(ChaosBehaviourEvent::Gossipsub(
                        gossipsub::Event::Message { propagation_source, message, .. },
                    )) => {
                        // Partition gate: drop messages whose publisher
                        // (propagation_source) lives in a different
                        // group than this node. propagation_source is
                        // the immediate gossipsub sender; in a small
                        // in-process mesh that is the same node that
                        // originally published. This is the simplest
                        // emulation that captures the partition
                        // semantics without modifying libp2p internals.
                        let local_group = node.group.load(Ordering::SeqCst);
                        let src_group = node.partition.group_of(&propagation_source);
                        if local_group == src_group {
                            let topic = message.topic.as_str();
                            let payload = message.data.clone();
                            if let Err(e) = dispatch(&mut node, &mut swarm, topic, &payload).await {
                                warn!(error = %e, "dispatch");
                            }
                        } else {
                            trace!(
                                src = %propagation_source,
                                src_group, local_group,
                                "dropping message · partition gate"
                            );
                        }
                    }
                    SwarmEvent::Behaviour(ChaosBehaviourEvent::StateSync(
                        request_response::Event::Message { peer, message },
                    )) => match message {
                        request_response::Message::Request {
                            request, channel, ..
                        } => {
                            let resp =
                                build_state_sync_response(&node, peer, &request);
                            if let Err(e) = swarm
                                .behaviour_mut()
                                .state_sync
                                .send_response(channel, resp)
                            {
                                warn!(error = ?e, "send state-sync response");
                            }
                        }
                        request_response::Message::Response {
                            response, ..
                        } => {
                            apply_state_sync_response(&mut node, peer, &response);
                        }
                    },
                    SwarmEvent::Behaviour(ChaosBehaviourEvent::StateSync(
                        request_response::Event::OutboundFailure { peer, error, .. },
                    )) => trace!(%peer, ?error, "state-sync outbound failure"),
                    SwarmEvent::Behaviour(ChaosBehaviourEvent::StateSync(
                        request_response::Event::InboundFailure { peer, error, .. },
                    )) => trace!(%peer, ?error, "state-sync inbound failure"),
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn publish_caps(node: &mut NodeLoop, swarm: &mut Swarm<ChaosBehaviour>) {
    let now = now_unix();
    let ad = CapabilityAdvertisement {
        peer_id: node.peer_id,
        version: CAPS_WIRE_VERSION,
        services: vec![ServiceKind::Inference],
        inference: Some(InferenceCapability {
            models: vec!["chaos-sha256".into()],
            context_size: 4096,
            estimated_tokens_per_sec: 100,
        }),
        relay: None,
        storage: None,
        network_address: node
            .listen_addr
            .clone()
            .unwrap_or_else(|| "/memory/0".parse().expect("static multiaddr")),
        signed_at: now,
        ttl_seconds: 300,
        verifying_key_bytes: *node.signing_key.verifying_key().as_bytes(),
        reachable_addrs: node.listen_addr.iter().cloned().collect(),
        readiness: node.readiness,
        has_external_internet: false,
        bandwidth_mbps_external: None,
    };
    let bytes = match encode_advertisement(&ad) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "encode caps");
            return;
        }
    };
    let topic = gossipsub::IdentTopic::new(TOPIC_CAPS);
    if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic, bytes) {
        trace!(error = %e, "no peers yet · skip caps");
    } else if matches!(
        node.readiness,
        ReadinessState::Initialised | ReadinessState::Connected | ReadinessState::Listening
    ) {
        node.readiness = ReadinessState::Ready;
    }
}

fn submit_spec(
    node: &mut NodeLoop,
    swarm: &mut Swarm<ChaosBehaviour>,
    spec: JobSpec,
) -> Result<()> {
    node.record_spec(&spec)?;
    let bytes = parseh_task::to_cbor_bytes(&spec).map_err(|e| anyhow!("encode JobSpec: {e}"))?;
    let topic = gossipsub::IdentTopic::new(TOPIC_TASKS);
    swarm
        .behaviour_mut()
        .gossipsub
        .publish(topic, bytes)
        .map_err(|e| anyhow!("publish JobSpec: {e}"))?;
    info!(peer = %node.peer_id, hash = %spec.content_hash(), "submitted JobSpec");
    Ok(())
}

/// Inject an already-built `StateDelta` (typically pre-corrupted) on
/// `parseh.state-deltas.v1` to assert peer behaviour.
fn inject_delta_raw(swarm: &mut Swarm<ChaosBehaviour>, delta: StateDelta) -> Result<()> {
    let bytes = delta
        .encode_cbor()
        .map_err(|e| anyhow!("encode injected delta: {e}"))?;
    let topic = gossipsub::IdentTopic::new(TOPIC_STATE_DELTAS);
    swarm
        .behaviour_mut()
        .gossipsub
        .publish(topic, bytes)
        .map_err(|e| anyhow!("publish injected delta: {e}"))?;
    Ok(())
}

/// Trigger side of `/parseh/state-sync/1.0.0`. Send a signed request to
/// every currently-connected peer asking for outcomes finalised at/after
/// `since_unix`. Mirrors the production miner's `issue_state_sync_request`
/// (here every connected peer is asked, since the in-process mesh is
/// tiny and the partition is dispatch-layer — fanning out is the
/// strongest convergence guarantee). Returns the number of peers asked.
fn issue_state_sync(
    node: &mut NodeLoop,
    swarm: &mut Swarm<ChaosBehaviour>,
    since_unix: u64,
) -> Result<usize> {
    let peers: Vec<PeerId> = swarm.connected_peers().copied().collect();
    for p in &peers {
        let req = StateSyncRequest::new_signed(
            since_unix,
            STATE_SYNC_HARD_CEILING,
            node.peer_id,
            now_unix(),
            &node.signing_key,
        );
        swarm.behaviour_mut().state_sync.send_request(p, req);
    }
    debug!(peer = %node.peer_id, asked = peers.len(), since = since_unix, "issued state-sync");
    Ok(peers.len())
}

/// Responder side. Verify the requester signature BEFORE any work
/// (cheap DoS guard), clamp `max_outcomes` to the hard ceiling, then
/// answer from local shared-state via the index-backed
/// `outcomes_since`. A bad/unknown-signer request gets an empty
/// response (no work done).
fn build_state_sync_response(
    node: &NodeLoop,
    peer: PeerId,
    request: &StateSyncRequest,
) -> StateSyncResponse {
    let empty = || {
        StateSyncResponse::new_signed(
            Vec::new(),
            false,
            node.peer_id,
            now_unix(),
            &node.signing_key,
        )
    };
    let requester_key = match node.registry.verifying_key(&request.requester) {
        Some(k) => k,
        None => {
            debug!(%peer, "state-sync: requester pubkey unknown · empty");
            return empty();
        }
    };
    if let Err(e) = request.verify_signature(&requester_key) {
        warn!(%peer, error = %e, "state-sync: bad requester sig · empty");
        return empty();
    }
    let limit = request.max_outcomes.min(STATE_SYNC_HARD_CEILING) as usize;
    if limit == 0 {
        return empty();
    }
    let mut outcomes = match node.shared.outcomes_since(request.since_unix, limit + 1) {
        Ok(v) => v,
        Err(e) => {
            warn!(%peer, error = %e, "state-sync: outcomes_since failed");
            return empty();
        }
    };
    let truncated = outcomes.len() > limit;
    if truncated {
        outcomes.truncate(limit);
    }
    info!(
        %peer,
        since = request.since_unix,
        returned = outcomes.len(),
        truncated,
        "state-sync: answered"
    );
    StateSyncResponse::new_signed(
        outcomes,
        truncated,
        node.peer_id,
        now_unix(),
        &node.signing_key,
    )
}

/// Apply side. The responder framing is NOT trusted: each inner
/// [`JobOutcome`] is re-verified against the `observed_by` peer's key
/// from this node's own registry before being persisted (idempotent).
/// A forged outcome fails the inner check and is dropped — a malicious
/// responder can withhold/reorder but cannot inject.
fn apply_state_sync_response(
    node: &mut NodeLoop,
    peer: PeerId,
    response: &StateSyncResponse,
) {
    if let Some(rk) = node.registry.verifying_key(&response.responder) {
        if response.verify_signature(&rk).is_err() {
            warn!(%peer, "state-sync: bad responder envelope sig · dropping");
            return;
        }
    }
    let mut applied = 0usize;
    let mut rejected = 0usize;
    for outcome in &response.outcomes {
        let observer_key = match node.registry.verifying_key(&outcome.observed_by) {
            Some(k) => k,
            None => {
                rejected += 1;
                continue;
            }
        };
        if outcome.verify_signature(&observer_key).is_err() {
            rejected += 1;
            continue;
        }
        // Persist via the sync-specific writer (stubs parent rows the
        // partitioned-away node never saw) and mark the spec caught-up
        // so `NodeSnapshot` surfaces it like a gossip-delivered one.
        if let Err(e) = node.shared.record_synced_outcome(outcome) {
            debug!(error = %e, "state-sync: record_synced_outcome failed");
            continue;
        }
        node.synced_outcomes.insert(outcome.spec_hash);
        applied += 1;
    }
    info!(%peer, applied, rejected, "state-sync: response applied");
}

async fn dispatch(
    node: &mut NodeLoop,
    swarm: &mut Swarm<ChaosBehaviour>,
    topic: &str,
    payload: &[u8],
) -> Result<()> {
    if topic == TOPIC_CAPS {
        let ad = match parseh_core::decode_advertisement(payload) {
            Ok(a) => a,
            Err(e) => {
                trace!(error = %e, "decode caps");
                return Ok(());
            }
        };
        node.registry.upsert(ad);
        return Ok(());
    }
    if topic == TOPIC_TASKS {
        let spec: JobSpec = parseh_task::from_cbor_bytes(payload)
            .map_err(|e| anyhow!("decode JobSpec: {e}"))?;
        let submitter_pk = match node.registry.verifying_key(&spec.submitter) {
            Some(k) => k,
            None => {
                debug!(submitter = %spec.submitter, "submitter unknown · dropping JobSpec");
                return Ok(());
            }
        };
        spec.verify_signature(&submitter_pk)
            .map_err(|e| anyhow!("bad JobSpec sig: {e}"))?;
        node.record_spec(&spec)?;

        if spec.submitter == node.peer_id {
            return Ok(());
        }
        let chosen = pick_executor(&spec, &node.registry, &node.peer_id);
        if chosen != Some(node.peer_id) {
            return Ok(());
        }
        let spec_hash = spec.content_hash();
        if let Some(task) = node.tasks.get_mut(&spec_hash) {
            if task.self_executed {
                return Ok(());
            }
            task.self_executed = true;
        }
        let payload_bytes = ChaosExecutor
            .execute(&spec)
            .map_err(|e| anyhow!("execute: {e}"))?;
        let meta = ResultMeta {
            verifier_method: VerifierMethod::Deterministic,
            execution_time_ms: 1,
            model_used: Some("chaos-sha256".into()),
            inference_token_count: None,
        };
        let (result, _h) = JobResult::new_signed(
            spec_hash,
            node.peer_id,
            meta,
            payload_bytes,
            &node.signing_key,
        );
        node.record_result(&result)?;
        let mut envelope = vec![TAG_JOB_RESULT];
        envelope.extend(
            parseh_task::to_cbor_bytes(&result).map_err(|e| anyhow!("encode: {e}"))?,
        );
        let t = gossipsub::IdentTopic::new(TOPIC_VERIFY);
        if let Err(e) = swarm.behaviour_mut().gossipsub.publish(t, envelope) {
            warn!(error = %e, "publish JobResult");
        } else {
            info!(peer = %node.peer_id, %spec_hash, "published JobResult");
        }
        return Ok(());
    }
    if topic == TOPIC_VERIFY {
        if payload.is_empty() {
            return Ok(());
        }
        let tag = payload[0];
        let body = &payload[1..];
        match tag {
            TAG_JOB_RESULT => {
                let result: JobResult = parseh_task::from_cbor_bytes(body)
                    .map_err(|e| anyhow!("decode JobResult: {e}"))?;
                let exec_pk = match node.registry.verifying_key(&result.executor) {
                    Some(k) => k,
                    None => {
                        debug!(executor = %result.executor, "executor unknown · dropping JobResult");
                        return Ok(());
                    }
                };
                result
                    .verify_signature(&exec_pk)
                    .map_err(|e| anyhow!("bad JobResult sig: {e}"))?;
                node.record_result(&result)?;
                let spec_hash = result.spec_hash;
                let Some(task) = node.tasks.get(&spec_hash) else {
                    return Ok(());
                };
                if task.self_verified || result.executor == node.peer_id {
                    return Ok(());
                }
                let spec_clone = task.spec.clone();
                // Malicious-mode injection point — replaces the honest
                // `DeterministicMethod::verify` with a misbehaving
                // verdict producer. The verdict is THIS node's view of
                // the result; honest verifiers re-execute and compare.
                let verdict = match node.malicious {
                    None => {
                        let method = DeterministicMethod::new(ChaosExecutor);
                        let outcome = method
                            .verify(&spec_clone, &result)
                            .map_err(|e| anyhow!("verify: {e}"))?;
                        if outcome.matched {
                            VerifierVerdict::Agreed
                        } else {
                            VerifierVerdict::Disagreed {
                                evidence_hash: outcome.evidence_hash.unwrap_or_default(),
                            }
                        }
                    }
                    Some(mode) => crate::malicious_verifier::malicious_verdict(mode, &result),
                };
                let (verification, _) = JobVerification::new_signed(
                    result.content_hash(),
                    node.peer_id,
                    verdict,
                    VerifierMethod::Deterministic,
                    &node.signing_key,
                );
                if let Some(t) = node.tasks.get_mut(&spec_hash) {
                    t.self_verified = true;
                }
                node.record_verification(&verification)?;
                let mut envelope = vec![TAG_JOB_VERIFICATION];
                envelope.extend(
                    parseh_task::to_cbor_bytes(&verification)
                        .map_err(|e| anyhow!("encode v: {e}"))?,
                );
                let t = gossipsub::IdentTopic::new(TOPIC_VERIFY);
                if let Err(e) = swarm.behaviour_mut().gossipsub.publish(t, envelope) {
                    warn!(error = %e, "publish JobVerification");
                }
            }
            TAG_JOB_VERIFICATION => {
                let v: JobVerification = parseh_task::from_cbor_bytes(body)
                    .map_err(|e| anyhow!("decode JobVerification: {e}"))?;
                let verifier_pk = match node.registry.verifying_key(&v.verifier) {
                    Some(k) => k,
                    None => {
                        debug!(verifier = %v.verifier, "verifier unknown · dropping");
                        return Ok(());
                    }
                };
                v.verify_signature(&verifier_pk)
                    .map_err(|e| anyhow!("bad verification sig: {e}"))?;
                node.record_verification(&v)?;
                let mut sh_to_publish: Option<ContentHash> = None;
                for (sh, t) in node.tasks.iter() {
                    if t.finalised {
                        if let Some(observed) = &t.observed_result {
                            if observed.content_hash() == v.result_hash {
                                sh_to_publish = Some(*sh);
                                break;
                            }
                        }
                    }
                }
                if let Some(sh) = sh_to_publish {
                    publish_outcome_and_rep(node, swarm, sh)?;
                }
            }
            _ => {}
        }
        return Ok(());
    }
    if topic == TOPIC_STATE_DELTAS {
        let delta: StateDelta = StateDelta::decode_cbor(payload)
            .map_err(|e| anyhow!("decode delta: {e}"))?;
        let observer_pk = match node.registry.verifying_key(&delta.observer) {
            Some(k) => k,
            None => {
                debug!(observer = %delta.observer, "observer pubkey unknown · dropping delta");
                return Ok(());
            }
        };
        // Verify the delta signature upfront. Corrupted deltas are
        // dropped here — `parseh-shared-state::verify_delta` returns
        // an error when the signature does not match. This is the
        // exact code path `parseh-chaos::corruption` tests asserts on.
        if let Err(e) = parseh_shared_state::verify_delta(&delta, &observer_pk) {
            debug!(error = %e, observer = %delta.observer, "corrupted delta · dropping");
            return Ok(());
        }
        match &delta.kind {
            DeltaKind::Outcome(o) => {
                if let Err(e) = node.shared.apply_delta(delta.clone(), &observer_pk) {
                    trace!(error = %e, "apply_delta(Outcome)");
                } else {
                    debug!(hash = %o.spec_hash, "applied outcome delta");
                }
            }
            DeltaKind::Reputation {
                peer,
                delta: d,
                reason,
                related_hash,
            } => {
                node.apply_reputation(*peer, *d, reason, *related_hash)?;
            }
            DeltaKind::GovernanceRule { .. } => {}
        }
        return Ok(());
    }
    Ok(())
}

fn publish_outcome_and_rep(
    node: &mut NodeLoop,
    swarm: &mut Swarm<ChaosBehaviour>,
    spec_hash: ContentHash,
) -> Result<()> {
    let Some(task) = node.tasks.get(&spec_hash) else {
        return Ok(());
    };
    if !task.finalised {
        return Ok(());
    }
    let Some(result) = task.observed_result.clone() else {
        return Ok(());
    };
    let executor = result.executor;
    let outcome = match node
        .shared
        .outcome_for_spec(&spec_hash)
        .map_err(|e| anyhow!("outcome_for_spec: {e}"))?
    {
        Some(o) => o,
        None => return Ok(()),
    };
    node.record_outcome(&outcome)?;
    let is_valid = matches!(outcome.verdict, OutcomeVerdict::Valid { .. });
    let now = now_unix();
    let topic = gossipsub::IdentTopic::new(TOPIC_STATE_DELTAS);

    if is_valid {
        node.apply_reputation(
            executor,
            REPUTATION_AWARD_EXECUTOR,
            "executor_consensus_reward",
            Some(outcome.content_hash()),
        )?;
        let verifications = node
            .shared
            .verifications_for_result(&result.content_hash())
            .unwrap_or_default();
        // Reward agreeing verifiers; penalise disagreeing ones. This
        // is the placeholder V0.2 reputation curve referenced in
        // `verifier-economics.md` §3.4 — the chaos harness uses it to
        // exercise the disagree-but-wrong path.
        for v in verifications {
            match v.verdict {
                VerifierVerdict::Agreed => {
                    node.apply_reputation(
                        v.verifier,
                        REPUTATION_AWARD_VERIFIER,
                        "verifier_consensus_reward",
                        Some(outcome.content_hash()),
                    )?;
                }
                VerifierVerdict::Disagreed { .. } => {
                    node.apply_reputation(
                        v.verifier,
                        REPUTATION_PENALTY_FALSE_DISPUTE,
                        "verifier_false_dispute_penalty",
                        Some(outcome.content_hash()),
                    )?;
                }
                VerifierVerdict::Abstained => {}
            }
        }
    }

    let outcome_delta =
        StateDelta::unsigned(DeltaKind::Outcome(outcome.clone()), node.peer_id, now);
    let signed = sign_delta(outcome_delta, &node.signing_key)
        .map_err(|e| anyhow!("sign outcome: {e}"))?;
    let bytes = signed
        .encode_cbor()
        .map_err(|e| anyhow!("encode outcome: {e}"))?;
    let _ = swarm.behaviour_mut().gossipsub.publish(topic.clone(), bytes);

    if is_valid {
        let rep_delta = StateDelta::unsigned(
            DeltaKind::Reputation {
                peer: executor,
                delta: REPUTATION_AWARD_EXECUTOR,
                reason: "executor_consensus_reward".into(),
                related_hash: Some(outcome.content_hash()),
            },
            node.peer_id,
            now,
        );
        let signed = sign_delta(rep_delta, &node.signing_key)
            .map_err(|e| anyhow!("sign rep: {e}"))?;
        let bytes = signed
            .encode_cbor()
            .map_err(|e| anyhow!("encode rep: {e}"))?;
        let _ = swarm.behaviour_mut().gossipsub.publish(topic, bytes);
    }
    Ok(())
}

async fn collect_listen_addrs(nodes: &[ChaosNode], timeout: Duration) -> Result<Vec<Multiaddr>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let mut addrs: Vec<Option<Multiaddr>> = Vec::with_capacity(nodes.len());
        for node in nodes {
            addrs.push(node.listen_addr().await);
        }
        if addrs.iter().all(|a| a.is_some()) {
            return Ok(addrs.into_iter().flatten().collect());
        }
        if std::time::Instant::now() > deadline {
            return Err(anyhow!(
                "not every node bound a listen addr in {:?}",
                timeout
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
