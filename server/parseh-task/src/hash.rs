//! Content addressing — SHA-256 over CBOR-encoded bytes.
//!
//! The hash is the **only** identity a message needs on the wire: two
//! peers that received byte-identical CBOR will compute the same
//! [`ContentHash`] without exchanging any extra metadata. The signature
//! that lives inside the struct is then bound to that identity by
//! construction — a tampered byte changes the hash, which changes the
//! signature input, which fails verification.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// SHA-256 content hash. 32 raw bytes; hex-encoded for human display.
///
/// `Serialize` / `Deserialize` use `serde_bytes` so the encoded form
/// is a CBOR byte string (`major type 2`) rather than a 32-element
/// array of small integers — which would be both larger on the wire
/// and slightly slower to decode.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(#[serde(with = "serde_bytes_array")] pub [u8; 32]);

impl ContentHash {
    /// Raw 32-byte hash.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lower-case hex encoding (64 chars).
    pub fn as_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Zero hash. Useful as a placeholder before signing — `Default` is
    /// also implemented.
    pub const fn zero() -> Self {
        Self([0u8; 32])
    }
}

impl Default for ContentHash {
    fn default() -> Self {
        Self::zero()
    }
}

impl std::fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Show only the first 16 hex chars (8 bytes) — enough to spot
        // mismatches at a glance, short enough to fit in log lines.
        write!(f, "ContentHash({})", &self.as_hex()[..16])
    }
}

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_hex())
    }
}

impl From<[u8; 32]> for ContentHash {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

/// Deterministic content hash over CBOR-encoded bytes.
///
/// Two byte-identical CBOR encodings produce the same hash; this is what
/// makes the wire format content-addressable.
pub fn content_hash(cbor_bytes: &[u8]) -> ContentHash {
    let mut hasher = Sha256::new();
    hasher.update(cbor_bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    ContentHash(out)
}

/// Adapter so `serde_bytes` can target a `[u8; 32]` array (it only
/// natively supports `Vec<u8>` / `&[u8]`). We keep the wire form as a
/// fixed-size byte string and reject anything else on decode.
mod serde_bytes_array {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::Bytes::new(bytes).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let v: serde_bytes::ByteBuf = serde_bytes::ByteBuf::deserialize(d)?;
        let bytes = v.into_vec();
        if bytes.len() != 32 {
            return Err(serde::de::Error::invalid_length(
                bytes.len(),
                &"32-byte SHA-256 digest",
            ));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_hash_matches_sha256_empty_string() {
        // NIST FIPS 180-4 test vector for SHA-256("") =
        // e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let h = content_hash(&[]);
        assert_eq!(
            h.as_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn debug_is_truncated_display_is_full() {
        let h = content_hash(b"hello");
        let dbg = format!("{:?}", h);
        let disp = format!("{}", h);
        assert!(dbg.contains("ContentHash("));
        assert!(dbg.len() < disp.len());
        assert_eq!(disp.len(), 64);
    }

    #[test]
    fn one_bit_change_produces_different_hash() {
        let a = content_hash(b"hello");
        let b = content_hash(b"hellp"); // one-bit flip
        assert_ne!(a, b);
    }

    #[test]
    fn roundtrip_through_cbor() {
        let h = content_hash(b"some payload");
        let bytes = crate::to_cbor_bytes(&h).unwrap();
        let back: ContentHash = crate::from_cbor_bytes(&bytes).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn zero_hash_default_matches() {
        assert_eq!(ContentHash::zero(), ContentHash::default());
        assert_eq!(ContentHash::zero().as_hex(), "0".repeat(64));
    }
}
