//! `TestNode` — one in-process libp2p node carrying the V0.2 coordination plane.
//!
//! Each node runs its own tokio task. It owns a libp2p `Swarm` over
//! `MemoryTransport`, a `parseh_shared_state::SharedState` backed by a
//! tempfile SQLite database, a per-node ed25519 signing key (the same
//! 32 bytes seed the libp2p identity uses, so `PeerId == verifying_key`),
//! and three gossipsub topic subscriptions:
//!
//! - `parseh.tasks.v1` — carries `JobSpec` and `JobResult` envelopes,
//!   tag-multiplexed by the leading byte per
//!   the project notes §4.
//! - `parseh.verify.v1` — carries `JobVerification` envelopes.
//! - `parseh.state-deltas.v1` — carries signed `StateDelta` envelopes
//!   (outcomes + reputation log entries).
//!
//! The node exposes a command channel (`mpsc`) that scenario code uses
//! to drive it (`Submit`, `Snapshot`, `Listening`), and a periodic
//! shutdown signal. Internally, every inbound gossipsub message is
//! routed by topic to a handler that persists into shared state and
//! optionally produces a downstream message.
//!
//! ## Why each node plays every role
//!
//! V0.2's selection rules (`parseh_verify::decide_to_verify`) already
//! gate self-verification (Rule 3a/3b) and own-result execution: with
//! all three nodes acting as executor *and* verifier, the protocol
//! itself decides who does what. The harness does not have to assign
//! roles statically. This is also how the production miner is shaped —
//! one binary, all behaviours.
//!
//! ## Quorum reduction
//!
//! Production V0.2 uses M=5/N=9. With three nodes the harness cannot
//! satisfy that, so callers supply a [`QuorumConfig`] override at
//! construction time (typically the `Scenario` builds an M=2/N=3
//! variant with a tightened `t_min` so the test finishes in seconds
//! rather than the production `T_min = 5s`). See module-level doc on
//! [`crate::Scenario`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context};
use ed25519_dalek::{SigningKey, VerifyingKey};
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
use parking_lot::Mutex;
use parseh_shared_state::{
    sign_delta, DeltaKind, KeyMaterial, KeySource, OpenOptions, SharedState, StateDelta,
};
use parseh_task::{
    ContentHash, JobOutcome, JobResult, JobSpec, JobVerification, OutcomeVerdict, ResultMeta,
    VerifierMethod, VerifierVerdict,
};
use parseh_verify::{
    DeterministicMethod, LocalExecutor, Quorum, QuorumConfig, RateLimit, SelectionConfig,
    Verifier, VerifierMethodImpl, VerifyError, VerifyOutcome,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, trace, warn};

/// Gossipsub topic names. Mirror the constants in
/// the project notes §4. We avoid
/// depending on a single source of truth at the crate boundary here
/// because the miner's V0.1 constants are out of date relative to V0.2;
/// the testnet asserts the V0.2 contract directly.
const TOPIC_TASKS: &str = "parseh.tasks.v1";
const TOPIC_VERIFY: &str = "parseh.verify.v1";
const TOPIC_STATE_DELTAS: &str = parseh_shared_state::GOSSIPSUB_TOPIC;

/// Leading tag byte multiplexing the `parseh.tasks.v1` payload.
const TAG_JOB_SPEC: u8 = 0x01;
/// Leading tag byte: this envelope carries a `JobResult`.
const TAG_JOB_RESULT: u8 = 0x02;
/// Leading tag byte: this envelope carries a `JobOutcome` (mirrored
/// inline on `parseh.tasks.v1` for completeness; the canonical outcome
/// propagation is via `parseh.state-deltas.v1`).
const TAG_JOB_OUTCOME: u8 = 0x04;

/// Reputation increment awarded to the executor on a successful
/// `Agreed` outcome. The number itself is a V0.2 placeholder — the
/// concrete reputation curve lives in
/// the project notes §3.4; this harness asserts only
/// that the increment is observed network-wide.
pub const REPUTATION_AWARD_EXECUTOR: i32 = 10;

/// Role hint for diagnostics. The protocol does not branch on role —
/// every node is willing to play every part — but the role label makes
/// scenario logs easier to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRole {
    /// Originates `JobSpec`s.
    Submitter,
    /// Executes specs and publishes `JobResult`s.
    Executor,
    /// Re-executes results and publishes `JobVerification`s.
    Verifier,
    /// Observes everything but does not author messages. Unused in V0.2
    /// — kept for forward-compat with `parseh-verify` future probationary
    /// modes.
    Observer,
}

/// A frozen view of one node's shared-state for assertion in tests.
///
/// Returned by [`TestNode::snapshot`] / [`crate::Scenario::dump_state`].
/// Fields are intentionally minimal — the acceptance test only needs to
/// know whether a particular `spec_hash` has reached terminal state and
/// what each peer's reputation looks like.
#[derive(Debug, Clone, Default)]
pub struct StateSnapshot {
    /// Set of `spec_hash`es the node has heard a `JobSpec` for.
    pub task_hashes: HashSet<ContentHash>,
    /// Map of `spec_hash` → finalised `JobOutcome`.
    pub outcomes: HashMap<ContentHash, JobOutcome>,
    /// Map of `PeerId` → summed reputation (positive only at V0.2 —
    /// there is no slashing path yet).
    pub reputation: HashMap<PeerId, i64>,
}

impl StateSnapshot {
    /// `true` iff the node has heard the spec referenced by `hash`.
    pub fn has_task(&self, hash: &ContentHash) -> bool {
        self.task_hashes.contains(hash)
    }

    /// `true` iff the node has a finalised outcome for `spec_hash`.
    pub fn has_outcome_for_spec(&self, spec_hash: &ContentHash) -> bool {
        self.outcomes.contains_key(spec_hash)
    }

    /// Reputation for `peer`. Returns 0 when no entries exist.
    pub fn reputation_of(&self, peer: PeerId) -> i64 {
        self.reputation.get(&peer).copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------
// libp2p behaviour
// ---------------------------------------------------------------------

#[derive(NetworkBehaviour)]
struct TestBehaviour {
    gossipsub: gossipsub::Behaviour,
}

// ---------------------------------------------------------------------
// Command channel
// ---------------------------------------------------------------------

/// Commands the scenario sends into a node's task.
enum NodeCommand {
    /// Originate a `JobSpec` and gossip it.
    Submit(JobSpec, oneshot::Sender<anyhow::Result<()>>),
    /// Return a snapshot of the node's shared state.
    Snapshot(oneshot::Sender<StateSnapshot>),
    /// Dial a memory multiaddr.
    Dial(Multiaddr, oneshot::Sender<anyhow::Result<()>>),
    /// Return the bound listen multiaddr (set after `Listening` event).
    ListenAddr(oneshot::Sender<Option<Multiaddr>>),
    /// Return the number of currently-connected peers.
    ConnectedCount(oneshot::Sender<usize>),
    /// Shut down the node task.
    Shutdown(oneshot::Sender<()>),
}

/// Handle returned to the scenario after spawning a node.
pub struct TestNode {
    /// libp2p `PeerId` of the local node.
    pub peer_id: PeerId,
    /// ed25519 signing key — the same key bytes the libp2p identity uses.
    pub signing_key: SigningKey,
    cmd_tx: mpsc::Sender<NodeCommand>,
    /// Tempdir holding the SQLite database. Kept alive for the lifetime
    /// of the node — dropping it removes the file.
    _tempdir: Arc<TempDir>,
}

impl TestNode {
    /// `peer_id` getter — preferred over the public field for forwards
    /// compatibility.
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Submit a `JobSpec` on this node. Returns once the spec is queued
    /// for gossip publish.
    pub async fn submit(&self, spec: JobSpec) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(NodeCommand::Submit(spec, tx))
            .await
            .map_err(|_| anyhow!("node command channel closed"))?;
        rx.await.map_err(|_| anyhow!("node dropped reply"))?
    }

    /// Snapshot of the node's shared state for assertions.
    pub async fn snapshot(&self) -> StateSnapshot {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(NodeCommand::Snapshot(tx)).await.is_err() {
            return StateSnapshot::default();
        }
        rx.await.unwrap_or_default()
    }

    /// Dial another node's memory multiaddr.
    pub async fn dial(&self, addr: Multiaddr) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(NodeCommand::Dial(addr, tx))
            .await
            .map_err(|_| anyhow!("node command channel closed"))?;
        rx.await.map_err(|_| anyhow!("node dropped reply"))?
    }

    /// Return the node's listening multiaddr, if any has been bound.
    pub async fn listen_addr(&self) -> Option<Multiaddr> {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(NodeCommand::ListenAddr(tx)).await.is_err() {
            return None;
        }
        rx.await.unwrap_or(None)
    }

    /// Number of currently-connected peers.
    pub async fn connected_count(&self) -> usize {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(NodeCommand::ConnectedCount(tx))
            .await
            .is_err()
        {
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    /// Shut the node down cleanly.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(NodeCommand::Shutdown(tx)).await.is_err() {
            return;
        }
        let _ = rx.await;
    }
}

// ---------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------

/// Build a 32-byte ed25519 seed, the dalek signing key, and the matching
/// libp2p `Keypair`. By using the same secret bytes for both we make
/// `PeerId` strictly equal to the ed25519 verifying key, which removes a
/// whole class of "which key was this signed by" confusion downstream.
fn fresh_identity() -> (SigningKey, Keypair, PeerId) {
    let mut seed = [0u8; 32];
    rand::Rng::fill(&mut rand::thread_rng(), &mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let mut seed_clone = seed;
    let libp2p_kp =
        Keypair::ed25519_from_bytes(&mut seed_clone).expect("32-byte ed25519 seed is valid");
    let peer_id = PeerId::from(libp2p_kp.public());
    // Wipe the local copy of the seed; the dalek SigningKey owns its
    // own copy and the libp2p Keypair owns its own copy.
    for b in seed.iter_mut() {
        *b = 0;
    }
    for b in seed_clone.iter_mut() {
        *b = 0;
    }
    (signing_key, libp2p_kp, peer_id)
}

/// In-memory directory mapping `PeerId` → `VerifyingKey`. Each node
/// learns peer keys lazily as it receives signed envelopes (every
/// envelope carries its own author's `PeerId`, and the harness — at
/// spawn time — exchanges raw verifying keys out-of-band).
///
/// At V0.3+ this is the chain's job (or a Kad-DHT-backed record). At
/// V0.2 in the wild it would come from a `parseh.caps.v1` advertisement
/// that includes the key. Here we just inject the keys at scenario
/// startup.
#[derive(Default, Debug, Clone)]
pub(crate) struct PeerKeyDirectory {
    inner: Arc<Mutex<HashMap<PeerId, VerifyingKey>>>,
}

impl PeerKeyDirectory {
    pub fn insert(&self, peer: PeerId, key: VerifyingKey) {
        self.inner.lock().insert(peer, key);
    }

    pub fn get(&self, peer: &PeerId) -> Option<VerifyingKey> {
        self.inner.lock().get(peer).copied()
    }

    /// Snapshot the current set of known peers. Used by tests for
    /// deterministic role assignment.
    pub fn known_peers(&self) -> Vec<PeerId> {
        self.inner.lock().keys().copied().collect()
    }
}

/// Spawn a fresh node with the given role, returning a [`TestNode`]
/// handle. The internal tokio task lives until [`TestNode::shutdown`]
/// is called or the handle is dropped.
///
/// Parameters:
/// - `role` — diagnostic-only label.
/// - `quorum_config` — used by the inline aggregator on every node.
///   The scenario passes a reduced (M=2/N=3, t_min compressed) variant.
/// - `directory` — shared pubkey directory. Pre-populated with all
///   nodes' keys before the scenario starts.
pub async fn spawn(
    role: NodeRole,
    quorum_config: QuorumConfig,
    directory: PeerKeyDirectory,
) -> anyhow::Result<TestNode> {
    let (signing_key, libp2p_kp, peer_id) = fresh_identity();
    // Make sure the directory has our own key. The scenario inserts all
    // three keys before connecting, but doing it here too is cheap and
    // means the helper can be reused outside a `Scenario`.
    directory.insert(peer_id, signing_key.verifying_key());

    let tempdir = Arc::new(TempDir::new().context("create tempdir for shared-state db")?);
    let db_path = tempdir.path().join("shared-state.sqlite3");
    let key = KeyMaterial::from_source(KeySource::Raw([0xAB; 32]))
        .context("derive shared-state key")?;
    let shared = SharedState::open(OpenOptions::create(db_path, key))
        .context("open shared-state")?;

    let (cmd_tx, cmd_rx) = mpsc::channel::<NodeCommand>(64);

    let inner_keypair = libp2p_kp.clone();
    let inner_signing_key = signing_key.clone();
    let inner_dir = directory.clone();

    tokio::spawn(async move {
        if let Err(e) = run_node(
            role,
            peer_id,
            inner_keypair,
            inner_signing_key,
            shared,
            quorum_config,
            inner_dir,
            cmd_rx,
        )
        .await
        {
            warn!(error = %e, %peer_id, "test node loop exited with error");
        } else {
            debug!(%peer_id, "test node loop exited cleanly");
        }
    });

    Ok(TestNode {
        peer_id,
        signing_key,
        cmd_tx,
        _tempdir: tempdir,
    })
}

// ---------------------------------------------------------------------
// Node event loop
// ---------------------------------------------------------------------

/// Per-task state cached in-memory by each node.
struct TaskState {
    /// Cached spec — used by the verifier to call DeterministicMethod.
    spec: JobSpec,
    /// Whether we have observed a `JobResult` for this spec.
    observed_result: Option<JobResult>,
    /// Open quorum, keyed only after we see the first JobResult.
    quorum: Option<Quorum>,
    /// Whether we have already produced a signed `JobOutcome` for this
    /// spec_hash. Used to short-circuit further finalisation attempts.
    finalised: bool,
    /// Whether *this* node already executed and published a JobResult
    /// for this spec. The protocol allows any peer to execute; the
    /// first JobResult on the wire wins (rest are duplicates).
    self_executed: bool,
    /// Whether *this* node already verified the observed result. One
    /// `JobVerification` per (verifier, result) is the network rule.
    self_verified: bool,
}

/// Inline shared-state helpers wrap a few common patterns. They are
/// methods on the event loop's mutable context rather than free
/// functions so they can mutate the quorum map without re-borrowing.
struct NodeLoop {
    role: NodeRole,
    peer_id: PeerId,
    libp2p_keypair: Keypair,
    signing_key: SigningKey,
    shared: SharedState,
    quorum_config: QuorumConfig,
    directory: PeerKeyDirectory,
    tasks: HashMap<ContentHash, TaskState>,
    /// Rolling cache of own reputation, used when scoring this node's
    /// verifier selection roll. V0.2 selection inputs include the local
    /// node's reputation tally; we read it from shared state.
    local_reputation: u32,
    /// The currently-bound memory listen multiaddr. Filled in once the
    /// first `NewListenAddr` event arrives.
    listen_addr: Option<Multiaddr>,
    /// Reputation cache snapshot of what we've already applied locally
    /// — used as the source of truth for `StateSnapshot`. Tracked
    /// separately because the shared-state SQL `SUM` is fine but
    /// snapshotting it at every assertion is wasteful.
    reputation_local: HashMap<PeerId, i64>,
    /// Set of (peer, related_hash, reason) tuples we have already
    /// applied locally. Reputation deltas are idempotent here because
    /// the test scenario republishes them whenever a node finalises;
    /// without this we'd double-count an executor's reward.
    applied_reputation_keys: HashSet<(PeerId, Option<ContentHash>, String)>,
}

impl NodeLoop {
    fn record_spec_locally(&mut self, spec: &JobSpec) -> anyhow::Result<()> {
        let hash = spec.content_hash();
        // Persist
        self.shared
            .record_spec(spec)
            .map_err(|e| anyhow!("record_spec: {e}"))?;
        // In-memory cache for quick lookup.
        self.tasks.entry(hash).or_insert_with(|| TaskState {
            spec: spec.clone(),
            observed_result: None,
            quorum: None,
            finalised: false,
            self_executed: false,
            self_verified: false,
        });
        Ok(())
    }

    fn record_result_locally(&mut self, result: &JobResult) -> anyhow::Result<()> {
        self.shared
            .record_result(result)
            .map_err(|e| anyhow!("record_result: {e}"))?;
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

    fn record_verification_locally(
        &mut self,
        verification: &JobVerification,
    ) -> anyhow::Result<()> {
        self.shared
            .record_verification(verification)
            .map_err(|e| anyhow!("record_verification: {e}"))?;
        // Bind into the open quorum if we have one open for this result.
        let result_hash = verification.result_hash;
        let mut to_finalise = None;
        for (spec_hash, task) in self.tasks.iter_mut() {
            let Some(observed) = &task.observed_result else {
                continue;
            };
            if observed.content_hash() != result_hash {
                continue;
            }
            let Some(quorum) = task.quorum.as_mut() else {
                continue;
            };
            let verifier_pubkey = match self.directory.get(&verification.verifier) {
                Some(k) => k,
                None => {
                    debug!(
                        verifier = %verification.verifier,
                        "no pubkey in directory yet · dropping verification"
                    );
                    return Ok(());
                }
            };
            // Each verifier carries reputation 100 in the harness — see
            // module-level note. The reputation tally drives only the
            // `rep_weighted` threshold, and we want a clean Agreed.
            match quorum.add_verification(verification.clone(), 100, &verifier_pubkey) {
                Ok(()) => {
                    trace!(%spec_hash, "added verification to quorum");
                }
                Err(VerifyError::Internal(msg))
                    if msg.contains("duplicate") || msg.contains("result_hash") =>
                {
                    debug!(error = %msg, "ignoring verification");
                    return Ok(());
                }
                Err(e) => return Err(anyhow!("add_verification: {e}")),
            }
            if !task.finalised {
                to_finalise = Some(*spec_hash);
            }
            break;
        }
        if let Some(spec_hash) = to_finalise {
            self.try_finalise_quorum(spec_hash)?;
        }
        Ok(())
    }

    fn try_finalise_quorum(&mut self, spec_hash: ContentHash) -> anyhow::Result<()> {
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

        info!(
            role = ?self.role,
            peer = %self.peer_id,
            decision = ?finalised.decision,
            agreements = finalised.agreements,
            disagreements = finalised.disagreements,
            "quorum finalised"
        );

        // Persist locally.
        self.shared
            .record_outcome(&finalised.outcome)
            .map_err(|e| anyhow!("record_outcome: {e}"))?;
        Ok(())
    }

    fn record_outcome_locally(&mut self, outcome: &JobOutcome) -> anyhow::Result<()> {
        self.shared
            .record_outcome(outcome)
            .map_err(|e| anyhow!("record_outcome: {e}"))?;
        Ok(())
    }

    fn apply_reputation_locally(
        &mut self,
        peer: PeerId,
        delta: i32,
        reason: &str,
        related_hash: Option<ContentHash>,
    ) -> anyhow::Result<()> {
        let key = (peer, related_hash, reason.to_string());
        if self.applied_reputation_keys.contains(&key) {
            return Ok(());
        }
        self.shared
            .apply_reputation_delta(peer, delta, reason, related_hash)
            .map_err(|e| anyhow!("apply_reputation_delta: {e}"))?;
        self.applied_reputation_keys.insert(key);
        *self.reputation_local.entry(peer).or_insert(0) += delta as i64;
        if peer == self.peer_id {
            self.local_reputation = self.local_reputation.saturating_add_signed(delta);
        }
        Ok(())
    }

    fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
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
        }
    }
}

/// Pick the deterministic executor PeerId from the directory.
///
/// Returns the non-submitter peer with the smallest byte-encoding of
/// its `PeerId`. Returns `None` if the directory does not yet contain
/// at least one non-submitter peer (which would mean spec arrived
/// before key exchange — should not happen in `Scenario::new`'s
/// orchestration).
fn pick_executor(node: &NodeLoop, submitter: &PeerId) -> Option<PeerId> {
    node.directory
        .known_peers()
        .into_iter()
        .filter(|p| p != submitter)
        .min_by_key(|p| p.to_bytes())
}

/// Deterministic local executor for the harness.
///
/// All three nodes run this identical executor: SHA-256 of the prompt
/// bytes plus the seed. That gives every node byte-equal `result_payload`
/// values, which is exactly what `DeterministicMethod` expects when it
/// re-executes a result. **This is V0.2 test-only**; production V0.2
/// will plug a real LLM through the [`LocalExecutor`] trait.
struct HarnessExecutor;

impl LocalExecutor for HarnessExecutor {
    fn execute(&self, spec: &JobSpec) -> Result<Vec<u8>, VerifyError> {
        let prompt = spec.inputs.prompt_text.as_deref().unwrap_or("");
        let seed = spec.inputs.seed.unwrap_or(0);
        let mut h = Sha256::new();
        h.update(prompt.as_bytes());
        h.update(seed.to_le_bytes());
        Ok(h.finalize().to_vec())
    }
}

/// One reception of a gossipsub message. Routed by topic; tag-byte
/// multiplexed for `parseh.tasks.v1`.
async fn dispatch_message(
    node: &mut NodeLoop,
    swarm: &mut Swarm<TestBehaviour>,
    topic: gossipsub::TopicHash,
    payload: &[u8],
) -> anyhow::Result<()> {
    if payload.is_empty() {
        return Ok(());
    }
    let topic_str = topic.as_str();

    if topic_str == TOPIC_TASKS {
        let tag = payload[0];
        let body = &payload[1..];
        match tag {
            TAG_JOB_SPEC => {
                let spec: JobSpec = parseh_task::from_cbor_bytes(body)
                    .map_err(|e| anyhow!("decode JobSpec: {e}"))?;
                let submitter_pk = node
                    .directory
                    .get(&spec.submitter)
                    .ok_or_else(|| anyhow!("submitter pubkey missing in directory"))?;
                spec.verify_signature(&submitter_pk)
                    .map_err(|e| anyhow!("bad JobSpec signature: {e}"))?;
                let hash = spec.content_hash();
                node.record_spec_locally(&spec)?;
                debug!(role = ?node.role, peer = %node.peer_id, %hash, "observed JobSpec");

                // Decide whether this node executes. Production V0.2
                // is "first executor to publish wins" (self-selected),
                // but with three nodes that race produces two
                // executors and two distinct `JobResult` hashes, which
                // breaks the FK constraint on `verifications` (we open
                // the quorum against the first result we see, then a
                // later verification arrives signed against the
                // *other* result hash and we have no row to attach it
                // to).
                //
                // The harness picks the executor **deterministically**
                // by lowest `PeerId.to_bytes()` among non-submitters
                // that appear in the directory. This is test-only
                // behaviour — V0.3 hardens the production wire path
                // against duplicate results — but it makes the
                // 3-node primitive proof tractable today.
                let executor_choice = pick_executor(node, &spec.submitter);
                let should_execute = executor_choice == Some(node.peer_id);
                if should_execute {
                    if let Some(task) = node.tasks.get_mut(&hash) {
                        if !task.self_executed {
                            task.self_executed = true;
                            let payload_bytes = HarnessExecutor.execute(&spec)
                                .map_err(|e| anyhow!("local execute: {e}"))?;
                            let meta = ResultMeta {
                                verifier_method: VerifierMethod::Deterministic,
                                execution_time_ms: 1,
                                model_used: Some("harness-sha256".into()),
                                inference_token_count: None,
                            };
                            let (result, _h) = JobResult::new_signed(
                                hash,
                                node.peer_id,
                                meta,
                                payload_bytes,
                                &node.signing_key,
                            );
                            // Record our own result locally first.
                            node.record_result_locally(&result)?;
                            // Publish on parseh.tasks.v1 with TAG_JOB_RESULT.
                            let mut envelope = vec![TAG_JOB_RESULT];
                            envelope.extend(parseh_task::to_cbor_bytes(&result)?);
                            let topic = gossipsub::IdentTopic::new(TOPIC_TASKS);
                            match swarm.behaviour_mut().gossipsub.publish(topic, envelope) {
                                Ok(_) => info!(
                                    role = ?node.role,
                                    peer = %node.peer_id,
                                    %hash,
                                    "published JobResult"
                                ),
                                Err(e) => warn!(error = %e, "publish JobResult"),
                            }
                        }
                    }
                }
            }
            TAG_JOB_RESULT => {
                let result: JobResult = parseh_task::from_cbor_bytes(body)
                    .map_err(|e| anyhow!("decode JobResult: {e}"))?;
                let exec_pk = node
                    .directory
                    .get(&result.executor)
                    .ok_or_else(|| anyhow!("executor pubkey missing"))?;
                result.verify_signature(&exec_pk)
                    .map_err(|e| anyhow!("bad JobResult signature: {e}"))?;
                debug!(role = ?node.role, peer = %node.peer_id, "observed JobResult");
                node.record_result_locally(&result)?;

                // Decide whether this node verifies. Production V0.2
                // applies Rule 3a (submitter does not verify its own
                // task) and Rule 3b (executor does not verify its own
                // result), see `docs/v0-2/architecture-and-state-
                // machines.md` §3.2. With 3 nodes and `M=2`, both
                // rules together leave only one eligible verifier,
                // which never closes the quorum.
                //
                // The harness relaxes **Rule 3a**: the submitter is
                // allowed to verify in the testnet, treated as just
                // another verifier. Rule 3b is preserved (the
                // executor never verifies its own result). This is
                // the smallest deviation that lets a 3-node mesh prove
                // the M-of-N flow primitive; production V0.2 holds
                // both rules and the same flow runs unchanged on a
                // ≥5-node deployment.
                let spec_hash = result.spec_hash;
                let Some(task) = node.tasks.get(&spec_hash) else {
                    return Ok(());
                };
                if task.self_verified {
                    return Ok(());
                }
                if result.executor == node.peer_id {
                    return Ok(());
                }
                let spec_clone = task.spec.clone();
                let signing_key = node.signing_key.clone();
                let local_peer_id = node.peer_id;
                let local_rep = node.local_reputation.max(parseh_verify::params::PROBATIONARY_REP_FLOOR);
                let _ = task;
                let cfg = SelectionConfig {
                    local_peer_id,
                    local_reputation: local_rep,
                    network_avg_reputation: 100,
                    rate_limit: RateLimit::v0_2_defaults(),
                    already_verified_this_task: false,
                };
                let verifier = Verifier::new(local_peer_id, signing_key, cfg);
                let method = DeterministicMethod::new(HarnessExecutor);

                // Drive selection deterministically: a seed of 0 lands
                // inside p_max=0.5 for some peers, not for others. We
                // bypass the dice roll for the acceptance test by
                // skipping the selection wrapper and calling the method
                // directly — this is documented as test-only below.
                //
                // Why bypass? Self-selection is statistical (`P_BASE=0.05`).
                // With 3 nodes and `M=2`, we need *both* non-executor
                // peers to verify; the dice roll on a low-reputation
                // node would skip almost every spec. The harness asserts
                // the FLOW (sign → verify → quorum → outcome → reputation
                // gossip), not the dice roll — which has its own unit
                // tests in `parseh-verify`.
                let outcome = method
                    .verify(&spec_clone, &result)
                    .map_err(|e| anyhow!("verify: {e}"))?;
                let verdict = if outcome.matched {
                    VerifierVerdict::Agreed
                } else {
                    VerifierVerdict::Disagreed {
                        evidence_hash: outcome.evidence_hash.unwrap_or_default(),
                    }
                };
                let (verification, _h) = JobVerification::new_signed(
                    result.content_hash(),
                    verifier.local_peer_id,
                    verdict,
                    VerifierMethod::Deterministic,
                    &verifier.local_signing_key,
                );
                // Mark ourselves as having verified.
                if let Some(t) = node.tasks.get_mut(&spec_hash) {
                    t.self_verified = true;
                }
                // Record locally first, then publish.
                node.record_verification_locally(&verification)?;
                let topic = gossipsub::IdentTopic::new(TOPIC_VERIFY);
                let bytes = parseh_task::to_cbor_bytes(&verification)?;
                match swarm.behaviour_mut().gossipsub.publish(topic, bytes) {
                    Ok(_) => info!(
                        role = ?node.role,
                        peer = %node.peer_id,
                        result_hash = %result.content_hash(),
                        "published JobVerification"
                    ),
                    Err(e) => warn!(error = %e, "publish JobVerification"),
                }
                let _ = VerifyOutcome::Agreed(verification);
            }
            TAG_JOB_OUTCOME => {
                let outcome: JobOutcome = parseh_task::from_cbor_bytes(body)
                    .map_err(|e| anyhow!("decode JobOutcome: {e}"))?;
                let observer_pk = node
                    .directory
                    .get(&outcome.observed_by)
                    .ok_or_else(|| anyhow!("observer pubkey missing"))?;
                outcome.verify_signature(&observer_pk)
                    .map_err(|e| anyhow!("bad JobOutcome signature: {e}"))?;
                node.record_outcome_locally(&outcome)?;
            }
            other => {
                debug!(tag = other, "unknown tag on parseh.tasks.v1");
            }
        }
        return Ok(());
    }

    if topic_str == TOPIC_VERIFY {
        let verification: JobVerification = parseh_task::from_cbor_bytes(payload)
            .map_err(|e| anyhow!("decode JobVerification: {e}"))?;
        let verifier_pk = node
            .directory
            .get(&verification.verifier)
            .ok_or_else(|| anyhow!("verifier pubkey missing"))?;
        verification
            .verify_signature(&verifier_pk)
            .map_err(|e| anyhow!("bad JobVerification signature: {e}"))?;
        debug!(role = ?node.role, peer = %node.peer_id, "observed JobVerification");
        node.record_verification_locally(&verification)?;
        // Check whether finalising this verification produced an outcome we
        // should broadcast.
        // Find the spec_hash whose result == verification.result_hash.
        let mut spec_hash_to_publish: Option<ContentHash> = None;
        for (sh, t) in node.tasks.iter() {
            if t.finalised {
                if let Some(observed) = &t.observed_result {
                    if observed.content_hash() == verification.result_hash {
                        spec_hash_to_publish = Some(*sh);
                        break;
                    }
                }
            }
        }
        if let Some(sh) = spec_hash_to_publish {
            publish_outcome_and_reputation(node, swarm, sh)?;
        }
        return Ok(());
    }

    if topic_str == TOPIC_STATE_DELTAS {
        let delta: StateDelta = parseh_shared_state::StateDelta::decode_cbor(payload)
            .map_err(|e| anyhow!("decode StateDelta: {e}"))?;
        let observer_pk = node
            .directory
            .get(&delta.observer)
            .ok_or_else(|| anyhow!("delta observer pubkey missing"))?;
        // Apply through the shared-state API to exercise its
        // signature-verification path.
        match &delta.kind {
            DeltaKind::Outcome(o) => {
                let hash = o.spec_hash;
                if let Err(e) = node.shared.apply_delta(delta.clone(), &observer_pk) {
                    warn!(error = %e, "apply_delta(Outcome) failed");
                } else {
                    debug!(role = ?node.role, peer = %node.peer_id, %hash, "applied outcome delta");
                }
            }
            DeltaKind::Reputation {
                peer,
                delta: d,
                reason,
                related_hash,
            } => {
                // Apply through our idempotent helper, so we do not
                // double-count if multiple peers re-publish the same
                // reward.
                node.apply_reputation_locally(*peer, *d, reason, *related_hash)?;
                // Also exercise the shared-state envelope path for at
                // least one delta — verifies the signature, drops the
                // result since our local helper already wrote.
                if let Err(e) = parseh_shared_state::verify_delta(&delta, &observer_pk) {
                    warn!(error = %e, "verify_delta(Reputation) failed");
                }
            }
            DeltaKind::GovernanceRule { .. } => { /* unused in this test */ }
        }
        return Ok(());
    }

    debug!(topic = %topic_str, "message on unrecognised topic");
    Ok(())
}

/// Once a node has finalised an outcome locally, publish the outcome
/// on `parseh.state-deltas.v1` and emit a reputation delta for the
/// executor.
fn publish_outcome_and_reputation(
    node: &mut NodeLoop,
    swarm: &mut Swarm<TestBehaviour>,
    spec_hash: ContentHash,
) -> anyhow::Result<()> {
    let task = match node.tasks.get(&spec_hash) {
        Some(t) => t,
        None => return Ok(()),
    };
    if !task.finalised {
        return Ok(());
    }
    let Some(result) = task.observed_result.clone() else {
        return Ok(());
    };
    let executor = result.executor;

    // Fetch the local outcome from shared state.
    let outcome = match node
        .shared
        .outcome_for_spec(&spec_hash)
        .map_err(|e| anyhow!("outcome_for_spec: {e}"))?
    {
        Some(o) => o,
        None => return Ok(()),
    };

    // Only emit rewards for Valid outcomes.
    let is_valid = matches!(outcome.verdict, OutcomeVerdict::Valid { .. });

    let now = now_unix();
    let topic = gossipsub::IdentTopic::new(TOPIC_STATE_DELTAS);

    // Reputation delta — applied locally first.
    if is_valid {
        node.apply_reputation_locally(
            executor,
            REPUTATION_AWARD_EXECUTOR,
            "executor_consensus_reward",
            Some(outcome.content_hash()),
        )?;
    }

    // Sign + publish the outcome delta.
    let outcome_delta = StateDelta::unsigned(
        DeltaKind::Outcome(outcome.clone()),
        node.peer_id,
        now,
    );
    let signed = sign_delta(outcome_delta, &node.signing_key)
        .map_err(|e| anyhow!("sign outcome delta: {e}"))?;
    let bytes = signed
        .encode_cbor()
        .map_err(|e| anyhow!("encode outcome delta: {e}"))?;
    if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), bytes) {
        warn!(error = %e, "publish outcome delta");
    }

    // Sign + publish the reputation delta.
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
        let signed_rep = sign_delta(rep_delta, &node.signing_key)
            .map_err(|e| anyhow!("sign rep delta: {e}"))?;
        let rep_bytes = signed_rep
            .encode_cbor()
            .map_err(|e| anyhow!("encode rep delta: {e}"))?;
        if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic, rep_bytes) {
            warn!(error = %e, "publish reputation delta");
        } else {
            info!(
                role = ?node.role,
                peer = %node.peer_id,
                executor = %executor,
                "published executor reward delta"
            );
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_node(
    role: NodeRole,
    peer_id: PeerId,
    libp2p_keypair: Keypair,
    signing_key: SigningKey,
    shared: SharedState,
    quorum_config: QuorumConfig,
    directory: PeerKeyDirectory,
    mut cmd_rx: mpsc::Receiver<NodeCommand>,
) -> anyhow::Result<()> {
    let mut swarm = build_swarm(libp2p_keypair.clone())?;

    let listen_addr: Multiaddr = "/memory/0".parse().expect("valid memory multiaddr");
    swarm
        .listen_on(listen_addr)
        .context("listen on /memory/0")?;

    // Subscribe to the three V0.2 topics.
    for topic_name in [TOPIC_TASKS, TOPIC_VERIFY, TOPIC_STATE_DELTAS] {
        let topic = gossipsub::IdentTopic::new(topic_name);
        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&topic)
            .with_context(|| format!("subscribe to {topic_name}"))?;
    }

    let mut node = NodeLoop {
        role,
        peer_id,
        libp2p_keypair,
        signing_key,
        shared,
        quorum_config,
        directory,
        tasks: HashMap::new(),
        local_reputation: 100,
        listen_addr: None,
        reputation_local: HashMap::new(),
        applied_reputation_keys: HashSet::new(),
    };

    // Periodic finalisation tick. The quorum's `t_min` window prevents
    // immediate finalisation — if all M verifications arrive inside
    // `t_min`, the only event that re-triggers `try_finalise_quorum`
    // is either the next verification (which never comes when we
    // already have M-of-N) or a deliberate tick like this one. Cadence:
    // every 100 ms, well below the reduced `t_min = 200 ms` so the
    // first eligible finalisation fires within ~100 ms of t_min.
    let mut finalise_tick = tokio::time::interval(Duration::from_millis(100));
    finalise_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;

            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    NodeCommand::Submit(spec, reply) => {
                        let res = (|| -> anyhow::Result<()> {
                            let hash = spec.content_hash();
                            node.record_spec_locally(&spec)?;
                            let mut envelope = vec![TAG_JOB_SPEC];
                            envelope.extend(parseh_task::to_cbor_bytes(&spec)?);
                            let topic = gossipsub::IdentTopic::new(TOPIC_TASKS);
                            swarm
                                .behaviour_mut()
                                .gossipsub
                                .publish(topic, envelope)
                                .map_err(|e| anyhow!("publish JobSpec: {e}"))?;
                            info!(role = ?node.role, peer = %node.peer_id, %hash, "submitted JobSpec");
                            Ok(())
                        })();
                        let _ = reply.send(res);
                    }
                    NodeCommand::Snapshot(reply) => {
                        let _ = reply.send(node.snapshot());
                    }
                    NodeCommand::Dial(addr, reply) => {
                        let res = swarm
                            .dial(addr.clone())
                            .map_err(|e| anyhow!("dial {addr}: {e}"));
                        let _ = reply.send(res);
                    }
                    NodeCommand::ListenAddr(reply) => {
                        let _ = reply.send(node.listen_addr.clone());
                    }
                    NodeCommand::ConnectedCount(reply) => {
                        let _ = reply.send(swarm.connected_peers().count());
                    }
                    NodeCommand::Shutdown(reply) => {
                        let _ = reply.send(());
                        break;
                    }
                }
            }

            _ = finalise_tick.tick() => {
                // Walk every open quorum and try to close it. If any
                // close, publish their outcome + reputation deltas.
                let mut to_publish: Vec<ContentHash> = Vec::new();
                let pending: Vec<ContentHash> = node.tasks.iter()
                    .filter(|(_, t)| !t.finalised && t.quorum.is_some())
                    .map(|(h, _)| *h)
                    .collect();
                for sh in pending {
                    let was_finalised = node.tasks.get(&sh).map(|t| t.finalised).unwrap_or(false);
                    if was_finalised {
                        continue;
                    }
                    if let Err(e) = node.try_finalise_quorum(sh) {
                        warn!(error = %e, "try_finalise_quorum");
                        continue;
                    }
                    if node.tasks.get(&sh).map(|t| t.finalised).unwrap_or(false) && !was_finalised {
                        to_publish.push(sh);
                    }
                }
                for sh in to_publish {
                    if let Err(e) = publish_outcome_and_reputation(&mut node, &mut swarm, sh) {
                        warn!(error = %e, "publish_outcome_and_reputation");
                    }
                }
            }

            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        info!(role = ?node.role, peer = %node.peer_id, %address, "memory listen");
                        node.listen_addr = Some(address);
                    }
                    SwarmEvent::ConnectionEstablished { peer_id: pid, .. } => {
                        debug!(role = ?node.role, peer = %node.peer_id, remote = %pid, "connected");
                    }
                    SwarmEvent::Behaviour(TestBehaviourEvent::Gossipsub(
                        gossipsub::Event::Message { message, .. },
                    )) => {
                        if let Err(e) =
                            dispatch_message(&mut node, &mut swarm, message.topic, &message.data).await
                        {
                            warn!(error = %e, "dispatch_message");
                        }
                    }
                    SwarmEvent::Behaviour(TestBehaviourEvent::Gossipsub(
                        gossipsub::Event::Subscribed { peer_id: pid, topic },
                    )) => {
                        trace!(role = ?node.role, peer = %node.peer_id, remote = %pid, %topic, "remote subscribed");
                    }
                    _ => {}
                }
            }
        }
    }

    info!(role = ?node.role, peer = %node.peer_id, "node loop ending");
    Ok(())
}

fn build_swarm(libp2p_keypair: Keypair) -> anyhow::Result<Swarm<TestBehaviour>> {
    let peer_id = PeerId::from(libp2p_keypair.public());

    // Memory transport with noise + yamux upgrades. The harness uses
    // `MemoryTransport` (not TCP) so the test never touches the OS
    // network stack — important on CI runners with locked-down
    // sandboxes.
    let transport = MemoryTransport::default()
        .upgrade(upgrade::Version::V1)
        .authenticate(noise::Config::new(&libp2p_keypair).map_err(|e| anyhow!("noise: {e}"))?)
        .multiplex(yamux::Config::default())
        .boxed();

    // Gossipsub with tightened parameters so the 3-node mesh actually
    // forms. Defaults assume mesh_n_low=5 which is impossible with 3
    // total peers; we shrink the mesh to fit. We also shorten the
    // heartbeat so quorum windows close inside the test's timeout.
    let gossipsub_cfg = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_millis(200))
        .heartbeat_initial_delay(Duration::from_millis(50))
        .mesh_n(2)
        .mesh_n_low(1)
        .mesh_n_high(3)
        .mesh_outbound_min(1)
        .validation_mode(gossipsub::ValidationMode::Strict)
        // We allow self-origin so the publishing node treats its own
        // outbound message as a fan-in — gossipsub by default does not
        // deliver back to the publisher, but the test relies on the
        // publisher's local state matching what its peers see, and the
        // publisher already applies its own data via direct calls. Set
        // to true so any "did the message land in our delivery loop"
        // diagnostics still trip.
        .allow_self_origin(true)
        .build()
        .map_err(|e| anyhow!("gossipsub config: {e}"))?;
    let gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(libp2p_keypair.clone()),
        gossipsub_cfg,
    )
    .map_err(|e| anyhow!("gossipsub behaviour: {e}"))?;

    let behaviour = TestBehaviour { gossipsub };

    let swarm = Swarm::new(
        transport,
        behaviour,
        peer_id,
        libp2p::swarm::Config::with_tokio_executor().with_idle_connection_timeout(Duration::from_secs(60)),
    );

    Ok(swarm)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// Silence unused-field warning for `libp2p_keypair` on NodeLoop — it
// is kept on the loop state for V0.3 re-publish flows that need to
// re-sign on rotation.
#[allow(dead_code)]
fn _suppress_node_loop_libp2p_keypair_warning(loop_: NodeLoop) {
    let _ = loop_.libp2p_keypair;
}
