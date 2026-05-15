//! Local-only SOCKS5 listener for the Hiderun browser.
//!
//! ## What this is (V0)
//!
//! This module runs a SOCKS5 server on `127.0.0.1:<port>` so the Hiderun
//! browser (or `curl --socks5-hostname`) can tunnel TCP through the miner
//! process. At this stage the miner forwards each accepted SOCKS5
//! request **directly** to the requested destination via a normal
//! `TcpStream::connect` (the default `fast-socks5` CONNECT handler).
//! No PARSEH routing yet — we just need the listener half to be
//! end-to-end testable (see seed issue #6 in `.github/SEED_ISSUES.md`).
//!
//! ## What this becomes (next PR)
//!
//! The follow-up PR replaces the direct connect with a libp2p
//! `request_response` stream against a chosen relay peer:
//!
//! ```text
//!     [browser] ──SOCKS5──► [miner.proxy] ──libp2p──► [relay] ──TCP──► [internet]
//! ```
//!
//! TODO(parseh-routing): replace `Socks5Socket::upgrade_to_socks5()`'s
//! default behaviour (direct TCP dial to target) with a libp2p
//! `request_response` round-trip to a relay peer. Tracked as the
//! follow-up to seed issue #6 — the libp2p wiring is intentionally
//! out of scope here so the listener half can land and be tested
//! independently.
//!
//! ## Why loopback only
//!
//! PARSEH is a humanitarian tool deployed inside hostile networks.
//! Binding `0.0.0.0` would turn every miner into an open SOCKS5 proxy
//! reachable from the LAN (or the public internet if the host has no
//! firewall) — exactly the kind of "open proxy" adversaries sweep for
//! and abuse. The CLI flag therefore takes only a port; the IP is
//! hard-coded to `127.0.0.1`. A future version may add an explicit
//! `--socks5-bind` once we have authentication and a security model that
//! justifies it. Until then: loopback only.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use fast_socks5::server::{AcceptAuthentication, Config, Socks5Server};
use tokio_stream::StreamExt;
use tracing::{debug, info, warn};

/// A configured but not-yet-running SOCKS5 listener handle.
///
/// Construct via [`Socks5Listener::loopback`] — the only supported
/// constructor by design (see module docs for the rationale).
pub struct Socks5Listener {
    addr: SocketAddr,
}

impl Socks5Listener {
    /// Build a listener handle bound to `127.0.0.1:<port>`.
    ///
    /// The IP is fixed — see the module docs for why we refuse to bind
    /// any non-loopback address in V0.
    pub fn loopback(port: u16) -> Self {
        Self {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
        }
    }

    /// The bound address (loopback, by construction).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// Run a SOCKS5 server on the given loopback address forever.
///
/// Each accepted client connection is handed off to a `tokio::spawn`ed
/// task that runs the SOCKS5 negotiation via `fast-socks5` and then
/// pipes bytes to the requested destination using `fast-socks5`'s
/// built-in TCP CONNECT handler. UDP ASSOCIATE and BIND are
/// disabled — only CONNECT is supported in V0.
///
/// Authentication is `no-auth` (the `AcceptAuthentication` strategy
/// from `fast-socks5`): V0 binds loopback only, so a password layer
/// would be security theatre. When/if we ever expose this on a
/// non-loopback interface, authentication becomes mandatory and that
/// change is gated on the same PR.
pub async fn run_socks5(addr: SocketAddr) -> Result<()> {
    // Defence-in-depth: even though `Socks5Listener::loopback` is the
    // only constructor, double-check at the entry point. A future
    // refactor that adds a second constructor must not accidentally
    // start binding wildcards.
    if !addr.ip().is_loopback() {
        anyhow::bail!(
            "refusing to bind SOCKS5 on non-loopback address {addr}: \
             V0 is loopback-only by policy"
        );
    }

    // Default config = no-auth, TCP CONNECT only, UDP disabled.
    let mut config: Config<AcceptAuthentication> = Config::default();
    config.set_udp_support(false);

    // `fast-socks5` accepts anything implementing its own
    // `AsyncToSocketAddrs` trait. Passing the address as a string
    // form is the path the upstream example uses, and avoids any
    // chance of a trait-impl mismatch across patch versions.
    let bind_str = addr.to_string();
    let listener: Socks5Server<AcceptAuthentication> = Socks5Server::<AcceptAuthentication>::bind(&bind_str)
        .await
        .with_context(|| format!("bind SOCKS5 listener on {addr}"))?
        .with_config(config);

    info!(%addr, "SOCKS5 listener ready (loopback only, no-auth, TCP CONNECT only)");

    let mut incoming = listener.incoming();
    while let Some(socket_res) = incoming.next().await {
        match socket_res {
            Ok(socket) => {
                // `Socks5Socket` doesn't expose `peer_addr()`, so we
                // log the listener's addr instead of the client's
                // ephemeral source port. Once the libp2p side lands
                // we'll log the chosen relay PeerId here too.
                info!(listener = %addr, "SOCKS5 connection accepted");

                // upgrade_to_socks5() performs the entire SOCKS5
                // handshake, DNS resolution, target TCP dial, and
                // full-duplex byte copy internally. When the
                // follow-up PR lands, this is the call site that
                // changes shape — the upgraded socket's "outbound"
                // half will be plumbed onto a libp2p stream instead
                // of letting fast-socks5 dial the target directly.
                tokio::spawn(async move {
                    match socket.upgrade_to_socks5().await {
                        Ok(_done) => {
                            debug!("SOCKS5 session closed cleanly");
                        }
                        Err(e) => {
                            debug!(error = %e, "SOCKS5 session ended with error");
                        }
                    }
                });
            }
            Err(e) => warn!(error = %e, "SOCKS5 accept failed"),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_constructor_uses_127_0_0_1() {
        let l = Socks5Listener::loopback(1080);
        assert!(l.addr().ip().is_loopback());
        assert_eq!(l.addr().port(), 1080);
    }

    #[tokio::test]
    async fn refuses_to_bind_non_loopback() {
        let bogus = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let err = run_socks5(bogus).await.expect_err("must refuse 0.0.0.0");
        assert!(err.to_string().contains("loopback-only"));
    }
}
