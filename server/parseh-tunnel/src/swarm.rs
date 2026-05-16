//! libp2p swarm for `parseh-tunnel`.
//!
//! # What the swarm participates in
//!
//! The tunnel client is a **lightweight non-mining peer**. It needs:
//!
//! - **TCP + Noise + Yamux** — baseline secure transport. Identical to
//!   the miner's choice so we are compatible on the wire.
//! - **Identify** — so peers learn each other's listen addresses and the
//!   exit can recognise the client as a real PARSEH peer (not a random
//!   internet probe).
//! - **Kademlia DHT** — for peer discovery beyond the bootstrap set.
//!   The miner already runs a Kad behaviour with the same protocol name
//!   (`/parseh/kad/1.0.0`), so the two interoperate.
//! - **Gossipsub on `parseh.caps.v1`** — so we learn about exits and
//!   their bandwidth. The tunnel does NOT subscribe to `parseh.tasks.v1`
//!   / `parseh.verify.v1` / `parseh.state-deltas.v1` — those are miner
//!   responsibilities and add bandwidth/CPU we do not want on a client.
//! - **Request-response on `/parseh/tunnel/1.0.0`** — outbound only; the
//!   client opens a stream per accepted SOCKS5 connection.
//!
//! # What it does NOT do
//!
//! - It does not advertise capabilities (it is not a relay / inference /
//!   storage provider). The miner's `publish_caps_v0_2` path is
//!   deliberately absent from this binary.
//! - It does not participate in V0.2 verification. No `parseh-task` /
//!   `parseh-verify` dependency.
//! - It does not own a long-lived shared-state DB. No `parseh-shared-state`
//!   dependency.
//!
//! # Scaffold status
//!
//! V0.2.5 ships the swarm construction + bootstrap dial + caps-topic
//! subscription. The end-to-end **stream open + bidirectional copy**
//! path is sketched in [`crate::tunnel`] but the request-response handler
//! that ultimately drives the stream is wiring that lands together with
//! the parallel `PeerIdentity::has_external_internet` registry agent —
//! the two are designed to merge in the same V0.2.5 milestone.

use std::time::Duration;

use anyhow::{Context, Result};
use libp2p::{
    gossipsub, identify, identity, kad, request_response,
    swarm::NetworkBehaviour,
    Multiaddr, PeerId, StreamProtocol, SwarmBuilder,
};
use serde::{Deserialize, Serialize};

/// V0.2 protocol-version string identifying this client to peers. We
/// piggy-back on the miner's identifier so any peer recognising the V0.2
/// protocol set is mutually compatible.
pub const TUNNEL_IDENTIFY_VERSION: &str = "/parseh/0.2.0/tunnel";

/// Kademlia protocol name. MUST match the miner's
/// (`/parseh/kad/1.0.0`) so the two share a DHT.
pub const TUNNEL_KAD_PROTOCOL: &str = "/parseh/kad/1.0.0";

/// Gossipsub topic for capability advertisements (read-only here). MUST
/// match the miner's [`parseh_core::peer_registry`] consumer.
pub const TOPIC_CAPS: &str = "parseh.caps.v1";

// ───── request-response wire types ─────────────────────────────────────

/// Outer wire type for the `/parseh/tunnel/1.0.0` request — a thin CBOR
/// wrapper so libp2p's `request_response::cbor::Behaviour` can hand us
/// length-delimited frames without an extra framer.
///
/// `parseh-tunnel` only uses the request-response Behaviour for the
/// initial control round-trip; once the exit answers `Accepted`, the
/// raw byte-stream transfer happens on a separate yamux substream
/// opened by the tunnel orchestrator (see [`crate::tunnel`]). Carrying
/// the bytes inside the request-response payload would buffer the whole
/// connection — defeats the point of a tunnel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelControlRequest {
    /// CBOR-encoded [`crate::protocol::TunnelRequest`].
    pub envelope: Vec<u8>,
}

/// Outer wire type for the response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelControlResponse {
    /// CBOR-encoded [`crate::protocol::TunnelResponse`].
    pub envelope: Vec<u8>,
}

// ───── behaviour ───────────────────────────────────────────────────────

/// Composite libp2p behaviour for the tunnel client.
#[derive(NetworkBehaviour)]
#[behaviour(to_swarm = "TunnelBehaviourEvent")]
pub struct TunnelBehaviour {
    /// Peer-identification handshake.
    pub identify: identify::Behaviour,
    /// DHT routing.
    pub kad: kad::Behaviour<kad::store::MemoryStore>,
    /// Capability advertisement subscriber.
    pub gossipsub: gossipsub::Behaviour,
    /// `/parseh/tunnel/1.0.0` control round-trip channel.
    pub tunnel:
        request_response::cbor::Behaviour<TunnelControlRequest, TunnelControlResponse>,
}

/// Coarse event enum for the swarm dispatch loop.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum TunnelBehaviourEvent {
    /// Identify event.
    Identify(identify::Event),
    /// Kad event.
    Kad(kad::Event),
    /// Gossipsub event.
    Gossipsub(gossipsub::Event),
    /// Tunnel control-protocol event.
    Tunnel(request_response::Event<TunnelControlRequest, TunnelControlResponse>),
}

impl From<identify::Event> for TunnelBehaviourEvent {
    fn from(e: identify::Event) -> Self {
        Self::Identify(e)
    }
}
impl From<kad::Event> for TunnelBehaviourEvent {
    fn from(e: kad::Event) -> Self {
        Self::Kad(e)
    }
}
impl From<gossipsub::Event> for TunnelBehaviourEvent {
    fn from(e: gossipsub::Event) -> Self {
        Self::Gossipsub(e)
    }
}
impl From<request_response::Event<TunnelControlRequest, TunnelControlResponse>>
    for TunnelBehaviourEvent
{
    fn from(
        e: request_response::Event<TunnelControlRequest, TunnelControlResponse>,
    ) -> Self {
        Self::Tunnel(e)
    }
}

// ───── swarm construction ──────────────────────────────────────────────

/// Build the libp2p swarm with the V0.2.5 behaviour set.
pub fn build_swarm(kp: identity::Keypair) -> Result<libp2p::Swarm<TunnelBehaviour>> {
    use libp2p::{noise, tcp, yamux};
    let swarm = SwarmBuilder::with_existing_identity(kp.clone())
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
            let local_peer = PeerId::from(key.public());

            let store = kad::store::MemoryStore::new(local_peer);
            let mut kad_cfg = kad::Config::default();
            kad_cfg.set_protocol_names(vec![StreamProtocol::new(TUNNEL_KAD_PROTOCOL)]);
            let kad = kad::Behaviour::with_config(local_peer, store, kad_cfg);

            let gossipsub_cfg = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10))
                .validation_mode(gossipsub::ValidationMode::Strict)
                .build()
                .map_err(std::io::Error::other)?;
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_cfg,
            )
            .map_err(std::io::Error::other)?;

            let tunnel_cfg = request_response::Config::default();
            let tunnel =
                request_response::cbor::Behaviour::<TunnelControlRequest, TunnelControlResponse>::new(
                    [(
                        StreamProtocol::new(crate::TUNNEL_PROTOCOL),
                        request_response::ProtocolSupport::Outbound,
                    )],
                    tunnel_cfg,
                );

            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(TunnelBehaviour {
                identify: identify::Behaviour::new(identify::Config::new(
                    TUNNEL_IDENTIFY_VERSION.into(),
                    key.public(),
                )),
                kad,
                gossipsub,
                tunnel,
            })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
        .build();
    Ok(swarm)
}

/// Subscribe to `parseh.caps.v1` so the registry can learn about exits.
pub fn subscribe_caps_topic(
    swarm: &mut libp2p::Swarm<TunnelBehaviour>,
) -> Result<gossipsub::IdentTopic> {
    let topic = gossipsub::IdentTopic::new(TOPIC_CAPS);
    swarm
        .behaviour_mut()
        .gossipsub
        .subscribe(&topic)
        .context("subscribe to parseh.caps.v1")?;
    Ok(topic)
}

/// Dial a bootstrap multiaddr, returning a parsed [`Multiaddr`] error if
/// the input was malformed.
///
/// We surface this as a free function (not a swarm method) because the
/// startup code wants to parse + log + dial each entry independently,
/// and the parse step is what we test in isolation.
pub fn parse_bootstrap(raw: &str) -> Result<Multiaddr, libp2p::multiaddr::Error> {
    raw.parse::<Multiaddr>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bootstrap_accepts_valid_multiaddr() {
        let m = parse_bootstrap("/ip4/127.0.0.1/tcp/8421").expect("parse");
        assert_eq!(m.to_string(), "/ip4/127.0.0.1/tcp/8421");
    }

    #[test]
    fn parse_bootstrap_rejects_garbage() {
        let err = parse_bootstrap("not a multiaddr").unwrap_err();
        // The exact wording of `multiaddr::Error` is not load-bearing,
        // but it must be a hard error rather than a silent default.
        assert!(!err.to_string().is_empty());
    }
}
