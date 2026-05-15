//! `parseh-tunnel` — client-side SOCKS5 tunnel that exits via volunteer
//! PARSEH peers.
//!
//! # What this crate is (V0.2.5 scaffold)
//!
//! A local SOCKS5 listener (default `127.0.0.1:9050`) that:
//!
//!   1. Joins the PARSEH libp2p network as a non-mining lightweight peer.
//!   2. Discovers PARSEH peers via the V0.2.5 `PeerRegistry`.
//!   3. For each accepted SOCKS5 CONNECT, picks a peer with
//!      `has_external_internet: true`, opens a libp2p stream over the
//!      `/parseh/tunnel/1.0.0` protocol, sends the target `host:port`,
//!      and after the remote peer connects to the target on the client's
//!      behalf, bidirectionally copies bytes.
//!
//! # What this crate is NOT
//!
//! Per [the project notes] and the
//! README disclaimer, this is binding:
//!
//! - **Not censorship-resistant.** Hostile-network survivability has not
//!   been measured. We do not make that claim until V0.2.5 measurement
//!   data exists.
//! - **Not anonymous.** Single-hop SOCKS5-over-libp2p reveals the
//!   destination `host:port` to the chosen exit peer. The same caveat as
//!   any single-hop proxy (and weaker than Tor, which is multi-hop). Multi-
//!   hop circuits are V0.3+ work.
//! - **Not production.** This is a SCAFFOLD: the architecture is concrete
//!   and the public API is the one the next iteration will keep, but
//!   several pieces are deliberately stubbed and listed in the README.
//!
//! # Module layout
//!
//! - [`protocol`] — wire format of `/parseh/tunnel/1.0.0`.
//! - [`socks5`] — minimal SOCKS5 (RFC 1928) server, CONNECT only.
//! - [`router`] — exit-peer selection over the `PeerRegistry`.
//! - [`tunnel`] — orchestration: SOCKS5 accept → router → libp2p stream
//!   → bidirectional copy.
//! - [`swarm`] — libp2p swarm construction (TCP + Noise + Yamux + Kad +
//!   Identify + request-response).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod protocol;
pub mod readiness;
pub mod router;
pub mod socks5;
pub mod swarm;
pub mod tunnel;

/// Crate version surfaced via `parseh_tunnel::VERSION`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default loopback SOCKS5 listen port — matches Tor's familiar `9050` so
/// that any application already configured for "SOCKS5 → 127.0.0.1:9050"
/// can repoint at `parseh-tunnel` with no further configuration. The IP
/// is hard-coded to `127.0.0.1` by policy (see [`socks5`] module docs).
pub const DEFAULT_SOCKS5_PORT: u16 = 9050;

/// libp2p stream protocol identifier for the V0.2.5 tunnel. CBOR-framed
/// `TunnelRequest` / `TunnelResponse`, then raw bidirectional bytes. A
/// breaking change to the wire format MUST bump the minor version.
pub const TUNNEL_PROTOCOL: &str = "/parseh/tunnel/1.0.0";
