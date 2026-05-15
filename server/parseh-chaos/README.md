# parseh-chaos

V0.2.5 adversarial test harness for the PARSEH verification + shared-state plane.

## Hard cultural boundary

This crate is **adversarial testing**, not exploit development.

- All scenarios run **in-process** over `libp2p::core::transport::MemoryTransport`.
- Faults are injected via test-only APIs we own.
- When an assertion FAILS, that signals a real V0.2 protocol bug.
  **Fix the protocol, not the test.** The tests are the spec.
- This crate is NOT a tool for attacking real PARSEH peers in production.
  There is no network-side adversarial code and no facility for
  targeting external IPs.
- This crate is NOT a public "attack toolkit". It is a workspace
  member with `publish = false`.

The one documented exception is the test
`more_than_half_rubber_stamp_compromises` — that test asserts the
empirical threshold at which V0.2's design is compromised and pins
the exact failure fraction (5/9 ≈ 55.6%) below which the protocol
holds and above which it cannot defend. The honest disclosure is
the goal; the failing-to-defend property is a known and documented
V0.2 limitation, not a bug.

## Scenarios

| Module                 | security model attacks covered | Notes |
| :--------------------- | :--------------------------- | :---- |
| `partition`            | §3.5, §3.9                   | **Priority milestone** per maintainer direction 2026-05-14. |
| `malicious_verifier`   | §3.2, §3.6                   | Five modes: AlwaysAgreed / AlwaysDisagreed / Random / RaceToVoteFirst / RubberStamp |
| `sybil`                | §3.1, §3.10                  | Empirical P=50 measurement; report in `results/sybil-empirical-2026-05-14.md` |
| `corruption`           | §3.3                         | FlipBitsInRow / TruncateRow / ReSignWithImposterKey / DeleteRow |

Cross-references:
- the project notes — the 12-attack
  surface we model against.
- the project notes — theoretical \$30–80 figure.
- the project notes — quorum + reputation parameters.

## Running

```bash
# Single-threaded (recommended for the chaos suite — multiple in-process
# libp2p meshes on a shared runtime get scheduler-starved otherwise).
cargo test -p parseh-chaos --release -- --test-threads=1

# With the Sybil empirical report written to disk:
PARSEH_CHAOS_WRITE_RESULTS=1 cargo test -p parseh-chaos --release \
    -- --test-threads=1 sybil_writes_empirical_report
```

The full suite is budgeted at < 5 minutes wall-clock. Individual
partition tests can take ~30 s each because they include a full
pre-partition baseline, partition window, heal window, and catchup
window.

## What the priority milestone (partition) asserts

Per maintainer direction 2026-05-14, the post-V0.2-PASS engineering
focus is **network-partition behaviour**. The `partition` module
spins up N nodes (default 6), splits them into a majority + minority,
lets each group run independently for D seconds, rejoins, and asserts:

1. The majority group continues to finalise tasks (M-of-N quorum
   holds within the larger half).
2. The minority group **stalls** — it cannot finalise on its own.
   This is the safety property: V0.2 must not finalise a phantom
   consensus.
3. After heal, the minority catches up within budget. **RESOLVED
   2026-05-15:** the harness originally surfaced gap #1 — V0.2 had
   *no anti-entropy / state-sync* and the minority could not replay
   outcomes finalised during the partition (gossipsub's IHAVE cache is
   too short). This is now CLOSED by **`/parseh/state-sync/1.0.0`**
   (see the project notes). `partition_recovery_
   converges_via_state_sync` ASSERTS post-heal minority convergence
   (observed ≈ 0.85 s); `partition_recovery_documents_protocol_gap` is
   retained as the no-sync-path regression guard.
4. **Critical**: every node converges on the same final state. The
   `histories_merged_correctly` field captures whether the two halves'
   `JobOutcome::content_hash()` values match per `spec_hash` on every
   node — a mismatch would indicate the protocol entered a divergent
   state.

The empirical recovery latency is reported in `PartitionResult::catchup_seconds`.

## What is NOT in scope here

- **Real-network attacks.** The chaos harness intentionally does not
  shape OS network traffic, does not open sockets, does not target
  external peers. If you want to chaos-test on a real network, you
  spin up your own throwaway nodes and use the production binary's
  observability surface — this harness will not help.
- **Verifier collusion in plain text.** V0.2's M-of-N + reputation
  is statistical defence; provable Byzantine fault tolerance is V1+
  work and out of scope for this harness.
- **Real economic Sybil cost.** The Sybil scenario measures structural
  overhead only (CPU + libp2p coordination). The compute + electricity
  + time tax that drives the \$30–80 figure in
  the project notes is out-of-process by definition.

## Output: empirical reports

The `sybil` scenario writes its findings to `results/sybil-empirical-YYYY-MM-DD.md`
when run with `PARSEH_CHAOS_WRITE_RESULTS=1`. The file is checked in
as a placeholder so the path is stable; running the test overwrites
it with fresh numbers.

The partition + malicious-verifier scenarios report through `tracing`
at INFO level; pipe with `RUST_LOG=info` to capture the empirical
numbers.

## Known threshold limits (honest disclosure)

- **RubberStamp ratio 5/9 (~55.6%)**: V0.2 cannot detect a bad
  result above this fraction of rubber-stamp adversaries. The
  rubber-stamps alone satisfy M=5 on the Agreed side, so honest
  re-execution cannot break the false quorum. Mitigation requires
  raising M, raising the reputation-weighted threshold, or adding
  a second-tier auditor — all V0.3+ work.
- **Minority partition of size 2 with M=2 reduced quorum**: cannot
  finalise during partition (correct safety property). The minority
  WAITS — it does not invent results. This is by design.
