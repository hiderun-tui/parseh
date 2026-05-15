//! Integration tests for the V0.2.5 `parseh-tunnel` scaffold.
//!
//! Coverage targets:
//!
//! 1. SOCKS5 RFC 1928 method-negotiation + CONNECT request roundtrip
//!    (no real libp2p; loopback-only TCP pair).
//! 2. SOCKS5 reply byte layout for the success path.
//! 3. Router picks the highest-bandwidth eligible exit.
//! 4. Router falls over when the primary returns a rejection reason.
//! 5. CBOR roundtrip for every [`TunnelResponse`] variant + [`TunnelRequest`].
//! 6. Domain-separated signing payload is stable across calls and rejects
//!    cross-field collisions.
//! 7. Tunnel refuses to start without bootstrap multiaddrs.
//!
//! These tests deliberately stay below the libp2p swarm boundary — they
//! exercise the SOCKS5 wire format, the router selection, the wire
//! envelope, and the bootstrap precondition without spinning up two real
//! libp2p nodes. End-to-end stream-open coverage lands with the parallel
//! `PeerIdentity::has_external_internet` registry merge; until then,
//! testing at this layer is what the scaffold's contract actually
//! promises.

use std::sync::Arc;

use libp2p::{identity::Keypair, Multiaddr, PeerId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use parseh_core::peer_registry::{
    CapabilityAdvertisement, RelayCapability, ServiceKind,
};
use parseh_core::PeerRegistry;
use parseh_tunnel::protocol::{
    from_cbor, signing_payload, to_cbor, RejectionReason, TunnelRequest, TunnelResponse,
    SIGNATURE_DOMAIN, WIRE_VERSION,
};
use parseh_tunnel::router::ExitSelector;
use parseh_tunnel::socks5::{self, Reply, Socks5Target};
use parseh_tunnel::tunnel::{rejection_to_socks5_reply, require_bootstrap};

// ─────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────

fn fresh_peer() -> PeerId {
    PeerId::from(Keypair::generate_ed25519().public())
}

fn relay_ad(peer: PeerId, bandwidth_mbps: u32) -> CapabilityAdvertisement {
    CapabilityAdvertisement {
        peer_id: peer,
        version: 1,
        services: vec![ServiceKind::Relay],
        inference: None,
        relay: Some(RelayCapability {
            bandwidth_mbps,
            transport_kinds: vec![],
        }),
        storage: None,
        network_address: loopback(),
        signed_at: 1_000,
        ttl_seconds: 600,
    }
}

fn loopback() -> Multiaddr {
    "/ip4/127.0.0.1/tcp/8421".parse().unwrap()
}

/// A connected loopback pair: returns `(server_side, client_side)`.
async fn pipe() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client_fut = TcpStream::connect(addr);
    let accept_fut = listener.accept();
    let (client, accepted) = tokio::join!(client_fut, accept_fut);
    (accepted.unwrap().0, client.unwrap())
}

// ─────────────────────────────────────────────────────────────────────────
// 1 · SOCKS5 method negotiation + CONNECT request roundtrip
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn socks5_full_roundtrip_for_domainname_target() {
    let (mut server, mut client) = pipe().await;

    let client_task = tokio::spawn(async move {
        // Method negotiation: SOCKS5, 1 method, NO_AUTH.
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);
        // CONNECT request: ATYP=DOMAIN, "whatsapp.com:443".
        let mut req = vec![0x05, 0x01, 0x00, 0x03, 0x0c];
        req.extend_from_slice(b"whatsapp.com");
        req.extend_from_slice(&443u16.to_be_bytes());
        client.write_all(&req).await.unwrap();
    });

    socks5::negotiate_no_auth(&mut server).await.unwrap();
    let target = socks5::read_connect_request(&mut server).await.unwrap();
    assert_eq!(target, Socks5Target { host: "whatsapp.com".to_string(), port: 443 });

    client_task.await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────
// 2 · SOCKS5 success reply has the canonical shape (`05 00 00 01 ... port`)
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn socks5_success_reply_has_canonical_shape() {
    let (mut server, mut client) = pipe().await;
    let bnd: std::net::SocketAddr = "127.0.0.1:9050".parse().unwrap();
    let server_task = tokio::spawn(async move {
        socks5::write_reply(&mut server, Reply::Succeeded, &bnd).await.unwrap();
    });
    let mut buf = [0u8; 10]; // VER REP RSV ATYP + 4 IPv4 + 2 port
    client.read_exact(&mut buf).await.unwrap();
    server_task.await.unwrap();
    assert_eq!(buf[0], 0x05, "VER");
    assert_eq!(buf[1], 0x00, "REP=Succeeded");
    assert_eq!(buf[2], 0x00, "RSV");
    assert_eq!(buf[3], 0x01, "ATYP=IPv4");
    assert_eq!(&buf[4..8], &[127, 0, 0, 1]);
    assert_eq!(u16::from_be_bytes([buf[8], buf[9]]), 9050);
}

// ─────────────────────────────────────────────────────────────────────────
// 3 · Router picks the highest-bandwidth eligible exit
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn router_picks_highest_bandwidth_eligible_exit() {
    let registry = Arc::new(PeerRegistry::new());
    let slow = fresh_peer();
    let medium = fresh_peer();
    let fast = fresh_peer();
    registry.upsert(relay_ad(slow, 10));
    registry.upsert(relay_ad(medium, 100));
    registry.upsert(relay_ad(fast, 1_000));

    let selector = ExitSelector::new(registry);
    let chosen = selector.pick_exit("whatsapp.com:443").expect("a peer");
    assert_eq!(chosen.peer_id, fast);
    assert_eq!(chosen.bandwidth_mbps_external, 1_000);
}

// ─────────────────────────────────────────────────────────────────────────
// 4 · Router fails over when the primary returns a rejection reason
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn router_failover_skips_failed_peer() {
    let registry = Arc::new(PeerRegistry::new());
    let slow = fresh_peer();
    let fast = fresh_peer();
    registry.upsert(relay_ad(slow, 10));
    registry.upsert(relay_ad(fast, 1_000));

    let selector = ExitSelector::new(registry);
    // Primary picked: fast. Now pretend fast rejected — failover.
    let backup = selector
        .failover(&fast, "whatsapp.com:443")
        .expect("a backup exit");
    assert_eq!(backup.peer_id, slow);

    // The simulated rejection reason maps to a sensible SOCKS5 reply.
    let reply = rejection_to_socks5_reply(&RejectionReason::NoExternalInternet);
    assert!(matches!(reply, Reply::NetworkUnreachable));
}

// ─────────────────────────────────────────────────────────────────────────
// 5 · CBOR roundtrip for TunnelRequest + every TunnelResponse variant
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn cbor_roundtrip_for_tunnel_request_and_every_response_variant() {
    let peer = fresh_peer();
    let req = TunnelRequest {
        wire_version: WIRE_VERSION,
        target_host: "instagram.com".to_string(),
        target_port: 443,
        client_peer_id: peer,
        signed_at: 1_715_700_000,
        signature: vec![7u8; 64],
    };
    let req_bytes = to_cbor(&req).expect("encode TunnelRequest");
    let req_decoded: TunnelRequest = from_cbor(&req_bytes).expect("decode TunnelRequest");
    assert_eq!(req, req_decoded);

    for variant in [
        TunnelResponse::Accepted,
        TunnelResponse::Rejected(RejectionReason::NoExternalInternet),
        TunnelResponse::Rejected(RejectionReason::RateLimited),
        TunnelResponse::Rejected(RejectionReason::TargetForbidden("LAN forbidden".to_string())),
        TunnelResponse::Rejected(RejectionReason::Other("clock-skew".to_string())),
    ] {
        let bytes = to_cbor(&variant).expect("encode TunnelResponse");
        let decoded: TunnelResponse = from_cbor(&bytes).expect("decode TunnelResponse");
        assert_eq!(variant, decoded);
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 6 · Signing payload is deterministic + domain-separated
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn signing_payload_is_domain_separated_and_deterministic() {
    let peer = fresh_peer();
    let p1 = signing_payload(WIRE_VERSION, "example.com", 443, peer, 1_000);
    let p2 = signing_payload(WIRE_VERSION, "example.com", 443, peer, 1_000);
    assert_eq!(p1, p2, "deterministic");
    assert!(p1.starts_with(SIGNATURE_DOMAIN), "domain-separated");

    // Distinct ports → distinct payloads.
    let p_diff_port = signing_payload(WIRE_VERSION, "example.com", 80, peer, 1_000);
    assert_ne!(p1, p_diff_port);

    // Distinct hosts of the same length → distinct payloads (would
    // collide under a naive byte-concat without length prefixing).
    let p_diff_host = signing_payload(WIRE_VERSION, "example.org", 443, peer, 1_000);
    assert_ne!(p1, p_diff_host);
}

// ─────────────────────────────────────────────────────────────────────────
// 7 · Tunnel refuses to start without bootstrap multiaddrs
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn tunnel_refuses_to_start_without_bootstrap_multiaddrs() {
    let err = require_bootstrap(&[]).expect_err("must refuse");
    assert!(
        err.contains("no bootstrap"),
        "error must mention the missing bootstrap addresses: {err}"
    );
    // Exactly one entry is sufficient.
    assert!(require_bootstrap(&["/ip4/1.2.3.4/tcp/8421".to_string()]).is_ok());
}

// ─────────────────────────────────────────────────────────────────────────
// 8 · Router returns None when no relay-advertising peer is known
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn router_returns_none_when_no_relay_peer_is_known() {
    let registry = Arc::new(PeerRegistry::new());
    // Empty registry first.
    let selector = ExitSelector::new(registry.clone());
    assert!(selector.pick_exit("whatever:443").is_none());

    // Register an inference-only peer — still no relay → still None.
    let peer = fresh_peer();
    registry.upsert(CapabilityAdvertisement {
        peer_id: peer,
        version: 1,
        services: vec![ServiceKind::Inference],
        inference: None,
        relay: None,
        storage: None,
        network_address: loopback(),
        signed_at: 1_000,
        ttl_seconds: 600,
    });
    assert!(selector.pick_exit("whatever:443").is_none());
}
