//! Catalog of known-good model URLs + expected sizes + expected hashes.
//!
//! Each entry pins a *specific* GGUF asset on HuggingFace. The
//! `expected_sha256` field is the security primitive: it MUST be filled with
//! a hash measured locally, not copied from the upstream README. In V0.1 the
//! catalog ships a placeholder hash and the downloader will log a mismatch
//! warning on the first download. The real hash gets pinned in a follow-up.

use serde::Serialize;

/// One model entry in the catalog.
///
/// All fields are `&'static str`/`u64` so the catalog can live in `.rodata`
/// and `ModelSpec` stays cheap to clone/serialize. `Deserialize` is *not*
/// derived because `&'static str` cannot be deserialized into — the catalog
/// is hard-coded at compile time, not consumed from the wire.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ModelSpec {
    /// Short stable identifier, used as the CLI/UI key.
    pub name: &'static str,
    /// Human-readable description (size, quantisation, speed estimate).
    pub description: &'static str,
    /// On-disk filename (lives under `~/.parseh/models/`).
    pub filename: &'static str,
    /// HTTPS URL of the GGUF asset on HuggingFace.
    pub url: &'static str,
    /// Expected file size in bytes — used for the on-disk fast path and as a
    /// fallback when the server omits `Content-Length`.
    pub expected_size_bytes: u64,
    /// Lowercase hex SHA-256 of the file. See module docs about the V0.1
    /// placeholder.
    pub expected_sha256: &'static str,
}

/// Marker type that namespaces lookup functions over the static catalog.
pub struct ModelCatalog;

/// V0.1 ships a single recommended model: TinyLlama 1.1B Chat Q4_K_M (~640 MB).
/// Future versions add Phi-3-mini, Llama-3.2-1B, etc.
pub const TINYLLAMA_1_1B_Q4_K_M: ModelSpec = ModelSpec {
    name: "tinyllama-1.1b-chat-q4_k_m",
    description: "TinyLlama 1.1B Chat v1.0 · Q4_K_M quantisation · ~640 MB · ~8 tok/s on modern CPU",
    filename: "tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf",
    url: "https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf",
    expected_size_bytes: 668_788_096,
    // TODO_REAL_HASH — verify on first download with:
    //   curl -L "<url>" -o /tmp/tl.gguf && sha256sum /tmp/tl.gguf
    // then replace this placeholder with the real hash and bump the crate
    // version. The downloader will log a SHA-256 mismatch warning until then.
    expected_sha256: "PLACEHOLDER_REPLACE_WITH_REAL_HASH_ON_FIRST_DOWNLOAD",
};

/// Static list of all models the miner is allowed to fetch. The downloader
/// never accepts a user-supplied URL; every download goes through one of
/// these entries.
pub const MODELS: &[&ModelSpec] = &[&TINYLLAMA_1_1B_Q4_K_M];

impl ModelCatalog {
    /// Look up a model by `name`. Returns `None` if unknown.
    pub fn get(name: &str) -> Option<&'static ModelSpec> {
        MODELS.iter().copied().find(|m| m.name == name)
    }

    /// The model the miner suggests by default when LLM detection finds
    /// nothing and the user grants consent.
    pub fn default_recommended() -> &'static ModelSpec {
        &TINYLLAMA_1_1B_Q4_K_M
    }

    /// Iterate over every model entry, for UI listings.
    pub fn all() -> impl Iterator<Item = &'static ModelSpec> {
        MODELS.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_recommended_is_in_catalog() {
        let rec = ModelCatalog::default_recommended();
        let by_name = ModelCatalog::get(rec.name).expect("default must be in catalog");
        assert_eq!(by_name.url, rec.url);
        assert_eq!(by_name.filename, rec.filename);
    }

    #[test]
    fn catalog_get_unknown_returns_none() {
        assert!(ModelCatalog::get("does-not-exist").is_none());
    }

    #[test]
    fn tinyllama_url_is_https_and_huggingface() {
        let m = ModelCatalog::default_recommended();
        assert!(m.url.starts_with("https://"));
        assert!(m.url.contains("huggingface.co"));
        assert!(m.filename.ends_with(".gguf"));
    }

    #[test]
    fn placeholder_hash_is_flagged() {
        // Documents the V0.1 invariant: the shipped hash is NOT a real
        // SHA-256. When this assertion flips, the placeholder has been
        // replaced and the downloader warning path will go quiet.
        assert!(
            TINYLLAMA_1_1B_Q4_K_M
                .expected_sha256
                .starts_with("PLACEHOLDER_"),
            "if the placeholder has been replaced, update this test and the downloader's warning copy"
        );
    }

    #[test]
    fn all_iterates_every_model() {
        let count = ModelCatalog::all().count();
        assert_eq!(count, MODELS.len());
        assert!(count >= 1);
    }
}
