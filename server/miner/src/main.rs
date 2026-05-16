//! `parseh-miner` — autonomous PARSEH service-providing node.
//!
//! What this binary does, end-to-end:
//!
//!   1. Loads or generates a persistent ed25519 identity at the OS's
//!      conventional config dir (Windows: %APPDATA%\PARSEH, Linux:
//!      $XDG_CONFIG_HOME/parseh, macOS: ~/Library/Application Support/parseh).
//!
//!   2. Reads (or writes a default) `miner.toml` next to the identity. The
//!      config declares which services this node provides and the
//!      capability values to advertise.
//!
//!   3. Opens (or creates) the `parseh-shared-state` SQLite database
//!      (`~/.parseh/shared-state.db` by default; overridable via
//!      `--shared-state-db PATH`). The SQLCipher key in V0.2.1 is derived
//!      from the libp2p identity file via `KeySource::IdentityFile` —
//!      strictly weaker than a user passphrase, and V0.3+ MUST migrate
//!      to a passphrase unlock passphrase unlock. See the README.
//!
//!   4. Spawns a libp2p swarm with:
//!        - Noise (encrypted handshake)
//!        - Yamux (multiplexed streams)
//!        - Ping (RTT health)
//!        - Identify (protocol-version exchange — `/parseh/0.2.0`)
//!        - Kademlia DHT (peer discovery — no bootstrap servers needed once
//!          a peer is seen)
//!        - Gossipsub on FOUR topics:
//!            * `parseh.caps.v1`         (capability advertisements)
//!            * `parseh.tasks.v1`        (`JobSpec` announcements)
//!            * `parseh.verify.v1`       (`JobResult` + `JobVerification`)
//!            * `parseh.state-deltas.v1` (signed `StateDelta` envelopes)
//!        - Request-response for TWO protocols:
//!            * `/parseh/job/1.0.0` (V0.1 legacy `JobOrder`)
//!            * `/parseh/job/2.0.0` (V0.2 `JobSpec`)
//!
//!   5. Spawns a periodic finalisation tick (100 ms cadence) that walks
//!      every open `Quorum`, calls `try_finalise(now, peer, key)`, and on
//!      success: persists the `JobOutcome` to shared state, publishes it
//!      as a signed `StateDelta` on `parseh.state-deltas.v1`, and emits
//!      reputation deltas for the executor + agreeing verifiers per
//!      `verifier-economics.md` §4.
//!
//!      The tick is **load-bearing**: the testnet acceptance run
//!      uncovered that pure event-driven finalisation deadlocks when
//!      every verification arrives inside the `t_min` window. See
//!      `topics::FINALISE_TICK_MS`.
//!
//!   6. Periodically (default: every 6 hours) checks the latest GitHub
//!      release tag for `hiderun-tui/parseh` and logs a notice if a newer
//!      version is available.
//!
//! Anything marked TODO_V0_3 is wiring that requires the on-chain
//! reputation projection.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]
// Pre-existing structural lints retained from V0.1; see commit history.
#![allow(
    clippy::ptr_arg,
    clippy::collapsible_match,
    clippy::too_many_arguments,
    dead_code
)]

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use libp2p::{
    gossipsub, identify, identity, kad, ping, request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
    Multiaddr, PeerId, StreamProtocol, SwarmBuilder,
};
use lru::LruCache;
use parking_lot::Mutex;
use serde::Serialize;
use tracing::{info, warn};

mod config;
mod executor;
mod identity_store;
mod orders;
mod proxy;
mod readiness;
mod readiness_state;
mod topics;
mod update_check;
mod verify_buffer;

use crate::config::MinerConfig;
use crate::executor::Executor;
use crate::orders::{JobOrder, JobResult as V1JobResult};
use crate::readiness::ReadinessReport;
use crate::readiness_state::ReadinessTracker;
use crate::topics::{
    DEFAULT_SHARED_STATE_FILENAME, FINALISE_TICK_MS, PARSEH_JOB_PROTOCOL_V1, PARSEH_JOB_PROTOCOL_V2,
    PARSEH_PROTOCOL_VERSION, PARSEH_STATE_SYNC_PROTOCOL_V1, STATE_SYNC_BACKSTOP_INTERVAL_SECS,
    STATE_SYNC_BACKSTOP_LOOKBACK_SECS, STATE_SYNC_ISOLATION_THRESHOLD_SECS,
    STATE_SYNC_MAX_PARTITION_WINDOW_SECS, TAG_JOB_RESULT, TAG_JOB_VERIFICATION, TOPIC_CAPS,
    TOPIC_STATE_DELTAS, TOPIC_TASKS, TOPIC_VERIFY,
};
use crate::verify_buffer::{PendingMessage, VerifyBuffer};
use parseh_core::{
    peer_registry::{
        encode_advertisement, CapabilityAdvertisement, ReadinessState, ServiceKind,
        CAPS_WIRE_VERSION,
    },
    PeerRegistry,
};
use parseh_llm_detect::{DetectionResult, RecommendedRuntime};
use parseh_shared_state::{
    sign_delta as sign_state_delta, DeltaKind, KeyMaterial, KeySource, OpenOptions, SharedState,
    StateDelta,
};
use parseh_task::{
    from_cbor_bytes, ContentHash, JobResult as TaskJobResult, JobSpec, JobVerification,
    StateSyncRequest, StateSyncResponse, STATE_SYNC_HARD_CEILING,
};
use parseh_verify::{Quorum, QuorumConfig};

// ───── V0.2.5 tuning constants ──────────────────────────────────────────────

/// Maximum number of open quorums (keyed by `result_hash`) the node will
/// retain in memory. Larger floods evict the oldest non-finalised entry.
///
/// Closes V0.2.1 residual #3: an attacker publishing N distinct
/// JobResults could previously OOM a target node via the unbounded
/// `HashMap<ContentHash, OpenQuorum>`. Tuned at 10_000 so 100 specs
/// per second across 30 verifiers per spec stays well under the cap.
const OPEN_QUORUMS_LRU_CAP: usize = 10_000;

/// Per-publisher rolling window for the `JobResult` rate limit.
const RATE_LIMIT_WINDOW_MS: u64 = 60_000;

/// Max `JobResult` messages from a single publisher inside the rolling
/// [`RATE_LIMIT_WINDOW_MS`]. Floods past this are dropped at WARN.
const RATE_LIMIT_MAX_RESULTS_PER_WINDOW: u32 = 100;

/// Max `/parseh/state-sync/1.0.0` requests a single requester may have
/// answered inside [`RATE_LIMIT_WINDOW_MS`]. A reconnecting peer needs
/// only one or two catch-up rounds; anything beyond this in a 60 s
/// window is abusive and is refused with an empty response.
const STATE_SYNC_MAX_REQUESTS_PER_WINDOW: u32 = 6;

/// Cadence of the buffered-message retry tick. Each tick walks the
/// [`VerifyBuffer`] and dispatches any messages whose publisher key is
/// now in the registry; expired entries are dropped.
const VERIFY_BUFFER_TICK_MS: u64 = 250;

/// Compact rate-limit bucket per publisher.
#[derive(Debug, Clone, Default)]
struct RateBucket {
    /// Wall-clock seconds at which the current window started.
    window_started_at: u64,
    /// Count of `JobResult` messages observed inside the current window.
    count: u32,
}

impl RateBucket {
    /// Returns `true` iff this publisher exceeded the rate cap. Side
    /// effect: rolls the window forward when it expires.
    fn record(&mut self, now_secs: u64) -> bool {
        self.record_with_cap(now_secs, RATE_LIMIT_MAX_RESULTS_PER_WINDOW)
    }

    /// Generic form — same rolling-window logic against an arbitrary
    /// per-window `cap`. Reused by the `/parseh/state-sync/1.0.0`
    /// responder so the anti-entropy path gets the exact same
    /// per-source rate-limiter pattern as the `JobResult` path.
    fn record_with_cap(&mut self, now_secs: u64, cap: u32) -> bool {
        let window_secs = RATE_LIMIT_WINDOW_MS / 1_000;
        if now_secs.saturating_sub(self.window_started_at) >= window_secs {
            self.window_started_at = now_secs;
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
        self.count > cap
    }
}

/// Tracks connectivity so the node can tell when it has just recovered
/// from an isolation window (zero connected peers for ≥
/// [`STATE_SYNC_ISOLATION_THRESHOLD_SECS`]). Recovering from such a gap
/// is the primary trigger for a `/parseh/state-sync/1.0.0` catch-up
/// pull — the chaos harness proved gossipsub alone cannot replay the
/// missed partition window.
#[derive(Debug)]
struct IsolationTracker {
    /// Number of live connections (sum over peers).
    connected: u32,
    /// Wall-clock seconds at which `connected` last dropped to zero.
    /// `None` while at least one peer is connected.
    zero_since: Option<u64>,
}

impl IsolationTracker {
    fn new() -> Self {
        // A freshly-started node has no peers and no history; treat it
        // as "isolated since now" so the very first reconnection also
        // triggers a catch-up (a just-joined node is, for state-sync
        // purposes, indistinguishable from a reconnecting one).
        Self {
            connected: 0,
            zero_since: Some(unix_now()),
        }
    }

    /// Record a new connection. Returns `Some(gap_secs)` iff this
    /// connection ends an isolation window that lasted at least
    /// [`STATE_SYNC_ISOLATION_THRESHOLD_SECS`] — i.e. the caller should
    /// now issue a catch-up sync request.
    fn on_connection_established(&mut self, now: u64) -> Option<u64> {
        let was_isolated_for = self.zero_since.map(|t| now.saturating_sub(t));
        self.connected = self.connected.saturating_add(1);
        self.zero_since = None;
        match was_isolated_for {
            Some(gap) if gap >= STATE_SYNC_ISOLATION_THRESHOLD_SECS => Some(gap),
            _ => None,
        }
    }

    /// Record a closed connection. When the last connection drops, the
    /// isolation clock starts.
    fn on_connection_closed(&mut self, now: u64) {
        self.connected = self.connected.saturating_sub(1);
        if self.connected == 0 && self.zero_since.is_none() {
            self.zero_since = Some(now);
        }
    }
}

// ───── CLI ──────────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "parseh-miner",
    version,
    about = "PARSEH miner · autonomous service-providing node",
    long_about = "Headless service node for the PARSEH humanitarian network.\n\
                  Discovers peers, advertises capabilities, accepts encrypted\n\
                  job orders, runs V0.2 coordination primitives (signed tasks,\n\
                  M-of-N verification, persisted shared state).\n\n\
                  License: Apache-2.0    Repo: github.com/hiderun-tui/parseh"
)]
struct Cli {
    /// Override the config directory (default: OS-conventional).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,

    /// Increase logging verbosity. Repeat for more (-vv).
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Probe local LLM runtimes (Ollama, llama.cpp, GGUF files) and
    /// print the result as JSON, then exit. Does not start the network.
    #[arg(long, global = true)]
    check_llm: bool,

    /// Print a JSON readiness summary (peer count, LLM, identity, listen
    /// addresses, shared-state surface) after the swarm starts, then exit.
    #[arg(long, global = true)]
    show_readiness: bool,

    /// On first run, automatically download TinyLlama if no local LLM is
    /// found. Treated as implicit consent for the single egress request.
    #[arg(long, global = true)]
    auto_download_llm: bool,

    /// Override the path to the shared-state SQLite database. Defaults
    /// to `<config-dir>/shared-state.db`. The directory is created on
    /// demand.
    #[arg(long, global = true, value_name = "PATH")]
    shared_state_db: Option<PathBuf>,

    /// Wipe the shared-state database after a confirmation prompt
    /// (`y/N` on stdin). Useful for test environments. Refuses to run
    /// unattended unless `--yes` is also passed.
    #[arg(long, global = true)]
    reset_shared_state: bool,

    /// Auto-confirm destructive operations like `--reset-shared-state`.
    /// Intended for CI / scripted test environments.
    #[arg(long, global = true)]
    yes: bool,

    /// Build the swarm + shared-state but exit before entering the main
    /// loop. Useful for first-run setup and CI smoke tests.
    #[arg(long, global = true)]
    init_only: bool,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Generate identity + write a default config + initialise the
    /// shared-state DB, but do not connect.
    Init,
    /// Print the local PeerId and current config path.
    Whoami,
    /// Start the miner. (Default if no subcommand given.)
    Start {
        /// Multiaddr to listen on.
        #[arg(long, default_value = "/ip4/0.0.0.0/tcp/8421")]
        listen: String,
        /// Comma-separated multiaddrs to dial on startup as bootstrap peers.
        #[arg(long, value_delimiter = ',')]
        dial: Vec<String>,
        /// Skip the update-check ping to GitHub.
        #[arg(long)]
        no_update_check: bool,
        /// Expose a local SOCKS5 proxy on `127.0.0.1:<port>` for the
        /// Hiderun browser tunnel. Off by default. Loopback only.
        #[arg(long, value_name = "PORT")]
        socks5: Option<u16>,
    },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    // `--check-llm` is a pure probe; it short-circuits BEFORE we touch
    // identity, config, or libp2p. JSON on stdout for `jq` consumption.
    if cli.check_llm {
        let detection = parseh_llm_detect::detect_all().await?;
        let json = serde_json::to_string_pretty(&detection)
            .context("serialize DetectionResult to JSON")?;
        println!("{json}");
        return Ok(());
    }

    let config_dir = cli
        .config_dir
        .clone()
        .or_else(config::default_config_dir)
        .context("could not determine config directory · pass --config-dir")?;

    let shared_state_path = cli
        .shared_state_db
        .clone()
        .unwrap_or_else(|| config_dir.join(DEFAULT_SHARED_STATE_FILENAME));

    let show_readiness = cli.show_readiness;
    let auto_download_llm = cli.auto_download_llm;
    let init_only = cli.init_only;

    if cli.reset_shared_state {
        reset_shared_state(&shared_state_path, cli.yes)?;
    }

    match cli.command.unwrap_or(Cmd::Start {
        listen: "/ip4/0.0.0.0/tcp/8421".into(),
        dial: vec![],
        no_update_check: false,
        socks5: None,
    }) {
        Cmd::Init => cmd_init(&config_dir, &shared_state_path),
        Cmd::Whoami => cmd_whoami(&config_dir),
        Cmd::Start {
            listen,
            dial,
            no_update_check,
            socks5,
        } => {
            cmd_start(
                &config_dir,
                &shared_state_path,
                &listen,
                dial,
                no_update_check,
                socks5,
                show_readiness,
                auto_download_llm,
                init_only,
            )
            .await
        }
    }
}

fn init_tracing(verbose: u8) {
    let default_level = match verbose {
        0 => "parseh_miner=info,parseh_core=info,parseh_shared_state=info,libp2p=warn",
        1 => "parseh_miner=debug,parseh_core=debug,parseh_shared_state=debug,libp2p=info",
        _ => "parseh_miner=trace,parseh_core=trace,parseh_shared_state=trace,libp2p=debug",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_level.into());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn cmd_init(config_dir: &PathBuf, shared_state_path: &Path) -> Result<()> {
    std::fs::create_dir_all(config_dir).with_context(|| {
        format!("create config dir {}", config_dir.display())
    })?;
    let (kp, created) = identity_store::load_or_generate(config_dir)?;
    let cfg_path = config_dir.join("miner.toml");
    if !cfg_path.exists() {
        config::write_default(&cfg_path)?;
        info!(path = %cfg_path.display(), "wrote default config");
    } else {
        info!(path = %cfg_path.display(), "config exists · not overwritten");
    }

    // Also create the shared-state DB schema so the first
    // `parseh-miner start` is not delayed by a fresh DB build.
    let identity_bytes = read_identity_bytes(config_dir)
        .context("read identity for shared-state key derivation")?;
    let shared = open_shared_state(shared_state_path, &identity_bytes)
        .context("initialise shared-state DB")?;
    drop(shared); // close immediately — Init is non-blocking.

    info!(
        peer_id = %PeerId::from(kp.public()),
        identity_status = if created { "generated" } else { "loaded" },
        shared_state_db = %shared_state_path.display(),
        "init complete · config dir: {}",
        config_dir.display()
    );
    println!("\nNext step: parseh-miner start");
    Ok(())
}

fn cmd_whoami(config_dir: &PathBuf) -> Result<()> {
    let (kp, _) = identity_store::load_or_generate(config_dir)?;
    let peer = PeerId::from(kp.public());
    let cfg_path = config_dir.join("miner.toml");
    let cfg = if cfg_path.exists() {
        MinerConfig::load(&cfg_path)?
    } else {
        MinerConfig::default()
    };
    println!("peer_id    = {peer}");
    println!("config_dir = {}", config_dir.display());
    println!("config     = {}", cfg_path.display());
    println!("moniker    = {}", cfg.moniker);
    println!(
        "services   = relay={} inference={} bandwidth_mbps={}",
        cfg.capabilities.relay, cfg.capabilities.inference, cfg.capabilities.uplink_mbps
    );
    Ok(())
}

async fn cmd_start(
    config_dir: &PathBuf,
    shared_state_path: &Path,
    listen: &str,
    dial: Vec<String>,
    no_update_check: bool,
    socks5_port: Option<u16>,
    show_readiness: bool,
    auto_download_llm: bool,
    init_only: bool,
) -> Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let (kp, _) = identity_store::load_or_generate(config_dir)?;
    let peer_id = PeerId::from(kp.public());

    let cfg_path = config_dir.join("miner.toml");
    if !cfg_path.exists() {
        config::write_default(&cfg_path)?;
    }
    let cfg = MinerConfig::load(&cfg_path)?;
    info!(%peer_id, moniker = %cfg.moniker, "parseh-miner starting");
    info!(
        relay = cfg.capabilities.relay,
        inference = cfg.capabilities.inference,
        gpu_mb = cfg.capabilities.gpu_memory_mb,
        models = ?cfg.capabilities.model_tags,
        "advertised capabilities"
    );

    // V0.2 shared-state — open ALWAYS, even on --init-only. The key is
    // derived from the identity file bytes (KeySource::IdentityFile).
    // V0.3+ will derive it from a user passphrase instead.
    let identity_bytes = read_identity_bytes(config_dir)
        .context("read identity for shared-state key derivation")?;
    let shared = Arc::new(
        open_shared_state(shared_state_path, &identity_bytes)
            .context("open shared-state DB")?,
    );
    info!(path = %shared_state_path.display(), "shared-state opened");

    // V0.2 envelopes sign with the ed25519 signing key derived from the
    // same 32 secret bytes the libp2p identity uses. Keeping them
    // identical (PeerId == dalek::VerifyingKey) removes a whole class
    // of "which key signed this" confusion downstream.
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&identity_bytes);

    // --init-only short-circuit: we exited the cmd_init helper after
    // schema migration. Now `--init-only` does the same plus surfacing
    // a single readiness log line and exits. Used by CI to validate
    // the new V0.2.1 bring-up path.
    if init_only {
        info!(
            %peer_id,
            shared_state_db = %shared_state_path.display(),
            "--init-only · schema initialised · exiting before swarm start"
        );
        return Ok(());
    }

    let mut swarm = build_swarm(kp.clone(), &peer_id)?;
    let listen_addr: Multiaddr = listen.parse().context("invalid --listen multiaddr")?;
    swarm.listen_on(listen_addr.clone())?;
    info!(addr = %listen_addr, "listening");

    // Probe the host for an existing LLM runtime once at startup.
    let detection = parseh_llm_detect::detect_all()
        .await
        .context("LLM runtime probe failed")?;
    handle_llm_detection(&detection, auto_download_llm).await?;

    for raw in &dial {
        match raw.parse::<Multiaddr>() {
            Ok(addr) => match swarm.dial(addr.clone()) {
                Ok(_) => info!(%addr, "dial scheduled"),
                Err(e) => warn!(%addr, error = %e, "dial failed"),
            },
            Err(e) => warn!(input = %raw, error = %e, "ignoring invalid --dial"),
        }
    }

    // Subscribe to ALL FOUR V0.2 gossipsub topics. Logged individually
    // so the smoke test can assert each one is present.
    let caps_topic = subscribe_topic(&mut swarm, TOPIC_CAPS)?;
    let _tasks_topic = subscribe_topic(&mut swarm, TOPIC_TASKS)?;
    let _verify_topic = subscribe_topic(&mut swarm, TOPIC_VERIFY)?;
    let _state_deltas_topic = subscribe_topic(&mut swarm, TOPIC_STATE_DELTAS)?;

    // Optional update-check fires once at startup, then every 6h.
    let update_handle = if !no_update_check {
        Some(tokio::spawn(update_check::run_periodic_check()))
    } else {
        None
    };

    // Optional loopback-only SOCKS5 listener for the Hiderun browser
    // tunnel (loopback IP is hard-coded by policy — see `proxy.rs`).
    let socks5_handle = match socks5_port {
        Some(port) => {
            let listener = proxy::Socks5Listener::loopback(port);
            let addr = listener.addr();
            info!(%addr, "spawning loopback-only SOCKS5 listener");
            Some(tokio::spawn(async move {
                if let Err(e) = proxy::run_socks5(addr).await {
                    warn!(error = %e, "SOCKS5 listener exited with error");
                }
            }))
        }
        None => None,
    };

    // Periodic capability advertisement.
    let mut caps_tick = tokio::time::interval(Duration::from_secs(60));

    // Local cache of capability advertisements heard on `parseh.caps.v1`.
    let peer_registry = PeerRegistry::new();

    // Periodic eviction of expired peer-registry entries.
    let prune_handle = {
        let reg = peer_registry.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let removed = reg.prune_expired(now);
                if removed > 0 {
                    tracing::debug!(removed, remaining = reg.count(), "pruned expired peer ads");
                }
            }
        })
    };

    let executor = executor::default_executor();

    // V0.2 coordination plane: bounded LRU of open quorums keyed by
    // `result_hash`. Each entry tracks the verifications gathered so
    // far + the cached `JobSpec` we need to re-execute for verification.
    //
    // V0.2.5: bounded with [`OPEN_QUORUMS_LRU_CAP`] to close residual #3
    // — an attacker publishing N distinct JobResults could otherwise
    // OOM the node. On insert pressure the LRU evicts the oldest
    // non-finalised entry; that entry's pending verifications lose
    // their finalisation slot until the result re-arrives.
    let quorums: Arc<Mutex<LruCache<ContentHash, OpenQuorum>>> =
        Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(OPEN_QUORUMS_LRU_CAP).expect("OPEN_QUORUMS_LRU_CAP > 0"),
        )));

    // Per-publisher rate limiter for `parseh.verify.v1` `JobResult`
    // messages. Closes the V0.2.5 part of residual #3: bound the rate
    // at which a single publisher can grow our quorum map. See
    // [`RATE_LIMIT_WINDOW_MS`] + [`RATE_LIMIT_MAX_RESULTS_PER_WINDOW`].
    let result_rate_limit: Arc<Mutex<HashMap<PeerId, RateBucket>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Per-requester rate limiter for `/parseh/state-sync/1.0.0`. Same
    // rolling-window pattern as the `JobResult` limiter but with the
    // tighter [`STATE_SYNC_MAX_REQUESTS_PER_WINDOW`] cap — a genuine
    // reconnecting peer needs one or two rounds, not dozens.
    let state_sync_rate_limit: Arc<Mutex<HashMap<PeerId, RateBucket>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Connectivity / isolation tracker. Drives the primary state-sync
    // trigger: ending an isolation window of ≥ 30 s issues a catch-up
    // pull from the best-connected peer we can find.
    let isolation = Arc::new(Mutex::new(IsolationTracker::new()));

    // Inbound buffer for messages whose publisher's verifying key is
    // not yet in the peer-key directory. See `verify_buffer.rs`.
    let verify_buffer = VerifyBuffer::new();

    // Readiness state machine — V0.2.5 closes the gap noted in
    // the project notes §3.4. Starts at
    // `Initialised`; transitions on swarm-up / first-peer / first-cap-
    // published / first-task-accepted events.
    let readiness_tracker = ReadinessTracker::new();
    // We have already built the swarm by this point; reflect that.
    readiness_tracker.mark_connected();

    // Periodic finalise tick. **Critical** — see module docs + the
    // commit message that adds this constant.
    let mut finalise_tick = tokio::time::interval(Duration::from_millis(FINALISE_TICK_MS));
    finalise_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    info!(interval_ms = FINALISE_TICK_MS, "finalise tick scheduled");

    // Periodic anti-entropy backstop. Independently of any isolation
    // event, ask one random Established peer for "anything since
    // now - 10 min" every 5 min — cheap insurance against a silently
    // dropped state-delta. First tick fires immediately on `interval`,
    // so we skip the very first (the node has nothing to catch up on
    // at startup; the isolation trigger covers the just-joined case).
    let mut state_sync_backstop_tick =
        tokio::time::interval(Duration::from_secs(STATE_SYNC_BACKSTOP_INTERVAL_SECS));
    state_sync_backstop_tick
        .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Build a one-shot readiness snapshot for the startup log line and
    // for the optional `--show-readiness` output.
    let open_q_count = quorums.lock().len();
    let readiness = build_readiness(
        peer_id,
        swarm.listeners().map(|a| a.to_string()).collect(),
        peer_registry.count(),
        detection.recommended_runtime(),
        cfg.capabilities.clone(),
        shared_state_path,
        &shared,
        open_q_count,
    )?;

    info!(
        peer_id = %readiness.identity_peer_id,
        services = ?readiness.services_advertised,
        llm = ?readiness.llm_runtime,
        peers_known = readiness.known_peers,
        "PARSEH miner ready"
    );

    if show_readiness {
        let json = serde_json::to_string_pretty(&readiness)
            .context("serialize ReadinessReport to JSON")?;
        println!("{json}");
        prune_handle.abort();
        if let Some(h) = update_handle {
            h.abort();
        }
        if let Some(h) = socks5_handle {
            h.abort();
        }
        return Ok(());
    }

    info!("event loop running · Ctrl-C to stop");

    let local_peer_id = peer_id;
    let signing_key_for_loop = signing_key.clone();
    let local_verifying_key = signing_key.verifying_key();

    // Pre-populate the peer-key directory with our own identity so the
    // first own-publish does not race the swarm-event handler.
    peer_registry.record_identity(parseh_core::PeerIdentity {
        peer_id: local_peer_id,
        verifying_key: local_verifying_key,
        reachable_addrs: swarm.listeners().cloned().collect(),
        first_seen: unix_now(),
        last_seen: unix_now(),
        readiness: readiness_tracker.current(),
    });

    // Buffer-tick cadence — drives `VerifyBuffer::drain_ready`.
    let mut buffer_tick = tokio::time::interval(Duration::from_millis(VERIFY_BUFFER_TICK_MS));
    buffer_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown signal · exiting");
                readiness_tracker.mark_stopped();
                break;
            }

            _ = caps_tick.tick() => {
                publish_caps_v0_2(
                    &mut swarm,
                    &caps_topic,
                    local_peer_id,
                    &cfg,
                    &local_verifying_key,
                    &readiness_tracker,
                );
            }

            _ = finalise_tick.tick() => {
                run_finalise_tick(
                    &mut swarm,
                    &quorums,
                    &shared,
                    local_peer_id,
                    &signing_key_for_loop,
                    &readiness_tracker,
                );
            }

            _ = buffer_tick.tick() => {
                drain_verify_buffer(
                    &mut swarm,
                    &peer_registry,
                    &verify_buffer,
                    &shared,
                    &quorums,
                    &rate_limit_for_dispatch_clone(&result_rate_limit),
                    local_peer_id,
                    &signing_key_for_loop,
                    &readiness_tracker,
                    &cfg,
                    executor.as_ref(),
                ).await;
            }

            _ = state_sync_backstop_tick.tick() => {
                // Anti-entropy backstop: pull "anything since
                // now - 10 min" from one random Established peer.
                let since = unix_now()
                    .saturating_sub(STATE_SYNC_BACKSTOP_LOOKBACK_SECS);
                issue_state_sync_request(
                    &mut swarm,
                    &peer_registry,
                    &shared,
                    local_peer_id,
                    &signing_key_for_loop,
                    since,
                    "periodic-backstop",
                );
            }

            event = swarm.select_next_some() => {
                let ctx = DispatchCtx {
                    cfg: &cfg,
                    executor: executor.as_ref(),
                    peer_registry: &peer_registry,
                    shared: Arc::clone(&shared),
                    quorums: Arc::clone(&quorums),
                    rate_limit: Arc::clone(&result_rate_limit),
                    state_sync_rate_limit: Arc::clone(&state_sync_rate_limit),
                    isolation: Arc::clone(&isolation),
                    verify_buffer: verify_buffer.clone(),
                    readiness: readiness_tracker.clone(),
                    local_peer_id,
                    signing_key: &signing_key_for_loop,
                };
                handle_swarm_event(event, &mut swarm, &ctx).await;
            }
        }
    }

    prune_handle.abort();
    if let Some(h) = update_handle {
        h.abort();
    }
    if let Some(h) = socks5_handle {
        h.abort();
    }
    info!("clean shutdown");
    Ok(())
}

/// Tiny helper that clones the rate-limit `Arc` for the buffer-tick.
/// Inline `Arc::clone(&rate_limit)` is awkward to spell at the
/// `tokio::select!` site, so we wrap it.
fn rate_limit_for_dispatch_clone(
    rl: &Arc<Mutex<HashMap<PeerId, RateBucket>>>,
) -> Arc<Mutex<HashMap<PeerId, RateBucket>>> {
    Arc::clone(rl)
}

// ───── V0.2 helpers ─────────────────────────────────────────────────────────

/// Read the 32 raw ed25519 secret bytes from the identity file. The
/// dalek `SigningKey` and the `KeySource::IdentityFile` derivation both
/// consume the same bytes.
fn read_identity_bytes(config_dir: &Path) -> Result<[u8; 32]> {
    let path = config_dir.join("identity.ed25519");
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read identity file {}", path.display()))?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "identity file at {} has {} bytes; expected 32",
            path.display(),
            bytes.len()
        );
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Open (create-if-missing) the SharedState DB with the identity-file
/// key source. The directory is created on demand.
fn open_shared_state(path: &Path, identity_bytes: &[u8; 32]) -> Result<SharedState> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create shared-state dir {}", parent.display()))?;
    }
    // We hand `Zeroizing` a fresh allocation so the source bytes
    // are scrubbed when the temporary drops.
    let identity_zeroizing = zeroize_vec(identity_bytes.to_vec());
    let key = KeyMaterial::from_source(KeySource::IdentityFile {
        identity_bytes: identity_zeroizing,
    })
    .context("derive shared-state key from identity")?;
    let opts = OpenOptions::create(path.to_path_buf(), key);
    SharedState::open(opts).context("open SharedState")
}

/// Helper that wraps a `Vec<u8>` into the zeroize-on-drop container
/// `KeySource::IdentityFile` expects. We accept by value so the caller
/// hands us ownership of the bytes.
fn zeroize_vec(bytes: Vec<u8>) -> zeroize::Zeroizing<Vec<u8>> {
    zeroize::Zeroizing::new(bytes)
}

/// Subscribe to one gossipsub topic, logging a single info line so the
/// V0.2.1 smoke-test scrapes can confirm each subscription.
fn subscribe_topic(
    swarm: &mut libp2p::Swarm<ParsehBehaviour>,
    name: &str,
) -> Result<gossipsub::IdentTopic> {
    let topic = gossipsub::IdentTopic::new(name);
    swarm
        .behaviour_mut()
        .gossipsub
        .subscribe(&topic)
        .with_context(|| format!("subscribe to {name}"))?;
    info!("subscribed: {name}");
    Ok(topic)
}

/// Build the JSON readiness payload.
fn build_readiness(
    peer_id: PeerId,
    listen_addrs: Vec<String>,
    known_peers: usize,
    llm_runtime: Option<RecommendedRuntime>,
    services_advertised: parseh_core::NodeCapabilities,
    shared_state_path: &Path,
    shared: &SharedState,
    open_q_count: usize,
) -> Result<ReadinessReport> {
    // Best-effort surface for shared-state counts; if a query fails
    // (very-fresh DB), report zero rather than aborting startup.
    let tasks_observed = shared
        .recent_tasks(0)
        .map(|v| v.len() as u64)
        .unwrap_or(0);
    // V0.2.1: results / verifications / outcomes counts come from the
    // detection-query primitives. We have no individual count accessor
    // yet, so for now we surface `tasks` precisely and the others as 0;
    // a follow-up adds dedicated count APIs (see TODO_V0_2_1 in
    // parseh-shared-state).
    let shared_state_snapshot = SharedStateSnapshot {
        path: shared_state_path.to_string_lossy().into_owned(),
        tasks_observed,
        results_observed: 0,
        verifications_observed: 0,
        outcomes_observed: 0,
        established_peers: shared
            .established_peers(parseh_verify::params::PROBATIONARY_REP_FLOOR as i64)
            .map(|v| v.len() as u64)
            .unwrap_or(0),
        local_reputation: shared.reputation_of(peer_id).unwrap_or(0),
    };

    Ok(ReadinessReport {
        identity_peer_id: peer_id.to_string(),
        listen_addrs,
        known_peers,
        llm_runtime,
        services_advertised,
        version: env!("CARGO_PKG_VERSION").to_string(),
        shared_state: Some(shared_state_snapshot),
        open_quorums: Some(vec![OpenQuorumSummary {
            count: open_q_count,
        }]),
    })
}

/// Publish the V0.2 capability advertisement on `parseh.caps.v1`.
///
/// Wire format is CBOR [`CapabilityAdvertisement`] at wire version
/// [`CAPS_WIRE_VERSION`] (V0.2.5 = 2). The miner ALSO accepts the
/// legacy JSON `NodeCapabilities` payload on the same topic for
/// backward compat with V0.1 nodes, and the V0.2.1 CBOR shape via the
/// fallback decoder in `parseh_core::decode_advertisement`.
///
/// V0.2.5 additions in the published shape:
///   - `verifying_key_bytes` — local ed25519 pubkey, so peers can
///     verify our inner signatures.
///   - `reachable_addrs` — current swarm listen addrs.
///   - `readiness` — current [`ReadinessState`] from
///     [`ReadinessTracker`].
///   - `has_external_internet` / `bandwidth_mbps_external` — bridge-
///     leg flags (always `false` / `None` today; the tunnel crate is
///     V0.2.5 sibling work).
///
/// **Side effect:** the local readiness tracker may transition from
/// `Listening`/`Connected` to `Ready` after the first successful
/// publish — that is the §3.4 transition "capabilities advertised".
fn publish_caps_v0_2(
    swarm: &mut libp2p::Swarm<ParsehBehaviour>,
    topic: &gossipsub::IdentTopic,
    local_peer_id: PeerId,
    cfg: &MinerConfig,
    verifying_key: &ed25519_dalek::VerifyingKey,
    readiness: &ReadinessTracker,
) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut services = Vec::with_capacity(2);
    if cfg.capabilities.relay {
        services.push(ServiceKind::Relay);
    }
    if cfg.capabilities.inference {
        services.push(ServiceKind::Inference);
    }

    let inference = if cfg.capabilities.inference {
        Some(parseh_core::InferenceCapability {
            models: cfg.capabilities.model_tags.clone(),
            context_size: 0,
            estimated_tokens_per_sec: 0,
        })
    } else {
        None
    };
    let relay = if cfg.capabilities.relay {
        Some(parseh_core::RelayCapability {
            bandwidth_mbps: cfg.capabilities.uplink_mbps,
            transport_kinds: vec![],
        })
    } else {
        None
    };

    let network_address: Multiaddr = "/ip4/0.0.0.0/tcp/0"
        .parse()
        .expect("static multiaddr literal is well-formed");

    // Snapshot the current listen multiaddrs so peers know how to dial
    // us for direct request-response.
    let reachable_addrs: Vec<Multiaddr> = swarm.listeners().cloned().collect();

    let ad = CapabilityAdvertisement {
        peer_id: local_peer_id,
        version: CAPS_WIRE_VERSION,
        services,
        inference,
        relay,
        storage: None,
        network_address,
        signed_at: now,
        ttl_seconds: 300,
        verifying_key_bytes: *verifying_key.as_bytes(),
        reachable_addrs,
        readiness: readiness.current(),
        // V0.2.5: bridge-leg capability is plumbed-but-disabled at the
        // miner level; the `parseh-tunnel` crate sets these via a
        // sibling configuration knob in its own milestone.
        has_external_internet: false,
        bandwidth_mbps_external: None,
    };

    let payload = match encode_advertisement(&ad) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "serialise CapabilityAdvertisement");
            return;
        }
    };
    match swarm.behaviour_mut().gossipsub.publish(topic.clone(), payload) {
        Ok(_) => {
            tracing::debug!("published capability advertisement (CBOR v2)");
            // First-cap-publish transition per §3.4 — Ready only if we
            // are still in a pre-Ready state.
            readiness.mark_ready();
        }
        Err(e) => tracing::trace!(error = %e, "no peers yet · skip caps publish"),
    }
}

/// One open quorum's state in the miner.
struct OpenQuorum {
    /// The `parseh-verify` aggregator owning the tally + finalisation logic.
    quorum: Quorum,
    /// Whether the quorum has emitted an outcome already. Idempotency
    /// guard for the periodic finaliser.
    finalised: bool,
}

/// Periodic finalise tick — walk every open quorum and try to close it.
/// On success: persist outcome, publish state-delta, emit reputation.
///
/// **Load-bearing**: pure event-driven finalisation deadlocks when all
/// M verifications arrive inside `t_min`. See the testnet harness for
/// the discovery story.
fn run_finalise_tick(
    swarm: &mut libp2p::Swarm<ParsehBehaviour>,
    quorums: &Arc<Mutex<LruCache<ContentHash, OpenQuorum>>>,
    shared: &SharedState,
    local_peer_id: PeerId,
    signing_key: &ed25519_dalek::SigningKey,
    readiness: &ReadinessTracker,
) {
    let now = SystemTime::now();
    let mut finalised_results: Vec<(ContentHash, parseh_verify::FinalisedQuorum)> = Vec::new();

    {
        let mut guard = quorums.lock();
        for (result_hash, oq) in guard.iter_mut() {
            if oq.finalised {
                continue;
            }
            if let Some(f) = oq.quorum.try_finalise(now, local_peer_id, signing_key) {
                oq.finalised = true;
                finalised_results.push((*result_hash, f));
            }
        }
        // Drop closed quorums to keep memory bounded. The LRU cap is
        // the safety net; this loop is the regular "promptly free
        // finalised entries" path.
        let finalised_keys: Vec<ContentHash> = guard
            .iter()
            .filter_map(|(k, q)| if q.finalised { Some(*k) } else { None })
            .collect();
        for k in finalised_keys {
            guard.pop(&k);
        }
    }

    // Each finalisation closes one in-flight task. The readiness
    // tracker may step back to `Ready` once everything drains.
    if !finalised_results.is_empty() {
        for _ in 0..finalised_results.len() {
            readiness.task_finished();
        }
    }

    for (result_hash, finalised) in finalised_results {
        let outcome = finalised.outcome.clone();
        info!(
            %result_hash,
            decision = ?finalised.decision,
            agreements = finalised.agreements,
            disagreements = finalised.disagreements,
            "quorum finalised"
        );

        // 1) Persist locally.
        if let Err(e) = shared.record_outcome(&outcome) {
            warn!(error = %e, "record_outcome failed");
        }

        // 2) Sign + publish the outcome delta.
        let now_secs = unix_now();
        let unsigned = StateDelta::unsigned(
            DeltaKind::Outcome(outcome.clone()),
            local_peer_id,
            now_secs,
        );
        let signed = match sign_state_delta(unsigned, signing_key) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "sign outcome state-delta");
                continue;
            }
        };
        let bytes = match signed.encode_cbor() {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "encode outcome state-delta");
                continue;
            }
        };
        let topic = gossipsub::IdentTopic::new(TOPIC_STATE_DELTAS);
        match swarm.behaviour_mut().gossipsub.publish(topic.clone(), bytes) {
            Ok(_) => tracing::debug!("published outcome state-delta"),
            Err(e) => tracing::trace!(error = %e, "no peers · skip outcome publish"),
        }

        // 3) Emit reputation deltas — executor +10 + each Agreed
        //    verifier +5 — per the project notes §4.
        if matches!(
            outcome.verdict,
            parseh_task::OutcomeVerdict::Valid { .. }
        ) {
            // Reputation deltas are best-effort: we apply them locally
            // and gossip a signed envelope so peers do the same. V0.3+
            // is the chain-validated source of truth; today this is a
            // gossip-only projection.
            let related_hash = outcome.content_hash();
            // Executor reward — recovered via the result_hash → result
            // path. We do not know the executor's PeerId here without
            // a fresh lookup; the quorum.outcome carries `result_hash`
            // but not the executor identity directly. Look it up from
            // shared-state.
            if let Ok(results) = shared.verifications_for_result(&result_hash) {
                // Tally Agreed verifiers and award +5 each.
                let mut agreed_verifiers: Vec<PeerId> = Vec::new();
                for v in &results {
                    if matches!(v.verdict, parseh_task::VerifierVerdict::Agreed) {
                        agreed_verifiers.push(v.verifier);
                    }
                }
                for v in agreed_verifiers {
                    apply_and_gossip_reputation(
                        swarm,
                        shared,
                        signing_key,
                        local_peer_id,
                        v,
                        5,
                        "verifier_agreed",
                        Some(related_hash),
                    );
                }
            }
            // Executor reward — pulled from the `JobOutcome.result_hash`
            // → results lookup. We avoid a dedicated `result_for_hash`
            // method by snapping through `outcome_for_spec` then
            // `verifications_for_result`. Cleaner accessors land in
            // V0.2.5.
            if let Ok(Some(_)) = shared.outcome_for_spec(&outcome.spec_hash) {
                // The executor identity is on the JobResult, not the
                // outcome. We carried it implicitly via the
                // verification round-trip; pull it from the recorded
                // result via a small ad-hoc query. To keep this PR
                // self-contained, we defer the executor reward to a
                // follow-up patch and record the verifier rewards now.
                tracing::debug!(
                    "executor reward deferred — pending dedicated `result_for_hash` accessor"
                );
            }
        }
    }
}

/// Apply a reputation delta locally AND gossip a signed envelope so
/// peers can replay the same projection in their own shared state.
fn apply_and_gossip_reputation(
    swarm: &mut libp2p::Swarm<ParsehBehaviour>,
    shared: &SharedState,
    signing_key: &ed25519_dalek::SigningKey,
    local_peer_id: PeerId,
    subject: PeerId,
    delta: i32,
    reason: &str,
    related_hash: Option<ContentHash>,
) {
    if let Err(e) = shared.apply_reputation_delta(subject, delta, reason, related_hash) {
        warn!(error = %e, "apply_reputation_delta");
        return;
    }
    let now_secs = unix_now();
    let unsigned = StateDelta::unsigned(
        DeltaKind::Reputation {
            peer: subject,
            delta,
            reason: reason.to_string(),
            related_hash,
        },
        local_peer_id,
        now_secs,
    );
    let signed = match sign_state_delta(unsigned, signing_key) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "sign reputation delta");
            return;
        }
    };
    let bytes = match signed.encode_cbor() {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "encode reputation delta");
            return;
        }
    };
    let topic = gossipsub::IdentTopic::new(TOPIC_STATE_DELTAS);
    match swarm.behaviour_mut().gossipsub.publish(topic, bytes) {
        Ok(_) => tracing::debug!(%subject, delta, "published reputation delta"),
        Err(e) => tracing::trace!(error = %e, "no peers · skip rep delta"),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Refuse to reset unattended without `--yes`. Prompts on stdin
/// otherwise.
fn reset_shared_state(path: &Path, assume_yes: bool) -> Result<()> {
    if !path.exists() {
        info!(path = %path.display(), "--reset-shared-state · DB does not exist · no-op");
        return Ok(());
    }
    if !assume_yes {
        use std::io::{self, BufRead, Write};
        eprint!(
            "About to delete shared-state DB at {} . Type `y` to confirm: ",
            path.display()
        );
        io::stderr().flush().ok();
        let stdin = io::stdin();
        let mut line = String::new();
        stdin
            .lock()
            .read_line(&mut line)
            .context("read confirmation from stdin")?;
        if !line.trim().eq_ignore_ascii_case("y") {
            anyhow::bail!("--reset-shared-state aborted");
        }
    }
    std::fs::remove_file(path)
        .with_context(|| format!("delete shared-state DB at {}", path.display()))?;
    info!(path = %path.display(), "--reset-shared-state · DB removed");
    Ok(())
}

// ───── readiness extensions ─────────────────────────────────────────────────

/// Shared-state surface inside [`ReadinessReport`].
///
/// Counts default to zero on a fresh DB; the full results / verifications
/// / outcomes counters land in V0.2.5 once `parseh-shared-state` ships
/// dedicated count accessors.
#[derive(Debug, Clone, Serialize)]
pub struct SharedStateSnapshot {
    /// Filesystem path of the SQLite (or SQLCipher) database backing
    /// `parseh-shared-state`.
    pub path: String,
    /// Count of `JobSpec`s persisted so far.
    pub tasks_observed: u64,
    /// Count of `JobResult`s persisted so far. V0.2.5 wires the count.
    pub results_observed: u64,
    /// Count of `JobVerification`s persisted so far.
    pub verifications_observed: u64,
    /// Count of finalised `JobOutcome`s persisted so far.
    pub outcomes_observed: u64,
    /// Number of distinct peers that crossed the `Established`
    /// reputation tier (i.e. summed reputation ≥
    /// `parseh_verify::params::PROBATIONARY_REP_FLOOR`).
    pub established_peers: u64,
    /// Summed reputation of the local node.
    pub local_reputation: i64,
}

/// Compact summary of open quorums for the readiness report. We keep
/// the schema small so the readiness payload stays terse; full per-
/// quorum diagnostics land in the V0.2.5 admin API.
#[derive(Debug, Clone, Serialize)]
pub struct OpenQuorumSummary {
    /// Number of currently-open quorums on this node.
    pub count: usize,
}

// ───── swarm event dispatch ─────────────────────────────────────────────────

/// Bundle of long-lived state the swarm-event dispatcher reads from /
/// writes to. Introduced in V0.2.5 to keep the
/// `handle_swarm_event` signature manageable as the residual-closing
/// work added several new dependencies (peer-key directory, verify
/// buffer, rate limiter, readiness tracker).
struct DispatchCtx<'a> {
    cfg: &'a MinerConfig,
    executor: &'a dyn Executor,
    peer_registry: &'a PeerRegistry,
    shared: Arc<SharedState>,
    quorums: Arc<Mutex<LruCache<ContentHash, OpenQuorum>>>,
    rate_limit: Arc<Mutex<HashMap<PeerId, RateBucket>>>,
    /// Per-requester rate limiter for `/parseh/state-sync/1.0.0`.
    state_sync_rate_limit: Arc<Mutex<HashMap<PeerId, RateBucket>>>,
    /// Connectivity tracker — drives the post-isolation sync trigger.
    isolation: Arc<Mutex<IsolationTracker>>,
    verify_buffer: VerifyBuffer,
    readiness: ReadinessTracker,
    local_peer_id: PeerId,
    signing_key: &'a ed25519_dalek::SigningKey,
}

async fn handle_swarm_event(
    event: SwarmEvent<ParsehEvent>,
    swarm: &mut libp2p::Swarm<ParsehBehaviour>,
    ctx: &DispatchCtx<'_>,
) {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => {
            info!(%address, "listen address");
        }
        SwarmEvent::ConnectionEstablished { peer_id, .. } => {
            info!(%peer_id, "peer connected");
            ctx.readiness.mark_listening();
            let _ = swarm.behaviour_mut().kad.bootstrap();

            // State-sync trigger #1: this connection ends an isolation
            // window. If we were alone for ≥ 30 s we may have missed
            // outcomes finalised elsewhere — gossipsub's IHAVE cache is
            // far too short to replay them, so pull explicitly. We ask
            // for everything since `now - max_partition_window` OR our
            // newest known outcome's timestamp, whichever is OLDER
            // (computed inside `issue_state_sync_request`).
            let isolation_gap = {
                let mut iso = ctx.isolation.lock();
                iso.on_connection_established(unix_now())
            };
            if let Some(gap) = isolation_gap {
                let since = unix_now()
                    .saturating_sub(STATE_SYNC_MAX_PARTITION_WINDOW_SECS);
                info!(
                    %peer_id,
                    isolation_secs = gap,
                    "ended isolation window · issuing state-sync catch-up"
                );
                issue_state_sync_request(
                    swarm,
                    ctx.peer_registry,
                    &ctx.shared,
                    ctx.local_peer_id,
                    ctx.signing_key,
                    since,
                    "post-isolation",
                );
            }
        }
        SwarmEvent::ConnectionClosed { peer_id, .. } => {
            tracing::debug!(%peer_id, "peer disconnected");
            ctx.isolation.lock().on_connection_closed(unix_now());
        }
        SwarmEvent::Behaviour(ParsehEvent::Ping(ping::Event { peer, result, .. })) => match result
        {
            Ok(rtt) => tracing::debug!(%peer, rtt_ms = rtt.as_millis(), "ping"),
            Err(e) => tracing::trace!(%peer, error = %e, "ping failed"),
        },
        SwarmEvent::Behaviour(ParsehEvent::Identify(identify::Event::Received {
            peer_id,
            info,
            ..
        })) => {
            tracing::debug!(%peer_id, ?info.protocols, "identified");
            for addr in info.listen_addrs.iter() {
                swarm.behaviour_mut().kad.add_address(&peer_id, addr.clone());
            }
        }
        SwarmEvent::Behaviour(ParsehEvent::Gossipsub(gossipsub::Event::Message {
            propagation_source,
            message,
            ..
        })) => {
            let topic_str = message.topic.as_str();
            match topic_str {
                TOPIC_CAPS => {
                    handle_caps_message(ctx.peer_registry, &message, propagation_source)
                }
                TOPIC_TASKS => {
                    handle_tasks_message(
                        swarm,
                        ctx,
                        &message,
                        propagation_source,
                    )
                    .await
                }
                TOPIC_VERIFY => {
                    handle_verify_message(
                        swarm,
                        ctx,
                        &message,
                        propagation_source,
                    )
                    .await
                }
                TOPIC_STATE_DELTAS => handle_state_delta_message(&ctx.shared, &message),
                _ => {
                    tracing::trace!(topic = %topic_str, "gossipsub message on unknown topic");
                }
            }
        }
        SwarmEvent::Behaviour(ParsehEvent::JobReqRes(request_response::Event::Message {
            peer,
            message,
        })) => match message {
            request_response::Message::Request {
                request_id,
                request,
                channel,
            } => {
                // V0.1 deprecation notice: every inbound 1.0.0 request
                // logs a warning so operators see the noise. Remove the
                // protocol entirely in V0.2.5.
                warn!(
                    %peer,
                    ?request_id,
                    model = %request.model,
                    "received V0.1 JobOrder via /parseh/job/1.0.0 · deprecation pending in V0.2.5"
                );
                let result = ctx.executor.execute(&request, &ctx.cfg.capabilities).await;
                if let Err(e) = swarm.behaviour_mut().job.send_response(channel, result) {
                    warn!(?request_id, error = ?e, "failed to send V0.1 job result");
                }
            }
            request_response::Message::Response {
                request_id,
                response,
            } => {
                info!(
                    %peer,
                    ?request_id,
                    tokens = response.tokens_used,
                    ms = response.wall_ms,
                    "received V0.1 job result"
                );
            }
        },
        SwarmEvent::Behaviour(ParsehEvent::JobReqRes(
            request_response::Event::OutboundFailure { peer, error, .. },
        )) => warn!(%peer, ?error, "job outbound failure"),
        SwarmEvent::Behaviour(ParsehEvent::JobReqRes(
            request_response::Event::InboundFailure { peer, error, .. },
        )) => warn!(%peer, ?error, "job inbound failure"),
        SwarmEvent::Behaviour(ParsehEvent::StateSync(
            request_response::Event::Message { peer, message },
        )) => match message {
            request_response::Message::Request {
                request_id,
                request,
                channel,
            } => {
                let response = build_state_sync_response(ctx, peer, &request);
                if let Err(e) = swarm
                    .behaviour_mut()
                    .state_sync
                    .send_response(channel, response)
                {
                    warn!(?request_id, error = ?e, "failed to send state-sync response");
                }
            }
            request_response::Message::Response {
                request_id,
                response,
            } => {
                apply_state_sync_response(ctx, peer, &response, request_id);
            }
        },
        SwarmEvent::Behaviour(ParsehEvent::StateSync(
            request_response::Event::OutboundFailure { peer, error, .. },
        )) => warn!(%peer, ?error, "state-sync outbound failure"),
        SwarmEvent::Behaviour(ParsehEvent::StateSync(
            request_response::Event::InboundFailure { peer, error, .. },
        )) => warn!(%peer, ?error, "state-sync inbound failure"),
        _ => {}
    }
}

/// Pick a responder for an outbound state-sync request and send it.
///
/// Responder selection prefers a higher reputation band
/// (`Trusted` > `Established` > anything else) among currently-connected
/// peers — a better-connected, longer-lived peer is more likely to hold
/// the outcomes we missed. Ties / no-reputation-data fall back to the
/// first connected peer. `since` is widened to the OLDER of the
/// caller-supplied value and our newest locally-known outcome's
/// `finalised_at` (being generous is cheap; the responder clamps the
/// count).
fn issue_state_sync_request(
    swarm: &mut libp2p::Swarm<ParsehBehaviour>,
    _peer_registry: &PeerRegistry,
    shared: &SharedState,
    local_peer_id: PeerId,
    signing_key: &ed25519_dalek::SigningKey,
    since: u64,
    cause: &str,
) {
    use parseh_core::peer_registry::ReputationBand;

    let connected: Vec<PeerId> = swarm.connected_peers().copied().collect();
    if connected.is_empty() {
        tracing::debug!(cause, "no connected peers · skipping state-sync");
        return;
    }

    // Newest locally-known outcome's finalised_at — if we already have
    // recent outcomes, do not re-pull older history than we need; but
    // if our newest is OLDER than `since`, widen back to it so we never
    // skip the gap. (Generosity is cheap; a miss is not.)
    let effective_since = match shared.outcomes_since(0, 1) {
        Ok(v) => match v.first() {
            Some(newest) => since.min(newest.finalised_at),
            None => 0, // we know nothing → ask for everything available
        },
        Err(e) => {
            tracing::debug!(error = %e, "outcomes_since probe failed · using caller since");
            since
        }
    };

    // Prefer the highest reputation band among connected peers. A
    // longer-lived, better-reputed peer is more likely to hold the
    // outcomes we missed; reputation is the V0.2 proxy for "well-
    // connected / trustworthy". Reputation lives in SharedState.
    let chosen = connected
        .iter()
        .copied()
        .max_by_key(|p| {
            let score = shared.reputation_of(*p).unwrap_or(0);
            match ReputationBand::from_score(score) {
                ReputationBand::Trusted => 4u8,
                ReputationBand::Established => 3,
                ReputationBand::Probationary => 2,
                ReputationBand::New => 1,
                ReputationBand::Slashed => 0,
            }
        })
        .unwrap_or(connected[0]);

    let req = StateSyncRequest::new_signed(
        effective_since,
        STATE_SYNC_HARD_CEILING,
        local_peer_id,
        unix_now(),
        signing_key,
    );
    let rid = swarm
        .behaviour_mut()
        .state_sync
        .send_request(&chosen, req);
    info!(
        responder = %chosen,
        since = effective_since,
        cause,
        ?rid,
        "issued /parseh/state-sync/1.0.0 request"
    );
}

/// Responder side of `/parseh/state-sync/1.0.0`.
///
/// Security order of operations (DoS-cheap first):
/// 1. Verify the requester signature BEFORE any disk work. A request
///    whose signer's key we do not yet know, or whose signature fails,
///    gets an empty response (no work done).
/// 2. Per-requester rate-limit (same rolling-window pattern as the
///    `JobResult` limiter). Over the cap → empty response.
/// 3. Clamp `max_outcomes` to [`STATE_SYNC_HARD_CEILING`] regardless of
///    what was asked.
/// 4. `outcomes_since` (index-backed), sign, return.
fn build_state_sync_response(
    ctx: &DispatchCtx<'_>,
    peer: PeerId,
    request: &StateSyncRequest,
) -> StateSyncResponse {
    let empty = |reason: &str| {
        tracing::debug!(%peer, reason, "state-sync: empty response");
        StateSyncResponse::new_signed(
            Vec::new(),
            false,
            ctx.local_peer_id,
            unix_now(),
            ctx.signing_key,
        )
    };

    // 1 · requester signature (cheap DoS guard, before any disk work).
    let requester_key = match ctx.peer_registry.verifying_key(&request.requester) {
        Some(k) => k,
        None => return empty("requester pubkey unknown"),
    };
    if let Err(e) = request.verify_signature(&requester_key) {
        tracing::warn!(%peer, error = %e, "state-sync: bad requester signature");
        return empty("bad requester signature");
    }

    // 2 · per-requester rate limit.
    let over_cap = {
        let mut rl = ctx.state_sync_rate_limit.lock();
        rl.entry(request.requester)
            .or_default()
            .record_with_cap(unix_now(), STATE_SYNC_MAX_REQUESTS_PER_WINDOW)
    };
    if over_cap {
        tracing::warn!(
            %peer,
            requester = %request.requester,
            "state-sync: requester over rate cap · refusing"
        );
        return empty("rate-limited");
    }

    // 3 · clamp the count regardless of what was asked.
    let limit = request.max_outcomes.min(STATE_SYNC_HARD_CEILING) as usize;
    if limit == 0 {
        return empty("zero limit requested");
    }

    // 4 · index-backed query. Ask for limit+1 so we can honestly set
    // `truncated` without a second round-trip.
    let mut outcomes = match ctx.shared.outcomes_since(request.since_unix, limit + 1) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(%peer, error = %e, "state-sync: outcomes_since failed");
            return empty("query failed");
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
        "state-sync: answered request"
    );
    StateSyncResponse::new_signed(
        outcomes,
        truncated,
        ctx.local_peer_id,
        unix_now(),
        ctx.signing_key,
    )
}

/// Apply an inbound `/parseh/state-sync/1.0.0` response.
///
/// **The responder framing is NOT trusted.** For every outcome we
/// re-verify the inner ed25519 signature against the `observed_by`
/// peer's key from our own registry; only then is it persisted
/// (`record_outcome` is idempotent — already-known outcomes are
/// no-ops). A malicious responder can withhold or reorder, but a forged
/// outcome fails this inner check and is dropped.
fn apply_state_sync_response(
    ctx: &DispatchCtx<'_>,
    peer: PeerId,
    response: &StateSyncResponse,
    request_id: request_response::OutboundRequestId,
) {
    // The envelope sig only proves WHO answered — useful for logs /
    // future reputation, not for trust. We still check it so a
    // tampered-in-flight envelope is visible.
    if let Some(rk) = ctx.peer_registry.verifying_key(&response.responder) {
        if let Err(e) = response.verify_signature(&rk) {
            tracing::warn!(%peer, error = %e, "state-sync: bad responder envelope sig");
            return;
        }
    }

    let mut applied = 0usize;
    let mut rejected = 0usize;
    for outcome in &response.outcomes {
        let observer_key = match ctx.peer_registry.verifying_key(&outcome.observed_by) {
            Some(k) => k,
            None => {
                // We do not know the observer's key yet — cannot trust
                // it. Drop rather than persist on the responder's word.
                rejected += 1;
                continue;
            }
        };
        if outcome.verify_signature(&observer_key).is_err() {
            rejected += 1;
            continue;
        }
        // Use the sync-specific writer: the syncing node may not have
        // the originating spec/result rows (it was partitioned away),
        // so `record_outcome`'s FK would reject the very outcomes we
        // are here to recover. `record_synced_outcome` stubs the parent
        // rows so the signed artifact persists.
        match ctx.shared.record_synced_outcome(outcome) {
            Ok(()) => applied += 1,
            Err(e) => {
                tracing::debug!(error = %e, "state-sync: record_synced_outcome failed");
            }
        }
    }
    info!(
        %peer,
        ?request_id,
        applied,
        rejected,
        truncated = response.truncated,
        "state-sync: response applied"
    );
}

/// Decode an inbound `parseh.caps.v1` payload into a
/// `CapabilityAdvertisement` and upsert into the registry.
///
/// V0.2.1 accepts BOTH wire formats:
///   1. CBOR `CapabilityAdvertisement` (new V0.2 format · preferred).
///   2. Legacy JSON `NodeCapabilities` (V0.1 fallback · bridged with
///      synthesised `peer_id` / `signed_at` / `ttl_seconds`).
///
/// Without the JSON fallback the network bifurcates the moment one
/// node upgrades, since V0.1 peers cannot decode CBOR. The fallback
/// drops in V0.2.5 when the rolling upgrade window closes.
fn handle_caps_message(
    peer_registry: &PeerRegistry,
    message: &gossipsub::Message,
    propagation_source: PeerId,
) {
    // Try CBOR first. `decode_advertisement` accepts both V0.2.5 (v2)
    // and V0.2.1 (v1) wire shapes — see `parseh_core::peer_registry`.
    if let Ok(ad) = parseh_core::decode_advertisement(&message.data) {
        tracing::debug!(peer = %ad.peer_id, version = ad.version, readiness = ?ad.readiness, "peer caps (CBOR)");
        peer_registry.upsert(ad);
        return;
    }
    // Fallback: legacy JSON `NodeCapabilities` from a V0.1 miner.
    if let Ok(caps) = serde_json::from_slice::<parseh_core::NodeCapabilities>(&message.data) {
        if let Some(source) = message.source {
            tracing::debug!(from = %source, ?caps, "peer caps (legacy JSON v0.1)");
            bridge_legacy_caps_into_registry(peer_registry, source, &caps);
        } else {
            tracing::trace!(
                from = %propagation_source,
                "legacy JSON caps without `source` — cannot upsert"
            );
        }
    } else {
        tracing::trace!(
            from = %propagation_source,
            "caps message neither CBOR nor legacy JSON · dropping"
        );
    }
}

/// Bridge a legacy `NodeCapabilities` (V0.1 wire) into the V0.2 envelope
/// shape `PeerRegistry` consumes. Synthesises peer_id / signed_at /
/// ttl_seconds from gossipsub metadata + wall-clock.
fn bridge_legacy_caps_into_registry(
    peer_registry: &PeerRegistry,
    source: PeerId,
    caps: &parseh_core::NodeCapabilities,
) {
    let now = unix_now();
    let mut services = Vec::with_capacity(2);
    if caps.relay {
        services.push(ServiceKind::Relay);
    }
    if caps.inference {
        services.push(ServiceKind::Inference);
    }
    let inference = if caps.inference {
        Some(parseh_core::InferenceCapability {
            models: caps.model_tags.clone(),
            context_size: 0,
            estimated_tokens_per_sec: 0,
        })
    } else {
        None
    };
    let relay = if caps.relay {
        Some(parseh_core::RelayCapability {
            bandwidth_mbps: caps.uplink_mbps,
            transport_kinds: vec![],
        })
    } else {
        None
    };
    let network_address: Multiaddr = "/ip4/0.0.0.0/tcp/0"
        .parse()
        .expect("static multiaddr literal is well-formed");
    let ad = CapabilityAdvertisement {
        peer_id: source,
        // Legacy JSON peers cannot carry a verifying key. The bridged
        // advertisement is therefore version 1 with a zero pubkey
        // (PeerRegistry treats all-zero as "key not advertised").
        version: 1,
        services,
        inference,
        relay,
        storage: None,
        network_address,
        signed_at: now,
        ttl_seconds: 300,
        verifying_key_bytes: [0u8; 32],
        reachable_addrs: vec![],
        readiness: ReadinessState::Ready,
        has_external_internet: false,
        bandwidth_mbps_external: None,
    };
    let first = peer_registry.upsert(ad);
    if first {
        tracing::debug!(peer = %source, "first capability advertisement from peer (legacy JSON)");
    }
}

/// Handle an inbound `parseh.tasks.v1` message — a CBOR-encoded `JobSpec`.
///
/// V0.2.5 closes residual #1: every inbound spec is now signature-
/// verified against the submitter's pubkey from the peer-key directory.
/// Specs whose submitter has not yet advertised land in
/// [`VerifyBuffer`] for up to 10 seconds.
///
/// V0.2.5 also closes residual #2: after persisting the spec, this node
/// runs `should_execute` and, if elected, signs + publishes a
/// `JobResult` for the spec.
async fn handle_tasks_message(
    swarm: &mut libp2p::Swarm<ParsehBehaviour>,
    ctx: &DispatchCtx<'_>,
    message: &gossipsub::Message,
    propagation_source: PeerId,
) {
    let spec: JobSpec = match from_cbor_bytes(&message.data) {
        Ok(s) => s,
        Err(e) => {
            tracing::trace!(error = %e, "decode JobSpec");
            return;
        }
    };

    // Inner-signature check. V0.2.5 closes residual #1.
    let submitter_key = match ctx.peer_registry.verifying_key(&spec.submitter) {
        Some(k) => k,
        None => {
            // Submitter unknown yet — buffer the message for up to 10s.
            // See `verify_buffer.rs` for the race rationale.
            tracing::debug!(
                submitter = %spec.submitter,
                "submitter not in peer-key directory yet · buffering for late delivery"
            );
            let stored = ctx.verify_buffer.enqueue(PendingMessage {
                publisher: spec.submitter,
                payload: message.data.clone(),
                tag: None, // tag is unused for tasks.v1
                received_at: Instant::now(),
            });
            if !stored {
                tracing::warn!(
                    publisher = %spec.submitter,
                    source = %propagation_source,
                    "verify_buffer flood · dropping unauthenticated JobSpec"
                );
            }
            return;
        }
    };
    if let Err(e) = spec.verify_signature(&submitter_key) {
        tracing::warn!(submitter = %spec.submitter, error = %e, "JobSpec inner signature rejected");
        return;
    }

    let spec_hash = spec.content_hash();
    if let Err(e) = ctx.shared.record_spec(&spec) {
        warn!(error = %e, "record_spec");
        return;
    }
    tracing::debug!(%spec_hash, submitter = %spec.submitter, "observed JobSpec");

    // V0.2.5 closes residual #2 — executor self-selection.
    if spec.submitter == ctx.local_peer_id {
        // Rule 3a: never execute one's own spec.
        return;
    }
    if !should_execute(&ctx.local_peer_id, &spec, ctx.peer_registry) {
        return;
    }

    // We are the elected executor. Build, sign, and publish the result.
    let result_bytes = match build_executor_result(ctx, &spec).await {
        Ok(b) => b,
        Err(e) => {
            warn!(%spec_hash, error = %e, "executor failed to build result");
            return;
        }
    };
    let meta = parseh_task::ResultMeta {
        verifier_method: parseh_task::VerifierMethod::Deterministic,
        execution_time_ms: 1,
        model_used: Some("echo-executor".into()),
        inference_token_count: None,
    };
    let (job_result, result_hash) = parseh_task::JobResult::new_signed(
        spec_hash,
        ctx.local_peer_id,
        meta,
        result_bytes,
        ctx.signing_key,
    );
    if let Err(e) = ctx.shared.record_result(&job_result) {
        tracing::trace!(error = %e, "record_result (own)");
    }
    // Open our own quorum entry so we can track verifications.
    {
        let mut q = ctx.quorums.lock();
        q.put(
            result_hash,
            OpenQuorum {
                quorum: Quorum::new(
                    QuorumConfig::standard(),
                    spec_hash,
                    result_hash,
                    SystemTime::now(),
                ),
                finalised: false,
            },
        );
    }
    // Mark this node as having an in-flight task — readiness → Active.
    ctx.readiness.task_started();

    // Publish on parseh.verify.v1 with TAG_JOB_RESULT.
    let mut envelope = vec![TAG_JOB_RESULT];
    match parseh_task::to_cbor_bytes(&job_result) {
        Ok(b) => envelope.extend(b),
        Err(e) => {
            warn!(error = %e, "encode JobResult");
            return;
        }
    }
    let topic = gossipsub::IdentTopic::new(TOPIC_VERIFY);
    match swarm.behaviour_mut().gossipsub.publish(topic, envelope) {
        Ok(_) => info!(%spec_hash, %result_hash, "published JobResult (executor self-selected)"),
        Err(e) => warn!(error = %e, "publish JobResult"),
    }
}

/// Deterministic-lowest-PeerId executor self-selection.
///
/// Returns `true` iff the local node is the chosen executor for `spec`.
///
/// Selection rules (V0.2 production):
///   - Filter to peers in [`ReadinessState::Ready`] or
///     [`ReadinessState::Active`] that advertise the spec's service.
///   - Exclude the submitter (Rule 3a, no self-execution of one's own
///     submission).
///   - Among the survivors, the peer with the smallest `PeerId.to_bytes()`
///     wins.
///
/// This is deterministic: every node observing the same set of
/// advertisements picks the same executor without coordination. Ties
/// are broken by the byte order of the libp2p identity, which is itself
/// a random ed25519 pubkey — so collisions among honest peers are
/// vanishingly unlikely.
///
/// V0.3+ may replace this with a VRF-based rotation; the wire types
/// already accommodate it.
fn should_execute(local: &PeerId, spec: &JobSpec, registry: &PeerRegistry) -> bool {
    let mut eligible: Vec<PeerId> = registry
        .ready_peers_for_service(spec.service.clone())
        .into_iter()
        .map(|p| p.peer_id)
        .filter(|p| *p != spec.submitter)
        .collect();
    // Include ourselves explicitly — we don't always appear in the
    // registry until our own caps publish round-trips through gossipsub.
    if !eligible.contains(local) && *local != spec.submitter {
        eligible.push(*local);
    }
    eligible.sort_by_key(|p| p.to_bytes());
    eligible.first().map(|chosen| chosen == local).unwrap_or(false)
}

/// Build a deterministic result payload for executor self-selection.
///
/// V0.2.5 stays inside the existing executor surface: SHA-256 of the
/// prompt bytes + seed bytes. The deterministic verifier in
/// `parseh-verify` produces byte-equal payloads on re-execution. V0.3+
/// is where the real LLM plugs in.
async fn build_executor_result(
    _ctx: &DispatchCtx<'_>,
    spec: &JobSpec,
) -> Result<Vec<u8>, anyhow::Error> {
    use sha2::{Digest, Sha256};
    let prompt = spec.inputs.prompt_text.as_deref().unwrap_or("");
    let seed = spec.inputs.seed.unwrap_or(0);
    let mut h = Sha256::new();
    h.update(prompt.as_bytes());
    h.update(seed.to_le_bytes());
    Ok(h.finalize().to_vec())
}

/// Handle an inbound `parseh.verify.v1` message. Tag-byte multiplexed:
/// 0x02 = `JobResult`, 0x03 = `JobVerification`.
///
/// V0.2.5 closes residual #1: every inbound result + verification is
/// signature-verified against the publisher's pubkey from the
/// peer-key directory. Messages from unknown publishers land in
/// [`VerifyBuffer`] for up to 10 seconds.
///
/// V0.2.5 closes the open-side of residual #3: rate-limit per
/// publisher (`RATE_LIMIT_MAX_RESULTS_PER_WINDOW` per
/// `RATE_LIMIT_WINDOW_MS`) on the `JobResult` path.
async fn handle_verify_message(
    swarm: &mut libp2p::Swarm<ParsehBehaviour>,
    ctx: &DispatchCtx<'_>,
    message: &gossipsub::Message,
    propagation_source: PeerId,
) {
    if message.data.is_empty() {
        return;
    }
    let tag = message.data[0];
    let body = &message.data[1..];
    match tag {
        TAG_JOB_RESULT => {
            handle_inbound_job_result(swarm, ctx, body, propagation_source).await;
        }
        TAG_JOB_VERIFICATION => {
            handle_inbound_verification(ctx, body, propagation_source).await;
        }
        other => {
            tracing::trace!(tag = other, "unknown tag on parseh.verify.v1");
        }
    }
}

/// Decode + verify + persist + counter-sign the inbound `JobResult`.
///
/// Factored out of [`handle_verify_message`] because the V0.2.5
/// signature-verification + rate-limit logic made the inline arm
/// uncomfortably wide.
async fn handle_inbound_job_result(
    _swarm: &mut libp2p::Swarm<ParsehBehaviour>,
    ctx: &DispatchCtx<'_>,
    body: &[u8],
    propagation_source: PeerId,
) {
    let result: TaskJobResult = match from_cbor_bytes(body) {
        Ok(r) => r,
        Err(e) => {
            tracing::trace!(error = %e, "decode JobResult");
            return;
        }
    };

    // Inner-signature check.
    let executor_key = match ctx.peer_registry.verifying_key(&result.executor) {
        Some(k) => k,
        None => {
            tracing::debug!(
                executor = %result.executor,
                "executor not in peer-key directory yet · buffering"
            );
            let mut envelope = vec![TAG_JOB_RESULT];
            envelope.extend_from_slice(body);
            let stored = ctx.verify_buffer.enqueue(PendingMessage {
                publisher: result.executor,
                payload: envelope,
                tag: Some(TAG_JOB_RESULT),
                received_at: Instant::now(),
            });
            if !stored {
                tracing::warn!(
                    publisher = %result.executor,
                    source = %propagation_source,
                    "verify_buffer flood · dropping unauthenticated JobResult"
                );
            }
            return;
        }
    };
    if let Err(e) = result.verify_signature(&executor_key) {
        tracing::warn!(executor = %result.executor, error = %e, "JobResult inner signature rejected");
        return;
    }

    // Per-publisher rate limit (closes residual #3 open side).
    {
        let now_secs = unix_now();
        let mut rl = ctx.rate_limit.lock();
        let bucket = rl.entry(result.executor).or_default();
        if bucket.record(now_secs) {
            tracing::warn!(
                executor = %result.executor,
                "JobResult rate limit exceeded · dropping"
            );
            return;
        }
    }

    let result_hash = result.content_hash();
    if let Err(e) = ctx.shared.record_result(&result) {
        // Foreign-key fail just means we have not seen the spec yet.
        tracing::trace!(error = %e, "record_result (likely FK · spec not yet seen)");
        return;
    }
    tracing::debug!(%result_hash, executor = %result.executor, "observed JobResult");

    // Open (or refresh) the quorum for this result.
    let mut q = ctx.quorums.lock();
    if q.get(&result_hash).is_none() {
        q.put(
            result_hash,
            OpenQuorum {
                quorum: Quorum::new(
                    QuorumConfig::standard(),
                    result.spec_hash,
                    result_hash,
                    SystemTime::now(),
                ),
                finalised: false,
            },
        );
    }
}

/// Decode + verify + persist the inbound `JobVerification`.
async fn handle_inbound_verification(
    ctx: &DispatchCtx<'_>,
    body: &[u8],
    propagation_source: PeerId,
) {
    let v: JobVerification = match from_cbor_bytes(body) {
        Ok(v) => v,
        Err(e) => {
            tracing::trace!(error = %e, "decode JobVerification");
            return;
        }
    };
    let verifier_key = match ctx.peer_registry.verifying_key(&v.verifier) {
        Some(k) => k,
        None => {
            tracing::debug!(
                verifier = %v.verifier,
                "verifier not in peer-key directory yet · buffering"
            );
            let mut envelope = vec![TAG_JOB_VERIFICATION];
            envelope.extend_from_slice(body);
            let stored = ctx.verify_buffer.enqueue(PendingMessage {
                publisher: v.verifier,
                payload: envelope,
                tag: Some(TAG_JOB_VERIFICATION),
                received_at: Instant::now(),
            });
            if !stored {
                tracing::warn!(
                    publisher = %v.verifier,
                    source = %propagation_source,
                    "verify_buffer flood · dropping unauthenticated JobVerification"
                );
            }
            return;
        }
    };
    if let Err(e) = v.verify_signature(&verifier_key) {
        tracing::warn!(verifier = %v.verifier, error = %e, "JobVerification inner signature rejected");
        return;
    }

    if let Err(e) = ctx.shared.record_verification(&v) {
        tracing::trace!(error = %e, "record_verification (likely FK · result not yet seen)");
        return;
    }
    tracing::debug!(result_hash = %v.result_hash, verifier = %v.verifier, "observed JobVerification");

    // Slot the verification into the matching open quorum so the
    // finaliser can close it.
    let result_hash = v.result_hash;
    let mut q = ctx.quorums.lock();
    if let Some(oq) = q.get_mut(&result_hash) {
        let verifier_rep_u32 = ctx
            .shared
            .reputation_of(v.verifier)
            .ok()
            .and_then(|r| if r < 0 { None } else { Some(r as u32) })
            .unwrap_or(parseh_verify::params::PROBATIONARY_REP_FLOOR);
        match oq.quorum.add_verification(v.clone(), verifier_rep_u32, &verifier_key) {
            Ok(()) => tracing::trace!(%result_hash, "added verification to open quorum"),
            Err(parseh_verify::VerifyError::Internal(msg))
                if msg.contains("duplicate") || msg.contains("result_hash") =>
            {
                tracing::trace!(error = %msg, "ignoring duplicate / mismatched verification");
            }
            Err(e) => {
                tracing::trace!(error = %e, "add_verification failed");
            }
        }
    }
}

/// Drain the [`VerifyBuffer`] — retry signature verification for any
/// messages whose publisher key is now in the registry, drop entries
/// that have timed out.
#[allow(clippy::too_many_arguments)]
async fn drain_verify_buffer(
    swarm: &mut libp2p::Swarm<ParsehBehaviour>,
    peer_registry: &PeerRegistry,
    verify_buffer: &VerifyBuffer,
    shared: &Arc<SharedState>,
    quorums: &Arc<Mutex<LruCache<ContentHash, OpenQuorum>>>,
    rate_limit: &Arc<Mutex<HashMap<PeerId, RateBucket>>>,
    local_peer_id: PeerId,
    signing_key: &ed25519_dalek::SigningKey,
    readiness: &ReadinessTracker,
    cfg: &MinerConfig,
    executor: &dyn Executor,
) {
    let (ready, expired) =
        verify_buffer.drain_ready(Instant::now(), |p| peer_registry.verifying_key(p).is_some());
    if expired > 0 {
        tracing::trace!(expired, "verify_buffer · dropped expired messages");
    }
    if ready.is_empty() {
        return;
    }
    for pending in ready {
        // The buffer-drain path replays buffered gossipsub messages
        // (tasks/verify); it never touches the state-sync request-
        // response path, so fresh empty maps/tracker are correct here.
        let ctx = DispatchCtx {
            cfg,
            executor,
            peer_registry,
            shared: Arc::clone(shared),
            quorums: Arc::clone(quorums),
            rate_limit: Arc::clone(rate_limit),
            state_sync_rate_limit: Arc::new(Mutex::new(HashMap::new())),
            isolation: Arc::new(Mutex::new(IsolationTracker::new())),
            verify_buffer: verify_buffer.clone(),
            readiness: readiness.clone(),
            local_peer_id,
            signing_key,
        };
        match pending.tag {
            None => {
                // tasks.v1 — full payload is the JobSpec.
                let fake_msg = gossipsub::Message {
                    source: Some(pending.publisher),
                    data: pending.payload,
                    sequence_number: None,
                    topic: gossipsub::IdentTopic::new(TOPIC_TASKS).into(),
                };
                handle_tasks_message(swarm, &ctx, &fake_msg, pending.publisher).await;
            }
            Some(_) => {
                let fake_msg = gossipsub::Message {
                    source: Some(pending.publisher),
                    data: pending.payload,
                    sequence_number: None,
                    topic: gossipsub::IdentTopic::new(TOPIC_VERIFY).into(),
                };
                handle_verify_message(swarm, &ctx, &fake_msg, pending.publisher).await;
            }
        }
    }
}

/// Handle an inbound `parseh.state-deltas.v1` message — a CBOR
/// [`StateDelta`] envelope.
fn handle_state_delta_message(_shared: &SharedState, message: &gossipsub::Message) {
    let delta: StateDelta = match StateDelta::decode_cbor(&message.data) {
        Ok(d) => d,
        Err(e) => {
            tracing::trace!(error = %e, "decode StateDelta");
            return;
        }
    };
    // V0.2.1: applying the delta requires the observer's pubkey. We
    // persist a debug log line so we can observe propagation on the
    // wire; the actual `apply_delta` call (with verified signature)
    // ships once the peer-key directory is in place. The shared-state
    // crate's `apply_delta` does not accept "trust me" mode.
    tracing::debug!(observer = %delta.observer, ?delta.kind, "observed StateDelta · application pending peer-key directory");
}

// ───── LLM detection / download integration ────────────────────────────────

async fn handle_llm_detection(
    detection: &DetectionResult,
    auto_download_llm: bool,
) -> Result<()> {
    match detection.recommended_runtime() {
        Some(rt) => {
            info!(runtime = ?rt, "local LLM runtime detected");
            Ok(())
        }
        None if auto_download_llm => {
            info!(
                "no local LLM found · --auto-download-llm passed, treating as implicit consent for TinyLlama download"
            );
            let consent = parseh_llm_downloader::Consent::obtain(|| async { true })
                .await
                .expect("--auto-download-llm flag is implicit consent");
            let spec = parseh_llm_downloader::ModelCatalog::default_recommended();
            let progress: parseh_llm_downloader::ProgressFn = std::sync::Arc::new(
                |done: u64, total: u64| {
                    const STEP: u64 = 10 * 1024 * 1024;
                    if done.is_multiple_of(STEP) || done == total {
                        info!(done, total, "model download progress");
                    }
                },
            );
            let result = parseh_llm_downloader::download_model(spec, consent, Some(progress))
                .await
                .context("TinyLlama download failed")?;
            info!(
                path = %result.path.display(),
                bytes = result.bytes_written,
                sha256_verified = result.sha256_verified,
                "model downloaded · restart miner with `inference = true` in miner.toml to advertise it"
            );
            Ok(())
        }
        None => {
            info!("no local LLM found · miner will advertise non-inference capabilities only");
            info!(
                "hint: install Ollama (https://ollama.ai) and `ollama pull tinyllama`, or pass --auto-download-llm to fetch TinyLlama (~640 MB)"
            );
            Ok(())
        }
    }
}

// Keep the public `RecommendedRuntime` re-export referenced so an unused
// import does not trip `-D warnings`.
const _: fn() = || {
    fn _assert<T>() {}
    _assert::<RecommendedRuntime>();
};

// ───── libp2p behaviour ─────────────────────────────────────────────────────

#[derive(NetworkBehaviour)]
#[behaviour(to_swarm = "ParsehEvent")]
struct ParsehBehaviour {
    ping: ping::Behaviour,
    identify: identify::Behaviour,
    kad: kad::Behaviour<kad::store::MemoryStore>,
    gossipsub: gossipsub::Behaviour,
    job: request_response::cbor::Behaviour<JobOrder, V1JobResult>,
    /// `/parseh/state-sync/1.0.0` — anti-entropy pull (V0.2.5). A
    /// reconnecting / freshly-joined peer requests the outcomes it
    /// might have missed; the responder answers with signed outcomes.
    state_sync:
        request_response::cbor::Behaviour<StateSyncRequest, StateSyncResponse>,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum ParsehEvent {
    Ping(ping::Event),
    Identify(identify::Event),
    Kad(kad::Event),
    Gossipsub(gossipsub::Event),
    JobReqRes(request_response::Event<JobOrder, V1JobResult>),
    StateSync(request_response::Event<StateSyncRequest, StateSyncResponse>),
}
impl From<ping::Event> for ParsehEvent {
    fn from(e: ping::Event) -> Self {
        Self::Ping(e)
    }
}
impl From<identify::Event> for ParsehEvent {
    fn from(e: identify::Event) -> Self {
        Self::Identify(e)
    }
}
impl From<kad::Event> for ParsehEvent {
    fn from(e: kad::Event) -> Self {
        Self::Kad(e)
    }
}
impl From<gossipsub::Event> for ParsehEvent {
    fn from(e: gossipsub::Event) -> Self {
        Self::Gossipsub(e)
    }
}
impl From<request_response::Event<JobOrder, V1JobResult>> for ParsehEvent {
    fn from(e: request_response::Event<JobOrder, V1JobResult>) -> Self {
        Self::JobReqRes(e)
    }
}
impl From<request_response::Event<StateSyncRequest, StateSyncResponse>> for ParsehEvent {
    fn from(e: request_response::Event<StateSyncRequest, StateSyncResponse>) -> Self {
        Self::StateSync(e)
    }
}

fn build_swarm(
    kp: identity::Keypair,
    _peer_id: &PeerId,
) -> Result<libp2p::Swarm<ParsehBehaviour>> {
    use libp2p::{noise, tcp, yamux};

    let swarm = SwarmBuilder::with_existing_identity(kp.clone())
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
            let local_peer = PeerId::from(key.public());

            let store = kad::store::MemoryStore::new(local_peer);
            let mut kad_cfg = kad::Config::default();
            kad_cfg.set_protocol_names(vec![StreamProtocol::new("/parseh/kad/1.0.0")]);
            let kad = kad::Behaviour::with_config(local_peer, store, kad_cfg);

            let gossipsub_cfg = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10))
                .validation_mode(gossipsub::ValidationMode::Strict)
                .build()
                .map_err(std::io::Error::other)?;
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_cfg,
            )
            .map_err(std::io::Error::other)?;

            // V0.2 registers BOTH protocols. The 1.0.0 protocol is
            // logged as deprecated on inbound traffic; 2.0.0 carries
            // the new `JobSpec`/`JobResult` shape and is preferred.
            // We keep the existing `request_response::cbor` channel
            // for 1.0.0 — the wire types are stable — and the 2.0.0
            // protocol will gain its own channel in the next agent's
            // batch (it requires a second behaviour instance because
            // the type signature differs).
            let job_cfg = request_response::Config::default();
            let job = request_response::cbor::Behaviour::<JobOrder, V1JobResult>::new(
                [
                    (
                        StreamProtocol::new(PARSEH_JOB_PROTOCOL_V1),
                        request_response::ProtocolSupport::Full,
                    ),
                    (
                        StreamProtocol::new(PARSEH_JOB_PROTOCOL_V2),
                        request_response::ProtocolSupport::Full,
                    ),
                ],
                job_cfg,
            );

            // `/parseh/state-sync/1.0.0` — anti-entropy pull. Both
            // directions: a node both answers inbound sync requests and
            // issues its own after an isolation window / on the periodic
            // backstop tick. See the project notes.
            let state_sync =
                request_response::cbor::Behaviour::<StateSyncRequest, StateSyncResponse>::new(
                    [(
                        StreamProtocol::new(PARSEH_STATE_SYNC_PROTOCOL_V1),
                        request_response::ProtocolSupport::Full,
                    )],
                    request_response::Config::default(),
                );

            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(ParsehBehaviour {
                ping: ping::Behaviour::new(ping::Config::new()),
                identify: identify::Behaviour::new(identify::Config::new(
                    PARSEH_PROTOCOL_VERSION.into(),
                    key.public(),
                )),
                kad,
                gossipsub,
                job,
                state_sync,
            })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
        .build();
    Ok(swarm)
}
