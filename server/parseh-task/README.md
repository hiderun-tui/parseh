# `parseh-task`

**V0.2 Primitive 1 — signed task abstraction.**

The unit of work in PARSEH. Four content-addressable types form the task lifecycle:

```
JobSpec   →  JobResult  →  JobVerification  →  JobOutcome
(submit)     (execute)      (M-of-N verify)     (consensus)
```

Every type is signed by its author (ed25519), CBOR-encoded, and content-addressed by SHA-256.

## What's here

- `src/spec.rs` — `JobSpec`: a signed work request (prompt, service kind, seed, sensitive flag)
- `src/result.rs` — `JobResult`: a signed completion (executor, result bytes, method declared)
- `src/verification.rs` — `JobVerification`: a signed counter-attestation (verdict: Agreed / Disagreed / Abstained)
- `src/outcome.rs` — `JobOutcome`: the consensus aggregator (M-of-N counter-signatures)
- `src/hash.rs` — `ContentHash` newtype
- `src/sign.rs` — ed25519 signing helpers

## Wire format

- CBOR over libp2p request-response (`/parseh/job/2.0.0`) for bulky payloads
- CBOR over gossipsub for signed envelopes

Each top-level type embeds a `wire_version: u32` for migration. Current is `WIRE_VERSION = 1`.

## Test count

**39 unit + integration tests · all passing.**

```bash
cargo test -p parseh-task --release
```

Covers: roundtrip determinism, signature verify/tamper-fail, content-hash one-bit sensitivity, MAX_MESSAGE_SIZE_BYTES enforcement, every variant of every enum.

## Dependents

- `parseh-verify` — multi-peer verification on these types
- `parseh-shared-state` — persistence
- `parseh-miner` — daemon
- `parseh-cli` — submit + query
- `parseh-agent-spec` — agent definitions reference content hashes
- `parseh-testnet` — acceptance test

## Design notes

- `JobOutcome` is signed by the OBSERVING node (the one that closes the quorum locally), not by the network as a whole. Each honest peer that observes the same quorum produces a byte-different `JobOutcome` (different `finalised_at` + signatures). The `outcomes` table in `parseh-shared-state` grows O(observers × tasks); V0.3+ may collapse to O(tasks) via multi-sig observers.
- `VerifierMethod` is an enum with `Deterministic | SpotCheck | Statistical`. Only `Deterministic` is implemented in V0.2; the other two are V0.3+ stubs that return `unimplemented` so the API surface is stable.
- Result payloads are signed FULL (not hash-only) because verifiers need to see the bytes to re-execute. The `MAX_MESSAGE_SIZE_BYTES = 1 MiB` cap keeps this affordable.

## Status

✅ Shipped V0.2 · 2026-05-14.

Apache-2.0.
