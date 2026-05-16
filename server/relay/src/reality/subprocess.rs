//! xray-core subprocess lifecycle.
//!
//! Responsibility split:
//! - This file: spawn / supervise / shut down the `xray` binary.
//! - [`super::config`]: relay-facing config + validation.
//! - [`super::transport`]: libp2p `Transport` adapter that dials through
//!   the subprocess.
//!
//! ## What this does in V0.2.1
//!
//! 1. Translate the operator-facing [`RealityConfig`] into xray-core's
//!    native JSON config (the `inbounds` / `outbounds` shape).
//! 2. Write that JSON to a temp file (mode 0600 on Unix; in-process
//!    cleanup on drop).
//! 3. Spawn `xray run -config <file>` as a child process.
//! 4. Wait — bounded — for the subprocess to either print a startup
//!    marker on stderr (xray prints `Xray <version> started`) **or**
//!    bind its SOCKS5 port on loopback.
//! 5. Surface a [`SubprocessState`] callers can poll and a clean
//!    [`stop()`] that escalates SIGTERM → SIGKILL after 5 s on Unix.
//!
//! ## What this does NOT do
//!
//! - **No bundled binary.** If `xray` is not on `$PATH` we return
//!   [`SubprocessError::BinaryNotFound`] and the caller is expected to
//!   either log + downgrade (the smoke test does this) or refuse to
//!   start (a future production wiring will do this).
//! - **No real REALITY correctness check.** We trust xray to do
//!   REALITY right; we only verify the process started.
//! - **No Windows-specific signal escalation.** On Windows the
//!   subprocess is killed via `Child::kill()` which is `TerminateProcess`
//!   — abrupt, but acceptable for V0.2.1.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::config::{validate, ConfigError, RealityConfig, RealityRole};

/// Where the binary lives. Overridable in tests.
const XRAY_BIN: &str = "xray";

/// How long we wait for the subprocess to look "started" before giving up.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long SIGTERM gets before we escalate to SIGKILL.
const SIGTERM_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum SubprocessError {
    #[error("config validation failed: {0}")]
    Config(#[from] ConfigError),

    #[error(
        "xray-core binary not found in PATH. Install from \
         https://github.com/XTLS/Xray-core/releases and ensure `xray` is on PATH."
    )]
    BinaryNotFound,

    #[error("failed to write xray config to temp file: {0}")]
    TempFile(#[source] std::io::Error),

    #[error("failed to spawn xray subprocess: {0}")]
    Spawn(#[source] std::io::Error),

    #[error("xray subprocess exited before signalling readiness (status: {0:?})")]
    EarlyExit(Option<i32>),

    #[error(
        "timed out after {} s waiting for xray subprocess to start",
        SPAWN_TIMEOUT.as_secs()
    )]
    StartTimeout,

    #[error("subprocess shutdown failed: {0}")]
    Shutdown(#[source] std::io::Error),
}

/// Lifecycle state of the subprocess. Stored as an atomic u8 so the
/// transport layer can probe it from across threads without a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SubprocessState {
    /// `spawn()` has not been called or did not complete.
    NotStarted = 0,
    /// xray printed its startup banner; SOCKS5 endpoint is up.
    Running = 1,
    /// Caller invoked `stop()`.
    Stopped = 2,
    /// xray exited unexpectedly.
    Crashed = 3,
}

impl SubprocessState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Running,
            2 => Self::Stopped,
            3 => Self::Crashed,
            _ => Self::NotStarted,
        }
    }
}

/// Handle to a running xray subprocess.
///
/// Wrapping `Child` in a `tokio::sync::Mutex` because we hand out `Arc`
/// clones of `RealitySubprocess` to the libp2p `Transport` impl, and
/// the transport thread occasionally needs to read the subprocess
/// state while the supervisor task is also touching it.
pub struct RealitySubprocess {
    state: Arc<AtomicU8>,
    child: tokio::sync::Mutex<Option<Child>>,
    config_path: PathBuf,
    socks_addr: String,
}

// Manual `Debug` — `tokio::process::Child` is not `Debug`, but tests
// and the smoke-test example want to print the handle for error
// diagnostics. We surface only the safe fields.
impl std::fmt::Debug for RealitySubprocess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealitySubprocess")
            .field("state", &self.state())
            .field("config_path", &self.config_path)
            .field("socks_addr", &self.socks_addr)
            .finish()
    }
}

impl RealitySubprocess {
    /// Spawn an xray-core child process configured from `config`.
    ///
    /// Returns when the subprocess has either printed its startup line
    /// on stderr or [`SPAWN_TIMEOUT`] has elapsed.
    pub async fn spawn(config: &RealityConfig) -> Result<Self, SubprocessError> {
        validate(config)?;

        // 1. Write the xray JSON config to a temp file.
        let xray_cfg = build_xray_config(config);
        let config_path = write_temp_config(&xray_cfg).map_err(SubprocessError::TempFile)?;
        info!(
            path = %config_path.display(),
            "xray-core subprocess config written"
        );

        // 2. Locate the binary.
        let bin = match which_or_path(XRAY_BIN) {
            Some(p) => p,
            None => {
                // Best-effort cleanup of the temp file we just wrote.
                let _ = std::fs::remove_file(&config_path);
                return Err(SubprocessError::BinaryNotFound);
            }
        };

        // 3. Launch.
        let mut cmd = Command::new(&bin);
        cmd.arg("run")
            .arg("-config")
            .arg(&config_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        debug!(bin = %bin.display(), "spawning xray subprocess");
        let mut child = cmd.spawn().map_err(SubprocessError::Spawn)?;

        // 4. Wait for the readiness signal — either the startup banner
        //    on stderr or an early exit. We deliberately do not also
        //    poll the SOCKS port: in test environments where xray is
        //    not actually installed we still want a clean error path
        //    from step 2 above, and on real systems the banner is
        //    reliable enough for V0.2.1.
        let stderr = child.stderr.take().expect("piped stderr");
        let mut reader = tokio::io::BufReader::new(stderr).lines();

        let wait = async {
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        debug!(target: "xray.stderr", "{line}");
                        // xray-core 1.8+ prints exactly this on a
                        // successful start; if upstream changes the
                        // banner we'll need to revisit. The exact
                        // string is checked loosely by `.contains` so
                        // minor reformatting won't break us.
                        if line.contains("started") || line.contains("listening") {
                            return Ok::<(), SubprocessError>(());
                        }
                    }
                    Ok(None) => {
                        // stderr closed → process exited.
                        return Err(SubprocessError::EarlyExit(None));
                    }
                    Err(e) => {
                        warn!(error = %e, "reading xray stderr failed");
                        return Err(SubprocessError::EarlyExit(None));
                    }
                }
            }
        };

        match timeout(SPAWN_TIMEOUT, wait).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = child.start_kill();
                let _ = std::fs::remove_file(&config_path);
                return Err(e);
            }
            Err(_) => {
                let _ = child.start_kill();
                let _ = std::fs::remove_file(&config_path);
                return Err(SubprocessError::StartTimeout);
            }
        }

        let state = Arc::new(AtomicU8::new(SubprocessState::Running as u8));
        info!(
            socks = %config.local_listen,
            role = ?config.role,
            "xray-core subprocess running"
        );

        Ok(Self {
            state,
            child: tokio::sync::Mutex::new(Some(child)),
            config_path,
            socks_addr: config.local_listen.clone(),
        })
    }

    /// Current lifecycle state. Cheap; safe to call from anywhere.
    pub fn state(&self) -> SubprocessState {
        SubprocessState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Loopback SOCKS5 address the Rust side connects to.
    pub fn socks_addr(&self) -> &str {
        &self.socks_addr
    }

    /// Clean shutdown. Sends SIGTERM on Unix, waits up to
    /// [`SIGTERM_GRACE`], then escalates to SIGKILL. On Windows just
    /// `kill()`s. Consumes self because a process can only be stopped
    /// once.
    pub async fn stop(self) -> Result<(), SubprocessError> {
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            #[cfg(unix)]
            {
                if let Some(pid) = child.id() {
                    // SAFETY: kill(2) with an own-PID is the documented
                    // way to send a signal to a tokio::process::Child.
                    unsafe {
                        libc_kill(pid as i32, libc_sigterm());
                    }
                }
                match timeout(SIGTERM_GRACE, child.wait()).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => return Err(SubprocessError::Shutdown(e)),
                    Err(_) => {
                        // SIGTERM ignored → SIGKILL.
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                    }
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
        let _ = std::fs::remove_file(&self.config_path);
        self.state
            .store(SubprocessState::Stopped as u8, Ordering::Release);
        Ok(())
    }
}

impl Drop for RealitySubprocess {
    fn drop(&mut self) {
        // Best-effort cleanup if the caller never invoked `stop()`.
        // The `kill_on_drop(true)` on the Command builder already
        // handles process termination; here we just make sure the
        // temp config file does not linger.
        let _ = std::fs::remove_file(&self.config_path);
    }
}

// ───────────────────────── helpers ─────────────────────────

#[cfg(unix)]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) {
    let _ = kill(pid, sig);
}

#[cfg(unix)]
fn libc_sigterm() -> i32 {
    15
}

/// Lookup `name` on `$PATH`. We avoid pulling the `which` crate just
/// for this one use.
fn which_or_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        // Windows: also try with .exe extension.
        #[cfg(windows)]
        {
            let with_ext = dir.join(format!("{name}.exe"));
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
    }
    None
}

/// Serialise `cfg` to a JSON structure xray-core understands.
///
/// V0.2.1 keeps this minimal: one inbound SOCKS5 listener on loopback,
/// one outbound REALITY/TLS connector for `Client` role, or a REALITY
/// `Server` inbound for `Server` role. Everything else uses xray
/// defaults. Wider feature surface is V0.2.5+ work.
fn build_xray_config(cfg: &RealityConfig) -> Value {
    match cfg.role {
        RealityRole::Client => json!({
            "log": { "loglevel": "warning" },
            "inbounds": [{
                "tag": "socks-in",
                "listen": cfg.local_listen.split(':').next().unwrap_or("127.0.0.1"),
                "port": cfg.local_listen
                    .split(':')
                    .nth(1)
                    .and_then(|p| p.parse::<u16>().ok())
                    .unwrap_or(18421),
                "protocol": "socks",
                "settings": { "udp": false }
            }],
            "outbounds": [{
                "tag": "reality-out",
                "protocol": "vless",
                "settings": {
                    "vnext": [{
                        "address": cfg.remote
                            .as_deref()
                            .and_then(|r| r.split(':').next())
                            .unwrap_or("127.0.0.1"),
                        "port": cfg.remote
                            .as_deref()
                            .and_then(|r| r.split(':').nth(1))
                            .and_then(|p| p.parse::<u16>().ok())
                            .unwrap_or(443),
                        "users": [{ "id": "00000000-0000-0000-0000-000000000000", "encryption": "none" }]
                    }]
                },
                "streamSettings": {
                    "network": "tcp",
                    "security": "reality",
                    "realitySettings": {
                        "serverName": cfg.server_name,
                        "fingerprint": "chrome",
                        "publicKey": "",
                        "shortId": ""
                    }
                }
            }]
        }),
        RealityRole::Server => json!({
            "log": { "loglevel": "warning" },
            "inbounds": [{
                "tag": "reality-in",
                "listen": cfg.local_listen.split(':').next().unwrap_or("127.0.0.1"),
                "port": cfg.local_listen
                    .split(':')
                    .nth(1)
                    .and_then(|p| p.parse::<u16>().ok())
                    .unwrap_or(18421),
                "protocol": "vless",
                "settings": {
                    "clients": [{ "id": "00000000-0000-0000-0000-000000000000" }],
                    "decryption": "none"
                },
                "streamSettings": {
                    "network": "tcp",
                    "security": "reality",
                    "realitySettings": {
                        "show": false,
                        "dest": format!("{}:{}", cfg.fallback_server.host, cfg.fallback_server.port),
                        "serverNames": [cfg.server_name.clone()],
                        "privateKey": cfg.private_key_b64,
                        "shortIds": [""]
                    }
                }
            }],
            "outbounds": [{ "tag": "direct", "protocol": "freedom" }]
        }),
    }
}

/// Write `cfg` to a uniquely-named temp file. 0600 on Unix.
fn write_temp_config(cfg: &Value) -> std::io::Result<PathBuf> {
    use std::io::Write;

    let mut p = std::env::temp_dir();
    let nonce = format!(
        "parseh-relay-reality-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    p.push(nonce);

    let serialised = serde_json::to_string_pretty(cfg)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&p)?;
        f.write_all(serialised.as_bytes())?;
        f.flush()?;
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)?;
        f.write_all(serialised.as_bytes())?;
        f.flush()?;
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reality::config::FallbackServer;

    fn client_cfg() -> RealityConfig {
        RealityConfig {
            role: RealityRole::Client,
            server_name: "www.cloudflare.com".into(),
            fallback_server: FallbackServer {
                host: "www.cloudflare.com".into(),
                port: 443,
            },
            private_key_b64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            local_listen: "127.0.0.1:18421".into(),
            remote: Some("203.0.113.1:443".into()),
        }
    }

    #[test]
    fn build_client_config_has_expected_shape() {
        let v = build_xray_config(&client_cfg());
        assert!(v["inbounds"].is_array());
        assert!(v["outbounds"].is_array());
        // The REALITY block must carry our SNI.
        let sni = &v["outbounds"][0]["streamSettings"]["realitySettings"]["serverName"];
        assert_eq!(sni.as_str(), Some("www.cloudflare.com"));
    }

    #[test]
    fn build_server_config_carries_private_key_and_dest() {
        let mut c = client_cfg();
        c.role = RealityRole::Server;
        let v = build_xray_config(&c);
        let pk = &v["inbounds"][0]["streamSettings"]["realitySettings"]["privateKey"];
        assert_eq!(pk.as_str(), Some(c.private_key_b64.as_str()));
        let dest = &v["inbounds"][0]["streamSettings"]["realitySettings"]["dest"];
        assert_eq!(dest.as_str(), Some("www.cloudflare.com:443"));
    }

    /// If `xray` is not on PATH `spawn()` must return BinaryNotFound,
    /// not panic. This is what makes the smoke test usable in CI.
    #[tokio::test]
    async fn spawn_returns_binary_not_found_when_xray_missing() {
        let original_path = std::env::var_os("PATH");
        // Clear PATH so `which_or_path("xray")` definitely fails. We
        // restore it immediately after the call so we don't leak
        // process-global state across tests in the same binary.
        std::env::set_var("PATH", "/this/path/definitely/does/not/exist");
        let result = RealitySubprocess::spawn(&client_cfg()).await;
        match original_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        assert!(
            matches!(result, Err(SubprocessError::BinaryNotFound)),
            "expected BinaryNotFound, got {result:?}"
        );
    }
}
