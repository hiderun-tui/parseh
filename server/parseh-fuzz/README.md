# parseh-fuzz

V0.2.5 cargo-fuzz harness for the wire-format deserialisation paths
that cross the trust boundary into a PARSEH node.

This crate is **excluded from the `server/` Cargo workspace** (see the
`exclude` line in `server/Cargo.toml`). The exclusion is the cargo-fuzz
convention: fuzz targets are built with a libFuzzer runtime and
optionally with `-Z sanitizer=address`, which we do not want infecting
the production workspace's feature-unification or lockfile.

## What is covered

Seven targets, one per byte sequence an adversary on the wire can put
in front of a node:

| Target | Decoder | Crate |
|---|---|---|
| `fuzz_job_spec` | `JobSpec` (CBOR) | `parseh-task` |
| `fuzz_job_result` | `JobResult` (CBOR) | `parseh-task` |
| `fuzz_job_verification` | `JobVerification` (CBOR) | `parseh-task` |
| `fuzz_job_outcome` | `JobOutcome` (CBOR) | `parseh-task` |
| `fuzz_capability_advertisement` | `decode_advertisement` (CBOR, v1+v2) | `parseh-core` |
| `fuzz_state_delta` | `StateDelta::decode_cbor` | `parseh-shared-state` |
| `fuzz_signature_verify` | `verify_bytes` (pk, msg, sig from raw bytes) | `parseh-task` |

The first six are pure decoders — they assert that no adversary-shaped
CBOR input can crash, panic, infinite-loop, or unexpectedly allocate
inside the `ciborium` decoder + the per-type `serde::Deserialize`
implementation.

The seventh exercises the ed25519 verify primitive every wire-type's
`verify_signature()` method ultimately delegates to. It is the one
place in the codebase where attacker bytes flow into the
`ed25519-dalek` crate, so fuzzing it directly is the highest-leverage
way to exercise the dependency under hostile input.

## Install

cargo-fuzz requires nightly Rust:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Run one target

```sh
cd server/parseh-fuzz
cargo +nightly fuzz run fuzz_job_spec
```

The runner reads from `corpus/fuzz_job_spec/` automatically (the
committed seeds — see [seed corpus](#seed-corpus)) and stops when
either (a) you press `Ctrl-C` or (b) `-max_total_time=N` seconds have
elapsed.

## Run every target

```sh
TIME_BUDGET=60 ./run-all.sh   # 7 × 60 s ≈ 7 min wall clock
```

The script exits non-zero on the first crash; the crash artifact is
recorded under `fuzz/artifacts/<target>/`.

## Seed corpus

Each target has 3–5 seeds in `corpus/<target>/`. They are committed
so a reviewer can audit what we consider "valid input" without
running anything. Regenerate them byte-for-byte:

```sh
cd server/parseh-fuzz
cargo run --bin gen_corpus
```

The generator (`gen/gen_corpus.rs`) uses deterministic 32-byte-fill
signing keys and hard-coded timestamps, so the output is stable
across machines and Rust patch-versions. It is committed alongside
the fixtures for full reproducibility.

## Crash artifact analysis

If a fuzz run finds a crash, you'll get output like:

```
ERROR: libFuzzer: deadly signal
artifact_prefix='./fuzz/artifacts/fuzz_job_spec/'; Test unit written to ./fuzz/artifacts/fuzz_job_spec/crash-<hex>
```

To reproduce:

```sh
cargo +nightly fuzz run fuzz_job_spec fuzz/artifacts/fuzz_job_spec/crash-<hex>
```

To minimise the artifact to the smallest input that still triggers
the crash:

```sh
cargo +nightly fuzz tmin fuzz_job_spec fuzz/artifacts/fuzz_job_spec/crash-<hex>
```

Then file the minimised input + stack trace as a release-blocking
issue.

## Cultural rule (binding)

Fuzz findings are **release-blocking**. The project's threshold doc
(the project notes)
captures the policy: any future feature that introduces a similar
"default-off for dev / required-for-release" toggle MUST have an
equivalent release-guard before the binary ships. Decoder-correctness
under hostile input is exactly such a release-guard — V0.2.5 ships
with the guard in place.

## Non-obvious design decisions

- **Workspace-excluded.** cargo-fuzz's libFuzzer runtime would
  otherwise pollute the production workspace's feature graph; the
  cost is that we depend on `parseh-task` / `parseh-verify` /
  `parseh-shared-state` / `parseh-core` via `path = ...` and rely on
  cargo walking up to find each crate's workspace. The exclusion is
  the canonical cargo-fuzz layout.
- **No explicit memory cap on `ciborium::from_reader`.** ciborium
  0.2 does not allocate ahead-of-time for nested arrays — it streams
  — so the OOM amplification vector usually associated with
  length-prefixed binary formats is not present here. libFuzzer's
  per-iteration `rss_limit_mb` (default 2 GiB) is the safety net.
  If a future audit identifies a length-prefix vector that
  ciborium *does* honour ahead-of-time, wrap the call in
  `ciborium::de::Decoder::with_recursion_limit(...)` and bump the
  V0.2.5 entry in [`CHANGELOG.md`](../../CHANGELOG.md).
- **`fuzz_signature_verify` slices the input fixed-width (32 + 64
  + N).** This is the smallest fuzzing surface that still covers
  every dalek error path (length-decode, edwards-point parse,
  cryptographic verify). A struct-aware fuzzer would have to import
  `arbitrary` or `serde_arbitrary` and shape the input, which buys
  nothing because `verify_bytes` already takes three independent
  byte-slices.

## What this crate intentionally does NOT do

- It does not fuzz transport-level framing (libp2p Noise / Yamux).
  Those crates have their own upstream fuzzing.
- It does not fuzz SQLite / SQLCipher migrations. Wire format only.
- It does not produce structured proofs (KLEE / symbolic execution).
  V0.3+ may add a `cargo-bolero` target for property-based shape
  coverage; for V0.2.5 the libFuzzer-style coverage is what releases
  block on.
