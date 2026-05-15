# `wallet`

**V0.1 wallet primitives: ed25519 keypairs + bech32 addresses.**

The PARSEH wallet's cryptographic foundation. Used by:

- `parseh-miner` for the libp2p identity (currently shared with the wallet key — V0.3+ may separate)
- `parseh-cli` for `parseh whoami`
- The future Hiderun client (V0.3+) for wallet display + signing

## What's here

- `src/lib.rs` — `Keypair::generate`, `Keypair::sign`, `verify`, `public_key`
- `Address` type that emits the bech32 form with HRP `parseh`
- `WalletError` enum

## Cryptographic choices

- **Curve**: ed25519 (via `ed25519-dalek` 2.x)
- **Address encoding**: bech32 0.11 with HRP `parseh`
- **CSPRNG**: OS-provided (`getrandom` family)

Addresses look like: `parseh1qpzry9x8gf2tvdw0s3jn54khce6mua7lc3v3le8`.

## What this crate does NOT include yet

- Mnemonic seed generation (BIP-39) — V0.3+ work
- Hierarchical-deterministic (HD) key derivation (BIP-44) — V0.3+
- Hardware-wallet support (Ledger app) — V1
- Multi-sig — V0.3+
- Key rotation protocol — V0.3+
- Encrypted seed export — V1
- Wallet key separation from libp2p identity — V0.3+

Per the project notes §1.6, the wallet posture is "solid primitives, weak key management." Strengthening key management is V0.3 work before chain emission.

## Status

✅ Shipped V0.1 · 2026-05-13. Stable surface; future enhancements additive.

Apache-2.0.
