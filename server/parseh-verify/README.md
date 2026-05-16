# `parseh-verify`

**V0.2 Primitive 2 — multi-peer verification.**

Given a `JobResult` heard on the network, this crate provides:

- **Selection** — should this node verify? (`selection.rs`)
- **Re-execution** — `Verifier::verify_task` runs the prompt under `VerifierMethod` and compares (`verifier.rs`)
- **Aggregation** — `Quorum` collects M-of-N counter-signatures with reputation-weighted threshold (`quorum.rs`)
- **Method dispatch** — `VerifierMethod::Deterministic` is the only one implemented in V0.2 (`methods.rs`)
- **Rate limiting** — `RateLimit` caps per-node verification frequency (`rate_limit.rs`)

## V0.2 parameters (from `verifier-economics.md`)

| Knob | Standard | Sensitive |
|---|---|---|
| `M` (agreement threshold) | 5 | 9 |
| `N` (target verifiers) | 9 | 15 |
| `t_min` (quorum dwell) | 5 s | 5 s |
| `t_max` (quorum timeout) | 30 s | 30 s |
| `rep_weighted_threshold` | 0.6 | 0.6 |
| `p_base` (selection prob) | 0.05 | 0.05 |
| Rate cap | 10 % / hour / node | 10 % / hour / node |

All pinned in `params` module — single source of truth.

## Test count

**46 unit + integration tests · all passing.**

```bash
cargo test -p parseh-verify --release
```

Covers: Rule 3 (no self-verification), Probationary gate, rate limiting, deterministic match/mismatch with evidence hash, all 4 quorum decisions (Agreed/Disagreed/Disputed/Indeterminate), reputation-weighted-veto blocking thin raw majorities, signature failure rejection, sensitive 9-of-15 quorum.

## Critical design call

**Reputation-weighted veto behaviour** when raw M-of-N is reached but rep-weighted < 0.6: quorum stays open until `t_max`, then promotes to `Disputed` if both sides have ≥⌊M/2⌋ votes else `Indeterminate`. Tested by `reputation_weighted_threshold_blocks_low_rep_majority`.

## V0.2 known limit (cultural rule)

The anti-rubber-stamp defence (honeypot injection) **depends on maintainer-team discipline through V0.2**. This violates the project notes Rule 9 in spirit (centralisation point). Accepted as a known limit because no in-protocol alternative composes cleanly with deterministic-mode inference at V0.2 scale. V0.3+ replaces with protocol-driven on-chain randomised challenges. See `src/methods.rs` module docs.

## Status

✅ Shipped V0.2 · 2026-05-14.

Apache-2.0.
