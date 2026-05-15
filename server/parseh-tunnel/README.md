# parseh-tunnel — client-side SOCKS5 tunnel via volunteer PARSEH peers

**Status: V0.2.5 SCAFFOLD.** This crate compiles, ships a usable
`parseh-tunnel` binary, and exposes the architecture-complete API
surface that V0.3+ keeps. Several pieces are deliberately stubbed and
listed in [§ "Stubbed vs real"](#stubbed-vs-real) below.

---

## DISCLAIMER · please read before installing

**We do NOT claim censorship resistance.** Hostile-network survivability
of this tunnel has not been measured. The cultural rule documented in
the project notes
is binding: no "censorship-resistant" language appears in user-facing
copy until V0.2.5 hostile-network measurement data exists. The
measurement methodology is documented in
the project notes
and runs as part of V0.2.5.

**We do NOT claim anonymity.** This is a **single-hop** tunnel. The
target `host:port` is plaintext to the exit operator. The exit knows
what site you asked them to reach. This is the same caveat as any
single-hop proxy (and weaker than Tor, which is multi-hop). Multi-hop
circuits are V0.3+ work — see [§ "What's deferred to V0.3+"](#whats-deferred-to-v03)
below.

**No telemetry, no analytics.** Per-tunnel diagnostics (target host,
exit PeerId, latency, bandwidth) are logged locally at `debug!` level
only, and nothing is persisted at rest. The binary makes **zero**
outbound connections to any analytics service.

---

## What this does

A client on a restricted network runs `parseh-tunnel` on their machine.
It:

1. Starts a SOCKS5 listener on `127.0.0.1:9050` (Tor-style default, so
   any application already configured for `socks5://127.0.0.1:9050`
   "just works" when repointed at PARSEH).
2. Joins the PARSEH libp2p network as a non-mining lightweight peer.
3. Discovers peers via the V0.2.5 `PeerRegistry`.
4. When a local app sends a SOCKS5 CONNECT (e.g., browser dials
   `whatsapp.com:443`), the tunnel:
   - Finds a PARSEH peer advertising `Relay` with external-internet
     capability,
   - Opens a libp2p stream to that peer over `/parseh/tunnel/1.0.0`,
   - Sends the target `host:port` (signed by the client's ed25519
     identity for non-repudiable audit),
   - The remote peer connects to `whatsapp.com:443` on the user's
     behalf,
   - Bidirectional copy.

Existing analogs you might know:

- **Tor** — more rigorous (multi-hop, traffic obfuscation), but blocked
  in a restricted jurisdiction. We are building an alternative, not a replacement.
- **Snowflake** — closest model. Volunteer browser-tab proxies. PARSEH's
  "any peer with external internet can volunteer" is the same shape.

---

## Architecture

```
   [local app: browser, curl, etc.]
            │
            │  SOCKS5 CONNECT
            ▼
   ┌──────────────────────────┐
   │   parseh-tunnel          │  127.0.0.1:9050 (loopback only)
   │   (this crate)           │
   │                          │
   │   1. SOCKS5 handshake    │
   │   2. Pick exit peer      │  ← PeerRegistry on parseh.caps.v1
   │   3. Sign request        │  ← ed25519 client identity
   │   4. Open libp2p stream  │  ← /parseh/tunnel/1.0.0
   │   5. Bidirectional copy  │
   └──────────────────────────┘
            │
            │  libp2p TCP + Noise + Yamux + request-response
            │
            ▼
   ┌──────────────────────────┐
   │   exit peer (PARSEH)     │
   │   has_external_internet  │
   │                          │
   │   1. Verify req signature│
   │   2. Dial target TCP     │
   │   3. Bidirectional copy  │
   └──────────────────────────┘
            │
            │  plain TCP (the destination is, by definition, not the censor)
            ▼
   [target: whatsapp.com:443 / instagram.com:443 / anything.tld:port]
```

Four moving parts:

- **SOCKS5 listener** (`src/socks5.rs`) — RFC 1928, CONNECT only,
  IPv4 + IPv6 + DOMAINNAME ATYPs.
- **Router** (`src/router.rs`) — ranks `PeerRegistry` peers by
  bandwidth, with deterministic PeerId tiebreak and `failover()` for
  rejected attempts.
- **Wire format** (`src/protocol.rs`) — CBOR `TunnelRequest` /
  `TunnelResponse`, with a domain-separated ed25519 signature over the
  request tuple.
- **Swarm** (`src/swarm.rs`) — minimal libp2p Swarm: TCP + Noise +
  Yamux + Identify + Kademlia DHT + Gossipsub (read-only on
  `parseh.caps.v1`) + outbound `/parseh/tunnel/1.0.0` request-response.

---

## Installation

```bash
cargo build --release -p parseh-tunnel
# Output: server/target/release/parseh-tunnel
```

A pre-built binary will accompany the V0.2.5 release of `parseh-miner`
once the binary release matrix is extended; see
[`CHANGELOG.md`](../../CHANGELOG.md).

---

## Usage

### From the standalone binary

```bash
# Start with a bootstrap multiaddr (required so the swarm can find peers):
parseh-tunnel start --bootstrap /ip4/1.2.3.4/tcp/8421/p2p/12D3Koo...

# Different port:
parseh-tunnel start --port 9051 --bootstrap /ip4/1.2.3.4/tcp/8421/p2p/12D3Koo...

# Inspect known exits:
parseh-tunnel status

# Smoke-test a target through a synthetic tunnel session:
parseh-tunnel test https://example.com
```

### From the developer CLI

`parseh-cli` ships an equivalent wrapper (shells out to
`parseh-tunnel`):

```bash
parseh tunnel start --bootstrap /ip4/.../tcp/.../p2p/...
parseh tunnel status
parseh tunnel test https://example.com
parseh tunnel stop      # best-effort; see below
```

We shell out (rather than embed) so the `parseh` CLI's dependency
closure stays small and short-lived. Users who prefer the standalone
binary should call it directly — same behaviour.

### Browser configuration

In Firefox: `Preferences → Network Settings → Manual proxy
configuration → SOCKS Host = 127.0.0.1, Port = 9050, SOCKS v5`. Tick
**"Proxy DNS when using SOCKS v5"** so DNS resolution happens on the
exit (this is the recommended setting; it also means
`parseh-tunnel` sees ATYP=DOMAINNAME requests, which is what the SOCKS5
handler expects).

In Chromium-family browsers, launch with
`--proxy-server=socks5://127.0.0.1:9050 --host-resolver-rules="MAP * ~NOTFOUND , EXCLUDE 127.0.0.1"`
to force proxy DNS.

### curl

```bash
curl --socks5-hostname 127.0.0.1:9050 https://example.com
```

The `-hostname` variant sends the hostname to the SOCKS5 proxy as a
domain (not a resolved IP), which is what we want — the exit performs
DNS.

---

## Privacy and security

### security model

The intended adversary is a **national-network filter** that drops or
fingerprints traffic it does not recognise. Strict adversarial-network
survivability is a property we are still gathering measurement data
for; until that data exists, the binding rule of the maintainer
note applies
and no "censorship-resistant" claim appears in this README or the CLI
copy.

### What the exit peer sees

The exit peer sees **target host + port**, the **client's libp2p
PeerId** (Noise-authenticated), the **client's signed request envelope**
(ed25519 over the target tuple), and the **bytes**. The same model as
any SOCKS5 proxy or Tor exit node. Operators who run an exit are
trusting their network operator to comply with applicable law, and we
explicitly do not promise they will not log traffic — that is up to
the operator. Volunteer exit operators are encouraged to read the project notes
"external connectivity" section before running.

### What the exit does NOT see

The exit does not see other PARSEH peers' traffic, your local DNS
queries that did NOT go via SOCKS5 (these are still leaked to your
ISP — see "Browser configuration" above), or anything from before the
SOCKS5 handshake started.

### Single-hop ≠ anonymous

We repeat this because it matters: a single-hop tunnel reveals **which
target** you asked for **to the exit operator**. If you are looking
for anonymity, this is not the tool. Tor remains the reference design
for low-latency anonymity (where it is reachable). Multi-hop circuits
inside PARSEH are tracked in [§ "What's deferred to V0.3+"](#whats-deferred-to-v03).

### Loopback only

The SOCKS5 listener binds `127.0.0.1` exclusively. The
[`socks5::run_socks5`] defensive check refuses any non-loopback
address — same policy as `server/miner/src/proxy.rs`. A non-loopback
SOCKS5 listener inside a hostile network is the kind of open-proxy
adversaries sweep for.

---

## Stubbed vs real

What V0.2.5 actually ships, end-to-end runnable:

- ✅ SOCKS5 (RFC 1928) handshake, method negotiation, CONNECT request
  parse + reply, IPv4 / IPv6 / DOMAINNAME — full RFC 1928 wire format,
  hand-rolled, ≥6 unit tests.
- ✅ Router with bandwidth-ranked exit selection + failover.
- ✅ CBOR wire format for `TunnelRequest` / `TunnelResponse`, with a
  domain-separated signing payload.
- ✅ libp2p swarm bring-up: TCP + Noise + Yamux + Identify + Kademlia
  DHT + Gossipsub on `parseh.caps.v1` + outbound `/parseh/tunnel/1.0.0`.
- ✅ CLI surface: `start`, `status`, `test`, plus the `parseh tunnel ...`
  wrapper.
- ✅ Friendly early exit when `--bootstrap` is missing.

Stubbed (deliberately, with the merge plan documented):

- ⏳ **Exit-side request-response handler.** The exit accepts inbound
  `/parseh/tunnel/1.0.0`, verifies the request, dials the target, and
  drives the bidirectional copy. This lands in the same V0.2.5
  milestone as the parallel
  `feat/peer-identity-registry-v0-2-5` agent's
  `PeerIdentity::has_external_internet` field — the two merge together
  so a registry value that says "this peer has external internet" is
  matched by a running handler that actually does.
- ⏳ **Client-side stream open** (after `Tunnel::run_session`'s control
  round-trip). Currently returns `TunnelError::ExitStreamNotImplemented`
  with a clean SOCKS5 `GeneralFailure` reply so an experimental client
  sees a deterministic error rather than a hang.
- ⏳ **`parseh tunnel stop`** — the binary does not yet write a pidfile,
  so `stop` prints a `pkill parseh-tunnel` hint. V0.3+ wires a real
  pidfile + control socket.
- ⏳ **Persistent identity.** V0.2.5 generates an ephemeral ed25519
  identity per `start` invocation. V0.3+ reads the same
  `~/.config/parseh/identity.ed25519` the miner uses, so reputation
  follows the user across binaries.

If you find any other gap not listed here, that is a bug — please
[`parseh report-issue`](../../README.md#read-also) it.

---

## What's deferred to V0.3+

- **Multi-hop circuits.** Tor-style onion routing inside PARSEH. The
  V0.2.5 scaffold is single-hop because (a) we do not yet have the
  latency headroom for multiple hops over our current peer set, and
  (b) anonymity properties require careful design — guard selection,
  congestion-aware path building, padding traffic — that we will not
  half-ship.
- **UDP-ASSOCIATE.** Needed for QUIC / WebRTC / WireGuard tunneling.
  V0.2.5 returns SOCKS5 `Command not supported` (REP=0x07) for
  UDP-ASSOCIATE and BIND. UDP transport needs a parallel relay path
  with its own framing.
- **REALITY-wrapped bridge leg.** When the chosen exit is in a
  different jurisdiction from the client, the cross-jurisdiction leg
  needs the stealth transport scaffolded in
  the project notes.
  The tunnel will call into the relay's REALITY transport once the
  swarm-level adapter from that scaffold lands.
- **Reputation-band tiebreak in the router.** Currently bandwidth +
  deterministic PeerId. Reputation plumbing lands when the parallel
  `PeerIdentity` registry agent exposes per-peer reputation.
- **Per-tunnel telemetry surface** (read by a future `parseh tunnel
  watch` subcommand). The plumbing exists at `debug!` log level; a
  structured surface for ops dashboards is V0.3+.

---

## Design decisions worth calling out

1. **Hand-rolled SOCKS5, not `fast-socks5`.** The miner already uses
   `fast-socks5` for its loopback listener — perfect when the outbound
   half is a direct TCP dial. The tunnel's outbound half is a libp2p
   stream, so we own the moment between "SOCKS5 reply" and "bytes
   flow"; ~100 lines of RFC 1928 buys that.
2. **request-response + a separate raw-byte yamux substream, not
   gossipsub.** The tunnel needs ordered + reliable + flow-controlled
   byte streams; gossipsub is publish-broadcast. The control round-trip
   uses `request_response::cbor::Behaviour` because it gives us
   length-delimited CBOR for free; the bidirectional copy happens on a
   yamux substream because carrying the payload inside the request-
   response message would buffer the whole connection.
3. **Domain-separated signing payload, ed25519 only.** Tunnel requests
   are signed; libp2p Noise has already authenticated the peer id at
   the transport layer, but the inner signature gives an exit operator
   non-repudiable proof of a target request if a downstream complaint
   arrives. The domain tag prevents cross-protocol replay.
4. **No anonymity claim.** Even if every other property held perfectly,
   single-hop SOCKS5 is not anonymous. We choose not to advertise a
   property we do not exhibit — see the cultural rule.

---

## Tests

```bash
cargo test -p parseh-tunnel
```

Coverage at V0.2.5 ship:

- 21 unit tests across `protocol`, `socks5`, `router`, `swarm`,
  `tunnel`.
- 8 integration tests in `tests/integration.rs` covering SOCKS5
  roundtrip, reply byte layout, router ranking + failover, CBOR
  roundtrip for every wire variant, domain-separated signing payload,
  bootstrap-precondition refusal, and the empty-registry None case.

Total: **29 tests** (target was ≥6).

---

## License

Apache-2.0. See `LICENSE` at the repository root.
