#!/usr/bin/env bash
# PARSEH 60-second demo — builds the core binaries and proves the
# coordination layer works in a test harness. NO network is contacted;
# this is single-node + in-process, exactly as honest as the README says.
#
# Usage:  bash scripts/demo.sh
# Requires: a Rust toolchain (rustup, stable). Nothing else.

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
echo "==> PARSEH demo · repo: $ROOT"

if ! command -v cargo >/dev/null 2>&1; then
  echo "!! cargo not found. Install Rust: https://rustup.rs  then re-run." >&2
  exit 1
fi

echo
echo "==> [1/4] Building core binaries (parseh-miner, parseh CLI) — release"
( cd server && cargo build --release -p parseh-miner -p parseh-cli )

MINER="$ROOT/server/target/release/parseh-miner"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
# --config-dir keeps the demo identity out of your real OS config dir.
echo
echo "==> [2/4] Generating a fresh ed25519 node identity (sandboxed in \$TMPDIR)"
"$MINER" --config-dir "$WORK" init
echo "    --- whoami ---"
"$MINER" --config-dir "$WORK" whoami

echo
echo "==> [3/4] 3-node acceptance test (deterministic in-process replay)"
( cd server && cargo test -p parseh-testnet --release -- --nocapture ) | tail -n 15

echo
echo "==> [4/4] parseh-coord operator tool — 17 offline tests"
( cd tools/parseh-coord && cargo test --release --quiet ) | tail -n 5

echo
echo "==> Done. What you just saw is the honest scope:"
echo "    - a real node identity + miner that boots single-node"
echo "    - the coordination flow proven in a 3-node harness (no internet)"
echo "    - the operator tooling green"
echo "    The network does NOT form yet — no public bootstrap nodes exist."
echo "    The network does not form yet (no public bootstrap nodes)."
