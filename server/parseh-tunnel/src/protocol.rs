//! Wire format for `/parseh/tunnel/1.0.0`.
//!
//! # Frame layout
//!
//! Each direction of the stream begins with a CBOR-encoded control
//! message prefixed by a 4-byte big-endian length:
//!
//! ```text
//!   client → exit:   [u32 BE len] [CBOR TunnelRequest]
//!   exit   → client: [u32 BE len] [CBOR TunnelResponse]
//! ```
//!
//! After a [`TunnelResponse::Accepted`] both sides switch to **raw bytes**
//! — the libp2p stream becomes a transparent bidirectional pipe between
//! the client's SOCKS5 connection and the exit's outbound TCP socket to
//! `target_host:target_port`.
//!
//! A length prefix is used (not delimiter framing) so that we never have
//! to escape bytes inside the control message and the payload-handoff is
//! byte-exact. CBOR (`ciborium`) handles the on-disk format; we just
//! length-prefix it.
//!
//! # Why request-response over a stream (not gossipsub)
//!
//! The tunnel needs an **ordered, reliable, flow-controlled byte stream**
//! between a single client and a single exit. gossipsub is a publish-
//! broadcast medium with per-message overhead, no flow control across
//! the path, and no per-connection ordering — wrong shape for tunnel
//! traffic. libp2p `request_response` (or a raw stream over Noise +
//! Yamux) gives us exactly the semantics SOCKS5 expects.
//!
//! # Signature semantics
//!
//! `TunnelRequest.signature` is the client's ed25519 signature over the
//! domain-separated tuple
//! `("parseh-tunnel-v1", wire_version, target_host, target_port,
//!   client_peer_id, signed_at)`. It is **defence-in-depth**, not the
//! primary authentication — libp2p Noise has already authenticated
//! `client_peer_id` at the transport layer. The inner signature lets an
//! exit operator log a non-repudiable proof of who requested what
//! target if a downstream complaint arrives. V0.2.5 ships verification
//! as best-effort logging (does not block the dial); strict verification
//! lands in V0.3+ once the peer-key directory is online.

use serde::{Deserialize, Serialize};

use libp2p::PeerId;

/// Currently-supported wire version. Increment on any breaking change to
/// the on-the-wire shape of [`TunnelRequest`] or [`TunnelResponse`].
pub const WIRE_VERSION: u32 = 1;

/// Domain-separation tag used inside the signed envelope (and inside the
/// length-prefix framing). Prevents an attacker from replaying a signed
/// request that happens to share bytes with a different message kind.
pub const SIGNATURE_DOMAIN: &[u8] = b"parseh-tunnel-v1";

/// Hard cap on a single CBOR control message. SOCKS5 hostnames max out
/// at 255 bytes, plus port + peer id + signature — comfortably under
/// 4 KiB. Anything bigger is a protocol error or a DOS attempt.
pub const MAX_CONTROL_MESSAGE_BYTES: usize = 4 * 1024;

/// Client → exit. Carries the connect target and the client's signed
/// proof-of-intent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunnelRequest {
    /// Wire-format version. Always equal to [`WIRE_VERSION`]; we let
    /// future versions remain decodable as `Unknown` so a smart
    /// roll-forward path is possible.
    pub wire_version: u32,
    /// Destination hostname or IP literal (RFC 1928 ATYP-compatible).
    /// V0.2.5 stores hostnames as-is — the exit performs DNS resolution.
    pub target_host: String,
    /// Destination TCP port.
    pub target_port: u16,
    /// libp2p identity of the originating client. Redundant with the
    /// authenticated Noise peer id, but included so an offline auditor
    /// can verify the signature without a live libp2p session.
    pub client_peer_id: PeerId,
    /// Unix-seconds at which the client signed the request. The exit
    /// MAY refuse requests with a clock skew larger than its tolerance
    /// (V0.2.5 default: ±300 s); rejection reason is `Other("clock-skew")`.
    pub signed_at: u64,
    /// ed25519 signature over the domain-separated message tuple. See
    /// the module docs for the exact byte layout.
    pub signature: Vec<u8>,
}

/// Exit → client. Acceptance closes the control phase and the stream
/// turns into a raw byte pipe; rejection ends the stream with the given
/// reason.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TunnelResponse {
    /// The exit dialled the target successfully. Subsequent bytes on the
    /// stream are application data.
    Accepted,
    /// The exit refused. The client SHOULD treat this as a failover
    /// trigger (see [`crate::router::ExitSelector::failover`]).
    Rejected(RejectionReason),
}

/// Reasons an exit may refuse a tunnel request. We use a closed enum for
/// the common cases and a free-form `Other` for everything else, so an
/// older client can still log the message even if it does not understand
/// a new exit-side reason.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RejectionReason {
    /// The exit lost its bridge to the open internet between accepting
    /// the libp2p stream and dialling the target. The client SHOULD
    /// failover.
    NoExternalInternet,
    /// The exit is rate-limiting this client or globally. Carries the
    /// recommended back-off in seconds.
    RateLimited,
    /// The exit refuses to dial this target. Common cases: RFC 1918
    /// addresses (it would tunnel the client into the exit's LAN), the
    /// exit's own loopback, or an operator-curated blocklist. The string
    /// is human-readable diagnostic, NOT machine-parseable.
    TargetForbidden(String),
    /// Catch-all. Stable on the wire; the string is the only place new
    /// reasons can land without a wire-version bump.
    Other(String),
}

/// Build the domain-separated byte sequence the client signs. Returned as
/// a fresh `Vec` so the caller can hand it to `SigningKey::sign`.
///
/// Layout:
/// `SIGNATURE_DOMAIN || wire_version (BE u32) || u16 host-len (BE) ||
///  target_host || target_port (BE u16) || client_peer_id-bytes ||
///  signed_at (BE u64)`.
pub fn signing_payload(
    wire_version: u32,
    target_host: &str,
    target_port: u16,
    client_peer_id: PeerId,
    signed_at: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(SIGNATURE_DOMAIN.len() + target_host.len() + 64);
    buf.extend_from_slice(SIGNATURE_DOMAIN);
    buf.extend_from_slice(&wire_version.to_be_bytes());
    let host_len: u16 = target_host.len().min(u16::MAX as usize) as u16;
    buf.extend_from_slice(&host_len.to_be_bytes());
    buf.extend_from_slice(target_host.as_bytes());
    buf.extend_from_slice(&target_port.to_be_bytes());
    buf.extend_from_slice(&client_peer_id.to_bytes());
    buf.extend_from_slice(&signed_at.to_be_bytes());
    buf
}

/// CBOR-encode a value into a freshly-allocated `Vec`. Re-exported so
/// callers do not have to depend on `ciborium` directly to round-trip.
pub fn to_cbor<T: serde::Serialize>(
    value: &T,
) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf)?;
    Ok(buf)
}

/// CBOR-decode a value from a byte slice.
pub fn from_cbor<T: for<'de> serde::Deserialize<'de>>(
    bytes: &[u8],
) -> Result<T, ciborium::de::Error<std::io::Error>> {
    ciborium::from_reader(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::identity::Keypair;

    fn fresh_peer() -> PeerId {
        PeerId::from(Keypair::generate_ed25519().public())
    }

    #[test]
    fn tunnel_request_cbor_roundtrip() {
        let peer = fresh_peer();
        let req = TunnelRequest {
            wire_version: WIRE_VERSION,
            target_host: "whatsapp.com".to_string(),
            target_port: 443,
            client_peer_id: peer,
            signed_at: 1_715_700_000,
            signature: vec![0u8; 64],
        };
        let bytes = to_cbor(&req).expect("encode");
        let decoded: TunnelRequest = from_cbor(&bytes).expect("decode");
        assert_eq!(req, decoded);
    }

    #[test]
    fn tunnel_response_cbor_roundtrip_accepted() {
        let resp = TunnelResponse::Accepted;
        let bytes = to_cbor(&resp).expect("encode");
        let decoded: TunnelResponse = from_cbor(&bytes).expect("decode");
        assert_eq!(resp, decoded);
    }

    #[test]
    fn tunnel_response_cbor_roundtrip_rejected_variants() {
        for variant in [
            TunnelResponse::Rejected(RejectionReason::NoExternalInternet),
            TunnelResponse::Rejected(RejectionReason::RateLimited),
            TunnelResponse::Rejected(RejectionReason::TargetForbidden(
                "10.0.0.0/8 forbidden".to_string(),
            )),
            TunnelResponse::Rejected(RejectionReason::Other("clock-skew".to_string())),
        ] {
            let bytes = to_cbor(&variant).expect("encode");
            let decoded: TunnelResponse = from_cbor(&bytes).expect("decode");
            assert_eq!(variant, decoded);
        }
    }

    #[test]
    fn signing_payload_is_deterministic_and_domain_separated() {
        let peer = fresh_peer();
        let p1 = signing_payload(1, "example.com", 443, peer, 1_000);
        let p2 = signing_payload(1, "example.com", 443, peer, 1_000);
        assert_eq!(p1, p2);
        // Must start with the domain tag (prevents cross-protocol replay).
        assert!(p1.starts_with(SIGNATURE_DOMAIN));
        // A different port produces a different payload.
        let p3 = signing_payload(1, "example.com", 80, peer, 1_000);
        assert_ne!(p1, p3);
    }
}
