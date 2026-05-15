# PARSEH

**Free, peer-verified AI for people priced out of or cut off from commercial AI — and a safer path onto the open internet.**

PARSEH is an open-source protocol where contributor machines run local LLMs, verify each other's work with M-of-N consensus, and share that verified compute with people who cannot reach or afford commercial AI. A safe tunnel onto the open internet is one duty of the network, not the whole point. It is built pseudonymously and developed in the open, with a hard rule against overclaiming.

> **Status: v0.1.0-alpha — engineering preview, not a product.**
> The coordination primitives work and are tested in harnesses. The network does **not** form across the real internet yet (no public bootstrap nodes). It is **not** a finished VPN, not anonymous, not censorship-resistant, not production-grade. There is no token. Treat it as a protocol experiment.

## What works today

Runnable at this commit, no network required:

- `parseh-miner` — generates an ed25519 identity, joins libp2p, subscribes to the gossip topics, runs the finalise tick, opens encrypted shared state, detects a local LLM. Single node.
- `parseh` CLI — local status / identity / submit / inspect commands against local state.
- `parseh-coord` — operator tool with real GitHub, Nostr, and Codeberg connectors and an explicit approve-before-send gate.
- A multi-crate Rust workspace with a passing test suite, a 3-node in-process acceptance test, plus fuzz and chaos harnesses.

## Try it

```bash
git clone https://github.com/hiderun-tui/parseh && cd parseh
bash scripts/demo.sh    # builds the core binaries + runs the 3-node acceptance test
```

The first run compiles the binaries (a few minutes); re-runs are fast. The demo is fully offline — it proves the coordination flow in a test harness, not across the internet.

## Build & test

```bash
cd server
cargo build --release --workspace
cargo test --workspace
```

Each component directory has its own README with specifics.

## How to contribute

Pseudonymous contributors are welcome — no real identity is ever required. Good places to start:

- A Nostr / Matrix connector or other improvements in `tools/parseh-coord`
- Additional fuzz / chaos targets under `server/`
- Bootstrap-node and multi-node testnet work (the load-bearing gap)
- Code clarity and test coverage

Open an issue or a pull request. Early, blunt technical feedback is more useful to this project than stars.

## Security

Report security issues privately — see [`SECURITY.md`](./SECURITY.md). Do not open public issues for vulnerabilities. Running a node, especially in some jurisdictions, may carry legal risk.

## License

[Apache 2.0](./LICENSE).
