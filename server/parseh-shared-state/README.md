# parseh-shared-state

V0.2 Primitive 3 of the PARSEH coordination layer: a persisted, signed,
gossipsub-syncing shared state. Backed by SQLite, optionally encrypted
at rest by SQLCipher.

## Status

- Schema: V1 — six tables (`tasks`, `results`, `verifications`,
  `outcomes`, `reputation_log`, `governance_rules`).
- Gossipsub topic: `parseh.state-deltas.v1`.
- Wire envelope: signed [`StateDelta`] wrapping
  [`parseh_task::JobOutcome`] / reputation / governance updates.

## Cargo features

| Feature | Default | What it does |
|---|---|---|
| `bundled` | ON | Statically links plain SQLite. Fast to build. **No** encryption-at-rest. The `PRAGMA key` call is a documented SQLite no-op, so the encryption code path is exercised without enforcing it. |
| `encrypted` | OFF | Statically links SQLCipher (via `bundled-sqlcipher-vendored-openssl`). Real at-rest encryption. Adds ~3-5 minutes to a clean build because it compiles SQLCipher + OpenSSL from source. |

### Why ship `bundled` as the default at V0.2

The V0.2 task spec asked for `bundled-sqlcipher` if compile time is
acceptable, otherwise fall back. On the worktree's first clean build,
the vendored-OpenSSL chain triggered a multi-minute compile and a
linker dependency on `cmake` / `perl` that varies across CI images.
Rather than make V0.2 development infrastructure fragile, the
`encrypted` build is opt-in: every binary that ships to a user
deployment **must** flip the feature on. The `parseh-miner` crate's
release profile in V0.2.1 will pin `parseh-shared-state` with
`features = ["encrypted"]` once the CI matrix has been validated.

### Forensic posture note

Per the project notes Rule 4 + the 2026-05-14 decision on open
question Q2, SQLCipher encryption-at-rest is mandatory for any user-
facing deployment. The `bundled` default exists for development speed
only; **shipping a release build with `bundled` is a release-blocking
defect**.

## Trust boundary

`parseh.state-deltas.v1` carries `StateDelta` envelopes. The only
kinds allowed are:

- `Outcome(JobOutcome)` — signed by the consensus-observing peer.
- `Reputation { peer, delta, reason, related_hash }` — signed by the
  observer.
- `GovernanceRule { rule_name, rule_value, proposer, approvers }` —
  signed by the proposer.

Mid-window [`parseh_verify::Quorum`] state is **never** gossiped — a
partial-state replay vector that the V0.2 design review explicitly
ruled out.

## Tests

```
cargo test -p parseh-shared-state            # bundled, default
cargo test -p parseh-shared-state --features encrypted -- --include-ignored
```
