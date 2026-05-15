//! `mesh` — 5-node V0.2.5 integration harness.
//!
//! Spawns N (default 5) in-process miners over `MemoryTransport`. Each
//! node:
//!
//! 1. Generates a fresh ed25519 identity (the libp2p `Keypair` and the
//!    dalek `SigningKey` share the same 32 secret bytes — so
//!    `PeerId == VerifyingKey`).
//! 2. Owns a tempfile `parseh-shared-state` SQLite database.
//! 3. Subscribes to `parseh.caps.v1` / `parseh.tasks.v1` /
//!    `parseh.verify.v1` / `parseh.state-deltas.v1`.
//! 4. Periodically publishes a V0.2.5 [`CapabilityAdvertisement`]
//!    (carrying its ed25519 pubkey + readiness state).
//! 5. Maintains a live [`PeerRegistry`] populated from the caps topic.
//! 6. On inbound `JobSpec`, runs the executor self-selection rule and
//!    if elected, signs + publishes a `JobResult`.
//! 7. On inbound `JobResult` from a non-self executor, re-executes
//!    deterministically and publishes a `JobVerification`.
//! 8. Maintains an M-of-N quorum per result and finalises via a
//!    periodic 100ms tick (same load-bearing pattern as the production
//!    miner).
//!
//! ## Why not call into `parseh-miner`?
//!
//! `parseh-miner` is a binary, not a library. We replicate its V0.2
//! coordination plane in a lib-friendly form so the harness can spin
//! up 5 instances inside one tokio runtime. Both code paths exercise
//! the same wire types and the same `parseh-core` peer-registry, which
//! is the V0.2.5 surface under test.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::SigningKey;
use futures::StreamExt;
use libp2p::core::transport::MemoryTransport;
use libp2p::{
    core::{transport::Transport, upgrade},
    gossipsub,
    identity::Keypair,
    noise,
    swarm::{NetworkBehaviour, Swarm, SwarmEvent},
    yamux, Multiaddr, PeerId,
};
use parseh_core::peer_registry::{
    encode_advertisement, CapabilityAdvertisement, InferenceCapability, PeerIdentity, PeerRegistry,
    ReadinessState, ServiceKind, CAPS_WIRE_VERSION,
};
use parseh_shared_state::{
    sign_delta, DeltaKind, KeyMaterial, KeySource, OpenOptions, SharedState, StateDelta,
};
use parseh_task::{
    ContentHash, JobOutcome, JobResult, JobSpec, JobVerification, OutcomeVerdict, ResultMeta,
    VerifierMethod, VerifierVerdict,
};
use parseh_verify::{
    DeterministicMethod, LocalExecutor, Quorum, QuorumConfig, VerificationOutcome, VerifierMethodImpl,
    VerifyError,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, trace, warn};

// ─── topic + tag constants ────────────────────────────────────────────

/// `parseh.caps.v1` — capability advertisements.
pub const TOPIC_CAPS: &str = "parseh.caps.v1";
/// `parseh.tasks.v1` — `JobSpec` envelopes.
pub const TOPIC_TASKS: &str = "parseh.tasks.v1";
/// `parseh.verify.v1` — `JobResult` + `JobVerification` envelopes.
pub const TOPIC_VERIFY: &str = "parseh.verify.v1";
/// `parseh.state-deltas.v1` — `StateDelta` envelopes.
pub const TOPIC_STATE_DELTAS: &str = parseh_shared_state::GOSSIPSUB_TOPIC;

/// Leading tag byte on `parseh.verify.v1` for a `JobResult`.
pub const TAG_JOB_RESULT: u8 = 0x02;
/// Leading tag byte on `parseh.verify.v1` for a `JobVerification`.
pub const TAG_JOB_VERIFICATION: u8 = 0x03;

/// Reputation reward awarded to the executor on a successful `Agreed`
/// outcome. Matches the V0.2 placeholder in `parseh-testnet`.
pub const REPUTATION_AWARD_EXECUTOR: i32 = 10;
/// Reputation reward awarded to each agreeing verifier.
pub const REPUTATION_AWARD_VERIFIER: i32 = 5;

// ─── public types ─────────────────────────────────────────────────────

/// One frozen view of a node's state. Returned by [`MeshNode::snapshot`].
#[derive(Debug, Clone, Default)]
pub struct NodeSnapshot {
    /// Set of spec_hashes the node has observed.
    pub task_hashes: HashSet<ContentHash>,
    /// Outcomes by spec_hash.
    pub outcomes: HashMap<ContentHash, JobOutcome>,
    /// Reputation summed per peer.
    pub reputation: HashMap<PeerId, i64>,
    /// Identities currently in the peer-key directory.
    pub known_identities: usize,
    /// Local readiness state.
    pub readiness: ReadinessState,
}

impl NodeSnapshot {
    /// `true` iff the node observed a finalised outcome for `spec_hash`.
    pub fn has_outcome_for_spec(&self, spec_hash: &ContentHash) -> bool {
        self.outcomes.contains_key(spec_hash)
    }
    /// Reputation tally for a peer. `0` when never seen.
    pub fn reputation_of(&self, peer: PeerId) -> i64 {
        self.reputation.get(&peer).copied().unwrap_or(0)
    }
}

/// Command channel between the mesh driver and an individual node task.
enum NodeCmd {
    Submit(JobSpec, oneshot::Sender<Result<()>>),
    Snapshot(oneshot::Sender<NodeSnapshot>),
    Dial(Multiaddr, oneshot::Sender<Result<()>>),
    ListenAddr(oneshot::Sender<Option<Multiaddr>>),
    ConnectedCount(oneshot::Sender<usize>),
    Shutdown(oneshot::Sender<()>),
}

/// Handle to one in-process miner.
#[derive(Clone)]
pub struct MeshNode {
    /// libp2p PeerId.
    pub peer_id: PeerId,
    /// ed25519 signing key (test-only — production never exposes this).
    pub signing_key: SigningKey,
    cmd_tx: mpsc::Sender<NodeCmd>,
    #[allow(dead_code)]
    tempdir: Arc<TempDir>,
}

impl MeshNode {
    /// Submit a signed `JobSpec` from this node.
    pub async fn submit(&self, spec: JobSpec) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(NodeCmd::Submit(spec, tx))
            .await
            .map_err(|_| anyhow!("node channel closed"))?;
        rx.await.map_err(|_| anyhow!("node dropped reply"))?
    }

    /// Snapshot the node's state.
    pub async fn snapshot(&self) -> NodeSnapshot {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(NodeCmd::Snapshot(tx)).await.is_err() {
            return NodeSnapshot::default();
        }
        rx.await.unwrap_or_default()
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

    /// Shut the node down cleanly.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(NodeCmd::Shutdown(tx)).await.is_err() {
            return;
        }
        let _ = rx.await;
    }
}

// ─── mesh orchestration ────────────────────────────────────────────────

/// `Mesh` is the top-level handle to a fully-connected N-node mesh.
pub struct Mesh {
    nodes: Vec<MeshNode>,
}

impl Mesh {
    /// Construct an `n`-node mesh with a documented test-only reduced
    /// quorum (M=2/N=3, t_min = 200 ms). For `n > 3` the quorum still
    /// expects M=2/N=3 — the harness asserts the flow, not the
    /// parameter sweep.
    pub async fn new(n: usize) -> Result<Self> {
        Self::with_quorum(n, reduced_quorum_for_test()).await
    }

    /// Like [`Self::new`] but with a custom [`QuorumConfig`].
    pub async fn with_quorum(n: usize, quorum: QuorumConfig) -> Result<Self> {
        if n < 2 {
            return Err(anyhow!("Mesh requires at least 2 nodes (got {n})"));
        }
        let mut nodes = Vec::with_capacity(n);
        for _ in 0..n {
            nodes.push(spawn_node(quorum).await?);
        }

        // Collect each node's listen addr, then dial the full N-clique.
        let addrs = collect_listen_addrs(&nodes, Duration::from_secs(5)).await?;
        for (i, node) in nodes.iter().enumerate() {
            for (j, addr) in addrs.iter().enumerate() {
                if i == j {
                    continue;
                }
                node.dial(addr.clone()).await.context("dial peer")?;
            }
        }

        // Wait for mesh formation: every node sees (n-1) connections.
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
                return Err(anyhow!("mesh did not form (each node needs {} peers)", n - 1));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Give gossipsub at least three heartbeats to GRAFT, then a
        // full caps round-trip so every node populates its registry.
        tokio::time::sleep(Duration::from_millis(1500)).await;

        info!(node_count = n, "mesh ready");
        Ok(Self { nodes })
    }

    /// Borrow a node by index.
    pub fn node(&self, idx: usize) -> &MeshNode {
        &self.nodes[idx]
    }

    /// Number of nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// `true` iff there are no nodes (false by construction).
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Submit a spec from node 0 (the conventional submitter index).
    pub async fn submit_from(&self, idx: usize, spec: JobSpec) -> Result<()> {
        self.nodes[idx].submit(spec).await
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

    /// Cleanly shut every node down.
    pub async fn shutdown(self) {
        for node in &self.nodes {
            node.shutdown().await;
        }
    }
}

/// Reduced quorum used by [`Mesh::new`].
pub fn reduced_quorum_for_test() -> QuorumConfig {
    QuorumConfig {
        m: 2,
        n: 3,
        t_min: Duration::from_millis(200),
        t_max: Duration::from_secs(30),
        rep_weighted_threshold: 0.6,
    }
}

// ─── node-task internals ──────────────────────────────────────────────

#[derive(NetworkBehaviour)]
struct MeshBehaviour {
    gossipsub: gossipsub::Behaviour,
}

/// Per-task state cached in-memory.
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
}

impl NodeLoop {
    fn record_spec(&mut self, spec: &JobSpec) -> Result<()> {
        self.shared.record_spec(spec).map_err(|e| anyhow!("record_spec: {e}"))?;
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
        self.shared.record_result(result).map_err(|e| anyhow!("record_result: {e}"))?;
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
        self.shared
            .record_verification(v)
            .map_err(|e| anyhow!("record_verification: {e}"))?;
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
        NodeSnapshot {
            task_hashes: self.tasks.keys().copied().collect(),
            outcomes: self
                .tasks
                .iter()
                .filter_map(|(hash, t)| {
                    if !t.finalised {
                        return None;
                    }
                    self.shared.outcome_for_spec(hash).ok().flatten().map(|o| (*hash, o))
                })
                .collect(),
            reputation: self.reputation_local.clone(),
            known_identities: self.registry.identity_count(),
            readiness: self.readiness,
        }
    }
}

/// Local executor for the harness — SHA-256(prompt + seed).
struct MeshExecutor;
impl LocalExecutor for MeshExecutor {
    fn execute(&self, spec: &JobSpec) -> Result<Vec<u8>, VerifyError> {
        let prompt = spec.inputs.prompt_text.as_deref().unwrap_or("");
        let seed = spec.inputs.seed.unwrap_or(0);
        let mut h = Sha256::new();
        h.update(prompt.as_bytes());
        h.update(seed.to_le_bytes());
        Ok(h.finalize().to_vec())
    }
}

/// V0.2.5 deterministic-lowest-PeerId executor self-selection — same
/// rule as `parseh-miner::should_execute`.
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

async fn spawn_node(quorum_config: QuorumConfig) -> Result<MeshNode> {
    let (signing_key, libp2p_kp, peer_id) = fresh_identity();

    let tempdir = Arc::new(TempDir::new().context("create tempdir")?);
    let db_path = tempdir.path().join("shared-state.sqlite3");
    let key = KeyMaterial::from_source(KeySource::Raw([0xCD; 32])).context("derive key")?;
    let shared = SharedState::open(OpenOptions::create(db_path, key)).context("open shared")?;

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
    tokio::spawn(async move {
        if let Err(e) = run_node_loop(
            peer_id,
            libp2p_kp,
            signing_key_inner,
            shared,
            quorum_config,
            registry,
            cmd_rx,
        )
        .await
        {
            warn!(error = %e, %peer_id, "mesh node loop exited with error");
        }
    });

    Ok(MeshNode {
        peer_id,
        signing_key,
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

fn build_swarm(kp: Keypair) -> Result<Swarm<MeshBehaviour>> {
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
    let behaviour = MeshBehaviour { gossipsub };
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
                    SwarmEvent::Behaviour(MeshBehaviourEvent::Gossipsub(
                        gossipsub::Event::Message { message, .. },
                    )) => {
                        let topic = message.topic.as_str();
                        let payload = message.data.clone();
                        if let Err(e) = dispatch(&mut node, &mut swarm, topic, &payload).await {
                            warn!(error = %e, "dispatch");
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn publish_caps(node: &mut NodeLoop, swarm: &mut Swarm<MeshBehaviour>) {
    let now = now_unix();
    let ad = CapabilityAdvertisement {
        peer_id: node.peer_id,
        version: CAPS_WIRE_VERSION,
        services: vec![ServiceKind::Inference],
        inference: Some(InferenceCapability {
            models: vec!["harness-sha256".into()],
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
    swarm: &mut Swarm<MeshBehaviour>,
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

async fn dispatch(
    node: &mut NodeLoop,
    swarm: &mut Swarm<MeshBehaviour>,
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
        let spec: JobSpec = parseh_task::from_cbor_bytes(payload).map_err(|e| anyhow!("decode JobSpec: {e}"))?;
        let submitter_pk = match node.registry.verifying_key(&spec.submitter) {
            Some(k) => k,
            None => {
                debug!(submitter = %spec.submitter, "submitter unknown · dropping JobSpec");
                return Ok(());
            }
        };
        spec.verify_signature(&submitter_pk).map_err(|e| anyhow!("bad JobSpec sig: {e}"))?;
        node.record_spec(&spec)?;

        if spec.submitter == node.peer_id {
            return Ok(());
        }
        let chosen = pick_executor(&spec, &node.registry, &node.peer_id);
        if chosen != Some(node.peer_id) {
            return Ok(());
        }
        // Build result + publish.
        let spec_hash = spec.content_hash();
        if let Some(task) = node.tasks.get_mut(&spec_hash) {
            if task.self_executed {
                return Ok(());
            }
            task.self_executed = true;
        }
        let payload = MeshExecutor.execute(&spec).map_err(|e| anyhow!("execute: {e}"))?;
        let meta = ResultMeta {
            verifier_method: VerifierMethod::Deterministic,
            execution_time_ms: 1,
            model_used: Some("harness-sha256".into()),
            inference_token_count: None,
        };
        let (result, _h) = JobResult::new_signed(spec_hash, node.peer_id, meta, payload, &node.signing_key);
        node.record_result(&result)?;
        let mut envelope = vec![TAG_JOB_RESULT];
        envelope.extend(parseh_task::to_cbor_bytes(&result).map_err(|e| anyhow!("encode: {e}"))?);
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
                result.verify_signature(&exec_pk).map_err(|e| anyhow!("bad JobResult sig: {e}"))?;
                node.record_result(&result)?;
                let spec_hash = result.spec_hash;
                let Some(task) = node.tasks.get(&spec_hash) else {
                    return Ok(());
                };
                if task.self_verified || result.executor == node.peer_id {
                    return Ok(());
                }
                let spec_clone = task.spec.clone();
                let method = DeterministicMethod::new(MeshExecutor);
                let outcome = method.verify(&spec_clone, &result).map_err(|e| anyhow!("verify: {e}"))?;
                let verdict = if outcome.matched {
                    VerifierVerdict::Agreed
                } else {
                    VerifierVerdict::Disagreed {
                        evidence_hash: outcome.evidence_hash.unwrap_or_default(),
                    }
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
                // Publish with the TAG_JOB_VERIFICATION tag — mirrors
                // the production miner's encoding.
                let mut envelope = vec![TAG_JOB_VERIFICATION];
                envelope.extend(parseh_task::to_cbor_bytes(&verification).map_err(|e| anyhow!("encode v: {e}"))?);
                let t = gossipsub::IdentTopic::new(TOPIC_VERIFY);
                if let Err(e) = swarm.behaviour_mut().gossipsub.publish(t, envelope) {
                    warn!(error = %e, "publish JobVerification");
                }
                let _ = VerificationOutcome { matched: outcome.matched, evidence_hash: None };
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
                v.verify_signature(&verifier_pk).map_err(|e| anyhow!("bad verification sig: {e}"))?;
                node.record_verification(&v)?;
                // Check finalisation.
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
        let delta: StateDelta = StateDelta::decode_cbor(payload).map_err(|e| anyhow!("decode delta: {e}"))?;
        let observer_pk = match node.registry.verifying_key(&delta.observer) {
            Some(k) => k,
            None => {
                debug!(observer = %delta.observer, "observer pubkey unknown · dropping delta");
                return Ok(());
            }
        };
        match &delta.kind {
            DeltaKind::Outcome(o) => {
                if let Err(e) = node.shared.apply_delta(delta.clone(), &observer_pk) {
                    trace!(error = %e, "apply_delta(Outcome)");
                } else {
                    debug!(hash = %o.spec_hash, "applied outcome delta");
                }
            }
            DeltaKind::Reputation { peer, delta: d, reason, related_hash } => {
                node.apply_reputation(*peer, *d, reason, *related_hash)?;
                if let Err(e) = parseh_shared_state::verify_delta(&delta, &observer_pk) {
                    trace!(error = %e, "verify_delta(Reputation)");
                }
            }
            DeltaKind::GovernanceRule { .. } => {}
        }
        return Ok(());
    }
    Ok(())
}

fn publish_outcome_and_rep(
    node: &mut NodeLoop,
    swarm: &mut Swarm<MeshBehaviour>,
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
    let outcome = match node.shared.outcome_for_spec(&spec_hash).map_err(|e| anyhow!("outcome_for_spec: {e}"))? {
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
        // Pull every Agreed verifier and credit them.
        let verifications = node
            .shared
            .verifications_for_result(&result.content_hash())
            .unwrap_or_default();
        for v in verifications {
            if matches!(v.verdict, VerifierVerdict::Agreed) {
                node.apply_reputation(
                    v.verifier,
                    REPUTATION_AWARD_VERIFIER,
                    "verifier_consensus_reward",
                    Some(outcome.content_hash()),
                )?;
            }
        }
    }

    // Sign + publish outcome.
    let outcome_delta = StateDelta::unsigned(DeltaKind::Outcome(outcome.clone()), node.peer_id, now);
    let signed = sign_delta(outcome_delta, &node.signing_key).map_err(|e| anyhow!("sign outcome: {e}"))?;
    let bytes = signed.encode_cbor().map_err(|e| anyhow!("encode outcome: {e}"))?;
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
        let signed = sign_delta(rep_delta, &node.signing_key).map_err(|e| anyhow!("sign rep: {e}"))?;
        let bytes = signed.encode_cbor().map_err(|e| anyhow!("encode rep: {e}"))?;
        let _ = swarm.behaviour_mut().gossipsub.publish(topic, bytes);
    }
    Ok(())
}

async fn collect_listen_addrs(nodes: &[MeshNode], timeout: Duration) -> Result<Vec<Multiaddr>> {
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
            return Err(anyhow!("not every node bound a listen addr in {:?}", timeout));
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
