//! V0.2 wiring integration tests for `parseh-miner`.
//!
//! These tests spawn the production binary as a subprocess (via
//! `CARGO_BIN_EXE_parseh-miner`) and assert observable behaviour:
//! subscription log lines, readiness JSON shape, SharedState DB
//! creation, identity persistence, and the finalise-tick log line.
//!
//! The tests are **subprocess-based on purpose**: the miner binary is
//! the unit under test, not an in-process library. Spawning it ensures
//! the CLI surface stays usable from operator workflows (systemd,
//! launchctl, CI smoke runs) and not just from `cargo test`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Path to the built `parseh-miner` binary, supplied by Cargo at compile
/// time. The constant is generated for every `[[bin]]` target reachable
/// from the package under test.
const MINER_BIN: &str = env!("CARGO_BIN_EXE_parseh-miner");

/// Spawn the miner with a private config-dir + shared-state path so the
/// test does not touch the developer's `~/.config/parseh/`. Stdout is
/// captured for assertions; stderr is mirrored to the test runner.
fn spawn_miner(
    config_dir: &Path,
    shared_state_db: &Path,
    extra_args: &[&str],
) -> std::io::Result<Child> {
    Command::new(MINER_BIN)
        .arg("--config-dir")
        .arg(config_dir)
        .arg("--shared-state-db")
        .arg(shared_state_db)
        .args(extra_args)
        // Force info logging so subscription lines are visible.
        .env(
            "RUST_LOG",
            "parseh_miner=info,parseh_shared_state=info,libp2p=warn",
        )
        // Run on a random TCP port to avoid clashing with a real miner.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

/// Read child stdout AND stderr into one combined Vec<String> until the
/// child exits or `wait_for` elapses, whichever comes first. We drain
/// concurrently so a slow stream cannot starve the other.
async fn drain_output(child: &mut Child, wait_for: Duration) -> Vec<String> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_handle = stdout.map(|s| {
        tokio::spawn(async move {
            let mut lines = Vec::new();
            let mut reader = BufReader::new(s).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                lines.push(line);
            }
            lines
        })
    });
    let stderr_handle = stderr.map(|s| {
        tokio::spawn(async move {
            let mut lines = Vec::new();
            let mut reader = BufReader::new(s).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                lines.push(line);
            }
            lines
        })
    });

    // Wait for the child to exit or for the deadline to elapse.
    let _ = timeout(wait_for, child.wait()).await;
    // If it is still running, terminate it.
    let _ = child.start_kill();
    let _ = child.wait().await;

    let mut combined = Vec::new();
    if let Some(h) = stdout_handle {
        if let Ok(v) = h.await {
            combined.extend(v);
        }
    }
    if let Some(h) = stderr_handle {
        if let Ok(v) = h.await {
            combined.extend(v);
        }
    }
    combined
}

/// Synchronous output drain — used by tests that already know the child
/// will exit promptly (e.g. `--init-only`).
fn run_to_completion(
    config_dir: &Path,
    shared_state_db: &Path,
    extra_args: &[&str],
) -> (String, String, i32) {
    let out = std::process::Command::new(MINER_BIN)
        .arg("--config-dir")
        .arg(config_dir)
        .arg("--shared-state-db")
        .arg(shared_state_db)
        .args(extra_args)
        .env(
            "RUST_LOG",
            "parseh_miner=info,parseh_shared_state=info,libp2p=warn",
        )
        .output()
        .expect("spawn parseh-miner");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let code = out.status.code().unwrap_or(-1);
    (stdout, stderr, code)
}

fn tmp_dirs() -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let cfg = dir.path().join("config");
    let db = dir.path().join("shared-state.db");
    (dir, cfg, db)
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

/// `parseh-miner start` (briefly) and verify ALL FOUR V0.2 gossipsub
/// topics are subscribed before we tear down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn miner_starts_with_v0_2_subscriptions() {
    let (_dir, cfg, db) = tmp_dirs();
    // Use a random port so we do not collide with a real miner. The
    // listen multiaddr `/ip4/127.0.0.1/tcp/0` asks the OS to pick.
    let mut child = spawn_miner(
        &cfg,
        &db,
        &[
            "start",
            "--listen",
            "/ip4/127.0.0.1/tcp/0",
            "--no-update-check",
        ],
    )
    .expect("spawn miner");
    let lines = drain_output(&mut child, Duration::from_secs(6)).await;

    let joined = strip_ansi_escapes(&lines.join("\n"));
    assert!(
        joined.contains("subscribed: parseh.caps.v1"),
        "expected caps subscription line; got:\n{joined}"
    );
    assert!(
        joined.contains("subscribed: parseh.tasks.v1"),
        "expected tasks subscription line; got:\n{joined}"
    );
    assert!(
        joined.contains("subscribed: parseh.verify.v1"),
        "expected verify subscription line; got:\n{joined}"
    );
    assert!(
        joined.contains("subscribed: parseh.state-deltas.v1"),
        "expected state-deltas subscription line; got:\n{joined}"
    );
}

/// The `--init-only` flag must NOT start the swarm but MUST initialise
/// the SharedState DB.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_only_creates_shared_state_without_swarm() {
    let (_dir, cfg, db) = tmp_dirs();
    assert!(!db.exists(), "DB should not exist yet");
    let (stdout, stderr, code) = run_to_completion(
        &cfg,
        &db,
        &["--init-only", "start", "--listen", "/ip4/127.0.0.1/tcp/0"],
    );
    assert_eq!(code, 0, "miner --init-only exited non-zero · stdout:{stdout}\nstderr:{stderr}");
    assert!(
        db.exists(),
        "SharedState DB should exist after --init-only"
    );
    let joined = strip_ansi_escapes(&format!("{stdout}\n{stderr}"));
    assert!(
        joined.contains("--init-only"),
        "expected --init-only log line; got:\n{joined}"
    );
    // A swarm would emit "listening" or "subscribed: parseh.caps.v1".
    // --init-only must short-circuit before either.
    assert!(
        !joined.contains("subscribed: parseh.caps.v1"),
        "--init-only should NOT subscribe; got:\n{joined}"
    );
}

/// Identity persistence — the on-disk `identity.ed25519` survives
/// restarts and the same `PeerId` is reported each time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn miner_persists_identity_across_restarts() {
    let (_dir, cfg, db) = tmp_dirs();

    // First run: `whoami` after `init` so the identity is generated
    // deterministically.
    let (out1, _err1, code1) = run_to_completion(&cfg, &db, &["init"]);
    assert_eq!(code1, 0, "init failed: {out1}");

    let (whoami1, _, code) = run_to_completion(&cfg, &db, &["whoami"]);
    assert_eq!(code, 0);
    let peer_id_line_1 = whoami1
        .lines()
        .find(|l| l.starts_with("peer_id"))
        .expect("peer_id line in whoami output")
        .to_string();

    let (whoami2, _, code) = run_to_completion(&cfg, &db, &["whoami"]);
    assert_eq!(code, 0);
    let peer_id_line_2 = whoami2
        .lines()
        .find(|l| l.starts_with("peer_id"))
        .expect("peer_id line in whoami output")
        .to_string();

    assert_eq!(
        peer_id_line_1, peer_id_line_2,
        "identity must be stable across restarts"
    );
}

/// `--check-llm` must NOT touch identity / shared-state — it is a pure
/// probe that emits JSON and exits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_llm_still_works() {
    let (_dir, cfg, db) = tmp_dirs();
    let (stdout, _stderr, code) = run_to_completion(&cfg, &db, &["--check-llm"]);
    assert_eq!(code, 0, "--check-llm exited non-zero: {stdout}");
    // The probe always emits a JSON object — its shape changes across
    // OSes but the outer wrapper is stable.
    assert!(
        stdout.trim_start().starts_with('{'),
        "--check-llm should print JSON; got:\n{stdout}"
    );
    // --check-llm must not create the shared-state DB (it short-
    // circuits before that path).
    assert!(
        !db.exists(),
        "--check-llm should not create shared-state DB"
    );
}

/// `--show-readiness` must include the new V0.2 SharedState surface in
/// the JSON output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_readiness_includes_shared_state_section() {
    let (_dir, cfg, db) = tmp_dirs();
    // Spawn with --show-readiness; the binary builds the readiness
    // snapshot then exits, so a short timeout suffices.
    let mut child = spawn_miner(
        &cfg,
        &db,
        &[
            "--show-readiness",
            "start",
            "--listen",
            "/ip4/127.0.0.1/tcp/0",
            "--no-update-check",
        ],
    )
    .expect("spawn");
    let lines = drain_output(&mut child, Duration::from_secs(8)).await;
    let joined = strip_ansi_escapes(&lines.join("\n"));

    // The JSON object is emitted on stdout (not stderr). The output
    // drainer concatenates both, so we just look for the key.
    assert!(
        joined.contains("\"shared_state\""),
        "readiness JSON should include shared_state section; got:\n{joined}"
    );
    assert!(
        joined.contains("\"path\""),
        "shared_state should include path; got:\n{joined}"
    );
    assert!(
        joined.contains("\"tasks_observed\""),
        "shared_state should include tasks_observed; got:\n{joined}"
    );
    assert!(
        joined.contains("\"local_reputation\""),
        "shared_state should include local_reputation; got:\n{joined}"
    );
}

/// The miner must emit the periodic-finalise-tick log line at startup
/// so operators can confirm the tick is wired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalise_tick_log_line_is_emitted() {
    let (_dir, cfg, db) = tmp_dirs();
    let mut child = spawn_miner(
        &cfg,
        &db,
        &[
            "start",
            "--listen",
            "/ip4/127.0.0.1/tcp/0",
            "--no-update-check",
        ],
    )
    .expect("spawn");
    let lines = drain_output(&mut child, Duration::from_secs(6)).await;
    // ANSI escape codes can intersperse the structured-log key/value
    // pair, so we strip them first and assert against the bare bytes.
    let raw = lines.join("\n");
    let joined = strip_ansi_escapes(&raw);
    assert!(
        joined.contains("finalise tick scheduled"),
        "miner should log the finalise tick at startup; got:\n{joined}"
    );
    assert!(
        joined.contains("interval_ms=100"),
        "finalise tick interval should be 100ms; got:\n{joined}"
    );
}

/// Minimal ANSI-escape stripper. tracing's `fmt` layer interleaves
/// `\x1b[…m` (SGR / CSI) sequences around field names + values when
/// stderr is a TTY-ish stream. The integration tests pipe stderr, but
/// the layer still emits colour codes; this helper makes the
/// assertions reliable.
///
/// We skip the introducer (`\x1b[`) explicitly and then consume until
/// the final byte in `0x40..0x7e` — the ANSI CSI grammar's spec. A
/// naive "next char in `@..~`" loop matches `[` immediately and breaks
/// too early; that's the bug this implementation guards against.
fn strip_ansi_escapes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // Skip ESC + `[`.
            i += 2;
            // Consume until the final byte (any byte in 0x40..=0x7e).
            while i < bytes.len() {
                let b = bytes[i];
                i += 1;
                if (0x40..=0x7e).contains(&b) {
                    break;
                }
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Self-published `JobSpec`s must persist into the SharedState DB.
///
/// We exercise this by writing a `JobSpec` directly into shared-state
/// via the library API (the network publishing path lands in the next
/// agent's batch). The test asserts the persistence primitive the
/// miner relies on is reachable from the same `~/.parseh/shared-state.db`
/// the binary opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn miner_records_self_published_spec_to_shared_state() {
    let (_dir, cfg, db) = tmp_dirs();
    // First: let the miner create the schema via `--init-only`.
    let (_so, _se, code) = run_to_completion(
        &cfg,
        &db,
        &["--init-only", "start", "--listen", "/ip4/127.0.0.1/tcp/0"],
    );
    assert_eq!(code, 0);

    // Now use the same identity file the miner generated to derive
    // the same SharedState key, and write a spec.
    let identity_path = cfg.join("identity.ed25519");
    let identity_bytes = std::fs::read(&identity_path).expect("read identity");
    assert_eq!(identity_bytes.len(), 32);
    let key = parseh_shared_state::KeyMaterial::from_source(
        parseh_shared_state::KeySource::IdentityFile {
            identity_bytes: zeroize::Zeroizing::new(identity_bytes.clone()),
        },
    )
    .expect("derive key");
    let opts = parseh_shared_state::OpenOptions::create(db.clone(), key);
    let shared = parseh_shared_state::SharedState::open(opts).expect("open shared-state");

    // Build a signed spec.
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&identity_bytes);
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    let mut seed_clone = seed;
    let kp = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed_clone)
        .expect("rebuild libp2p keypair");
    let peer = libp2p::PeerId::from(kp.public());

    let (spec, hash) = parseh_task::JobSpec::new_signed_at(
        parseh_task::JobKind::Inference,
        parseh_task::JobInputs::inference_prompt("integration-test", 1),
        parseh_core::ServiceKind::Inference,
        false,
        1_700_000_000,
        peer,
        &sk,
    );
    shared.record_spec(&spec).expect("record_spec");

    let recent = shared.recent_tasks(0).expect("recent_tasks");
    assert!(
        recent.iter().any(|s| s.content_hash() == hash),
        "spec we wrote should appear in recent_tasks"
    );
}

/// Applying a `StateDelta::Outcome` via the shared-state primitive must
/// produce an observable outcome row.
///
/// `record_outcome` has FK constraints back to `tasks` + `results`, so
/// we pre-record both before applying the delta. This mirrors the
/// production flow: the miner observes the spec on `parseh.tasks.v1`
/// and the result on `parseh.verify.v1` BEFORE the outcome delta
/// arrives on `parseh.state-deltas.v1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_delta_persists_outcome() {
    let (_dir, cfg, db) = tmp_dirs();
    let (_so, _se, code) = run_to_completion(
        &cfg,
        &db,
        &["--init-only", "start", "--listen", "/ip4/127.0.0.1/tcp/0"],
    );
    assert_eq!(code, 0);

    let identity_path = cfg.join("identity.ed25519");
    let identity_bytes = std::fs::read(&identity_path).expect("read identity");
    let key = parseh_shared_state::KeyMaterial::from_source(
        parseh_shared_state::KeySource::IdentityFile {
            identity_bytes: zeroize::Zeroizing::new(identity_bytes.clone()),
        },
    )
    .expect("key");
    let opts = parseh_shared_state::OpenOptions::create(db.clone(), key);
    let shared = parseh_shared_state::SharedState::open(opts).expect("open");

    let mut seed = [0u8; 32];
    seed.copy_from_slice(&identity_bytes);
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    let mut seed_clone = seed;
    let kp = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed_clone).expect("kp");
    let peer = libp2p::PeerId::from(kp.public());

    // 1. Spec.
    let (spec, spec_hash) = parseh_task::JobSpec::new_signed_at(
        parseh_task::JobKind::Inference,
        parseh_task::JobInputs::inference_prompt("delta-test", 7),
        parseh_core::ServiceKind::Inference,
        false,
        1_700_000_000,
        peer,
        &sk,
    );
    shared.record_spec(&spec).expect("record_spec");

    // 2. Result (so the outcome's `result_hash` resolves the FK).
    let meta = parseh_task::ResultMeta {
        verifier_method: parseh_task::VerifierMethod::Deterministic,
        execution_time_ms: 1,
        model_used: None,
        inference_token_count: None,
    };
    let (result, result_hash) = parseh_task::JobResult::new_signed_at(
        spec_hash,
        peer,
        1_700_000_100,
        meta,
        b"completion".to_vec(),
        &sk,
    );
    shared.record_result(&result).expect("record_result");

    // 3. Outcome via delta envelope.
    let (outcome, _h) = parseh_task::JobOutcome::new_signed_at(
        spec_hash,
        result_hash,
        vec![],
        parseh_task::OutcomeVerdict::Indeterminate,
        1_700_000_500,
        peer,
        &sk,
    );
    let unsigned = parseh_shared_state::StateDelta::unsigned(
        parseh_shared_state::DeltaKind::Outcome(outcome.clone()),
        peer,
        1_700_000_600,
    );
    let signed = parseh_shared_state::sign_delta(unsigned, &sk).expect("sign delta");

    shared
        .apply_delta(signed, &sk.verifying_key())
        .expect("apply_delta");

    let got = shared
        .outcome_for_spec(&outcome.spec_hash)
        .expect("outcome_for_spec query");
    assert!(got.is_some(), "outcome should be persisted after apply_delta");
}

/// Best-effort smoke test that the finalise tick actually fires —
/// running for slightly more than `2 * FINALISE_TICK_MS` we expect the
/// internal interval to have ticked at least once. We assert by
/// observing the miner does NOT exit before the deadline (the tick is
/// a `tokio::select!` arm that must keep the loop healthy).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalise_tick_loop_stays_healthy() {
    let (_dir, cfg, db) = tmp_dirs();
    let mut child = spawn_miner(
        &cfg,
        &db,
        &[
            "start",
            "--listen",
            "/ip4/127.0.0.1/tcp/0",
            "--no-update-check",
        ],
    )
    .expect("spawn");

    // Let the miner run for a few ticks.
    sleep(Duration::from_millis(800)).await;
    let still_running = match child.try_wait() {
        Ok(None) => true,
        Ok(Some(_)) => false,
        Err(_) => false,
    };
    let _ = child.start_kill();
    let _ = child.wait().await;
    assert!(
        still_running,
        "miner should still be running after 800ms (finalise tick should not crash the loop)"
    );
}
