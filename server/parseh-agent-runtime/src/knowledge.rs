//! RAG knowledge resolution.
//!
//! An [`parseh_agent_spec::AgentDefinition`] carries
//! `knowledge_refs: Vec<KnowledgeRef>`. To execute the agent we must
//! materialise those refs into bytes the prompt can embed.
//!
//! ## V0.2.5 scope (honest)
//!
//! - [`KnowledgeKind::TextCorpus`] is resolved from a **local
//!   content-addressed cache** at `~/.parseh/knowledge/<sha256-hex>`.
//!   The cached bytes are SHA-256-checked against the ref's
//!   `content_hash`; a mismatch is a hard error (a verifier must
//!   observe byte-identical knowledge or the deterministic-mode result
//!   is meaningless).
//! - If the corpus is not in the local cache, we return a clear
//!   [`KnowledgeError::NotInLocalCache`]. **Network fetch of knowledge
//!   is V0.3+** — this crate does not reach out to IPFS / libp2p /
//!   HTTP mirrors. The honest reason: a fetch path that silently
//!   pulled different bytes under the same hash would be a verification
//!   hole, and the secure fetch+verify pipeline is its own piece of
//!   work.
//! - [`KnowledgeKind::EmbeddingIndex`],
//!   [`KnowledgeKind::StructuredDataset`], and
//!   [`KnowledgeKind::UpstreamAgentOutput`] return typed
//!   `*Unsupported` errors, documented as V0.3+. (Workflow chaining —
//!   the executor-side analogue of `UpstreamAgentOutput` — is provided
//!   by [`crate::Workflow`], which wires step outputs explicitly rather
//!   than through a knowledge ref.)

use parseh_agent_spec::{KnowledgeKind, KnowledgeRef};
use parseh_task::ContentHash;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use thiserror::Error;

/// Errors raised while resolving knowledge refs.
#[derive(Error, Debug)]
pub enum KnowledgeError {
    /// The corpus is not present in the local content-addressed cache.
    /// Network fetch is V0.3+.
    #[error("knowledge corpus {0} not in local cache (~/.parseh/knowledge/); network fetch is V0.3+")]
    NotInLocalCache(String),
    /// The cached bytes did not hash to the ref's declared hash.
    #[error("knowledge corpus {expected} hash mismatch: cached bytes hash to {actual}")]
    HashMismatch {
        /// Hash declared by the agent definition.
        expected: String,
        /// Hash of the bytes actually found in the cache.
        actual: String,
    },
    /// Reading the cache file failed (permissions, I/O).
    #[error("reading cached corpus failed: {0}")]
    Io(String),
    /// The home directory could not be located.
    #[error("could not locate home directory for ~/.parseh/knowledge/")]
    NoHomeDir,
    /// An embedding-index ref was encountered. V0.3+.
    #[error("EmbeddingIndex knowledge refs are not supported in V0.2.5 (V0.3+)")]
    EmbeddingIndexUnsupported,
    /// A structured-dataset ref was encountered. V0.3+.
    #[error("StructuredDataset knowledge refs are not supported in V0.2.5 (V0.3+)")]
    StructuredDatasetUnsupported,
    /// An upstream-agent-output ref was encountered. Use [`crate::Workflow`].
    #[error("UpstreamAgentOutput refs are not resolved here — compose agents via Workflow (V0.3+ for ref-style chaining)")]
    UpstreamAgentOutputUnsupported,
}

/// A single resolved knowledge corpus, ready to inject into the prompt
/// context under the reserved `knowledge` key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedKnowledge {
    /// SHA-256 hex of the corpus (matches the ref's declared hash).
    pub content_hash_hex: String,
    /// Format hint copied from the ref (e.g. `"utf-8"`, `"markdown"`).
    pub encoding: String,
    /// The corpus text. We only resolve text corpora in V0.2.5, so
    /// this is always valid UTF-8 (lossily decoded if the cache file
    /// contained invalid bytes — the hash check already guarantees the
    /// bytes are the author's, so lossy decode is a display concern,
    /// not a verification one).
    pub text: String,
}

/// Resolve every knowledge ref from the local cache.
///
/// `cache_root` is the content-addressed cache directory; pass `None`
/// to use the default `~/.parseh/knowledge/`. Returns the resolved
/// corpora in ref order so the executor can inject them deterministically.
pub fn resolve_knowledge(
    refs: &[KnowledgeRef],
    cache_root: Option<PathBuf>,
) -> Result<Vec<ResolvedKnowledge>, KnowledgeError> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let root = match cache_root {
        Some(r) => r,
        None => default_cache_root()?,
    };
    let mut out = Vec::with_capacity(refs.len());
    for r in refs {
        out.push(resolve_one(r, &root)?);
    }
    Ok(out)
}

/// Default cache root: `~/.parseh/knowledge/`.
fn default_cache_root() -> Result<PathBuf, KnowledgeError> {
    let home = dirs::home_dir().ok_or(KnowledgeError::NoHomeDir)?;
    Ok(home.join(".parseh").join("knowledge"))
}

fn resolve_one(
    r: &KnowledgeRef,
    root: &std::path::Path,
) -> Result<ResolvedKnowledge, KnowledgeError> {
    match &r.kind {
        KnowledgeKind::TextCorpus {
            content_hash,
            encoding,
        } => {
            let hex = content_hash.as_hex();
            let path = root.join(&hex);
            if !path.exists() {
                return Err(KnowledgeError::NotInLocalCache(hex));
            }
            let bytes = std::fs::read(&path).map_err(|e| KnowledgeError::Io(e.to_string()))?;
            let actual = sha256_hex(&bytes);
            if actual != hex {
                return Err(KnowledgeError::HashMismatch {
                    expected: hex,
                    actual,
                });
            }
            Ok(ResolvedKnowledge {
                content_hash_hex: hex,
                encoding: encoding.clone(),
                text: String::from_utf8_lossy(&bytes).into_owned(),
            })
        }
        KnowledgeKind::EmbeddingIndex { .. } => {
            Err(KnowledgeError::EmbeddingIndexUnsupported)
        }
        KnowledgeKind::StructuredDataset { .. } => {
            Err(KnowledgeError::StructuredDatasetUnsupported)
        }
        KnowledgeKind::UpstreamAgentOutput { .. } => {
            Err(KnowledgeError::UpstreamAgentOutputUnsupported)
        }
    }
}

/// SHA-256 → lower-case hex, matching [`ContentHash::as_hex`].
fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&d);
    ContentHash(out).as_hex()
}

#[cfg(test)]
mod tests {
    use super::*;
    use parseh_agent_spec::KnowledgeRef;
    use tempfile::tempdir;

    #[test]
    fn resolves_text_corpus_from_cache() {
        let dir = tempdir().unwrap();
        let body = b"the quick brown fox";
        let r = KnowledgeRef::from_text_bytes(body, "utf-8");
        let hex = match &r.kind {
            KnowledgeKind::TextCorpus { content_hash, .. } => content_hash.as_hex(),
            _ => unreachable!(),
        };
        std::fs::write(dir.path().join(&hex), body).unwrap();
        let resolved =
            resolve_knowledge(std::slice::from_ref(&r), Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].text, "the quick brown fox");
        assert_eq!(resolved[0].content_hash_hex, hex);
    }

    #[test]
    fn missing_corpus_is_clear_error() {
        let dir = tempdir().unwrap();
        let r = KnowledgeRef::from_text_bytes(b"absent", "utf-8");
        let err = resolve_knowledge(
            std::slice::from_ref(&r),
            Some(dir.path().to_path_buf()),
        )
        .unwrap_err();
        assert!(matches!(err, KnowledgeError::NotInLocalCache(_)));
    }

    #[test]
    fn tampered_cache_file_is_hash_mismatch() {
        let dir = tempdir().unwrap();
        let r = KnowledgeRef::from_text_bytes(b"original", "utf-8");
        let hex = match &r.kind {
            KnowledgeKind::TextCorpus { content_hash, .. } => content_hash.as_hex(),
            _ => unreachable!(),
        };
        std::fs::write(dir.path().join(&hex), b"TAMPERED").unwrap();
        let err = resolve_knowledge(
            std::slice::from_ref(&r),
            Some(dir.path().to_path_buf()),
        )
        .unwrap_err();
        assert!(matches!(err, KnowledgeError::HashMismatch { .. }));
    }
}
