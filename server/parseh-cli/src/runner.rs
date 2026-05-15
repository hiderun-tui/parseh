//! Process-runner helpers used by `parseh test` and the TTS/STT bridges.
//!
//! All command invocations stream stdout/stderr through this module so
//! we have a single place to enforce: no shell metacharacters, explicit
//! working directory, and a child-process timeout if we ever want one.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// The result of running a subprocess.
pub struct CapturedRun {
    /// Exit code (0 on success, non-zero on failure, 127 on spawn error).
    pub status_code: i32,
    /// stdout captured verbatim.
    pub stdout: String,
    /// stderr captured verbatim.
    pub stderr: String,
}

impl CapturedRun {
    /// `true` when the child exited with status 0.
    pub fn success(&self) -> bool {
        self.status_code == 0
    }

    /// Return the last `n` non-empty lines of stdout for inclusion in a
    /// markdown report. Stdout is preferred because cargo's
    /// human-readable summary lives there; stderr supplements when stdout
    /// is sparse.
    pub fn tail(&self, n: usize) -> String {
        let combined = format!("{}\n{}", self.stdout, self.stderr);
        let lines: Vec<&str> = combined
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }
}

/// Run a command in `cwd` with the given args, streaming output to the
/// terminal AND capturing it for later inclusion in a report.
///
/// `cargo` writes progress to stderr; we capture both pipes and re-emit
/// to the corresponding parent pipe before returning.
pub fn run_capture(program: &str, args: &[&str], cwd: &Path) -> Result<CapturedRun> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawn {program} {args:?}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // Echo to the calling process so the user sees output even though we
    // captured it.
    if !stdout.is_empty() {
        print!("{stdout}");
    }
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    let status_code = output.status.code().unwrap_or(127);
    Ok(CapturedRun {
        status_code,
        stdout,
        stderr,
    })
}

/// Locate `tools/parseh-tts/speak.sh` relative to a workspace root.
///
/// We do not bundle the wrapper inside the CLI binary; callers shell out
/// to whatever lives in the repo. If the wrapper is missing, the caller
/// surfaces the "install parseh-tts" hint and exits 3.
pub fn find_tts_script(workspace_root: &Path) -> Option<PathBuf> {
    let p = workspace_root.join("tools").join("parseh-tts").join("speak.sh");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// Locate `tools/parseh-stt/listen.sh` (V0.3+ — does not exist yet).
pub fn find_stt_script(workspace_root: &Path) -> Option<PathBuf> {
    let p = workspace_root.join("tools").join("parseh-stt").join("listen.sh");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// Check whether `gh` (the GitHub CLI) is on PATH.
pub fn gh_available() -> bool {
    Command::new("gh")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
