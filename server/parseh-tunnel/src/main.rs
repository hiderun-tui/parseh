//! `parseh-tunnel` binary entrypoint.
//!
//! See the crate-level docs (`lib.rs`) for what this binary does and
//! the binding non-claims around censorship resistance / anonymity.
//!
//! Subcommands:
//!
//! ```text
//!   parseh-tunnel start [--port 9050] [--bootstrap MULTIADDR]...
//!   parseh-tunnel status [--port 9050]
//!   parseh-tunnel test  URL    # round-trip a target via a synthetic tunnel
//! ```

#![forbid(unsafe_code)]

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use libp2p::identity;
use tracing::{info, warn};

use parseh_core::PeerRegistry;
use parseh_tunnel::{
    readiness::{DISCLAIMER, ExitRow, StatusReport},
    router::ExitSelector,
    swarm::{build_swarm, parse_bootstrap, subscribe_caps_topic},
    tunnel::require_bootstrap,
    DEFAULT_SOCKS5_PORT, VERSION,
};

// ───── CLI ──────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "parseh-tunnel",
    version,
    about = "Client-side SOCKS5 tunnel that exits via volunteer PARSEH peers · V0.2.5 scaffold",
    long_about = "parseh-tunnel — SOCKS5 (RFC 1928) → libp2p stream → exit peer → open internet.\n\
                  \n\
                  V0.2.5 SCAFFOLD. NOT anonymous (single-hop reveals target to exit).\n\
                  NOT censorship-resistant (hostile-network survivability NOT yet measured).\n\
                  See README for what is stubbed vs real.\n\
                  \n\
                  License: Apache-2.0  ·  Repo: github.com/hiderun-tui/parseh"
)]
struct Cli {
    /// Increase logging verbosity. Repeat for more (-vv).
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the SOCKS5 listener + join the PARSEH network.
    Start {
        /// SOCKS5 listen port on `127.0.0.1`. Defaults to 9050 (Tor-style).
        #[arg(long, default_value_t = DEFAULT_SOCKS5_PORT)]
        port: u16,
        /// Bootstrap multiaddrs (repeatable). At least one is required so
        /// the swarm has somewhere to dial; without it the tunnel cannot
        /// discover exits and exits early with a friendly error.
        #[arg(long, value_name = "MULTIADDR")]
        bootstrap: Vec<String>,
    },
    /// Print the currently known exits + their advertised bandwidth.
    Status {
        /// SOCKS5 port to report in the status payload (for consistency
        /// with the running daemon's `start --port`).
        #[arg(long, default_value_t = DEFAULT_SOCKS5_PORT)]
        port: u16,
    },
    /// Round-trip a target URL through a synthetic tunnel session. V0.2.5
    /// stops at the control round-trip and prints a non-zero exit code,
    /// so the smoke-test reflects what the scaffold can actually do.
    Test {
        /// Target URL. The scheme is ignored; `host:port` is extracted.
        url: String,
    },
}

// ───── entry ────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Command::Start { port, bootstrap } => cmd_start(port, bootstrap).await,
        Command::Status { port } => cmd_status(port).await,
        Command::Test { url } => cmd_test(url).await,
    }
}

fn init_tracing(verbose: u8) {
    let default_level = match verbose {
        0 => "parseh_tunnel=info,libp2p=warn",
        1 => "parseh_tunnel=debug,libp2p=info",
        _ => "parseh_tunnel=trace,libp2p=debug",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_level.into());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

async fn cmd_start(port: u16, bootstrap: Vec<String>) -> Result<()> {
    // Friendly early exit if no bootstrap addresses were provided. We
    // refuse to bind the SOCKS5 port in that case because every accepted
    // connection would fail with NoExitAvailable.
    if let Err(msg) = require_bootstrap(&bootstrap) {
        eprintln!("parseh-tunnel: {msg}");
        eprintln!();
        eprintln!("Example:");
        eprintln!(
            "  parseh-tunnel start --bootstrap /ip4/1.2.3.4/tcp/8421/p2p/12D3Koo..."
        );
        std::process::exit(2);
    }

    let kp = identity::Keypair::generate_ed25519();
    let peer_id = libp2p::PeerId::from(kp.public());
    info!(%peer_id, version = VERSION, "parseh-tunnel starting (ephemeral identity)");
    info!("DISCLAIMER · {DISCLAIMER}");

    let mut swarm = build_swarm(kp).context("build swarm")?;
    let _topic = subscribe_caps_topic(&mut swarm).context("subscribe caps topic")?;

    for raw in &bootstrap {
        match parse_bootstrap(raw) {
            Ok(addr) => match swarm.dial(addr.clone()) {
                Ok(_) => info!(%addr, "bootstrap dial scheduled"),
                Err(e) => warn!(%addr, error = %e, "bootstrap dial failed"),
            },
            Err(e) => warn!(input = %raw, error = %e, "ignoring invalid --bootstrap"),
        }
    }

    let registry = Arc::new(PeerRegistry::new());
    let _selector = ExitSelector::new(registry.clone());

    // SOCKS5 listener bring-up. The V0.2.5 scaffold logs that the
    // listener WOULD bind at the requested loopback port; the swarm-
    // driven accept loop that hands each SOCKS5 socket to `Tunnel::
    // run_session` is the merge-time wiring with the parallel registry
    // agent. We log the configured address + the next steps so an
    // operator running the binary today gets accurate observability.
    let listen_addr = parseh_tunnel::socks5::Socks5Listener::loopback(port).addr();
    info!(
        %listen_addr,
        bootstrap_count = bootstrap.len(),
        "SOCKS5 listener configured (loopback only) · V0.2.5 swarm-accept wiring pending PeerIdentity registry merge"
    );

    // Honour Ctrl-C so the binary is a well-behaved foreground process.
    info!("running · Ctrl-C to stop");
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal · exiting");
    Ok(())
}

async fn cmd_status(port: u16) -> Result<()> {
    // V0.2.5: status without a running daemon is a snapshot of an empty
    // registry. Once `parseh-tunnel start` writes its discovered peers
    // to a shared file (or surfaces them via a local control socket —
    // see V0.3+ roadmap), `status` reads from that surface; today we
    // emit the deterministic empty-state report so the schema is stable.
    let report = StatusReport {
        version: VERSION.to_string(),
        local_peer_id: None,
        socks5_listen: parseh_tunnel::socks5::Socks5Listener::loopback(port)
            .addr()
            .to_string(),
        ranked_exits: Vec::<ExitRow>::new(),
        disclaimer: DISCLAIMER,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn cmd_test(url: String) -> Result<()> {
    let (host, port) = parse_host_port(&url)?;
    eprintln!("parseh-tunnel test: target = {host}:{port}");
    eprintln!(
        "parseh-tunnel test: V0.2.5 scaffold stops at the control round-trip · \
         end-to-end byte copy lands with the PeerIdentity registry merge"
    );
    std::process::exit(1);
}

fn parse_host_port(url: &str) -> Result<(String, u16)> {
    // Trim a leading scheme if present.
    let mut s = url;
    if let Some(rest) = s.strip_prefix("https://") {
        s = rest;
    } else if let Some(rest) = s.strip_prefix("http://") {
        s = rest;
    }
    // Drop any path.
    if let Some(idx) = s.find('/') {
        s = &s[..idx];
    }
    // Split host:port; default port 443 (we assume HTTPS targets).
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().context("parse port")?),
        None => (s.to_string(), 443u16),
    };
    if host.is_empty() {
        anyhow::bail!("URL has no host: {url}");
    }
    Ok((host, port))
}
