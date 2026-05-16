//! Knowledge-base references — the "RAG" piece.
//!
//! Each [`KnowledgeRef`] is content-addressable: the actual corpus
//! lives somewhere reachable (IPFS, a libp2p peer, an HTTP mirror)
//! and the agent definition embeds only the SHA-256 hash + a
//! best-effort fetch hint. Two peers running the same agent
//! definition must observe byte-identical knowledge, or the
//! deterministic-mode verifier rejects the result.
//!
//! ## Why content addresses, not URLs
//!
//! - Hash-stable across mirrors. A Persian contributor can host the
//!   corpus on a domestically-reachable mirror (avoiding TLS handshake
//!   fingerprinting against blocked CDNs) without changing the agent's
//!   [`crate::AgentId`].
//! - Verification: a verifier that fetches a different byte stream
//!   under the same hash detects tampering immediately (SHA-256
//!   mismatch).
//! - Privacy: the fetch hint is optional. Peers without it can ask
//!   the network "who has hash X?" via libp2p Kademlia provider
//!   records, avoiding any reveal of which corpus they're loading.

use crate::ContentHash;
use parseh_task::content_hash;
use serde::{Deserialize, Serialize};

// Forward declaration of the agent-id type for circular use across
// the lineage module — we re-export it here just so the
// `UpstreamAgentOutput` variant compiles against the same type.
use crate::definition::{AgentId, AgentVersion};

/// The kind of knowledge being referenced.
///
/// Each variant carries the structural information a verifier needs
/// to confirm "this peer's local copy is the same bytes the agent
/// author signed against."
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KnowledgeKind {
    /// A signed text corpus — e.g. a PDF, a manual, a knowledge base
    /// dump. `encoding` is a hint: `"utf-8"`, `"pdf"`, `"markdown"`.
    /// Not enforced by this crate; the executor interprets it.
    TextCorpus {
        /// SHA-256 of the corpus bytes (after any documented
        /// decoding step — see `encoding`).
        content_hash: ContentHash,
        /// Format hint. Plain string; common values: `"utf-8"`,
        /// `"markdown"`, `"pdf"`, `"html"`.
        encoding: String,
    },
    /// A pre-built vector embedding index. The `model_name` field
    /// pins the embedding model: two indexes built with different
    /// embedders have different bytes and so different hashes, but
    /// authors who change embedders for an "improved" agent fork
    /// must declare so explicitly in lineage.
    EmbeddingIndex {
        /// SHA-256 of the on-disk index.
        content_hash: ContentHash,
        /// Embedding model identifier — e.g. `"bge-small-en-v1.5"`.
        model_name: String,
        /// Vector dimension. Stored separately for cheap filtering
        /// before fetching the (potentially large) index.
        dimension: u32,
    },
    /// A structured dataset — e.g. a JSON document, CSV file, or
    /// SQLite-export. `schema_hash` lets verifiers confirm both the
    /// data AND the schema match. Schema-vs-data drift is a common
    /// silent bug in RAG pipelines; surfacing it here is cheap.
    StructuredDataset {
        /// SHA-256 of the dataset bytes.
        content_hash: ContentHash,
        /// SHA-256 of the dataset's schema document.
        schema_hash: ContentHash,
    },
    /// A chained-agent reference. The agent reads another agent's
    /// output as input. The receiving network is responsible for
    /// detecting upstream cycles (a hard problem in general; for
    /// now we recommend depth-limited static analysis on the
    /// receiving side — V0.3+ work to formalise).
    UpstreamAgentOutput {
        /// The upstream agent's content hash.
        agent_id: AgentId,
        /// Version of the upstream agent. Pinning is important —
        /// upgrading the upstream silently would change this
        /// agent's behaviour without changing its own definition.
        version: AgentVersion,
    },
}

/// A single knowledge-base reference, with fetch metadata for
/// verifier bandwidth planning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeRef {
    /// What is being referenced.
    pub kind: KnowledgeKind,
    /// Optional fetch hint. Format-free string — common values:
    /// IPFS CID (`ipfs://...`), HTTP URL, libp2p multiaddr
    /// (`/ip4/.../p2p/...`). Verifiers MAY ignore this and use
    /// content-addressed discovery (Kademlia provider records) when
    /// the hint is blocked, missing, or untrusted.
    ///
    /// Privacy / Persian-contributor consideration: leaving this
    /// `None` lets the verifier choose its own route — useful when
    /// the author's fetch URL is sanctions-blocked or
    /// fingerprintable.
    pub fetch_hint: Option<String>,
    /// Expected size in bytes. Verifiers use this to budget
    /// bandwidth before committing to a fetch — large knowledge
    /// refs may steer the verifier away from accepting the agent's
    /// task at all (M-of-N quorums tolerate this).
    pub size_bytes: u64,
}

impl KnowledgeRef {
    /// Helper: build a text-corpus reference from raw bytes, computing
    /// the content hash automatically.
    pub fn from_text_bytes(bytes: &[u8], encoding: impl Into<String>) -> Self {
        Self {
            kind: KnowledgeKind::TextCorpus {
                content_hash: content_hash(bytes),
                encoding: encoding.into(),
            },
            fetch_hint: None,
            size_bytes: bytes.len() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{from_cbor_bytes, to_cbor_bytes};

    #[test]
    fn knowledge_ref_roundtrip_through_cbor() {
        let r = KnowledgeRef::from_text_bytes(
            "the quick brown fox jumps over the lazy dog".as_bytes(),
            "utf-8",
        );
        let bytes = to_cbor_bytes(&r).unwrap();
        let back: KnowledgeRef = from_cbor_bytes(&bytes).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn embedding_index_variant_roundtrip() {
        let r = KnowledgeRef {
            kind: KnowledgeKind::EmbeddingIndex {
                content_hash: content_hash(b"fake index bytes"),
                model_name: "bge-small-en-v1.5".into(),
                dimension: 384,
            },
            fetch_hint: Some("ipfs://bafy...example".into()),
            size_bytes: 12_345_678,
        };
        let bytes = to_cbor_bytes(&r).unwrap();
        let back: KnowledgeRef = from_cbor_bytes(&bytes).unwrap();
        assert_eq!(r, back);
    }
}
