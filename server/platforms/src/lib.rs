//! `parseh-platforms` — OS-specific service installation helpers.
//!
//! Goal: a contributor running `parseh-relay install-service` on Linux,
//! macOS, or Windows should end up with the relay registered as a
//! background service that starts on boot.
//!
//! Today this crate just exposes the per-OS module so the build chain
//! verifies compilation on every platform.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(target_os = "linux")]
pub mod systemd {
    //! Linux systemd unit installer (stub).
    /// Returns the canonical unit name we register on.
    pub const UNIT_NAME: &str = "parseh-relay.service";
}

#[cfg(target_os = "macos")]
pub mod launchd {
    //! macOS launchd plist installer (stub).
    /// Reverse-DNS label for the launchd job.
    pub const LABEL: &str = "com.parseh.relay";
}

#[cfg(target_os = "windows")]
pub mod windows_service {
    //! Windows Service Control Manager installer (stub).
    /// Service display name.
    pub const SERVICE_NAME: &str = "PARSEH Relay";
}

/// Identifies the host OS as a stable string for logs and chain capability.
pub fn os_name() -> &'static str {
    if cfg!(target_os = "linux")   { "linux" }
    else if cfg!(target_os = "macos") { "macos" }
    else if cfg!(target_os = "windows") { "windows" }
    else { "other" }
}
