//! REALITY stealth-transport integration scaffold (V0.2.1).
//!
//! REALITY is V2Ray / XTLS's TLS-mimicry transport: the wire-level bytes
//! look indistinguishable from a real TLS 1.3 handshake to a real
//! third-party website (the "fallback server", e.g. `cloudflare.com`).
//! Only post-handshake encryption is replaced with the tunnel's stream
//! cipher. See the project notes for the rationale and
//! the project notes for what this scaffold
//! actually delivers vs what V0.2.5 will need.
//!
//! ## Approach (Path A)
//!
//! There is no production-grade pure-Rust REALITY library as of 2026.
//! V0.2.1 ships **Path A**: spawn a forked `xray-core` Go binary as a
//! subprocess that handles the REALITY layer. The Rust side speaks
//! libp2p as today; the subprocess presents REALITY-over-TLS on the
//! wire and forwards plain bytes back over a loopback SOCKS5 endpoint.
//!
//! ## What is stubbed vs real
//!
//! **Real:**
//! - Crate structure + API surface (`RealityConfig`, `RealitySubprocess`,
//!   `RealityTransport`)
//! - Subprocess lifecycle (spawn, state probe, clean stop with
//!   SIGTERM-then-SIGKILL escalation on Unix)
//! - Config serialisation (TOML for relay input, JSON for xray-core's
//!   native config format)
//! - Graceful "xray-core not installed" error path so the example /
//!   smoke test runs everywhere CI runs
//!
//! **Stubbed (V0.2.5 work):**
//! - The actual REALITY handshake correctness depends on a working
//!   `xray` binary the maintainer must install separately (`xray-core`
//!   from <https://github.com/XTLS/Xray-core>). We do NOT ship a
//!   pre-bundled binary — license + provenance + binary-size all argue
//!   against it.
//! - Hostile-network measurement (DPI / active-probing survivability).
//! - libp2p `Transport` wiring is a thin adapter today; verifying that
//!   yamux + Noise multiplex cleanly across the SOCKS5 hop is open.
//!
//! No "censorship-resistant" claim should be made in user-facing copy
//! about this code until V0.2.5 produces measurement data. This is
//! binding per the project notes.

mod config;
mod subprocess;
mod transport;

pub use config::{validate, ConfigError, FallbackServer, RealityConfig, RealityRole};
pub use subprocess::{RealitySubprocess, SubprocessError, SubprocessState};
pub use transport::{RealityListener, RealityTransport, TransportError};
