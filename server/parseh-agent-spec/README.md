# `parseh-agent-spec`

**Signed, content-addressable agent definitions.**

Contributors design agents (prompt template + RAG knowledge refs + exact-input + exact-output JSON Schemas), sign them with their ed25519 key, and the signed definition becomes a content-addressable artefact other peers can reference.

## What's NOT in this crate (deferred per maintainer note)

- Marketplace / investment / trading mechanics — explicitly refused per the project notes §5
- Economic emission for agent use
- Quality-weighted PoW
- Any chain interaction

This crate ships the SCHEMA + types layer. Execution + reputation + reward are explicit V0.3+ work blocked behind the adversarial-testing gate.

## What IS in this crate

- `src/definition.rs` — `AgentDefinition`, `AgentId`, `AgentVersion`, `AgentMetadata`, `ModelRequirements`
- `src/schema_validation.rs` — JSON Schema (Draft 2020-12) for exact-input + exact-output
- `src/knowledge_ref.rs` — `KnowledgeRef` (text corpus / embedding index / structured dataset / upstream-agent-output)
- `src/lineage.rs` — `ParentRef`, `ForkReason` for derivative-work tracking

## Why content-addressable

Two agents with identical fields produce identical `AgentId`. One-bit change → different `AgentId`. This means:

- Anyone can reference an agent by its hash and be sure they're talking about the same thing
- Forks track provenance through `parents: Vec<ParentRef>`
- The "ownership" concept is structural: who signed the definition IS the owner (Rule 8 of the project notes)

## Lineage warning (documented in design doc)

Lineage with multiple parents enables derivative-work attribution that could feed an economic-attribution layer at V0.3+. **The doc explicitly warns: lineage is causal-history metadata, NOT a royalty contract.** Multi-parent merges have no algorithmic "how much value flows to which ancestor?" that doesn't reduce to a policy choice. Any future economic layer reading `parents` needs a separate, governance-reviewed attribution policy.

See the project notes for the full spec + worked example.

## Test count

**35 unit + integration tests · all passing.**

```bash
cargo test -p parseh-agent-spec --release
```

## Status

✅ Shipped V0.2 · 2026-05-14.

Apache-2.0.
