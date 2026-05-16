# PARSEH SDKs

Developer libraries for integrating with the PARSEH network.

## SDKs

| SDK | Purpose | Status |
|---|---|---|
| `merchant/` | Accept PARSEH at stores and e-commerce sites | Spec |

## Merchant SDK (`merchant/`)

For brick-and-mortar shops and online merchants to accept PARSEH payments.

### Flow

1. Merchant creates an **invoice** (amount in IRR, USD, or PARSEH).
2. SDK returns a **payment QR code** + **NFC NDEF payload** + REST endpoint.
3. Customer scans (or taps) with Hiderun.
4. Customer's Hiderun signs and broadcasts a payment transaction.
5. Merchant's SDK is notified via webhook on chain confirmation (~2 seconds).

### Integration paths

- **Drop-in JavaScript** for e-commerce platforms (Shopify, WooCommerce, custom)
- **Mobile POS app** for in-store counters (Android tablet → NFC reader)
- **REST API** for custom integrations
- **CLI tool** for testing and small operators

### Components (planned)

- Invoice service: creates and tracks invoices
- Price oracle: PARSEH ↔ IRR / USD conversion (DEX-derived + fallback feeds)
- Settlement listener: watches the chain, fires webhooks on confirmation
- Refund flow: signed refund requests from merchant → customer wallet

### What this SDK does NOT do

- It does not act as a custodian. Payments settle on-chain to the merchant's wallet.
- It does not store customer identifying data beyond what the merchant chooses.
- It does not enforce KYC. That is the merchant's responsibility.

## Code layout (current scaffold)

```
sdk/
├── Cargo.toml             # workspace manifest
├── core/                  # cross-platform Rust core (UniFFI)
│   ├── Cargo.toml
│   ├── build.rs           # generates Kotlin/Swift bindings at build time
│   └── src/
│       ├── lib.rs         # Client, ClientConfig, NetworkStatus, ConnectionState
│       └── parseh.udl     # UniFFI interface definition (source of truth)
└── merchant/              # merchant SDK (V2 — stub today)
    ├── Cargo.toml
    └── src/lib.rs
```

## Quick build

```bash
cd sdk/
cargo build --workspace
# Generate Kotlin bindings (for Android):
cargo run --package parseh-sdk --features uniffi/cli -- \
  generate ./core/src/parseh.udl --language kotlin --out-dir ./bindings/kotlin
# Generate Swift bindings (for iOS):
cargo run --package parseh-sdk --features uniffi/cli -- \
  generate ./core/src/parseh.udl --language swift --out-dir ./bindings/swift
```

See the project notes for the Rust + UniFFI install.

## Status

| Crate | State |
|---|---|
| `core` | ✅ Scaffold compiles; UniFFI-exposed `Client`, `NetworkStatus`, config types |
| `merchant` | ⏳ Type stubs only; full SDK is V2 scope (roadmap) |
