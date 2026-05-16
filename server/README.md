# PARSEH Server (Node Software)

The software contributors install to participate in the network and earn PARSEH.

## What it does

- Detects available hardware (GPU/CPU/RAM/bandwidth)
- Registers with the chain advertising capabilities
- Runs one or more services: **inference**, **relay**, **storage**
- Submits signed service receipts on-chain
- Hosts the operator's wallet

## Target platforms

- **Linux** — primary target (servers + gaming rigs running Linux)
- **Windows** — primary target (gaming PCs)
- macOS — secondary (small share of GPU contributors)
- Mobile — **not** a target for the server software (battery, thermals, churn make mobile unviable as a node provider)

## Layout

```
core/          Node daemon: P2P (libp2p), capability advertisement, receipt submission
inference/     llama.cpp / Ollama integration; model management; quantization handling
relay/         Stealth transport (V2Ray REALITY / Hysteria 2 integration)
wallet/        Operator-side wallet (stake, claim rewards, transfer)
platforms/
  linux/       systemd unit, .deb / .rpm packaging
  windows/     Windows service, MSI installer
```

## Language choices

- **Rust** for `core`, `relay`, `wallet` — async I/O, FFI for Cosmos chain client, mature crypto libs
- **Go** for chain interaction shims (CosmWasm / Cosmos SDK is Go-native)
- **C++/Rust** bindings to llama.cpp in `inference/`

## Hardware detection

On install, the server probes:

- GPU(s): vendor, VRAM, CUDA/ROCm capability
- CPU: cores, AVX-512 support
- RAM: total, available
- Disk: free, model-cache budget
- Network: upload/download throughput, MTU, NAT type
- Public IP detection (for relay capability)

From this it picks compatible models:

| Tier | VRAM | Models served |
|---|---|---|
| Mobile-class | 0 (CPU) | TinyLlama, Phi-3 mini, Gemma 2B |
| Mid-range GPU | 8–12 GB | Llama 3.1 8B, Mistral 7B, Qwen 7B |
| Enthusiast GPU | 16–24 GB | Llama 3.1 70B (Q4), Mixtral 8x7B (Q4) |
| Workstation | 40+ GB | DeepSeek-V4, Llama 3.1 70B (full), Qwen 2.5 72B |

## Service modes

A node can opt into any combination:

- **Inference-only**: GPU power, no relay traffic. Lower legal exposure.
- **Relay-only**: bandwidth donor, no GPU usage. Suitable for VPS contributors.
- **Storage-only**: disk donor.
- **Full**: all three.

This is set in the node config.

## Earning PARSEH

Earnings depend on:

- Service type (inference > relay > storage in terms of rate per unit)
- Verified work (signed receipts from clients)
- Uptime
- Reputation (newer nodes start at discount; build up over weeks)

See [chain/README.md](../chain/README.md) for receipt and reward mechanics.

## Operator safety

Running a network node may carry legal risk depending on your jurisdiction. Understand that risk before installing.

The server software is designed for **operator anonymity**:

- No "PARSEH" branding in process names, ports, or banners
- Runs on standard port 443; can co-host with a real website
- Receives payments to its wallet address
- Generic TLS certificates that mirror a known public site

This is necessary in jurisdictions where running a circumvention node carries legal risk. **It is not a guarantee.** Run a node only if you understand the risks in your specific jurisdiction.

## Code layout (current scaffold)

```
server/
├── Cargo.toml             # workspace manifest
├── core/                  # shared types, NodeConfig, NodeCapabilities
│   ├── Cargo.toml
│   └── src/lib.rs
├── miner/                 # ★ autonomous service-providing daemon (V0 main deliverable)
│   ├── Cargo.toml
│   ├── README.md
│   └── src/
│       ├── main.rs              # libp2p swarm + event loop
│       ├── identity_store.rs    # ed25519 persistence
│       ├── config.rs            # TOML config in OS config dir
│       ├── orders.rs            # JobOrder + JobResult CBOR wire types
│       ├── executor.rs          # pluggable executor; EchoExecutor today
│       └── update_check.rs      # GitHub releases polling
├── relay/                 # standalone libp2p relay binary (testing helper)
│   ├── Cargo.toml
│   └── src/main.rs
├── inference/             # llama.cpp host (stub today)
│   ├── Cargo.toml
│   └── src/main.rs
├── wallet/                # PARSEH chain client (stub today)
│   ├── Cargo.toml
│   └── src/lib.rs
└── platforms/             # OS-specific service install (stub today)
    ├── Cargo.toml
    └── src/lib.rs
```

## Quick build

```bash
cd server/
cargo build --release --workspace

# Generate identity + default config (one-time per machine)
./target/release/parseh-miner init

# Print identity + config locations
./target/release/parseh-miner whoami

# Start mining — listens on TCP/8421, joins the network, accepts encrypted job orders
./target/release/parseh-miner start

# Or run the bare relay test helper (no DHT / no gossip / no orders — just ping)
./target/release/parseh-relay --listen /ip4/0.0.0.0/tcp/8421
```

See the root README for the Rust install
and per-OS libp2p prerequisites.

## Status

| Crate | State |
|---|---|
| `core` | ✅ Compiles; `NodeConfig` + `NodeCapabilities` types defined |
| `miner` | ✅ **Full V0 daemon** — persistent ed25519 identity, libp2p Noise + Yamux + Kad DHT + Identify + Gossipsub + request-response, TOML config, GitHub release update check. Echo executor today. |
| `relay` | ✅ Bare Ping + Identify swarm (kept for tests + as a teaching example) |
| `inference` | ⏳ Stub binary; llama.cpp integration drops into `miner/src/executor.rs` |
| `wallet` | ⏳ Stub library; ed25519 + bech32 + chain client land in V0.1 |
| `platforms` | ⏳ Per-OS service install scaffolds; logic pending |

Released binaries appear at
[github.com/hiderun-tui/parseh/releases](https://github.com/hiderun-tui/parseh/releases)
once a `v*` tag is pushed (Windows / Linux / macOS · x86_64 + aarch64,
SHA-256 checksums included).

Pick up issues labelled
[`server`](https://github.com/hiderun-tui/parseh/issues?q=is%3Aissue+is%3Aopen+label%3Aserver) or
[`v0`](https://github.com/hiderun-tui/parseh/issues?q=is%3Aissue+is%3Aopen+label%3Av0).
