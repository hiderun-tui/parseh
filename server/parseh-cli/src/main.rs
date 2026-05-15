//! `parseh-cli` — cross-platform CLI for PARSEH developers.
//!
//! Users type `parseh` in their terminal (Linux / macOS / Windows) and get
//! a clap-derived menu of subcommands for: submitting test jobs, querying
//! the local network state, running protocol acceptance tests, and
//! reporting issues for OSS-contributor fix-up.
//!
//! This binary is **developer ergonomics — not protocol semantics, not
//! economic features.** Specifically, the following are deliberately
//! refused (see `README.md` for the full list with reasons):
//!
//! - Agent marketplace / investment / trading
//! - "Better-agent-earns-more-PARSEH" PoW
//! - Any chain / token logic
//!
//! Reason: V0.2 has just crossed the threshold described in
//! the project notes. The binding
//! guidance from that note is "do NOT immediately jump to token launch,
//! public mining, exchange discussion, large-scale onboarding" — and
//! instead move into adversarial testing, replay fuzzing, network
//! partition simulation, malicious verifier modeling, Sybil stress
//! testing, and state corruption testing. A developer-ergonomics CLI is
//! the right shape for that phase; an economic/marketplace surface is
//! not.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

use anyhow::Result;
use clap::Parser;

mod cli;
mod commands;
mod env_info;
mod identity;
mod paths;
mod runner;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    init_tracing(cli.verbose);

    let outcome = commands::dispatch(cli).await;
    match outcome {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("parseh: error: {e:#}");
            std::process::exit(1);
        }
    }
}

fn init_tracing(verbose: u8) {
    let default_level = match verbose {
        0 => "parseh_cli=info,parseh_shared_state=warn,parseh_task=warn,libp2p=warn",
        1 => "parseh_cli=debug,parseh_shared_state=info,parseh_task=info,libp2p=info",
        _ => "parseh_cli=trace,parseh_shared_state=debug,parseh_task=debug,libp2p=debug",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_level.into());
    // Use try_init — when invoked twice within an integration test the
    // global subscriber is already set and we should not panic.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
