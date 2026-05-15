//! libp2p transport adapter that routes outbound TCP through the
//! local xray-core SOCKS5 endpoint.
//!
//! ## What this is in V0.2.1
//!
//! A **scaffold adapter** with the API surface the eventual libp2p
//! `Transport` impl will expose:
//!
//! - [`RealityTransport`] holds an `Arc<RealitySubprocess>` and an
//!   inner `libp2p::tcp::tokio::Transport`.
//! - [`RealityTransport::dial_via_socks5`] performs an RFC 1928 SOCKS5
//!   handshake against the subprocess's loopback endpoint and returns
//!   the resulting `TcpStream`. This is the byte-level primitive a
//!   future `libp2p::Transport::dial` will build on.
//! - [`RealityListener`] is a stub for the inbound side; real listener
//!   integration is V0.2.5 work because it requires the libp2p
//!   `Boxed<Transport>` plumbing AND a working REALITY server config
//!   to test against.
//!
//! ## Why not implement `libp2p::Transport` directly today
//!
//! libp2p 0.53's `Transport` trait is generic over output types and
//! upgrade chains; a faithful impl is ~500–800 LOC of plumbing whose
//! correctness can only be verified against a real REALITY peer, which
//! we do not have. Shipping a half-working `Transport` impl would let
//! the relay's `SwarmBuilder` compile but fail mysteriously at runtime
//! — the opposite of the protocol-humility rule. Instead we ship the
//! byte-level primitive + tests now and add the trait impl when
//! V0.2.5 has an end-to-end test rig.
//!
//! See the `## Open questions` section of
//! the project notes for the design questions
//! that drive the trait-impl shape.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;

use super::subprocess::{RealitySubprocess, SubprocessState};

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("subprocess is not running (state: {0:?})")]
    SubprocessNotRunning(SubprocessState),

    #[error("connect to SOCKS5 endpoint failed: {0}")]
    SocksConnect(#[source] std::io::Error),

    #[error("SOCKS5 handshake failed: {0}")]
    SocksHandshake(String),

    #[error("invalid target address '{0}'")]
    InvalidTarget(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// libp2p transport adapter. Today this is a thin wrapper around the
/// SOCKS5 endpoint exposed by [`RealitySubprocess`]; V0.2.5 will grow
/// the full `libp2p::Transport` trait impl on top.
pub struct RealityTransport {
    subprocess: Arc<RealitySubprocess>,
}

impl RealityTransport {
    pub fn new(subprocess: Arc<RealitySubprocess>) -> Self {
        Self { subprocess }
    }

    /// Open a TCP connection to `target` (a `host:port` string) tunneled
    /// through the local SOCKS5 endpoint that xray-core exposes.
    ///
    /// This is the byte-level primitive a future libp2p `Transport`
    /// impl will sit on top of. Returns a plain `TcpStream` because
    /// from libp2p's perspective the post-handshake bytes look like a
    /// normal TCP stream — Noise + Yamux will run inside it as usual.
    pub async fn dial_via_socks5(&self, target: &str) -> Result<TcpStream, TransportError> {
        if self.subprocess.state() != SubprocessState::Running {
            return Err(TransportError::SubprocessNotRunning(self.subprocess.state()));
        }
        let (host, port) = parse_host_port(target)
            .ok_or_else(|| TransportError::InvalidTarget(target.to_string()))?;

        let mut stream = TcpStream::connect(self.subprocess.socks_addr())
            .await
            .map_err(TransportError::SocksConnect)?;

        socks5_handshake(&mut stream, &host, port).await?;
        debug!(target = %target, "SOCKS5 tunnel established");
        Ok(stream)
    }

    /// Probe whether the underlying subprocess is healthy.
    pub fn subprocess_state(&self) -> SubprocessState {
        self.subprocess.state()
    }
}

/// Stub for the inbound side. Returning concrete listener wiring is
/// V0.2.5 work — see `reality-integration-plan.md`.
pub struct RealityListener {
    _subprocess: Arc<RealitySubprocess>,
}

impl RealityListener {
    pub fn new(subprocess: Arc<RealitySubprocess>) -> Self {
        Self {
            _subprocess: subprocess,
        }
    }
}

// ───────────────────────── SOCKS5 (RFC 1928) ─────────────────────────

/// Minimal SOCKS5 CONNECT, no auth. xray-core's SOCKS5 inbound
/// supports `noauth` by default for loopback listeners. Implementing
/// only this subset keeps the dependency surface zero — we don't pull
/// `tokio-socks` for one round-trip.
async fn socks5_handshake(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
) -> Result<(), TransportError> {
    // Greeting: VER=5, NMETHODS=1, METHODS=[0x00 NO_AUTH]
    stream.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut greet_reply = [0u8; 2];
    stream.read_exact(&mut greet_reply).await?;
    if greet_reply[0] != 0x05 {
        return Err(TransportError::SocksHandshake(format!(
            "bad protocol version in greeting reply: {:#x}",
            greet_reply[0]
        )));
    }
    if greet_reply[1] != 0x00 {
        return Err(TransportError::SocksHandshake(format!(
            "server rejected NO_AUTH (got method {:#x})",
            greet_reply[1]
        )));
    }

    // Request: VER=5, CMD=1 (CONNECT), RSV=0, ATYP, DST.ADDR, DST.PORT
    let mut req = vec![0x05, 0x01, 0x00];
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        req.push(0x01); // ATYP IPv4
        req.extend_from_slice(&v4.octets());
    } else if let Ok(v6) = host.parse::<Ipv6Addr>() {
        req.push(0x04); // ATYP IPv6
        req.extend_from_slice(&v6.octets());
    } else {
        let hostname = host.as_bytes();
        if hostname.len() > 255 {
            return Err(TransportError::InvalidTarget(host.to_string()));
        }
        req.push(0x03); // ATYP DOMAINNAME
        req.push(hostname.len() as u8);
        req.extend_from_slice(hostname);
    }
    req.push((port >> 8) as u8);
    req.push((port & 0xff) as u8);
    stream.write_all(&req).await?;

    // Reply: VER, REP, RSV, ATYP, BND.ADDR, BND.PORT
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr).await?;
    if hdr[0] != 0x05 {
        return Err(TransportError::SocksHandshake(format!(
            "bad protocol version in connect reply: {:#x}",
            hdr[0]
        )));
    }
    if hdr[1] != 0x00 {
        return Err(TransportError::SocksHandshake(format!(
            "CONNECT failed (REP={:#x})",
            hdr[1]
        )));
    }
    // Drain the bound-address field — its length depends on ATYP.
    match hdr[3] {
        0x01 => {
            let mut buf = [0u8; 4 + 2];
            stream.read_exact(&mut buf).await?;
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut buf = vec![0u8; len[0] as usize + 2];
            stream.read_exact(&mut buf).await?;
        }
        0x04 => {
            let mut buf = [0u8; 16 + 2];
            stream.read_exact(&mut buf).await?;
        }
        other => {
            return Err(TransportError::SocksHandshake(format!(
                "unknown ATYP in connect reply: {other:#x}"
            )))
        }
    }
    Ok(())
}

fn parse_host_port(s: &str) -> Option<(String, u16)> {
    // Be a little forgiving — bracketed IPv6 `[::1]:443` as well as
    // plain `host:port`.
    if let Some(rest) = s.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        let port = port.parse().ok()?;
        return Some((host.to_string(), port));
    }
    let (host, port) = s.rsplit_once(':')?;
    let port = port.parse().ok()?;
    Some((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_port_plain() {
        assert_eq!(
            parse_host_port("127.0.0.1:8080"),
            Some(("127.0.0.1".to_string(), 8080))
        );
    }

    #[test]
    fn parse_host_port_domain() {
        assert_eq!(
            parse_host_port("example.com:443"),
            Some(("example.com".to_string(), 443))
        );
    }

    #[test]
    fn parse_host_port_ipv6_bracketed() {
        assert_eq!(
            parse_host_port("[::1]:443"),
            Some(("::1".to_string(), 443))
        );
    }

    #[test]
    fn parse_host_port_rejects_garbage() {
        assert_eq!(parse_host_port("not-a-host-port"), None);
        assert_eq!(parse_host_port(""), None);
    }
}
