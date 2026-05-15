//! `Scenario` — orchestrates a 3-node testnet and asserts the V0.2
//! coordination flow.
//!
//! The scenario:
//!
//! 1. Spawns N (= 3 for the acceptance test) [`TestNode`]s, each on its
//!    own `MemoryTransport` listener.
//! 2. Exchanges peer pubkeys via the shared [`PeerKeyDirectory`] so
//!    every node can verify signed envelopes it receives.
//! 3. Dials each pair so the gossipsub mesh forms a triangle (every
//!    node has every other node as a peer).
//! 4. Polls until every node reports 2 peers — the mesh-formation
//!    condition the acceptance test depends on.
//!
//! The scenario then exposes `submit_from`, `await_outcome`,
//! `await_state_predicate`, and `dump_state` helpers the test uses for
//! assertions.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use libp2p::Multiaddr;
use parseh_task::{ContentHash, JobOutcome, JobSpec};
use parseh_verify::QuorumConfig;
use thiserror::Error;
use tokio::time::sleep;
use tracing::{debug, info};

use crate::node::{spawn, NodeRole, PeerKeyDirectory, StateSnapshot, TestNode};

/// Errors a [`Scenario`] surfaces.
#[derive(Error, Debug)]
pub enum ScenarioError {
    /// A predicate did not become true within the supplied timeout.
    #[error("timed out waiting for: {reason}")]
    Timeout {
        /// What we were waiting for, for diagnostics.
        reason: String,
    },
    /// Node count mismatch — the caller asked for an index that does
    /// not exist.
    #[error("no node at index {0}")]
    NoSuchNode(usize),
    /// Bubbled-up anyhow.
    #[error("scenario error: {0}")]
    Other(#[from] anyhow::Error),
}

/// One in-process testnet.
pub struct Scenario {
    nodes: Vec<Arc<TestNode>>,
    #[allow(dead_code)]
    directory: PeerKeyDirectory,
}

impl Scenario {
    /// Construct an `n`-node scenario with a reduced (M=2/N=3) quorum
    /// suitable for the acceptance test. The mesh is fully connected
    /// (each node is dialled to every other node).
    ///
    /// **Documented test-only quorum reduction:** V0.2 production uses
    /// `M=5/N=9` per the project notes §3.1. With 3
    /// nodes that is unsatisfiable; this constructor injects a
    /// M=2/N=3, `t_min=200ms` variant. The protocol primitive proven
    /// here is the *flow*, not the parameter sweep — load tests at
    /// production parameters require ≥9 nodes and live in a follow-up
    /// PR.
    pub async fn new(n: usize) -> Result<Self, ScenarioError> {
        let reduced = reduced_quorum_for_test();
        Self::with_quorum(n, reduced).await
    }

    /// Like [`Self::new`] but lets the caller supply the [`QuorumConfig`].
    pub async fn with_quorum(n: usize, quorum: QuorumConfig) -> Result<Self, ScenarioError> {
        if n < 2 {
            return Err(ScenarioError::Other(anyhow!(
                "Scenario requires at least 2 nodes"
            )));
        }
        let directory = PeerKeyDirectory::default();

        // Spawn all nodes.
        let mut nodes = Vec::with_capacity(n);
        for i in 0..n {
            let role = match i {
                0 => NodeRole::Submitter,
                1 => NodeRole::Executor,
                _ => NodeRole::Verifier,
            };
            let node = spawn(role, quorum, directory.clone())
                .await
                .context("spawn TestNode")?;
            // Eagerly insert the node's verifying key in case the helper
            // path above missed it.
            directory.insert(node.peer_id, node.signing_key.verifying_key());
            nodes.push(Arc::new(node));
        }

        // Wait for each node's MemoryTransport listener to publish its
        // address, then dial the full triangle.
        let listen_addrs = collect_listen_addrs(&nodes, Duration::from_secs(5)).await?;

        // Connect each pair (skip self).
        for (i, node) in nodes.iter().enumerate() {
            for (j, addr) in listen_addrs.iter().enumerate() {
                if i == j {
                    continue;
                }
                node.dial(addr.clone())
                    .await
                    .with_context(|| format!("dial node {j} from node {i}"))?;
            }
        }

        // Wait for the mesh to form: every node must report (n-1) peers
        // AND the gossipsub heartbeat must have fired at least once so
        // mesh peers exchange GRAFT messages. The libp2p memory transport
        // accepts the dial immediately, but gossipsub mesh formation
        // depends on heartbeat ticks (200ms in our config) — see the
        // node.rs build_swarm comment about heartbeat tuning.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut ok = true;
            for node in &nodes {
                let count = node.connected_count().await;
                if count < n - 1 {
                    ok = false;
                    break;
                }
            }
            if ok {
                break;
            }
            if Instant::now() > deadline {
                return Err(ScenarioError::Timeout {
                    reason: format!("mesh formation (need each node to see {} peers)", n - 1),
                });
            }
            sleep(Duration::from_millis(50)).await;
        }
        // Give gossipsub at least three heartbeat ticks to GRAFT before
        // any publishes — see TROUBLESHOOTING in node.rs.
        sleep(Duration::from_millis(800)).await;

        info!(node_count = n, "scenario ready · mesh formed");
        Ok(Self { nodes, directory })
    }

    /// Borrow the node at `idx`.
    pub fn node(&self, idx: usize) -> &TestNode {
        &self.nodes[idx]
    }

    /// Submit a `JobSpec` from `idx`.
    pub async fn submit_from(&self, idx: usize, spec: JobSpec) -> Result<(), ScenarioError> {
        self.nodes
            .get(idx)
            .ok_or(ScenarioError::NoSuchNode(idx))?
            .submit(spec)
            .await?;
        Ok(())
    }

    /// Snapshot of node `idx`'s shared state.
    pub async fn dump_state(&self, idx: usize) -> StateSnapshot {
        self.nodes[idx].snapshot().await
    }

    /// Block until every node satisfies `predicate(&snapshot)`. Polls
    /// at 50 ms cadence; returns [`ScenarioError::Timeout`] after
    /// `timeout`.
    pub async fn await_state_predicate<F>(
        &self,
        predicate: F,
        timeout: Duration,
        reason: &str,
    ) -> Result<(), ScenarioError>
    where
        F: Fn(&StateSnapshot) -> bool,
    {
        let deadline = Instant::now() + timeout;
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
            if Instant::now() > deadline {
                return Err(ScenarioError::Timeout {
                    reason: reason.to_string(),
                });
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait until at least one node observes a finalised outcome for
    /// `spec_hash`. Returns the observed outcome.
    pub async fn await_outcome(
        &self,
        spec_hash: ContentHash,
        timeout: Duration,
    ) -> Result<JobOutcome, ScenarioError> {
        let deadline = Instant::now() + timeout;
        loop {
            for node in &self.nodes {
                let snap = node.snapshot().await;
                if let Some(o) = snap.outcomes.get(&spec_hash) {
                    return Ok(o.clone());
                }
            }
            if Instant::now() > deadline {
                return Err(ScenarioError::Timeout {
                    reason: format!("outcome for spec_hash={spec_hash}"),
                });
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Number of nodes in the scenario.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the scenario is empty (false by construction at V0.2).
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Shut down all nodes cleanly.
    pub async fn shutdown(self) {
        for node in self.nodes.iter() {
            node.shutdown().await;
        }
    }
}

/// Reduced quorum used by the harness's `Scenario::new`.
///
/// Calls out the divergence from V0.2 production values:
/// - `m = 2` (vs `M_STANDARD = 5`)
/// - `n = 3` (vs `N_STANDARD = 9`)
/// - `t_min = 200 ms` (vs `T_MIN_SECS = 5`)
/// - `t_max = 30 s` (unchanged)
/// - `rep_weighted_threshold = 0.6` (unchanged)
pub fn reduced_quorum_for_test() -> QuorumConfig {
    QuorumConfig {
        m: 2,
        n: 3,
        t_min: Duration::from_millis(200),
        t_max: Duration::from_secs(30),
        rep_weighted_threshold: 0.6,
    }
}

/// Poll each node's `listen_addr` until it reports its `MemoryTransport`
/// listen address. Returns the addresses in node-index order.
async fn collect_listen_addrs(
    nodes: &[Arc<TestNode>],
    timeout: Duration,
) -> Result<Vec<Multiaddr>, ScenarioError> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut addrs: Vec<Option<Multiaddr>> = Vec::with_capacity(nodes.len());
        for node in nodes {
            addrs.push(node.listen_addr().await);
        }
        if addrs.iter().all(|a| a.is_some()) {
            let out = addrs.into_iter().map(|a| a.unwrap()).collect();
            return Ok(out);
        }
        if Instant::now() > deadline {
            return Err(ScenarioError::Timeout {
                reason: "memory listen addresses".into(),
            });
        }
        debug!("waiting for listen addresses to bind");
        sleep(Duration::from_millis(50)).await;
    }
}
