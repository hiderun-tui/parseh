//! Minimal SOCKS5 (RFC 1928) server.
//!
//! # Why a hand-rolled implementation
//!
//! The miner uses `fast-socks5` to expose a loopback listener that
//! forwards each accepted CONNECT to the *real* destination via a direct
//! TCP dial — perfect for the listener-half smoke test it ships today.
//! `parseh-tunnel` has a different shape: after we negotiate the SOCKS5
//! method + parse the CONNECT request, the outbound half is **not** a
//! TCP socket — it's a libp2p stream over `/parseh/tunnel/1.0.0`. We
//! need to own the moment between "SOCKS5 reply sent" and "bytes start
//! flowing" so we can hand the inbound `TcpStream` off to the tunnel
//! orchestrator with the negotiated target already in hand.
//!
//! Implementing the few packets we actually use (method negotiation +
//! CONNECT request + reply) is ~100 lines and removes a transitive
//! dependency on `fast-socks5`'s lifecycle. We pay this cost once.
//!
//! # What's implemented (V0.2.5)
//!
//! - CONNECT (CMD = 0x01).
//! - No-auth (METHOD = 0x00).
//! - ATYP `IPv4`, `IPv6`, and `DOMAINNAME`. The latter is what browsers
//!   send when configured with "Proxy DNS" (which is the recommended
//!   client setting — see the README); the exit performs DNS resolution.
//!
//! # What's deliberately NOT implemented
//!
//! - **BIND** (CMD = 0x02): only useful for FTP-PORT-mode and unsupported
//!   by Tor; we reply with `Command not supported` (REP = 0x07).
//! - **UDP ASSOCIATE** (CMD = 0x03): needs a parallel UDP relay path;
//!   V0.3+ feature. We reply with `Command not supported`.
//! - **Authentication beyond no-auth**: the listener is loopback by
//!   policy, so a password layer would be security theatre at this
//!   trust boundary. When/if we ever bind a non-loopback interface, the
//!   same PR introduces auth.
//!
//! # Loopback-only policy
//!
//! [`Socks5Listener::loopback`] is the only supported constructor. The
//! defensive check inside [`run_socks5`] refuses any non-loopback address
//! — same rationale as `server/miner/src/proxy.rs`: a non-loopback SOCKS5
//! listener inside a hostile network is the kind of open-proxy adversaries
//! sweep for, and refusing to bind it removes a whole class of accidents.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ───── public types ────────────────────────────────────────────────────

/// A bound-but-not-yet-running SOCKS5 listener handle.
#[derive(Debug, Clone)]
pub struct Socks5Listener {
    addr: SocketAddr,
}

impl Socks5Listener {
    /// Build a listener handle bound to `127.0.0.1:<port>`.
    ///
    /// The IP is hard-coded by policy. See module docs.
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

/// Negotiated SOCKS5 target — what the client wants us to connect to.
///
/// `parseh-tunnel` does NOT dial this itself; it hands the parsed value
/// (plus the underlying `TcpStream` already positioned past the SOCKS5
/// reply) to the tunnel orchestrator, which routes it through a PARSEH
/// peer's libp2p stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Socks5Target {
    /// Hostname or IP literal as the client requested it.
    pub host: String,
    /// TCP port.
    pub port: u16,
}

impl std::fmt::Display for Socks5Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

// ───── error type ──────────────────────────────────────────────────────

/// Errors returned by the SOCKS5 negotiation path.
#[derive(Debug, thiserror::Error)]
pub enum Socks5Error {
    /// I/O failure during handshake or reply.
    #[error("socks5 i/o: {0}")]
    Io(#[from] io::Error),
    /// The client sent a SOCKS version byte we do not speak. SOCKS5
    /// requires the first byte to be `0x05`.
    #[error("socks5 protocol error: {0}")]
    Protocol(String),
    /// The client sent a CMD other than CONNECT. We reply with REP=0x07
    /// (Command not supported) before returning this error.
    #[error("socks5 unsupported command: {0}")]
    UnsupportedCommand(u8),
}

// ───── wire constants ──────────────────────────────────────────────────

const SOCKS5_VERSION: u8 = 0x05;
const METHOD_NO_AUTH: u8 = 0x00;
const METHOD_NO_ACCEPTABLE: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAINNAME: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

/// REP byte values that this implementation can produce. The enum is
/// only used inside this module — we expose it as a number on the wire.
#[derive(Debug, Copy, Clone)]
pub enum Reply {
    /// Succeeded — REP = 0x00.
    Succeeded = 0x00,
    /// General SOCKS server failure — REP = 0x01.
    GeneralFailure = 0x01,
    /// Network unreachable — REP = 0x03.
    NetworkUnreachable = 0x03,
    /// Host unreachable — REP = 0x04.
    HostUnreachable = 0x04,
    /// Connection refused — REP = 0x05.
    ConnectionRefused = 0x05,
    /// Command not supported — REP = 0x07. Returned for BIND / UDP-ASSOCIATE.
    CommandNotSupported = 0x07,
}

// ───── handshake ───────────────────────────────────────────────────────

/// Run the method-negotiation half of the handshake. After this call,
/// the next bytes on the wire are the CONNECT request.
///
/// Wire shape:
/// ```text
///   client → server: VER(0x05) NMETHODS [METHOD; NMETHODS]
///   server → client: VER(0x05) METHOD(0x00 if NO_AUTH offered else 0xFF)
/// ```
pub async fn negotiate_no_auth(sock: &mut TcpStream) -> Result<(), Socks5Error> {
    let mut head = [0u8; 2];
    sock.read_exact(&mut head).await?;
    if head[0] != SOCKS5_VERSION {
        return Err(Socks5Error::Protocol(format!(
            "client offered SOCKS version 0x{:02x}; only SOCKS5 (0x05) is supported",
            head[0]
        )));
    }
    let nmethods = head[1] as usize;
    let mut methods = vec![0u8; nmethods];
    sock.read_exact(&mut methods).await?;
    let chosen = if methods.contains(&METHOD_NO_AUTH) {
        METHOD_NO_AUTH
    } else {
        METHOD_NO_ACCEPTABLE
    };
    sock.write_all(&[SOCKS5_VERSION, chosen]).await?;
    if chosen == METHOD_NO_ACCEPTABLE {
        return Err(Socks5Error::Protocol(
            "client offered no acceptable method (NO-AUTH required)".to_string(),
        ));
    }
    Ok(())
}

/// Read the CONNECT request, parse the target, and return it. The
/// SOCKS5 reply is NOT sent here — the caller decides what to reply
/// based on whether it can actually establish the tunnel.
///
/// Wire shape:
/// ```text
///   client → server: VER(0x05) CMD ATYP DST.ADDR DST.PORT
///   DST.ADDR is:
///     ATYP=0x01 IPv4 (4 bytes)
///     ATYP=0x03 DOMAINNAME (1-byte length + bytes)
///     ATYP=0x04 IPv6 (16 bytes)
/// ```
///
/// On unsupported CMD this function sends the appropriate REP=0x07
/// reply before returning [`Socks5Error::UnsupportedCommand`], so the
/// caller can simply `?` and drop the socket.
pub async fn read_connect_request(sock: &mut TcpStream) -> Result<Socks5Target, Socks5Error> {
    let mut head = [0u8; 4];
    sock.read_exact(&mut head).await?;
    if head[0] != SOCKS5_VERSION {
        return Err(Socks5Error::Protocol(format!(
            "CONNECT request used SOCKS version 0x{:02x}; only 0x05 is supported",
            head[0]
        )));
    }
    if head[1] != CMD_CONNECT {
        // RFC 1928 §6: a server replying REP=0x07 still includes a
        // valid BND.ADDR/BND.PORT pair. We use the loopback / port-0
        // pair which is the conventional "no address" filler.
        write_reply(sock, Reply::CommandNotSupported, &dummy_bnd()).await?;
        return Err(Socks5Error::UnsupportedCommand(head[1]));
    }
    // head[2] is RSV — must be 0x00. We ignore non-conforming clients
    // rather than refusing; the worst case is a slightly noisy header
    // byte from a buggy SOCKS5 client.
    let atyp = head[3];
    let host = match atyp {
        ATYP_IPV4 => {
            let mut buf = [0u8; 4];
            sock.read_exact(&mut buf).await?;
            Ipv4Addr::from(buf).to_string()
        }
        ATYP_IPV6 => {
            let mut buf = [0u8; 16];
            sock.read_exact(&mut buf).await?;
            Ipv6Addr::from(buf).to_string()
        }
        ATYP_DOMAINNAME => {
            let mut len_buf = [0u8; 1];
            sock.read_exact(&mut len_buf).await?;
            let len = len_buf[0] as usize;
            let mut name = vec![0u8; len];
            sock.read_exact(&mut name).await?;
            match String::from_utf8(name) {
                Ok(s) => s,
                Err(_) => {
                    write_reply(sock, Reply::GeneralFailure, &dummy_bnd()).await?;
                    return Err(Socks5Error::Protocol(
                        "DOMAINNAME ATYP carried non-UTF-8 bytes".to_string(),
                    ));
                }
            }
        }
        other => {
            write_reply(sock, Reply::GeneralFailure, &dummy_bnd()).await?;
            return Err(Socks5Error::Protocol(format!(
                "unsupported ATYP 0x{other:02x}"
            )));
        }
    };
    let mut port_buf = [0u8; 2];
    sock.read_exact(&mut port_buf).await?;
    let port = u16::from_be_bytes(port_buf);
    Ok(Socks5Target { host, port })
}

/// Send a CONNECT reply with the given REP byte and BND address.
///
/// Wire shape:
/// ```text
///   server → client: VER(0x05) REP RSV(0x00) ATYP BND.ADDR BND.PORT
/// ```
pub async fn write_reply(
    sock: &mut TcpStream,
    reply: Reply,
    bnd: &SocketAddr,
) -> Result<(), Socks5Error> {
    let mut buf = Vec::with_capacity(22);
    buf.push(SOCKS5_VERSION);
    buf.push(reply as u8);
    buf.push(0x00); // RSV
    match bnd.ip() {
        IpAddr::V4(v4) => {
            buf.push(ATYP_IPV4);
            buf.extend_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            buf.push(ATYP_IPV6);
            buf.extend_from_slice(&v6.octets());
        }
    }
    buf.extend_from_slice(&bnd.port().to_be_bytes());
    sock.write_all(&buf).await?;
    Ok(())
}

/// Conventional "no address" placeholder for REP-only replies. The
/// SOCKS5 client typically ignores BND on a non-success reply.
fn dummy_bnd() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
}

// ───── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spawn a one-shot loopback pair and return (server-side, client-side)
    /// TcpStream handles.
    async fn pipe() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client_fut = TcpStream::connect(addr);
        let accept_fut = listener.accept();
        let (client_res, accept_res) = tokio::join!(client_fut, accept_fut);
        (accept_res.unwrap().0, client_res.unwrap())
    }

    #[test]
    fn loopback_constructor_is_127_0_0_1() {
        let l = Socks5Listener::loopback(9050);
        assert!(l.addr().ip().is_loopback());
        assert_eq!(l.addr().port(), 9050);
    }

    #[tokio::test]
    async fn method_negotiation_chooses_no_auth() {
        let (mut server, mut client) = pipe().await;
        let client_task = tokio::spawn(async move {
            // SOCKS5, 1 method, NO_AUTH.
            client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
            let mut reply = [0u8; 2];
            client.read_exact(&mut reply).await.unwrap();
            reply
        });
        negotiate_no_auth(&mut server).await.unwrap();
        let reply = client_task.await.unwrap();
        assert_eq!(reply, [0x05, 0x00]);
    }

    #[tokio::test]
    async fn method_negotiation_rejects_when_no_auth_not_offered() {
        let (mut server, mut client) = pipe().await;
        let client_task = tokio::spawn(async move {
            // SOCKS5, 1 method, GSSAPI (0x01). No NO_AUTH offered.
            client.write_all(&[0x05, 0x01, 0x01]).await.unwrap();
            let mut reply = [0u8; 2];
            client.read_exact(&mut reply).await.unwrap();
            reply
        });
        let err = negotiate_no_auth(&mut server).await.unwrap_err();
        let reply = client_task.await.unwrap();
        assert_eq!(reply, [0x05, 0xFF]);
        assert!(matches!(err, Socks5Error::Protocol(_)));
    }

    #[tokio::test]
    async fn connect_request_parses_domainname() {
        let (mut server, mut client) = pipe().await;
        let client_task = tokio::spawn(async move {
            // VER, CMD=CONNECT, RSV, ATYP=DOMAIN, len=12, "whatsapp.com", port=443.
            let mut req = vec![0x05, 0x01, 0x00, 0x03, 0x0c];
            req.extend_from_slice(b"whatsapp.com");
            req.extend_from_slice(&443u16.to_be_bytes());
            client.write_all(&req).await.unwrap();
        });
        let target = read_connect_request(&mut server).await.unwrap();
        client_task.await.unwrap();
        assert_eq!(target.host, "whatsapp.com");
        assert_eq!(target.port, 443);
    }

    #[tokio::test]
    async fn connect_request_parses_ipv4() {
        let (mut server, mut client) = pipe().await;
        let client_task = tokio::spawn(async move {
            // VER, CMD=CONNECT, RSV, ATYP=IPv4, 1.2.3.4, port=80.
            let mut req = vec![0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4];
            req.extend_from_slice(&80u16.to_be_bytes());
            client.write_all(&req).await.unwrap();
        });
        let target = read_connect_request(&mut server).await.unwrap();
        client_task.await.unwrap();
        assert_eq!(target.host, "1.2.3.4");
        assert_eq!(target.port, 80);
    }

    #[tokio::test]
    async fn connect_request_rejects_bind_command() {
        let (mut server, mut client) = pipe().await;
        let client_task = tokio::spawn(async move {
            // VER, CMD=BIND (0x02), RSV, ATYP=IPv4, ...
            let mut req = vec![0x05, 0x02, 0x00, 0x01, 0, 0, 0, 0];
            req.extend_from_slice(&0u16.to_be_bytes());
            client.write_all(&req).await.unwrap();
            // We should receive a REP=0x07 reply (10 bytes for IPv4).
            let mut reply = [0u8; 10];
            client.read_exact(&mut reply).await.unwrap();
            reply
        });
        let err = read_connect_request(&mut server).await.unwrap_err();
        let reply = client_task.await.unwrap();
        assert!(matches!(err, Socks5Error::UnsupportedCommand(0x02)));
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], 0x07); // CommandNotSupported
    }
}
