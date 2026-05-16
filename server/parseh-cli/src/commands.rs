//! Subcommand dispatchers.
//!
//! Each `cmd_*` function returns a process exit code as `Result<i32>`.
//! `0` means success; non-zero codes are documented per-command.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as OsCommand, Stdio};

use anyhow::{Context, Result};
use libp2p::PeerId;
use parseh_core::ServiceKind;
use parseh_shared_state::{
    KeyMaterial, KeySource, OpenError, OpenOptions, SharedState,
};
use parseh_task::{JobInputs, JobKind, JobSpec};
use serde::Serialize;

use crate::cli::{Cli, Command, TunnelAction};
use crate::env_info::EnvInfo;
use crate::identity;
use crate::paths;
use crate::runner;

/// Exit code returned when an external tool (TTS / STT) is not installed.
pub const EXIT_TOOL_MISSING: i32 = 3;
/// Exit code returned when the acceptance test FAILS in `parseh test`.
pub const EXIT_ACCEPTANCE_FAIL: i32 = 4;

/// Top-level dispatcher.
pub async fn dispatch(mut cli: Cli) -> Result<i32> {
    // Take the subcommand out of `cli` so the remaining `Cli` (which
    // carries `--db` / `--identity` / `--verbose`) can be borrowed by
    // the dispatcher arms without partially-moved-value errors.
    let cmd = cli.command.take();
    match cmd {
        None => {
            print_overview();
            Ok(0)
        }
        Some(Command::Status { text }) => cmd_status(&cli, text).await,
        Some(Command::Detect { text }) => cmd_detect(text).await,
        Some(Command::Submit {
            prompt,
            file,
            speak,
            seed,
            sensitive,
        }) => cmd_submit(&cli, prompt, file, speak, seed, sensitive).await,
        Some(Command::Tail { interval_ms, max }) => {
            cmd_tail(&cli, interval_ms, max).await
        }
        Some(Command::Test { acceptance, report }) => {
            cmd_test(acceptance, report).await
        }
        Some(Command::ReportIssue {
            attach,
            title,
            dry_run,
        }) => cmd_report_issue(&cli, attach, title, dry_run).await,
        Some(Command::Peers { filter }) => cmd_peers(&cli, filter).await,
        Some(Command::Whoami) => cmd_whoami(&cli).await,
        Some(Command::Tts { text, lang }) => cmd_tts(text, lang).await,
        Some(Command::Stt { seconds }) => cmd_stt(seconds).await,
        Some(Command::Tunnel { action }) => cmd_tunnel(action).await,
    }
}

// ---------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------

fn print_overview() {
    println!("parseh · PARSEH developer CLI");
    println!();
    println!("Try one of:");
    println!("  parseh status              network + local state");
    println!("  parseh whoami              local identity + reputation");
    println!("  parseh peers               list peers in SharedState");
    println!("  parseh detect              probe local LLM runtimes");
    println!("  parseh submit \"prompt\"     submit a JobSpec");
    println!("  parseh tail                tail SharedState delta stream");
    println!("  parseh test                run V0.2 unit tests");
    println!("  parseh test --acceptance   run the 3-node acceptance test");
    println!("  parseh test --report       run all tests + write markdown");
    println!("  parseh report-issue        template-driven `gh issue create`");
    println!("  parseh tts \"سلام\"          speak via parseh-tts");
    println!("  parseh stt                 transcribe via parseh-stt");
    println!();
    println!("All commands accept --help with examples.");
}

// ---------------------------------------------------------------------
// status
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct StatusReport {
    parseh_cli_version: String,
    identity: IdentityReport,
    shared_state: SharedStateReport,
    llm_runtime: serde_json::Value,
    miner_running: bool,
    miner_pid: Option<u32>,
    uptime_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
struct IdentityReport {
    peer_id: String,
    config_dir: String,
    identity_path: String,
}

#[derive(Debug, Serialize)]
struct SharedStateReport {
    db_path: String,
    db_exists: bool,
    schema_version: u32,
    tasks_count: u64,
    outcomes_count: u64,
    reputation_log_entries: u64,
    established_peers: u64,
}

async fn cmd_status(cli: &Cli, text: bool) -> Result<i32> {
    let id_path = paths::resolve_identity_path(cli.identity.clone())?;
    let db_path = paths::resolve_db_path(cli.db.clone())?;

    let (_kp, peer_id, _was_created) = identity::load_or_generate(&id_path)?;

    let config_dir = id_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let db_exists = db_path.exists();
    let shared_state = if db_exists {
        match open_shared_state(&db_path, &id_path) {
            Ok(s) => snapshot_shared_state(&s, &db_path),
            Err(e) => {
                tracing::warn!(error = %e, "could not open SharedState · counts will be zero");
                SharedStateReport {
                    db_path: db_path.display().to_string(),
                    db_exists,
                    schema_version: parseh_shared_state::SCHEMA_VERSION,
                    tasks_count: 0,
                    outcomes_count: 0,
                    reputation_log_entries: 0,
                    established_peers: 0,
                }
            }
        }
    } else {
        SharedStateReport {
            db_path: db_path.display().to_string(),
            db_exists,
            schema_version: parseh_shared_state::SCHEMA_VERSION,
            tasks_count: 0,
            outcomes_count: 0,
            reputation_log_entries: 0,
            established_peers: 0,
        }
    };

    // LLM detection is best-effort — if the host has no Ollama, the
    // probe returns empty rather than failing.
    let detection = parseh_llm_detect::detect_all().await.ok();
    let llm_runtime = match detection {
        Some(d) => serde_json::to_value(&d).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    };

    let (miner_running, miner_pid) = detect_miner_running();

    let report = StatusReport {
        parseh_cli_version: crate::cli::VERSION.to_string(),
        identity: IdentityReport {
            peer_id: peer_id.to_string(),
            config_dir: config_dir.display().to_string(),
            identity_path: id_path.display().to_string(),
        },
        shared_state,
        llm_runtime,
        miner_running,
        miner_pid,
        uptime_secs: None,
    };

    if text {
        print_status_text(&report);
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(0)
}

fn print_status_text(r: &StatusReport) {
    println!("parseh-cli  : {}", r.parseh_cli_version);
    println!("peer_id     : {}", r.identity.peer_id);
    println!("config_dir  : {}", r.identity.config_dir);
    println!("identity    : {}", r.identity.identity_path);
    println!();
    println!("shared-state");
    println!("  path           : {}", r.shared_state.db_path);
    println!("  exists         : {}", r.shared_state.db_exists);
    println!("  schema_version : {}", r.shared_state.schema_version);
    println!("  tasks          : {}", r.shared_state.tasks_count);
    println!("  outcomes       : {}", r.shared_state.outcomes_count);
    println!(
        "  reputation_log : {}",
        r.shared_state.reputation_log_entries
    );
    println!(
        "  established_peers: {}",
        r.shared_state.established_peers
    );
    println!();
    println!(
        "miner_running   : {}{}",
        r.miner_running,
        match r.miner_pid {
            Some(p) => format!(" (pid {p})"),
            None => String::new(),
        }
    );
    if r.llm_runtime.is_null() {
        println!("llm_runtime     : (probe failed or none installed)");
    } else {
        println!("llm_runtime     : (see --json for detail)");
    }
}

fn snapshot_shared_state(s: &SharedState, db_path: &Path) -> SharedStateReport {
    // We don't have a single "count" query; use the existing helpers
    // and let the row counts approximate. recent_tasks(0) returns all
    // tasks regardless of submitted_at.
    let tasks = s.recent_tasks(0).map(|v| v.len() as u64).unwrap_or(0);
    let outcomes = s.deltas_since(0).map(|v| v.len() as u64).unwrap_or(0);
    // established_peers(0) returns peers whose summed reputation is
    // >= 0, which approximates "any peer mentioned in the reputation
    // log". For richer counts the maintainer SQL queries in
    // `detect_repeating_verifier_sets` give exact numbers.
    let est = s
        .established_peers(0)
        .map(|v| v.len() as u64)
        .unwrap_or(0);

    SharedStateReport {
        db_path: db_path.display().to_string(),
        db_exists: true,
        schema_version: parseh_shared_state::SCHEMA_VERSION,
        tasks_count: tasks,
        // `deltas_since` returns one StateDelta per outcome; the count
        // matches the outcomes table. reputation_log_entries is not
        // exposed by the public API and is left as the same approximate
        // floor (established_peers).
        outcomes_count: outcomes,
        reputation_log_entries: est,
        established_peers: est,
    }
}

/// Open the SharedState DB using the identity-file-derived key.
///
/// V0.2 has no passphrase UI; the convention agreed with the miner is to
/// derive the SQLCipher key by SHA-256 of the identity file bytes. See
/// `server/parseh-shared-state/src/cipher.rs` for the security caveat
/// — anyone with read access to `identity.ed25519` can decrypt the DB.
fn open_shared_state(db_path: &Path, identity_path: &Path) -> Result<SharedState> {
    let key = build_identity_key(identity_path)?;
    let opts = OpenOptions {
        path: db_path.to_path_buf(),
        key,
        // We don't create on `status` — that would be confusing. A
        // fresh box with no miner has no DB and we report db_exists=false.
        create_if_missing: db_path.exists(),
    };
    match SharedState::open(opts) {
        Ok(s) => Ok(s),
        Err(OpenError::NotFound) => Err(anyhow::anyhow!(
            "SharedState DB at {} does not exist · run `parseh-miner start` first to create it",
            db_path.display()
        )),
        Err(OpenError::WrongKey) => Err(anyhow::anyhow!(
            "wrong key for SharedState DB at {} · is your identity file the same one the miner used?",
            db_path.display()
        )),
        Err(e) => Err(anyhow::anyhow!("open SharedState: {e}")),
    }
}

/// Build an identity-file-derived SQLCipher key. Used by every command
/// that opens the SharedState DB.
fn build_identity_key(identity_path: &Path) -> Result<KeyMaterial> {
    let bytes = fs::read(identity_path)
        .with_context(|| format!("read identity {}", identity_path.display()))?;
    let key = KeyMaterial::from_source(KeySource::IdentityFile {
        identity_bytes: zeroize::Zeroizing::new(bytes),
    })?;
    Ok(key)
}

/// Detect a running miner by checking for a PID file or socket at one of
/// the conventional locations. The miner does not currently write a PID
/// file — V0.3 work — so this returns `(false, None)` on a stock setup.
/// Documented to keep the schema stable for when the miner does.
fn detect_miner_running() -> (bool, Option<u32>) {
    // Conventional location (matches `~/.parseh/miner.pid` once the
    // miner writes one). Defensive: read but never trust the integer
    // beyond a sanity check that it parses.
    if let Some(home) = dirs::home_dir() {
        let pid_file = home.join(".parseh").join("miner.pid");
        if pid_file.exists() {
            if let Ok(s) = fs::read_to_string(&pid_file) {
                if let Ok(pid) = s.trim().parse::<u32>() {
                    return (true, Some(pid));
                }
            }
        }
    }
    (false, None)
}

// ---------------------------------------------------------------------
// detect
// ---------------------------------------------------------------------

async fn cmd_detect(text: bool) -> Result<i32> {
    let detection = parseh_llm_detect::detect_all()
        .await
        .context("LLM detection failed")?;
    if text {
        println!("recommended_runtime : {:?}", detection.recommended_runtime());
        match detection.ollama.as_ref() {
            Some(o) => {
                println!("ollama              : {} ({})", o.endpoint, o.version);
                for m in &o.models {
                    println!("  model: {}", m.name);
                }
            }
            None => println!("ollama              : (not running)"),
        }
        match detection.llama_cpp.as_ref() {
            Some(l) => println!("llama.cpp           : {}", l.binary_path.display()),
            None => println!("llama.cpp           : (not on PATH)"),
        }
        println!("gguf_files          : {}", detection.gguf_files.len());
        match detection.gpu.as_ref() {
            Some(g) => println!("gpu                 : {} ({} MB)", g.name, g.vram_mb),
            None => println!("gpu                 : (none detected)"),
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&detection)?);
    }
    Ok(0)
}

// ---------------------------------------------------------------------
// submit
// ---------------------------------------------------------------------

async fn cmd_submit(
    cli: &Cli,
    prompt: Option<String>,
    file: Option<PathBuf>,
    speak: bool,
    seed: u64,
    sensitive: bool,
) -> Result<i32> {
    let prompt_text = match (prompt, file) {
        (Some(p), None) => p,
        (None, Some(f)) => fs::read_to_string(&f)
            .with_context(|| format!("read prompt file {}", f.display()))?,
        (None, None) => anyhow::bail!(
            "no prompt given · pass a positional string or --file PATH (--speak alone is V0.3+)"
        ),
        (Some(_), Some(_)) => unreachable!("clap conflicts_with prevents both"),
    };

    let id_path = paths::resolve_identity_path(cli.identity.clone())?;
    let (kp, peer_id, was_created) = identity::load_or_generate(&id_path)?;
    if was_created {
        eprintln!("parseh: generated new identity at {}", id_path.display());
    }
    let signing_key = identity::signing_key_from_libp2p(&kp)?;

    let (spec, hash) = JobSpec::new_signed(
        JobKind::Inference,
        JobInputs::inference_prompt(prompt_text.trim().to_string(), seed),
        ServiceKind::Inference,
        sensitive,
        peer_id,
        &signing_key,
    );

    // Round-trip through CBOR so we surface any encoding edge case
    // immediately. The bytes themselves go on the wire when the V0.3+
    // libp2p submission path lands; for V0.2 we surface them so a user
    // can pipe into a separate transport (e.g. the testnet harness).
    let cbor = parseh_task::to_cbor_bytes(&spec).context("CBOR encode JobSpec")?;

    #[derive(Serialize)]
    struct SubmitReport {
        task_id: String,
        spec_cbor_hex: String,
        submitter: String,
        sensitive: bool,
        bytes: usize,
        note: &'static str,
    }
    let r = SubmitReport {
        task_id: hex::encode(hash.as_bytes()),
        spec_cbor_hex: hex::encode(&cbor),
        submitter: peer_id.to_string(),
        sensitive,
        bytes: cbor.len(),
        note:
            "V0.2: signed offline · network submission via parseh-miner request-response is V0.3+ wiring",
    };
    println!("{}", serde_json::to_string_pretty(&r)?);

    if speak {
        if let Err(e) = shell_to_tts(&prompt_text, "fa").await {
            eprintln!("parseh: --speak requested but TTS failed: {e}");
        }
    }

    Ok(0)
}

// ---------------------------------------------------------------------
// tail
// ---------------------------------------------------------------------

async fn cmd_tail(cli: &Cli, interval_ms: u64, max: u64) -> Result<i32> {
    let id_path = paths::resolve_identity_path(cli.identity.clone())?;
    let db_path = paths::resolve_db_path(cli.db.clone())?;
    if !db_path.exists() {
        eprintln!(
            "parseh: SharedState DB does not exist at {} · run `parseh-miner start` first",
            db_path.display()
        );
        return Ok(2);
    }
    let _ = identity::load_or_generate(&id_path)?; // ensure exists
    let s = open_shared_state(&db_path, &id_path)?;

    let mut watermark: u64 = chrono::Utc::now().timestamp().max(0) as u64;
    let mut emitted: u64 = 0;
    loop {
        let deltas = s.deltas_since(watermark)?;
        for d in deltas {
            // The observer-signed JobOutcome lives inside the delta;
            // emit its JSON shape so a script can pipe `parseh tail | jq`.
            #[derive(Serialize)]
            struct TailLine {
                observer: String,
                observed_at: u64,
                kind: String,
            }
            let line = TailLine {
                observer: d.observer.to_string(),
                observed_at: d.observed_at,
                kind: match &d.kind {
                    parseh_shared_state::DeltaKind::Outcome(_) => "Outcome".to_string(),
                    parseh_shared_state::DeltaKind::Reputation { .. } => "Reputation".to_string(),
                    parseh_shared_state::DeltaKind::GovernanceRule { .. } => {
                        "GovernanceRule".to_string()
                    }
                },
            };
            println!("{}", serde_json::to_string(&line)?);
            watermark = watermark.max(d.observed_at);
            emitted += 1;
            if max != 0 && emitted >= max {
                return Ok(0);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
    }
}

// ---------------------------------------------------------------------
// test
// ---------------------------------------------------------------------

async fn cmd_test(acceptance: bool, report: bool) -> Result<i32> {
    let cwd = std::env::current_dir().context("get cwd")?;
    let workspace = paths::find_workspace_root(&cwd).or_else(|_| {
        // Fall back to the server/ workspace if we're under it.
        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        paths::find_workspace_root(&here)
    })?;

    if report {
        return cmd_test_report(&workspace).await;
    }
    if acceptance {
        return cmd_test_acceptance(&workspace).await;
    }
    // Default: cargo test --workspace --release.
    println!("parseh: running `cargo test --workspace --release` in {}", workspace.display());
    let run = runner::run_capture(
        "cargo",
        &["test", "--workspace", "--release"],
        &workspace,
    )?;
    Ok(if run.success() { 0 } else { 1 })
}

async fn cmd_test_acceptance(workspace: &Path) -> Result<i32> {
    println!(
        "parseh: running 3-node acceptance test `cargo test -p parseh-testnet --release -- --nocapture --test-threads=1` in {}",
        workspace.display()
    );
    let run = runner::run_capture(
        "cargo",
        &[
            "test",
            "-p",
            "parseh-testnet",
            "--release",
            "--",
            "--nocapture",
            "--test-threads=1",
        ],
        workspace,
    )?;
    if run.success() {
        print_acceptance_pass_banner();
        Ok(0)
    } else {
        print_acceptance_fail_banner(&run.tail(30));
        Ok(EXIT_ACCEPTANCE_FAIL)
    }
}

fn print_acceptance_pass_banner() {
    println!();
    println!("==============================================================");
    println!(" parseh V0.2 acceptance: PASS");
    println!();
    println!(" V0.2 is now a functioning distributed coordination primitive.");
    println!("   NOT production-ready");
    println!("   NOT censorship-resistant");
    println!("   NOT economically hardened");
    println!();
    println!(" But REAL.");
    println!();
    println!(" See the project notes for");
    println!(" what comes next (adversarial testing, NOT token launch).");
    println!("==============================================================");
}

fn print_acceptance_fail_banner(tail: &str) {
    println!();
    println!("==============================================================");
    println!(" parseh V0.2 acceptance: FAIL");
    println!();
    println!(" A failed deterministic test with good observability is more");
    println!(" useful than a passing unverifiable prototype. This run is");
    println!(" valuable diagnostic data — `parseh report-issue --attach …`");
    println!(" attaches a report for an OSS contributor to pick up.");
    println!();
    println!(" Last 30 lines of output:");
    println!("--------------------------------------------------------------");
    println!("{tail}");
    println!("==============================================================");
}

async fn cmd_test_report(workspace: &Path) -> Result<i32> {
    println!("parseh: running `cargo test --workspace --release`");
    let unit = runner::run_capture(
        "cargo",
        &["test", "--workspace", "--release"],
        workspace,
    )?;
    println!("parseh: running 3-node acceptance test");
    let acc = runner::run_capture(
        "cargo",
        &[
            "test",
            "-p",
            "parseh-testnet",
            "--release",
            "--",
            "--nocapture",
            "--test-threads=1",
        ],
        workspace,
    )?;

    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let path = std::env::temp_dir().join(format!("parseh-test-report-{ts}.md"));
    let env = EnvInfo::gather();

    let body = format!(
        "# parseh test report · {ts}\n\
         \n\
         Generated by `parseh test --report`. Attach this to a GitHub\n\
         issue with `parseh report-issue --attach {path}`.\n\
         \n\
         ## Environment\n\
         \n\
         {env_md}\n\
         ## Workspace tests · `cargo test --workspace --release`\n\
         \n\
         - exit code: `{unit_code}`\n\
         - verdict: **{unit_verdict}**\n\
         \n\
         <details><summary>tail (50 lines)</summary>\n\n```\n{unit_tail}\n```\n\n</details>\n\
         \n\
         ## 3-node acceptance · `cargo test -p parseh-testnet --release`\n\
         \n\
         - exit code: `{acc_code}`\n\
         - verdict: **{acc_verdict}**\n\
         \n\
         <details><summary>tail (50 lines)</summary>\n\n```\n{acc_tail}\n```\n\n</details>\n\
         \n\
         ## Binary framing\n\
         \n\
         Per the project notes:\n\
         \n\
         - PASS = \"functioning distributed coordination primitive\" — REAL but not production-ready.\n\
         - FAIL = valuable diagnostic data; the next adversarial-testing PR fixes the failure mode.\n\
         \n\
         Reporter must NOT use this report to claim token / marketplace readiness.\n",
        env_md = env.to_markdown(),
        unit_code = unit.status_code,
        unit_verdict = if unit.success() { "PASS" } else { "FAIL" },
        unit_tail = unit.tail(50),
        acc_code = acc.status_code,
        acc_verdict = if acc.success() { "PASS" } else { "FAIL" },
        acc_tail = acc.tail(50),
        path = path.display(),
    );
    fs::write(&path, body).with_context(|| format!("write report {}", path.display()))?;
    println!();
    println!(
        "report saved · attach to a GitHub issue with `parseh report-issue --attach {}`",
        path.display()
    );
    let overall = if unit.success() && acc.success() {
        0
    } else if !acc.success() {
        EXIT_ACCEPTANCE_FAIL
    } else {
        1
    };
    Ok(overall)
}

// ---------------------------------------------------------------------
// report-issue
// ---------------------------------------------------------------------

async fn cmd_report_issue(
    cli: &Cli,
    attach: Option<PathBuf>,
    title: String,
    dry_run: bool,
) -> Result<i32> {
    let id_path = paths::resolve_identity_path(cli.identity.clone())?;
    let (_kp, peer_id, _) = identity::load_or_generate(&id_path)?;
    let env = EnvInfo::gather();
    let logs_tail = tail_local_logs(50);

    let mut body = String::new();
    body.push_str(&format!("# {title}\n\n"));
    body.push_str("## Environment\n\n");
    body.push_str(&env.to_markdown());
    body.push('\n');
    body.push_str(&format!("- peer_id: `{peer_id}`\n"));
    body.push_str(&format!(
        "- identity: `{}`\n\n",
        id_path.display()
    ));
    body.push_str("## Steps to reproduce\n\n");
    body.push_str("1. \n2. \n3. \n\n");
    body.push_str("## Expected\n\n_describe expected behaviour_\n\n");
    body.push_str("## Actual\n\n_describe actual behaviour, paste error message_\n\n");
    if let Some(p) = attach.as_ref() {
        match fs::read_to_string(p) {
            Ok(content) => {
                body.push_str(&format!(
                    "## Attached report · `{}`\n\n",
                    p.display()
                ));
                body.push_str(&content);
                body.push('\n');
            }
            Err(e) => {
                eprintln!(
                    "parseh: could not read --attach {} ({e}) · skipping",
                    p.display()
                );
            }
        }
    }
    body.push_str("\n## Last 50 log lines\n\n```\n");
    body.push_str(&logs_tail);
    body.push_str("\n```\n");

    if dry_run || !runner::gh_available() {
        if !dry_run && !runner::gh_available() {
            eprintln!("parseh: `gh` not found on PATH · printing body instead");
        }
        println!("{body}");
        return Ok(0);
    }

    // Write the body to a temp file because `gh issue create` won't
    // accept arbitrarily large bodies via the shell.
    let tmp = std::env::temp_dir().join(format!(
        "parseh-issue-body-{}.md",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;

    println!(
        "parseh: launching `gh issue create --title \"{}\" --body-file {}`",
        title,
        tmp.display()
    );
    let mut cmd = OsCommand::new("gh");
    cmd.arg("issue")
        .arg("create")
        .arg("--title")
        .arg(&title)
        .arg("--body-file")
        .arg(&tmp);
    let status = cmd.status().context("spawn gh")?;
    Ok(status.code().unwrap_or(1))
}

fn tail_local_logs(n: usize) -> String {
    // Best-effort: look for a conventional log location. The miner does
    // not yet write to a file (it logs to stderr/journald), so on a
    // stock setup this returns a hint rather than panicking.
    let candidates: Vec<PathBuf> = match dirs::home_dir() {
        Some(h) => vec![
            h.join(".parseh").join("miner.log"),
            h.join(".parseh").join("logs").join("miner.log"),
        ],
        None => vec![],
    };
    for c in candidates {
        if let Ok(s) = fs::read_to_string(&c) {
            let lines: Vec<&str> = s.lines().collect();
            let start = lines.len().saturating_sub(n);
            return lines[start..].join("\n");
        }
    }
    "(no log file found at ~/.parseh/miner.log — the miner currently logs to stderr/journald)"
        .to_string()
}

// ---------------------------------------------------------------------
// peers
// ---------------------------------------------------------------------

async fn cmd_peers(cli: &Cli, filter: Option<String>) -> Result<i32> {
    let id_path = paths::resolve_identity_path(cli.identity.clone())?;
    let db_path = paths::resolve_db_path(cli.db.clone())?;
    if !db_path.exists() {
        eprintln!(
            "parseh: SharedState DB does not exist at {} · run `parseh-miner start` first",
            db_path.display()
        );
        println!("[]");
        return Ok(0);
    }
    let _ = identity::load_or_generate(&id_path)?;
    let s = open_shared_state(&db_path, &id_path)?;

    let peers = s.established_peers(0)?;
    #[derive(Serialize)]
    struct PeerRow {
        peer_id: String,
        reputation: i64,
        capabilities: Vec<String>,
    }
    let mut rows: Vec<PeerRow> = Vec::new();
    for p in peers {
        let rep = s.reputation_of(p).unwrap_or(0);
        let caps = guess_capabilities_for(&p);
        if let Some(f) = filter.as_ref() {
            let needle = f.to_lowercase();
            if !caps.iter().any(|c| c.to_lowercase().contains(&needle)) {
                continue;
            }
        }
        rows.push(PeerRow {
            peer_id: p.to_string(),
            reputation: rep,
            capabilities: caps,
        });
    }
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(0)
}

/// V0.2: the SharedState does not yet store capability advertisements
/// (those live in the in-memory `PeerRegistry` owned by the miner). We
/// surface the empty list; a richer capability surface lands when the
/// miner persists ads to the DB. Returning an empty list is the
/// honest answer for V0.2.
fn guess_capabilities_for(_p: &PeerId) -> Vec<String> {
    Vec::new()
}

// ---------------------------------------------------------------------
// whoami
// ---------------------------------------------------------------------

async fn cmd_whoami(cli: &Cli) -> Result<i32> {
    let id_path = paths::resolve_identity_path(cli.identity.clone())?;
    let db_path = paths::resolve_db_path(cli.db.clone())?;
    let (_kp, peer_id, was_created) = identity::load_or_generate(&id_path)?;
    let reputation = if db_path.exists() {
        match open_shared_state(&db_path, &id_path) {
            Ok(s) => s.reputation_of(peer_id).unwrap_or(0),
            Err(_) => 0,
        }
    } else {
        0
    };
    #[derive(Serialize)]
    struct Whoami {
        peer_id: String,
        identity_path: String,
        identity_created_now: bool,
        reputation: i64,
    }
    let w = Whoami {
        peer_id: peer_id.to_string(),
        identity_path: id_path.display().to_string(),
        identity_created_now: was_created,
        reputation,
    };
    println!("{}", serde_json::to_string_pretty(&w)?);
    Ok(0)
}

// ---------------------------------------------------------------------
// tts / stt
// ---------------------------------------------------------------------

async fn cmd_tts(text: String, lang: String) -> Result<i32> {
    shell_to_tts(&text, &lang).await
}

async fn shell_to_tts(text: &str, lang: &str) -> Result<i32> {
    let workspace = paths::find_workspace_root(&PathBuf::from(env!("CARGO_MANIFEST_DIR")))?;
    let script = match runner::find_tts_script(&workspace) {
        Some(p) => p,
        None => {
            eprintln!(
                "parseh: parseh-tts not installed at an optional local Persian TTS helper · install or run from a checkout"
            );
            return Ok(EXIT_TOOL_MISSING);
        }
    };
    let mut cmd = OsCommand::new("bash");
    cmd.arg(&script)
        .arg("--lang")
        .arg(lang)
        .arg(text)
        .stdin(Stdio::null());
    let status = cmd
        .status()
        .with_context(|| format!("spawn {}", script.display()))?;
    Ok(status.code().unwrap_or(EXIT_TOOL_MISSING))
}

async fn cmd_stt(seconds: u32) -> Result<i32> {
    let workspace = paths::find_workspace_root(&PathBuf::from(env!("CARGO_MANIFEST_DIR")))?;
    let script = match runner::find_stt_script(&workspace) {
        Some(p) => p,
        None => {
            eprintln!(
                "parseh: parseh-stt not installed at an optional local STT helper (V0.3+) · this wrapper lands in V0.3"
            );
            return Ok(EXIT_TOOL_MISSING);
        }
    };
    let mut cmd = OsCommand::new("bash");
    cmd.arg(&script)
        .arg("--seconds")
        .arg(seconds.to_string())
        .stdin(Stdio::null());
    let status = cmd
        .status()
        .with_context(|| format!("spawn {}", script.display()))?;
    Ok(status.code().unwrap_or(EXIT_TOOL_MISSING))
}

// ---------------------------------------------------------------------
// tunnel — shell out to `parseh-tunnel`
// ---------------------------------------------------------------------
//
// We shell out (rather than embed the tunnel as a module inside this
// crate) for three reasons:
//
//   1. `parseh-tunnel` pulls in a libp2p Swarm; this CLI is built to be
//      short-lived and small. Linking libp2p into every invocation of
//      `parseh status` would bloat the cold-start time and the binary
//      size for the 99 % of CLI invocations that have nothing to do
//      with the tunnel.
//
//   2. Users who prefer the standalone binary can already type
//      `parseh-tunnel start ...` directly; the `parseh tunnel ...`
//      wrapper exists for discoverability via `parseh --help`, not as
//      a different implementation.
//
//   3. The boundary lets the tunnel evolve its CLI surface (additional
//      subcommands, daemon-mode flags) without re-publishing this CLI
//      crate. We forward args verbatim.

const EXIT_TUNNEL_BINARY_MISSING: i32 = 3;

async fn cmd_tunnel(action: TunnelAction) -> Result<i32> {
    let bin = match resolve_tunnel_binary() {
        Some(p) => p,
        None => {
            eprintln!(
                "parseh: `parseh-tunnel` binary not found. Build with `cargo build --release -p parseh-tunnel` and make sure the resulting binary is on PATH or in `server/target/{{debug,release}}/`."
            );
            return Ok(EXIT_TUNNEL_BINARY_MISSING);
        }
    };
    let mut cmd = OsCommand::new(&bin);
    match action {
        TunnelAction::Start { port, bootstrap } => {
            cmd.arg("start").arg("--port").arg(port.to_string());
            for b in &bootstrap {
                cmd.arg("--bootstrap").arg(b);
            }
        }
        TunnelAction::Status { port } => {
            cmd.arg("status").arg("--port").arg(port.to_string());
        }
        TunnelAction::Test { url } => {
            cmd.arg("test").arg(url);
        }
        TunnelAction::Stop => {
            // V0.2.5: the tunnel binary does not yet write a pidfile, so
            // stop is a best-effort search of common locations. We print
            // a clear note rather than pretending we found something.
            eprintln!(
                "parseh tunnel stop: V0.2.5 scaffold does not yet write a pidfile · send SIGTERM to the running `parseh-tunnel` process manually (e.g. `pkill parseh-tunnel`)."
            );
            return Ok(0);
        }
    }
    let status = cmd
        .status()
        .with_context(|| format!("spawn {}", bin.display()))?;
    Ok(status.code().unwrap_or(EXIT_TUNNEL_BINARY_MISSING))
}

/// Resolve the `parseh-tunnel` binary path.
///
/// Lookup order:
///   1. `PATH` (release-installed binary; most common deployment).
///   2. The workspace `target/release/parseh-tunnel` next to the
///      `parseh` binary (the developer-build path).
///   3. The workspace `target/debug/parseh-tunnel` (CI / contributor
///      path; logged with a debug line because release is preferred).
fn resolve_tunnel_binary() -> Option<PathBuf> {
    // 1. PATH lookup.
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(tunnel_bin_name());
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // 2 + 3. Walk up from CARGO_MANIFEST_DIR to find `server/target/...`.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = paths::find_workspace_root(&manifest).ok()?;
    for sub in ["target/release", "target/debug"] {
        let candidate = workspace.join(sub).join(tunnel_bin_name());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn tunnel_bin_name() -> &'static str {
    "parseh-tunnel"
}

#[cfg(windows)]
fn tunnel_bin_name() -> &'static str {
    "parseh-tunnel.exe"
}

