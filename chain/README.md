# PARSEH Chain

The PARSEH L1 blockchain — a fork of [Cosmos SDK](https://github.com/cosmos/cosmos-sdk) with custom modules.

> **Scaffold state.** The directory now compiles to a working `parsehd` binary
> that prints help and stubs but produces no blocks. Real Cosmos SDK
> integration lands in V0.1 (see the project notes).

## Code layout (current scaffold)

```
chain/
├── go.mod                  # module declaration; SDK deps commented for now
├── Makefile                # build / test / lint targets
├── cmd/
│   └── parsehd/main.go     # daemon entry point (cobra CLI; version / init / start stubs)
└── x/
    └── inference/          # distributed LLM inference module
        ├── module.go       # module wiring + contract notes
        ├── types/types.go  # Provider, Job, Attestation type stubs
        └── keeper/         # state-modifying logic stubs
            ├── keeper.go
            └── errors.go
```

## Quick build

```bash
cd chain/
go mod tidy
make build
./bin/parsehd version
./bin/parsehd --help
```

See the project notes for the full toolchain install.

## Why Cosmos SDK

- **CometBFT BFT consensus** with instant finality (~2 second blocks) fits Proof of Service well
- **Mature ecosystem**: Lokinet/Oxen, Akash, dYdX, Celestia all run on Cosmos SDK in production
- **IBC** opens future cross-chain bridges without reinventing
- **~18 months saved** vs. building an L1 from scratch in Rust/Go

## Modules

| Module | Purpose | Status |
|---|---|---|
| `x/nodes` | Node registration, stake bonding, capability declarations | Spec |
| `x/services` | Service receipt submission, reward computation | Spec |
| `x/parseh` | Native PARSEH token (transfers, balance, stealth addresses) | Spec |
| `x/slashing` (extended) | Slash conditions for bad service | Spec |
| `x/gov` (standard) | Governance proposals and voting | Use upstream |
| `x/auth` (standard) | Account management | Use upstream |
| `x/bank` (standard) | Token transfers | Use upstream + stealth-address overlay |

## Consensus model

**Proof of Service over CometBFT**:

- **Validators** (21–101): produce blocks. Selected stake-weighted, rotated periodically.
- **Service nodes**: stake PARSEH, advertise capabilities, serve user requests, submit signed receipts.
- **Rewards**: emission from genesis allocation, distributed pro-rata by verified work delivered.
- **Slashing**: misbehavior (false inference, dropping packets, downtime) cuts stake.

Receipt verification is the hard problem. Approach:

1. Clients sign attestations on receiving service
2. Service nodes batch and submit on-chain
3. Random audits via redundant execution
4. Reputation accumulates over time (newer nodes earn at a discount until they build trust)

## Genesis parameters (preliminary)

| Parameter | Value | Notes |
|---|---|---|
| Block time | 2s | Tunable |
| Validator set size | 21 → 101 | Grows as stake distributes |
| Total supply cap | ~150M PARSEH | Asymptotic emission curve |
| Genesis: team | 20% | 4-year linear vest |
| Genesis: treasury | 20% | Multi-sig with governance |
| Genesis: early operators | 10% | First 100 service nodes |
| Genesis: service rewards | 50% | Emitted over 10 years |
| Min validator stake | TBD | High enough to deter sybil |
| Min service node stake | TBD | Lower than validator |

## What goes on-chain

| Object | Approx size | Frequency |
|---|---|---|
| Node registration | 256 B | Once per node |
| Capability update | 128 B | As needed |
| Service receipt (batched, per receipt) | 64 B | Per work unit |
| PARSEH transfer | 128 B | Per tx |
| Anchored content hash (optional) | 32 B | Per anchor |
| Slashing event | 128 B | Rare |
| Governance vote | varies | Rare |

**What never goes on-chain**: prompts, message bodies, relay packets, model weights, video, anything large.

## Build (when code exists)

```bash
make install
parsehd init <node-name> --chain-id parseh-mainnet
parsehd start
```

## Status

| | |
|---|---|
| Spec | Drafted (this file + the project notes) |
| Scaffold | ✅ Compiles to a hello-world `parsehd` binary |
| Cosmos SDK integration | ⏳ V0.1 milestone, open for contributors |
| Two-node devnet | ⏳ V0.1 milestone |
| Mainnet | ⏳ V1 milestone (after audit) |

Pick up a [V0 chain issue](https://github.com/hiderun-tui/parseh/issues?q=is%3Aissue+is%3Aopen+label%3Av0+label%3Achain)
or open a new one.

## Open design questions

1. **Receipt verification scheme**: redundant-execution + statistical agreement vs. lightweight ZK proofs. Tradeoff: overhead vs. confidence.
2. **Stake economics**: what stake-to-reward ratio prevents sybil attacks without locking out small operators?
3. **Validator geographic distribution**: how to verify validator location claims for diversity guarantees?
4. **Stealth-address scheme**: native to PARSEH token or via a privacy overlay (zk-SNARKs, ring signatures)?
5. **Upgradability**: governance-driven upgrades vs. hard-fork releases?
