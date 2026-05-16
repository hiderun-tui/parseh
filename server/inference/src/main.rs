//! `parseh-inference` — distributed LLM inference host (stub).
//!
//! In V0.1 this binary will:
//!   1. Load a quantised model via llama.cpp (or candle-core, TBD)
//!   2. Connect to a PARSEH relay over libp2p
//!   3. Register on-chain as an inference provider (capabilities + stake)
//!   4. Pull jobs from the bounty queue
//!   5. Execute prompts and return signed Attestations
//!
//! Today it parses arguments and prints what it WOULD do. With
//! `--features candle` the binary additionally accepts `--verify-model PATH`
//! to prove a local GGUF/safetensors file loads, returning JSON metadata.

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "parseh-inference", about = "PARSEH inference host", version)]
struct Cli {
    /// Path to a GGUF model file (e.g. ./models/qwen2.5-7b-instruct-q4_k_m.gguf).
    #[arg(long)]
    model: Option<String>,

    /// Self-reported GPU memory in MB (used for capability declaration).
    #[arg(long, default_value_t = 0)]
    gpu_mb: u32,

    /// Relay node libp2p multiaddr to connect to.
    #[arg(long, default_value = "/ip4/127.0.0.1/tcp/8421")]
    relay: String,

    /// LOAD-ONLY model verification: parse the file header, emit JSON metadata,
    /// and exit. Requires the binary to be built with `--features candle`.
    /// Used by the readiness probe to advertise "LLM=Candle/<model>" in the UI.
    #[arg(long, value_name = "PATH")]
    verify_model: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "parseh_inference=info".into()),
        )
        .init();

    let cli = Cli::parse();

    // Early-exit path: --verify-model is a one-shot diagnostic. It runs before
    // any relay/host startup and prints JSON to stdout for the UI to consume.
    if let Some(path) = cli.verify_model.as_deref() {
        return run_verify_model(path).await;
    }

    info!("parseh-inference (stub) starting");
    info!(model = ?cli.model, gpu_mb = cli.gpu_mb, relay = %cli.relay, "config");

    if cli.model.is_none() {
        info!("no --model path · running in capability-advertising mode");
    }

    info!("STUB · would connect to relay and register as inference provider");
    info!("STUB · llama.cpp host loop will land in V0.1");

    // Keep the process alive for a moment so contributors can confirm it ran.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    info!("exiting · build chain is healthy");
    Ok(())
}

// ---------------------------------------------------------------------------
// --verify-model implementation
// ---------------------------------------------------------------------------

#[cfg(feature = "candle")]
async fn run_verify_model(path: &str) -> Result<()> {
    use parseh_inference::candle_runtime;
    use std::path::Path;

    let info = candle_runtime::verify_model_loads(Path::new(path)).await?;
    let tps = candle_runtime::estimated_tokens_per_sec(&info);

    // Emit a single JSON line so the readiness probe / UI can parse it.
    let payload = serde_json::json!({
        "ok": true,
        "runtime": "candle",
        "model": info,
        "estimated_tokens_per_sec": tps,
    });
    println!("{}", serde_json::to_string(&payload)?);
    Ok(())
}

#[cfg(not(feature = "candle"))]
async fn run_verify_model(_path: &str) -> Result<()> {
    anyhow::bail!(
        "--verify-model requires building parseh-inference with `--features candle`. \
         Default builds ship without the Candle runtime to keep binary size small."
    )
}
