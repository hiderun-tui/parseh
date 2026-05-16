//! Environment information gathered for `parseh status` and the issue
//! report template. All values are best-effort and never panic.

use serde::Serialize;

/// Snapshot of the local environment.
#[derive(Debug, Serialize)]
pub struct EnvInfo {
    /// Operating system identifier (e.g. `linux`, `macos`, `windows`).
    pub os: String,
    /// Pointer width / architecture (e.g. `x86_64`).
    pub arch: String,
    /// Family (`unix` or `windows`).
    pub family: String,
    /// `parseh-cli` version (its own `CARGO_PKG_VERSION`).
    pub parseh_cli_version: String,
    /// PATH-resolved rustc version, if `rustc --version` works. `None`
    /// when rustc is missing — that is normal on a release-binary box.
    pub rustc_version: Option<String>,
    /// PATH-resolved cargo version (same caveat).
    pub cargo_version: Option<String>,
}

impl EnvInfo {
    /// Gather the environment snapshot. Probes the toolchain
    /// out-of-process; each probe has a small timeout so a stuck child
    /// cannot wedge the CLI.
    pub fn gather() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            family: std::env::consts::FAMILY.to_string(),
            parseh_cli_version: crate::cli::VERSION.to_string(),
            rustc_version: probe_tool("rustc", &["--version"]),
            cargo_version: probe_tool("cargo", &["--version"]),
        }
    }

    /// Render as a markdown bullet list for the issue template.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("- OS: `{}` ({})\n", self.os, self.family));
        out.push_str(&format!("- Arch: `{}`\n", self.arch));
        out.push_str(&format!("- parseh-cli: `{}`\n", self.parseh_cli_version));
        out.push_str(&format!(
            "- rustc: `{}`\n",
            self.rustc_version.as_deref().unwrap_or("(not on PATH)")
        ));
        out.push_str(&format!(
            "- cargo: `{}`\n",
            self.cargo_version.as_deref().unwrap_or("(not on PATH)")
        ));
        out
    }
}

fn probe_tool(cmd: &str, args: &[&str]) -> Option<String> {
    use std::process::{Command, Stdio};
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(s.trim().to_string())
}
