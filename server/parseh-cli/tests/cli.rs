//! Integration tests for the `parseh` binary.
//!
//! All tests run the `parseh` binary as a subprocess via `assert_cmd`.
//! Each test pins `--db` and `--identity` to a fresh tempdir so they
//! never touch the developer's real `~/.parseh` / `~/.config/parseh`.
//!
//! No network is touched: the `submit` command builds + signs + encodes
//! offline (per the V0.2 implementation note); `status` reads the
//! optional SharedState DB directly.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Build a fresh `(tempdir, identity_path, db_path)` triple. The
/// identity file is NOT pre-created — commands that need an identity
/// generate one on first run.
fn fresh_env() -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let id = dir.path().join("identity.ed25519");
    let db = dir.path().join("shared-state.db");
    (dir, id, db)
}

fn parseh() -> Command {
    Command::cargo_bin("parseh").expect("parseh binary built")
}

#[test]
fn help_exits_zero_and_lists_subcommands() {
    parseh()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("submit"))
        .stdout(predicate::str::contains("status"))
        .stdout(predicate::str::contains("test"));
}

#[test]
fn version_prints_semver_string() {
    parseh()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("0.1.0-dev"));
}

#[test]
fn no_subcommand_prints_overview() {
    parseh()
        .assert()
        .success()
        .stdout(predicate::str::contains("PARSEH developer CLI"))
        .stdout(predicate::str::contains("parseh status"));
}

#[test]
fn whoami_generates_identity_if_missing() {
    let (_dir, id, db) = fresh_env();
    assert!(!id.exists(), "identity should not exist before whoami");
    parseh()
        .arg("--identity")
        .arg(&id)
        .arg("--db")
        .arg(&db)
        .arg("whoami")
        .assert()
        .success()
        .stdout(predicate::str::contains("peer_id"))
        .stdout(predicate::str::contains("identity_created_now"));
    assert!(id.exists(), "identity should exist after whoami");
    // Mode 0600 on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&id).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "identity must be 0600");
    }
}

#[test]
fn whoami_is_stable_across_invocations() {
    let (_dir, id, db) = fresh_env();
    let out_a = parseh()
        .arg("--identity")
        .arg(&id)
        .arg("--db")
        .arg(&db)
        .arg("whoami")
        .output()
        .unwrap();
    assert!(out_a.status.success());
    let a: serde_json::Value =
        serde_json::from_slice(&out_a.stdout).expect("whoami output JSON");
    let peer_a = a["peer_id"].as_str().unwrap().to_string();

    let out_b = parseh()
        .arg("--identity")
        .arg(&id)
        .arg("--db")
        .arg(&db)
        .arg("whoami")
        .output()
        .unwrap();
    assert!(out_b.status.success());
    let b: serde_json::Value =
        serde_json::from_slice(&out_b.stdout).expect("whoami output JSON");
    let peer_b = b["peer_id"].as_str().unwrap().to_string();

    assert_eq!(peer_a, peer_b, "PeerId must be stable across runs");
}

#[test]
fn status_text_runs_on_fresh_state() {
    let (_dir, id, db) = fresh_env();
    parseh()
        .arg("--identity")
        .arg(&id)
        .arg("--db")
        .arg(&db)
        .arg("status")
        .arg("--text")
        .assert()
        .success()
        .stdout(predicate::str::contains("peer_id"))
        .stdout(predicate::str::contains("shared-state"));
}

#[test]
fn status_json_on_fresh_state_returns_zero_counts() {
    let (_dir, id, db) = fresh_env();
    let out = parseh()
        .arg("--identity")
        .arg(&id)
        .arg("--db")
        .arg(&db)
        .arg("status")
        .output()
        .unwrap();
    assert!(out.status.success(), "status should succeed on fresh box");
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("status JSON");
    assert_eq!(v["shared_state"]["tasks_count"], 0);
    assert_eq!(v["shared_state"]["outcomes_count"], 0);
    assert_eq!(v["shared_state"]["db_exists"], false);
    assert_eq!(v["miner_running"], false);
}

#[test]
fn submit_signs_and_returns_task_id() {
    let (_dir, id, db) = fresh_env();
    let out = parseh()
        .arg("--identity")
        .arg(&id)
        .arg("--db")
        .arg(&db)
        .arg("submit")
        .arg("hello-parseh-cli-test")
        .output()
        .unwrap();
    assert!(out.status.success(), "submit failed: {out:?}");
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("submit JSON");
    let task_id = v["task_id"].as_str().expect("task_id");
    // task_id is sha256 hex -> 64 chars
    assert_eq!(task_id.len(), 64, "task_id should be 64-hex-char sha256");
    assert!(v["bytes"].as_u64().unwrap() > 0);
    assert_eq!(v["sensitive"], false);
}

#[test]
fn submit_from_file() {
    let (dir, id, db) = fresh_env();
    let prompt_path = dir.path().join("prompt.txt");
    fs::write(&prompt_path, "from-a-file-prompt").unwrap();
    parseh()
        .arg("--identity")
        .arg(&id)
        .arg("--db")
        .arg(&db)
        .arg("submit")
        .arg("--file")
        .arg(&prompt_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("task_id"));
}

#[test]
fn submit_rejects_empty_invocation() {
    let (_dir, id, db) = fresh_env();
    parseh()
        .arg("--identity")
        .arg(&id)
        .arg("--db")
        .arg(&db)
        .arg("submit")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no prompt given"));
}

#[test]
fn peers_on_empty_db_returns_empty_list() {
    let (_dir, id, db) = fresh_env();
    let out = parseh()
        .arg("--identity")
        .arg(&id)
        .arg("--db")
        .arg(&db)
        .arg("peers")
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&s).expect("JSON");
    assert!(v.is_array(), "peers must emit a JSON array");
    assert_eq!(v.as_array().unwrap().len(), 0);
}

#[test]
fn tts_missing_wrapper_exits_3() {
    // We don't have an STT wrapper today (per spec — tools/parseh-stt
    // lands later). Make sure the graceful fallback fires.
    parseh()
        .arg("stt")
        .arg("--seconds")
        .arg("1")
        .assert()
        .code(3);
}

#[test]
fn report_issue_dry_run_prints_body() {
    let (_dir, id, db) = fresh_env();
    parseh()
        .arg("--identity")
        .arg(&id)
        .arg("--db")
        .arg(&db)
        .arg("report-issue")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains("Steps to reproduce"))
        .stdout(predicate::str::contains("Environment"));
}

#[test]
fn detect_text_does_not_panic() {
    // We don't assert success because the underlying detect_all() may
    // legitimately fail in a sandboxed CI box without network/runtime;
    // the only requirement is the process completes and emits SOMETHING
    // before exiting.
    let out = parseh().arg("detect").arg("--text").output().unwrap();
    // Either success or graceful error — never SIGABRT / hang.
    let _ = out.status.code().expect("must exit");
}

#[test]
fn submit_with_seed_changes_task_id() {
    let (_dir, id, db) = fresh_env();
    let mk = |seed: &str| -> String {
        let out = parseh()
            .arg("--identity")
            .arg(&id)
            .arg("--db")
            .arg(&db)
            .arg("submit")
            .arg("--seed")
            .arg(seed)
            .arg("identical-prompt")
            .output()
            .unwrap();
        assert!(out.status.success(), "submit failed");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["task_id"].as_str().unwrap().to_string()
    };
    // Different seeds must produce different task IDs even with the
    // same prompt and identity — this confirms the seed is part of the
    // signed body, which V0.2 verifiers depend on for deterministic
    // replay.
    let a = mk("11");
    let b = mk("22");
    assert_ne!(a, b);
}
