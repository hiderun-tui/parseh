# `parseh-miner` — autonomous PARSEH service-providing node

Headless mining daemon. Runs in the background on a contributor's PC
(Windows, Linux, macOS) and:

1. **Generates and persists** an ed25519 identity at the OS's standard config dir.
2. **Opens** the V0.2 SharedState SQLite database (`<config-dir>/shared-state.db` by default) where tasks, results, verifications, and outcomes are persisted.
3. **Connects autonomously** to other PARSEH peers using libp2p Kademlia DHT — no central seed required once a peer is known.
4. **Subscribes** to FOUR gossipsub topics: `parseh.caps.v1`, `parseh.tasks.v1`, `parseh.verify.v1`, `parseh.state-deltas.v1`.
5. **Advertises capabilities** via CBOR `CapabilityAdvertisement` on `parseh.caps.v1`. A legacy-JSON fallback decoder accepts V0.1 ads during the rolling upgrade (dropped in V0.2.5).
6. **Accepts encrypted job orders** over TWO request-response protocols: `/parseh/job/1.0.0` (V0.1 `JobOrder`/`JobResult` legacy; deprecation warning on every inbound; removed in V0.2.5) and `/parseh/job/2.0.0` (V0.2 `JobSpec`/`JobResult`).
7. **Runs the V0.2 finalisation tick** every 100 ms — walks every open `parseh-verify::Quorum`, and on close: signs + publishes the `JobOutcome` as a `StateDelta` on `parseh.state-deltas.v1`, plus reputation deltas per the project notes §4.
8. **Executes work** via a pluggable executor — today an echo stub.
9. **Polls GitHub for updates** every 6 hours and logs when a newer release is available.

## V0.2 SharedState — encryption key source

The SharedState DB is opened (optionally SQLCipher-encrypted) with the
key derived from `KeySource::IdentityFile`, i.e. SHA-256 over the local
`identity.ed25519` bytes. **This is strictly weaker than a user
passphrase**: anyone with file-system read access to the identity file
can derive the database key.

V0.3+ MUST migrate to `KeySource::Passphrase` via the passphrase unlock
unlock flow (see the project notes §5).
Until then, treat the SharedState DB as confidential-only-against-
remote-attackers and back it up alongside `identity.ed25519`.

Override the path with `--shared-state-db PATH`; wipe with
`--reset-shared-state` (prompts for confirmation unless `--yes` is also
passed).

## Quick start

```bash
# Build
cd server
cargo build --release --bin parseh-miner

# Initialise (writes identity + default config)
./target/release/parseh-miner init

# Check identity + config
./target/release/parseh-miner whoami

# Start mining
./target/release/parseh-miner start
# or dial a known peer:
./target/release/parseh-miner start --dial /ip4/198.51.100.7/tcp/8421
```

## Config — where it lives

| OS | Config dir |
|---|---|
| Windows | `%APPDATA%\parseh\` |
| Linux | `$XDG_CONFIG_HOME/parseh/` (typically `~/.config/parseh/`) |
| macOS | `~/Library/Application Support/parseh/` |

Two files:

- `identity.ed25519` — 32 raw bytes, mode `0600` on Unix. **Back this up** — it is your earning identity.
- `miner.toml` — declared capabilities; commented and human-editable.

## What goes on the wire (and what doesn't)

```
[your miner]                                  [requester's relay]
    │                                                 │
    │ ──── encrypted Noise handshake ───────────────► │
    │ ◄── identify protocols + listen addrs ───────── │
    │ ──── kad bootstrap ──────────────────────────── │
    │ ◄── kad routing-table update ──────────────────
    │
    │ ◄── JobOrder{ model, prompt_hash, prompt,
    │              max_tokens, bounty_upar }  (CBOR over libp2p stream)
    │ ──── JobResult{ outcome, completion,
    │                tokens_used, wall_ms }   (CBOR over libp2p stream)
    │
    └─── gossipsub( parseh.caps.v1 ) every 60s ───►
```

Every byte is encrypted end-to-end between the two PARSEH peers by
libp2p's Noise transport. The chain validators see only the
`Attestation` the miner posts on-chain — never the prompt, never the
completion.

## Subcommands

| Command | What it does |
|---|---|
| `parseh-miner init` | Generates identity + writes default `miner.toml` + creates the SharedState DB schema. Idempotent. |
| `parseh-miner whoami` | Prints local PeerId + config path + advertised capabilities. |
| `parseh-miner start` | Default. Listens on TCP/8421 by default, joins the network, runs V0.2 verification + finalise tick until Ctrl-C. |
| `parseh-miner start --dial <multiaddr>` | Add an explicit bootstrap peer. |
| `parseh-miner start --no-update-check` | Skip the periodic GitHub poll (useful in air-gapped tests). |
| `parseh-miner start --socks5 <port>` | Expose a loopback-only SOCKS5 proxy on `127.0.0.1:<port>` for the Hiderun browser tunnel. Off by default. **The IP is not configurable** — the listener binds `127.0.0.1` only (never `0.0.0.0`) to avoid turning every miner into an open proxy. |
| `parseh-miner --shared-state-db PATH ...` | Override the SharedState DB path. Default: `<config-dir>/shared-state.db`. |
| `parseh-miner --reset-shared-state ...` | Wipe the SharedState DB (prompts to confirm). Pair with `--yes` for unattended CI. |
| `parseh-miner --init-only start` | Open identity + SharedState + emit a single readiness log line, then exit without starting the swarm. Useful for CI smoke tests. |
| `parseh-miner --show-readiness start` | Build the swarm + SharedState + emit one JSON readiness object on stdout, then exit. The JSON now includes a `shared_state` section (path, counts, reputation) and an `open_quorums` summary. |
| `parseh-miner --help` | Full CLI reference. |

### CLI from the user's perspective

Once the miner is running with `--socks5 1080`, a quick smoke test is:

```bash
parseh-miner start --socks5 1080 &
curl --socks5-hostname localhost:1080 https://example.com
```

The miner logs an `info` line for every accepted SOCKS5 connection and
`debug` lines for byte-level activity (`-v` or `RUST_LOG=parseh_miner=debug`).

## Status

| Subsystem | Today | V0.1 |
|---|---|---|
| Persistent identity (ed25519) | ✅ working | (no change) |
| TOML config | ✅ working | (no change) |
| libp2p Noise + Yamux + Ping + Identify | ✅ working | (no change) |
| Kademlia DHT peer discovery | ✅ working | Disk-backed record store |
| Gossipsub capability ads | ✅ working | Signed advert blobs |
| Encrypted job request-response | ✅ working (echo executor) | Real `llama-cpp-2` executor + attestation signing |
| Pluggable executor swap pattern | ✅ Two executors live (Echo default, Canary opt-in via `--features canary-executor`); real LLM slots in via same trait | LlamaExecutor or CandleExecutor as the third implementation |
| GitHub release update check | ✅ working (notice only) | Sigstore-signed binaries + automatic install (V0.2) |
| Wallet integration | ⏳ stub | ed25519 + bech32 + chain RPC client |
| On-chain attestation submission | ⏳ stub | Once chain produces blocks |

## Layout

```
server/miner/
├── Cargo.toml
├── README.md  (you are here)
├── src/
│   ├── main.rs              # CLI parsing, swarm setup, V0.2 event loop, finalise tick
│   ├── identity_store.rs    # ed25519 persistence (0600 on Unix)
│   ├── config.rs            # TOML config + OS config-dir resolution
│   ├── orders.rs            # V0.1 legacy JobOrder + JobResult wire types (CBOR)
│   ├── executor.rs          # Executor trait + EchoExecutor (echo stub)
│   ├── topics.rs            # V0.2 gossipsub topic + protocol constants
│   ├── readiness.rs         # ReadinessReport JSON (extended with shared_state / open_quorums)
│   ├── proxy.rs             # Loopback SOCKS5 listener (Hiderun browser tunnel)
│   └── update_check.rs      # GitHub releases polling
└── tests/
    └── integration_v0_2.rs  # V0.2 wiring tests — subscriptions, --init-only,
                             # SharedState round-trip, finalise tick, identity persistence
```

## security model

Read the project notes before running this
in some jurisdictions. Short version:

- Identity is local-only and never leaves the machine.
- Job traffic is end-to-end encrypted between libp2p peers.
- The miner does **not** announce its real IP to the chain; the chain
  only sees PeerId + Attestation hashes.
- Running an `inference` or `relay` capability inside a restricted jurisdiction carries
  Article 286 exposure. **Default config has `inference = false`** —
  toggle deliberately if and only if you accept the risk.
