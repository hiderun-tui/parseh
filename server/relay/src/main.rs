//! `parseh-relay` — libp2p-based relay node.
//!
//! What this binary does today:
//!   1. Loads (or, on first run, generates and persists) a stable ed25519
//!      identity at `<config_dir>/relay-identity.ed25519` (mode 0600 on Unix).
//!      Two consecutive starts therefore advertise the same PeerId, which is
//!      a hard prerequisite for peer reputation and DHT bootstrap.
//!   2. Starts a libp2p swarm with Ping + Identify + Kademlia DHT + Gossipsub
//!      protocols. Two relays on the same LAN will find each other through
//!      DHT once at least one has seen the other (no manual `--dial` needed).
//!   3. Listens on the configured TCP multiaddr.
//!   4. Subscribes to the `parseh.caps.v1` gossipsub topic so it sees miner
//!      capability advertisements as they propagate.
//!   5. Prints discovered peers, ping RTTs, and (at debug level) peer caps.
//!
//! What it will do in V0.1:
//!   - Speak the PARSEH stealth-transport protocol (TLS-mimicry)
//!   - Submit signed service receipts to the chain
//!
//! See: the project notes §3 for the transport requirements.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use libp2p::{
    gossipsub, identify, kad, noise, ping,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, SwarmBuilder,
};
use tracing::{debug, info, warn};

mod identity_store;

#[cfg(feature = "reality")]
use parseh_relay::reality;

const PARSEH_PROTOCOL_VERSION: &str = "/parseh/0.1.0";
const PARSEH_KAD_PROTOCOL:     &str = "/parseh/kad/1.0.0";
const PARSEH_CAPS_TOPIC:       &str = "parseh.caps.v1";

/// Command-line options.
#[derive(Debug, Parser)]
#[command(
    name = "parseh-relay",
    about = "PARSEH stealth-transport relay node (libp2p)",
    version
)]
struct Cli {
    /// Multiaddr to listen on (e.g. /ip4/0.0.0.0/tcp/8421).
    #[arg(long, default_value = "/ip4/0.0.0.0/tcp/8421")]
    listen: String,

    /// Optional peer multiaddrs to dial on startup.
    #[arg(long = "dial", value_delimiter = ',')]
    dial: Vec<String>,

    /// Override the config directory (default: OS-conventional, e.g.
    /// `$XDG_CONFIG_HOME/parseh` on Linux, `%APPDATA%\parseh` on Windows,
    /// `~/Library/Application Support/parseh` on macOS). The persistent
    /// ed25519 identity is stored as `relay-identity.ed25519` inside it.
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// Path to a TOML file containing a `[reality]` section. Only
    /// honoured when this binary was built with `--features reality`;
    /// in default builds the flag is accepted (for forward compat)
    /// but a warning is logged and the value is ignored.
    #[arg(long)]
    reality_config: Option<PathBuf>,
}

/// Returns the OS-conventional config directory for PARSEH binaries.
/// Mirrors `parseh-miner`'s `config::default_config_dir` so a host can
/// share one directory across both daemons.
fn default_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("parseh"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "parseh_relay=info,libp2p=warn".into()),
        )
        .init();

    let cli = Cli::parse();

    let config_dir = cli
        .config_dir
        .clone()
        .or_else(default_config_dir)
        .context("could not determine config directory · pass --config-dir")?;

    let (local_key, created) = identity_store::load_or_generate(&config_dir)
        .with_context(|| format!("load relay identity from {}", config_dir.display()))?;
    let local_peer_id = PeerId::from(local_key.public());
    info!(
        peer = %local_peer_id,
        config_dir = %config_dir.display(),
        identity_status = if created { "generated" } else { "loaded" },
        "parseh-relay starting"
    );

    let mut swarm = build_swarm(local_key)?;

    // ── REALITY stealth-transport wiring (feature-gated) ────────────
    //
    // V0.2.1 scaffold: when `--features reality` is on AND the operator
    // points us at a `[reality]` TOML config, we spawn the xray-core
    // subprocess so the libp2p transport can later route through it.
    // The libp2p swarm itself still listens on plain TCP today — the
    // full transport-replacement is V0.2.5 work documented in
    // the project notes. Keeping the spawn here
    // (vs hiding it inside `build_swarm`) so the lifecycle is obvious
    // in the binary's startup log.
    #[cfg(feature = "reality")]
    let _reality_handle = match cli.reality_config.as_ref() {
        Some(path) => match load_reality_config(path) {
            Ok(cfg) => match reality::RealitySubprocess::spawn(&cfg).await {
                Ok(sp) => {
                    info!(socks = %sp.socks_addr(), "REALITY subprocess online");
                    Some(sp)
                }
                Err(e) => {
                    warn!(error = %e, "REALITY subprocess failed to spawn · falling back to plain TCP");
                    None
                }
            },
            Err(e) => {
                warn!(error = %e, path = %path.display(), "could not load --reality-config");
                None
            }
        },
        None => None,
    };

    #[cfg(not(feature = "reality"))]
    if cli.reality_config.is_some() {
        warn!(
            "--reality-config supplied but binary was built without --features reality · ignoring"
        );
    }

    let listen: Multiaddr = cli.listen.parse().context("invalid --listen multiaddr")?;
    swarm.listen_on(listen.clone())?;

    for raw in &cli.dial {
        let addr: Multiaddr = match raw.parse() {
            Ok(a) => a,
            Err(e) => {
                warn!(addr = %raw, err = %e, "ignoring invalid --dial");
                continue;
            }
        };
        if let Err(e) = swarm.dial(addr.clone()) {
            warn!(addr = %addr, err = %e, "dial failed");
        } else {
            info!(addr = %addr, "dial scheduled");
        }
    }

    // Subscribe to the capability gossipsub topic so the relay observes
    // miner capability advertisements as they propagate through the mesh.
    let caps_topic = gossipsub::IdentTopic::new(PARSEH_CAPS_TOPIC);
    swarm
        .behaviour_mut()
        .gossipsub
        .subscribe(&caps_topic)
        .context("subscribe to caps topic")?;

    info!("event loop running · Ctrl-C to stop");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown signal · exiting");
                break;
            }
            ev = swarm.select_next_some() => match ev {
                SwarmEvent::NewListenAddr { address, .. } => {
                    info!(address = %address, "listening");
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    info!(%peer_id, "peer connected");
                    // Kick a Kademlia bootstrap as soon as we have any peer
                    // — this is what lets a second relay find a third etc.
                    // without an explicit bootnode list.
                    if let Err(e) = swarm.behaviour_mut().kad.bootstrap() {
                        debug!(error = %e, "kad bootstrap skipped");
                    }
                }
                SwarmEvent::Behaviour(ParsehEvent::Ping(e)) => match e.result {
                    Ok(rtt) => info!(peer = %e.peer, rtt_ms = rtt.as_millis(), "ping"),
                    Err(err) => warn!(peer = %e.peer, error = %err, "ping failed"),
                },
                SwarmEvent::Behaviour(ParsehEvent::Identify(identify::Event::Received {
                    peer_id, info, ..
                })) => {
                    info!(peer = %peer_id, protocols = ?info.protocols, "identified");
                    // Once we know the peer's listen addrs we can teach
                    // Kademlia where to find it. Without this step Kad
                    // routing tables stay empty even after connection.
                    for addr in info.listen_addrs.iter() {
                        swarm
                            .behaviour_mut()
                            .kad
                            .add_address(&peer_id, addr.clone());
                    }
                }
                SwarmEvent::Behaviour(ParsehEvent::Kad(ev)) => {
                    debug!(?ev, "kad");
                }
                SwarmEvent::Behaviour(ParsehEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source, message, ..
                })) if message.topic.as_str() == PARSEH_CAPS_TOPIC => {
                    debug!(
                        from = %propagation_source,
                        bytes = message.data.len(),
                        "peer caps received"
                    );
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Construct the libp2p `Swarm` for the relay.
///
/// Split out of `main()` so the integration test can spin up swarms on
/// ephemeral ports without going through `Cli`.
fn build_swarm(local_key: libp2p::identity::Keypair) -> Result<libp2p::Swarm<ParsehBehaviour>> {
    let swarm = SwarmBuilder::with_existing_identity(local_key)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
            let local_peer = PeerId::from(key.public());

            // Kademlia DHT — in-memory record store; V0.2 swaps for disk-backed.
            // Custom protocol name keeps PARSEH off the public IPFS DHT.
            let store = kad::store::MemoryStore::new(local_peer);
            let mut kad_cfg = kad::Config::default();
            kad_cfg.set_protocol_names(vec![StreamProtocol::new(PARSEH_KAD_PROTOCOL)]);
            let kad = kad::Behaviour::with_config(local_peer, store, kad_cfg);

            // Gossipsub — used by miners to broadcast capability blobs on
            // `parseh.caps.v1`. The relay only subscribes (read-only).
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

            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(ParsehBehaviour {
                ping: ping::Behaviour::new(ping::Config::new()),
                identify: identify::Behaviour::new(identify::Config::new(
                    PARSEH_PROTOCOL_VERSION.into(),
                    key.public(),
                )),
                kad,
                gossipsub,
            })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();
    Ok(swarm)
}

#[derive(NetworkBehaviour)]
#[behaviour(to_swarm = "ParsehEvent")]
struct ParsehBehaviour {
    ping:      ping::Behaviour,
    identify:  identify::Behaviour,
    kad:       kad::Behaviour<kad::store::MemoryStore>,
    gossipsub: gossipsub::Behaviour,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum ParsehEvent {
    Ping(ping::Event),
    Identify(identify::Event),
    Kad(kad::Event),
    Gossipsub(gossipsub::Event),
}
impl From<ping::Event>      for ParsehEvent { fn from(e: ping::Event) -> Self      { Self::Ping(e) } }
impl From<identify::Event>  for ParsehEvent { fn from(e: identify::Event) -> Self  { Self::Identify(e) } }
impl From<kad::Event>       for ParsehEvent { fn from(e: kad::Event) -> Self       { Self::Kad(e) } }
impl From<gossipsub::Event> for ParsehEvent { fn from(e: gossipsub::Event) -> Self { Self::Gossipsub(e) } }

/// Parse a `relay.toml`-style file with a top-level `[reality]` table
/// into a [`reality::RealityConfig`]. Loose schema — extra keys are
/// ignored so this same file can hold other relay settings later.
#[cfg(feature = "reality")]
fn load_reality_config(path: &PathBuf) -> Result<reality::RealityConfig> {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        reality: reality::RealityConfig,
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let w: Wrapper = toml::from_str(&raw)
        .with_context(|| format!("parse TOML at {}", path.display()))?;
    reality::validate(&w.reality)
        .with_context(|| format!("validate [reality] section in {}", path.display()))?;
    Ok(w.reality)
}

// ───── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::identity;

    /// Spin up two relay swarms on ephemeral TCP ports, dial A from B, and
    /// assert that both sides complete an `Identify::Received` handshake
    /// within 5 seconds. Receiving Identify is the trigger our event loop
    /// uses to call `kad.add_address(...)`, so if both sides see it the
    /// Kademlia routing table is being populated — which is the wiring
    /// this issue is about.
    ///
    /// We deliberately avoid probing `kad.kbucket(...)` directly because
    /// that API's exact signature varies across libp2p minor releases and
    /// this test is meant to be a stable smoke check, not a Kad unit test.
    #[tokio::test(flavor = "current_thread")]
    async fn two_relays_discover_each_other_via_identify() -> Result<()> {
        let kp_a = identity::Keypair::generate_ed25519();
        let kp_b = identity::Keypair::generate_ed25519();
        let peer_a = PeerId::from(kp_a.public());
        let peer_b = PeerId::from(kp_b.public());

        let mut sw_a = build_swarm(kp_a)?;
        let mut sw_b = build_swarm(kp_b)?;

        // Listen on ephemeral ports (port 0 → OS-assigned).
        sw_a.listen_on("/ip4/127.0.0.1/tcp/0".parse()?)?;
        sw_b.listen_on("/ip4/127.0.0.1/tcp/0".parse()?)?;

        // Pump events until A has a concrete listen address, then dial it from B.
        let a_addr: Multiaddr = loop {
            match sw_a.select_next_some().await {
                SwarmEvent::NewListenAddr { address, .. } => break address,
                _ => continue,
            }
        };
        let dial_target = a_addr
            .with_p2p(peer_a)
            .map_err(|_| anyhow::anyhow!("could not attach /p2p/ component to multiaddr"))?;
        sw_b.dial(dial_target)?;

        let mut a_identified_b = false;
        let mut b_identified_a = false;

        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            while !(a_identified_b && b_identified_a) {
                tokio::select! {
                    ev = sw_a.select_next_some() => {
                        if let SwarmEvent::Behaviour(ParsehEvent::Identify(
                            identify::Event::Received { peer_id, info, .. }
                        )) = ev {
                            for addr in info.listen_addrs.iter() {
                                sw_a.behaviour_mut().kad.add_address(&peer_id, addr.clone());
                            }
                            if peer_id == peer_b {
                                a_identified_b = true;
                            }
                        }
                    }
                    ev = sw_b.select_next_some() => {
                        if let SwarmEvent::Behaviour(ParsehEvent::Identify(
                            identify::Event::Received { peer_id, info, .. }
                        )) = ev {
                            for addr in info.listen_addrs.iter() {
                                sw_b.behaviour_mut().kad.add_address(&peer_id, addr.clone());
                            }
                            if peer_id == peer_a {
                                b_identified_a = true;
                            }
                        }
                    }
                }
            }
        })
        .await;

        assert!(outcome.is_ok(), "relays did not identify each other within 5s");
        assert!(a_identified_b, "relay A did not identify relay B within 5s");
        assert!(b_identified_a, "relay B did not identify relay A within 5s");
        Ok(())
    }
}
