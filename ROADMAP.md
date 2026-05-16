# Roadmap

Contribution map and module status. Technical only. v0.1.0-alpha.

## Status

The coordination layer is implemented and tested in-process. It does not
operate across the real internet: there are no public bootstrap nodes, no
live tunnels, no chain, no token. `bash scripts/demo.sh` builds the core
binaries and runs the offline 3-node acceptance test.

Test suite: 340+ test functions across the `server/` Rust workspace,
including fuzz and chaos harnesses. Coverage is in-process only; behaviour
across a real multi-node network is unmeasured — this is the primary gap.

## Module status

| Module | Purpose | Status |
|---|---|---|
| `server/core` | peer registry, readiness state machine, capability ads | implemented, tested |
| `server/parseh-task` | signed task/result/verification/outcome types | implemented, tested |
| `server/parseh-verify` | M-of-N quorum, reputation-weighted verification | implemented, tested |
| `server/parseh-shared-state` | encrypted append-only store + signed deltas | implemented, tested |
| `server/parseh-agent-spec` | signed agent/workflow definitions | implemented, tested |
| `server/parseh-agent-runtime` | executes agent/workflow definitions | implemented, tested |
| `server/parseh-llm-downloader` / `llm-detect` | model fetch + local-LLM detection | implemented, tested |
| `server/miner` | the node binary wiring the above onto libp2p | implemented; single-node only (no bootstrap) |
| `server/parseh-cli` | operator CLI against local state | implemented, tested |
| `server/parseh-testnet` / `parseh-integration-tests` | in-process multi-node harnesses | implemented, tested |
| `server/parseh-fuzz` / `parseh-chaos` | adversarial harnesses | implemented, tested |
| `server/parseh-tunnel` | client-side tunnel | scaffold; not operational |
| `server/relay` | stealth-transport relay | scaffold; not measured |
| `server/wallet` | operator-side key/value primitives | primitives only |
| `chain/` | Cosmos-SDK L1 | design-only; stub binary, no blocks |
| `client/` | user apps (browser/messenger/wallet) | design-only |
| `sdk/merchant` | merchant payment integration | specification only |

## Where to contribute

Open issues map to the gaps above:

- **#20** bootstrap nodes + live multi-node testnet — the load-bearing gap; nothing runs on the real internet until this exists.
- **#21** live encrypted tunnel / open-internet path — design + implementation.
- **#22** contribution accounting & reward layer — design before code.
- **#23** client transport options for restricted networks — research.
- **#24** security behaviour at real-network scale — extend fuzz/chaos to multi-node.
- **#25** code clarity & docs pass on `server/` crates — good first issue.

## How to contribute

1. `git clone https://github.com/hiderun-tui/parseh && cd parseh`
2. `bash scripts/demo.sh` — confirms the offline acceptance test passes locally.
3. Pick an issue above. Comment with your approach before large changes.
4. Build and test: `cd server && cargo build --workspace && cargo test --workspace`.
5. Open a pull request. Keep changes focused; explain the *why*. No overclaiming in code, comments, or docs.

Contributors are pseudonymous; no real-world identity is required. Report
security issues privately per [`SECURITY.md`](./SECURITY.md), never in a
public issue.
