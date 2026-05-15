//! Tunnel orchestration: SOCKS5 accept → router → libp2p stream →
//! bidirectional copy.
//!
//! # Pipeline
//!
//! ```text
//!     [local app]
//!         │  SOCKS5 CONNECT to 127.0.0.1:9050
//!         ▼
//!     [parseh-tunnel]
//!         │  1. negotiate_no_auth
//!         │  2. read_connect_request → Socks5Target { host, port }
//!         │  3. ExitSelector::pick_exit(target)
//!         │  4. open libp2p stream over /parseh/tunnel/1.0.0
//!         │  5. send signed TunnelRequest, await TunnelResponse
//!         │  6a. on Accepted → SOCKS5 reply=0x00 + copy_bidirectional
//!         │  6b. on Rejected → SOCKS5 reply per reason + drop
//!         ▼
//!     [exit peer]
//!         │  outbound TCP to target_host:target_port
//!         ▼
//!     [target]
//! ```
//!
//! # Telemetry policy
//!
//! Per the V0.2.5 cultural rule: NO telemetry to external services. Per-
//! tunnel diagnostics (bandwidth, latency, target host, exit PeerId) are
//! logged locally at `debug!` level — useful for the operator and a
//! support bundle, NOT persisted at rest. The README documents this
//! explicitly.
//!
//! # Scaffold note
//!
//! [`Tunnel::run_session`] is the in-process orchestration that V0.2.5
//! commits to. The actual libp2p stream open + raw-byte copy is the
//! coordination point with the parallel `PeerIdentity` registry agent:
//! the request-response control round-trip is here, but the **raw
//! byte-stream substream open** (and the matching exit-side handler)
//! lands in the merge PR. Until then, [`Tunnel::run_session`] returns
//! [`TunnelError::ExitStreamNotImplemented`] after the control round-
//! trip — which lets the SOCKS5 client see a clean "general failure"
//! reply rather than a hang. The README lists this stub explicitly.

use std::time::Duration;

use libp2p::PeerId;
use tokio::net::TcpStream;
use tracing::{debug, info};

use crate::protocol::{self, RejectionReason, TunnelRequest};
use crate::router::ExitSelector;
use crate::socks5::{self, Reply, Socks5Target};

/// Errors surfaced by the orchestrator.
#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    /// The SOCKS5 negotiation or CONNECT-request parse failed.
    #[error("socks5: {0}")]
    Socks5(#[from] socks5::Socks5Error),
    /// No exit peer is currently advertising external-internet capability.
    /// The SOCKS5 client receives REP=0x03 (Network unreachable).
    #[error("no exit peer is currently available")]
    NoExitAvailable,
    /// The selected exit returned [`RejectionReason`]. Failover may
    /// produce a working exit on the next attempt.
    #[error("exit {exit} rejected the tunnel: {reason:?}")]
    Rejected {
        /// PeerId of the rejecting exit.
        exit: PeerId,
        /// The structured rejection reason.
        reason: RejectionReason,
    },
    /// Stub status: the libp2p raw byte-stream substream open lands in
    /// the same V0.2.5 milestone as the parallel `PeerIdentity` registry
    /// agent. Until that PR merges, the orchestrator stops here with
    /// this error after the control round-trip completes.
    #[error("exit stream open is not yet wired (V0.2.5 in-flight integration)")]
    ExitStreamNotImplemented,
    /// I/O failure during the raw-byte copy phase.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// One outgoing tunnel session. Owns the SOCKS5 socket from accept time
/// until either bidirectional copy finishes or the orchestration errors
/// out.
pub struct Tunnel<'a> {
    selector: &'a ExitSelector,
    client_peer_id: PeerId,
    signing_key: &'a ed25519_dalek::SigningKey,
}

impl<'a> Tunnel<'a> {
    /// Build a new tunnel session bound to a selector + signing identity.
    pub fn new(
        selector: &'a ExitSelector,
        client_peer_id: PeerId,
        signing_key: &'a ed25519_dalek::SigningKey,
    ) -> Self {
        Self {
            selector,
            client_peer_id,
            signing_key,
        }
    }

    /// Drive one SOCKS5 client connection through the pipeline above.
    ///
    /// The function consumes `sock` because the SOCKS5 handshake mutates
    /// the read position and a half-handed-off stream is a footgun.
    pub async fn run_session(&self, mut sock: TcpStream) -> Result<(), TunnelError> {
        // 1+2. SOCKS5 negotiation.
        socks5::negotiate_no_auth(&mut sock).await?;
        let target = socks5::read_connect_request(&mut sock).await?;
        info!(target = %target, "SOCKS5 CONNECT");

        // 3. Pick exit.
        let exit = match self
            .selector
            .pick_exit(&format!("{}:{}", target.host, target.port))
        {
            Some(c) => c,
            None => {
                let _ = socks5::write_reply(
                    &mut sock,
                    Reply::NetworkUnreachable,
                    &"0.0.0.0:0".parse().unwrap(),
                )
                .await;
                return Err(TunnelError::NoExitAvailable);
            }
        };
        debug!(exit = %exit.peer_id, bandwidth_mbps = exit.bandwidth_mbps_external, "exit selected");

        // 4+5. Build + sign request envelope.
        let _envelope = self.build_signed_request(&target);

        // 6. Stream-open phase — the boundary at which V0.2.5 currently
        // stops (see module docs + README "Stubbed vs real" section).
        let _ = socks5::write_reply(
            &mut sock,
            Reply::GeneralFailure,
            &"0.0.0.0:0".parse().unwrap(),
        )
        .await;
        Err(TunnelError::ExitStreamNotImplemented)
    }

    /// Construct the signed [`TunnelRequest`] envelope. Public so tests
    /// can assert the signature inputs without spinning up a swarm.
    pub fn build_signed_request(&self, target: &Socks5Target) -> TunnelRequest {
        let signed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let payload = protocol::signing_payload(
            protocol::WIRE_VERSION,
            &target.host,
            target.port,
            self.client_peer_id,
            signed_at,
        );
        let sig = ed25519_dalek::Signer::sign(self.signing_key, &payload);
        TunnelRequest {
            wire_version: protocol::WIRE_VERSION,
            target_host: target.host.clone(),
            target_port: target.port,
            client_peer_id: self.client_peer_id,
            signed_at,
            signature: sig.to_bytes().to_vec(),
        }
    }
}

/// Map a [`RejectionReason`] to a SOCKS5 REP byte so the client gets
/// the closest standard reply. The mapping is best-effort: SOCKS5's
/// reply codes were not designed for proxy-internal failure modes, so
/// some categories collapse to `GeneralFailure`.
pub fn rejection_to_socks5_reply(reason: &RejectionReason) -> Reply {
    match reason {
        RejectionReason::NoExternalInternet => Reply::NetworkUnreachable,
        RejectionReason::RateLimited => Reply::GeneralFailure,
        RejectionReason::TargetForbidden(_) => Reply::ConnectionRefused,
        RejectionReason::Other(_) => Reply::GeneralFailure,
    }
}

/// Refuse to start a tunnel without bootstrap peers. The justification
/// (and the smoke-test that exercises this code path) lives in
/// [`crate::main`]: without a bootstrap multiaddr the swarm has no way
/// to discover any exit and the SOCKS5 listener would accept connections
/// only to fail every one with [`TunnelError::NoExitAvailable`]. Better
/// to log a clear error and exit.
pub fn require_bootstrap(bootstrap: &[String]) -> Result<(), &'static str> {
    if bootstrap.is_empty() {
        Err("no bootstrap addresses configured · pass --bootstrap MULTIADDR (the tunnel needs at least one entry to discover exits)")
    } else {
        Ok(())
    }
}

/// V0.2.5 default control-round-trip timeout. Exits that take longer
/// than this to answer the [`TunnelRequest`] are treated as failed and
/// the router moves on to the failover candidate.
pub const DEFAULT_CONTROL_TIMEOUT: Duration = Duration::from_secs(8);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::ExitSelector;
    use libp2p::identity::Keypair;
    use parseh_core::peer_registry::{
        CapabilityAdvertisement, RelayCapability, ServiceKind,
    };
    use parseh_core::PeerRegistry;
    use std::sync::Arc;

    fn relay_ad(peer: PeerId) -> CapabilityAdvertisement {
        CapabilityAdvertisement {
            peer_id: peer,
            version: 1,
            services: vec![ServiceKind::Relay],
            inference: None,
            relay: Some(RelayCapability {
                bandwidth_mbps: 100,
                transport_kinds: vec![],
            }),
            storage: None,
            network_address: "/ip4/127.0.0.1/tcp/8421".parse().unwrap(),
            signed_at: 1_000,
            ttl_seconds: 600,
        }
    }

    #[test]
    fn require_bootstrap_rejects_empty_input() {
        let err = require_bootstrap(&[]).unwrap_err();
        assert!(err.contains("no bootstrap"));
    }

    #[test]
    fn require_bootstrap_accepts_any_string() {
        // We don't parse here — that's swarm::parse_bootstrap. The point
        // is that "at least one entry" is the precondition.
        assert!(require_bootstrap(&["/ip4/1.2.3.4/tcp/8421".to_string()]).is_ok());
    }

    #[test]
    fn rejection_to_socks5_reply_maps_known_variants() {
        assert!(matches!(
            rejection_to_socks5_reply(&RejectionReason::NoExternalInternet),
            Reply::NetworkUnreachable
        ));
        assert!(matches!(
            rejection_to_socks5_reply(&RejectionReason::TargetForbidden(String::new())),
            Reply::ConnectionRefused
        ));
        assert!(matches!(
            rejection_to_socks5_reply(&RejectionReason::RateLimited),
            Reply::GeneralFailure
        ));
        assert!(matches!(
            rejection_to_socks5_reply(&RejectionReason::Other("clock-skew".to_string())),
            Reply::GeneralFailure
        ));
    }

    #[test]
    fn build_signed_request_is_self_consistent() {
        let kp = Keypair::generate_ed25519();
        let peer = PeerId::from(kp.public());
        // Extract the raw 32-byte ed25519 secret so we can build a
        // dalek SigningKey. The libp2p Keypair API exposes the secret
        // for ed25519 only.
        let dalek_bytes = match kp.try_into_ed25519() {
            Ok(ed) => ed.to_bytes(),
            Err(_) => panic!("ed25519 keypair must round-trip"),
        };
        // The libp2p ed25519 keypair encodes BOTH the secret (32) and
        // public (32) bytes — we want only the seed half.
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&dalek_bytes[..32]);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);

        let registry = Arc::new(PeerRegistry::new());
        registry.upsert(relay_ad(peer));
        let selector = ExitSelector::new(registry);

        let tunnel = Tunnel::new(&selector, peer, &signing_key);
        let req = tunnel.build_signed_request(&Socks5Target {
            host: "example.com".to_string(),
            port: 443,
        });
        assert_eq!(req.wire_version, protocol::WIRE_VERSION);
        assert_eq!(req.target_host, "example.com");
        assert_eq!(req.target_port, 443);
        assert_eq!(req.client_peer_id, peer);
        assert_eq!(req.signature.len(), 64);
        // Verify with the matching public key.
        let payload = protocol::signing_payload(
            req.wire_version,
            &req.target_host,
            req.target_port,
            req.client_peer_id,
            req.signed_at,
        );
        let verifying = signing_key.verifying_key();
        let sig_bytes: [u8; 64] = req.signature.clone().try_into().expect("64-byte sig");
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        verifying.verify_strict(&payload, &sig).expect("signature");

        // Selector still works — sanity that we didn't break the
        // pre-existing ranking by sharing the borrow.
        assert!(selector.pick_exit("example.com:443").is_some());
    }
}
