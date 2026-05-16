//! Clap-derived CLI surface.
//!
//! Every subcommand has its own `--help` with examples. Global flags
//! `--db`, `--identity`, and `--verbose` are propagated to every
//! subcommand.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Crate version surfaced via `parseh_cli::VERSION` and `parseh --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// PARSEH developer CLI.
///
/// Type `parseh <subcommand> --help` for per-command examples.
#[derive(Debug, Parser)]
#[command(
    name = "parseh",
    version = VERSION,
    about = "Cross-platform PARSEH developer CLI · task submission · network status · acceptance tests · issue reporting",
    long_about = "PARSEH developer CLI.\n\
                  \n\
                  Short-lived invocations only. The miner binary (`parseh-miner`) is the\n\
                  long-lived process; this CLI reads its SharedState DB and submits via\n\
                  short libp2p sessions when needed.\n\
                  \n\
                  License: Apache-2.0  ·  Repo: github.com/hiderun-tui/parseh"
)]
pub struct Cli {
    /// Override the SharedState DB path.
    ///
    /// Default: `$HOME/.parseh/shared-state.db` (matching miner default).
    /// Can also be set via the `PARSEH_DB` environment variable.
    #[arg(long, global = true, env = "PARSEH_DB", value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Override the libp2p identity file path.
    ///
    /// Default: `$HOME/.config/parseh/identity.ed25519` (matching miner
    /// default). Set via `PARSEH_IDENTITY`.
    #[arg(long, global = true, env = "PARSEH_IDENTITY", value_name = "PATH")]
    pub identity: Option<PathBuf>,

    /// Increase logging verbosity (-v info, -vv debug, -vvv trace).
    ///
    /// Logs go to stderr; subcommand output stays on stdout for piping.
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print network + local-state status (JSON by default).
    ///
    /// Examples:
    ///   parseh status
    ///   parseh status --text
    ///   parseh status --db /custom/path.db
    Status {
        /// Human-readable output instead of JSON.
        #[arg(long)]
        text: bool,
    },

    /// Probe local LLM runtimes (Ollama, llama.cpp, GGUF, GPU).
    ///
    /// Wraps `parseh-llm-detect`. Output: JSON by default.
    Detect {
        /// Human-readable output instead of JSON.
        #[arg(long)]
        text: bool,
    },

    /// Submit a JobSpec to the network.
    ///
    /// Builds + signs + serialises a JobSpec. With no running miner this
    /// is offline — the CLI prints the signed CBOR and content hash but
    /// does not propagate. With a miner running the CLI delegates
    /// submission (V0.3+ wiring; for V0.2 the offline path is sufficient
    /// because the testnet drives end-to-end coverage).
    ///
    /// Examples:
    ///   parseh submit "Hello, PARSEH"
    ///   parseh submit --file ./prompt.txt
    ///   parseh submit --speak    # speak prompt in Persian (needs parseh-stt)
    Submit {
        /// Prompt text (positional). Conflicts with `--file`.
        #[arg(value_name = "PROMPT")]
        prompt: Option<String>,

        /// Read prompt from a file. Conflicts with positional prompt.
        #[arg(long, value_name = "PATH", conflicts_with = "prompt")]
        file: Option<PathBuf>,

        /// Speak the prompt in Persian via parseh-tts after submission.
        #[arg(long)]
        speak: bool,

        /// Seed for deterministic inference. Default: 0 (V0.2 testnet uses
        /// a fixed seed for reproducibility).
        #[arg(long, default_value = "0")]
        seed: u64,

        /// Mark this submission as sensitive (9-of-15 quorum). Default:
        /// insensitive (3-of-5).
        #[arg(long)]
        sensitive: bool,
    },

    /// Tail the SharedState delta stream (live view).
    ///
    /// Polls the DB for outcomes observed after the start time and prints
    /// each on a new line as JSON. Exits on Ctrl-C.
    Tail {
        /// Poll interval in milliseconds.
        #[arg(long, default_value = "1000")]
        interval_ms: u64,
        /// Maximum number of deltas to print before exiting. 0 = unlimited.
        #[arg(long, default_value = "0")]
        max: u64,
    },

    /// Run protocol tests.
    ///
    /// Examples:
    ///   parseh test                 # cargo test --workspace --release
    ///   parseh test --acceptance    # 3-node acceptance test
    ///   parseh test --report        # both + write a markdown report
    Test {
        /// Run the 3-node acceptance test from `parseh-testnet`.
        #[arg(long)]
        acceptance: bool,
        /// Run both unit + acceptance, then write a markdown report to
        /// the OS temp dir.
        #[arg(long)]
        report: bool,
    },

    /// Open a `gh issue create` with logs, env info, and a steps-to-
    /// reproduce template. Falls back to printing markdown if `gh` is
    /// missing.
    ///
    /// Examples:
    ///   parseh report-issue
    ///   parseh report-issue --attach /tmp/parseh-test-report-XXXXXX.md
    #[command(name = "report-issue")]
    ReportIssue {
        /// Attach an existing report file to the issue body.
        #[arg(long, value_name = "PATH")]
        attach: Option<PathBuf>,
        /// Title for the GitHub issue.
        #[arg(long, default_value = "parseh: developer report")]
        title: String,
        /// Print the body and exit even if `gh` is on PATH.
        #[arg(long)]
        dry_run: bool,
    },

    /// List peers in SharedState with capabilities + reputation.
    ///
    /// V0.2 reads from the local SharedState only; live capability gossip
    /// is owned by the miner. With no miner, expect an empty list.
    Peers {
        /// Filter peers by service kind (case-insensitive substring).
        #[arg(long, value_name = "KIND")]
        filter: Option<String>,
    },

    /// Print the local identity (PeerId + reputation) and exit.
    Whoami,

    /// Speak text in Persian via `an optional local Persian TTS helper`.
    ///
    /// Exits with code 3 if the wrapper is not installed.
    Tts {
        /// Text to speak.
        #[arg(value_name = "TEXT")]
        text: String,
        /// Language hint ("fa" or "en"). Passed straight to the wrapper.
        #[arg(long, default_value = "fa")]
        lang: String,
    },

    /// Transcribe speech via `an optional local STT helper (V0.3+)` (once landed).
    ///
    /// Exits with code 3 if the wrapper is not installed.
    Stt {
        /// Duration to record in seconds.
        #[arg(long, default_value = "5")]
        seconds: u32,
    },

    /// Manage the local SOCKS5 → PARSEH-peer tunnel.
    ///
    /// Thin wrapper over the `parseh-tunnel` binary (which lives in its
    /// own crate, `server/parseh-tunnel/`). The wrapper shells out so
    /// the CLI's dependency closure does not pull libp2p; users who
    /// prefer to invoke the binary directly can do so with no behaviour
    /// change. V0.2.5 SCAFFOLD — see `server/parseh-tunnel/README.md`.
    ///
    /// Examples:
    ///   parseh tunnel start --bootstrap /ip4/1.2.3.4/tcp/8421/p2p/12D3Koo...
    ///   parseh tunnel status
    ///   parseh tunnel test https://example.com
    ///   parseh tunnel stop
    Tunnel {
        #[command(subcommand)]
        action: TunnelAction,
    },
}

/// Tunnel subcommand actions. The CLI shells out to `parseh-tunnel` for
/// `start`, `status`, and `test`; `stop` is implemented locally by
/// killing a discovered pidfile (best-effort — the binary does not yet
/// write one; V0.3+ feature).
#[derive(Debug, Subcommand)]
pub enum TunnelAction {
    /// Start the SOCKS5 listener and join the PARSEH network.
    Start {
        /// SOCKS5 listen port on `127.0.0.1`.
        #[arg(long, default_value = "9050")]
        port: u16,
        /// Bootstrap multiaddrs (repeatable).
        #[arg(long, value_name = "MULTIADDR")]
        bootstrap: Vec<String>,
    },
    /// Print known exit peers + their advertised bandwidth.
    Status {
        /// SOCKS5 port to report in the status payload.
        #[arg(long, default_value = "9050")]
        port: u16,
    },
    /// Round-trip a target URL through a synthetic tunnel session.
    Test {
        /// Target URL.
        #[arg(value_name = "URL")]
        url: String,
    },
    /// Stop a running `parseh-tunnel` daemon (best-effort).
    Stop,
}
