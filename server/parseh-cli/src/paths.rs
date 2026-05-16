//! Filesystem paths resolved with miner-compatible defaults.
//!
//! The miner stores:
//!   - identity at `$HOME/.config/parseh/identity.ed25519` (Linux/macOS
//!     via `dirs::config_dir()`; `%APPDATA%\parseh\identity.ed25519` on
//!     Windows).
//!   - shared-state DB at `$HOME/.parseh/shared-state.db`.
//!
//! The CLI honours `--db` / `--identity` flags and the `PARSEH_DB` /
//! `PARSEH_IDENTITY` environment variables, falling back to these
//! defaults so a user who just installed `parseh-miner` does not have to
//! configure the CLI separately.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Default SharedState DB path: `$HOME/.parseh/shared-state.db`.
///
/// `dirs::home_dir()` returns `Some(_)` on every supported platform
/// (Linux, macOS, Windows). We surface a clear error otherwise so the
/// caller can pass `--db`.
pub fn default_db_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine $HOME · pass --db PATH")?;
    Ok(home.join(".parseh").join("shared-state.db"))
}

/// Default identity file path: `dirs::config_dir().join("parseh").join("identity.ed25519")`.
///
/// Matches `server/miner/src/config.rs::default_config_dir()` so the CLI
/// and miner share a single identity by default.
pub fn default_identity_path() -> Result<PathBuf> {
    let cfg =
        dirs::config_dir().context("could not determine config dir · pass --identity PATH")?;
    Ok(cfg.join("parseh").join("identity.ed25519"))
}

/// Resolve the DB path: CLI override wins, else default.
pub fn resolve_db_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    match override_path {
        Some(p) => Ok(p),
        None => default_db_path(),
    }
}

/// Resolve the identity path: CLI override wins, else default.
pub fn resolve_identity_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    match override_path {
        Some(p) => Ok(p),
        None => default_identity_path(),
    }
}

/// Walk upwards from `start` looking for a directory containing
/// `Cargo.toml` with a `[workspace]` section. Returns the workspace root.
///
/// Used by `parseh test` to invoke `cargo test --workspace` from the
/// correct directory regardless of where the user is.
pub fn find_workspace_root(start: &std::path::Path) -> Result<PathBuf> {
    let mut cur: PathBuf = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    loop {
        let candidate = cur.join("Cargo.toml");
        if candidate.exists() {
            if let Ok(s) = std::fs::read_to_string(&candidate) {
                if s.contains("[workspace]") {
                    return Ok(cur);
                }
            }
        }
        if !cur.pop() {
            anyhow::bail!(
                "could not find a workspace `Cargo.toml` walking upwards from {}",
                start.display()
            );
        }
    }
}
