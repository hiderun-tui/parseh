//! Key material handling for SQLCipher-encrypted SQLite.
//!
//! SQLCipher needs a 256-bit key. We derive that key one of three ways
//! depending on the deployment context:
//!
//! - [`KeySource::Passphrase`] — Argon2id of a user-supplied phrase
//!   plus a stored salt. This is the **recommended** path for any
//!   sensitive deployment.
//! - [`KeySource::IdentityFile`] — domain-separated SHA-256 over the
//!   raw bytes of the libp2p identity file. Useful while V0.2 lacks a
//!   passphrase UI; **does not protect against an attacker with
//!   read-access to the file system**.
//! - [`KeySource::Raw`] — direct 32-byte key. For tests only — there
//!   is no UX for a user to type 32 random bytes.
//!
//! All key material is wrapped in [`zeroize::Zeroizing`] so the bytes
//! are scrubbed from memory on drop. This is a defence-in-depth measure
//! (the bytes can still leak through swap, cores, or live process
//! introspection); the primary protection is encryption-at-rest.

use anyhow::anyhow;
use argon2::{Algorithm, Argon2, Params, Version};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// Domain-separation prefix mixed into [`KeySource::IdentityFile`] key
/// derivation. Prevents the same identity file from yielding the same
/// key bytes used by a different PARSEH subsystem (none today, but the
/// hygiene is cheap).
pub const IDENTITY_KEY_DOMAIN: &[u8] = b"parseh-shared-state-v1";

/// Argon2id parameters used by [`KeySource::Passphrase`].
///
/// 256 MiB memory · 3 iterations · parallelism 2 · 32-byte output.
///
/// These are deliberately on the heavier side: shared-state DBs are
/// long-lived and a user only pays this cost once at open time. If you
/// need to derive a key inside a CI loop, use [`KeySource::Raw`] for
/// the test and write a separate test that exercises this path on its
/// own.
pub const ARGON2_MEMORY_KIB: u32 = 256 * 1024;
/// Number of Argon2 iterations.
pub const ARGON2_ITERATIONS: u32 = 3;
/// Argon2 parallelism factor.
pub const ARGON2_PARALLELISM: u32 = 2;
/// Output length of the derived key, in bytes.
pub const KEY_BYTES: usize = 32;

/// Source of the 256-bit SQLCipher key.
pub enum KeySource {
    /// Argon2id-derived from a user-supplied passphrase. RECOMMENDED.
    Passphrase {
        /// The passphrase. Zeroized when this variant is dropped.
        phrase: Zeroizing<String>,
        /// A salt — must be stable across opens of the same database,
        /// and unique across databases. Treat as a configuration value:
        /// generate once at db init and persist in a sibling file
        /// (e.g. `shared-state.salt`).
        salt: Vec<u8>,
    },
    /// Derived from the local libp2p identity file via SHA-256. This
    /// is **strictly weaker** than `Passphrase`: anyone who can read
    /// the identity file can derive the database key. Useful for V0.2
    /// dev only — a passphrase UI is a V0.3 prerequisite for any
    /// production deployment that handles sensitive prompts.
    IdentityFile {
        /// Raw bytes of the libp2p identity file. Zeroized when this
        /// variant is dropped.
        identity_bytes: Zeroizing<Vec<u8>>,
    },
    /// Direct 32-byte key — for tests only.
    Raw([u8; KEY_BYTES]),
}

/// A 32-byte SQLCipher key, scrubbed from memory on drop.
///
/// We expose the bytes via [`KeyMaterial::as_bytes`] rather than a
/// public field so callers cannot accidentally clone the key away from
/// the zeroizing wrapper.
pub struct KeyMaterial(Zeroizing<[u8; KEY_BYTES]>);

impl Clone for KeyMaterial {
    fn clone(&self) -> Self {
        // `Zeroizing<[u8; N]>` does not implement Clone; copy the
        // inner bytes through a fresh `Zeroizing` so each `KeyMaterial`
        // owns an independently-zeroized buffer.
        let mut out = Zeroizing::new([0u8; KEY_BYTES]);
        out.copy_from_slice(self.0.as_ref());
        Self(out)
    }
}

impl std::fmt::Debug for KeyMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the actual bytes.
        f.write_str("KeyMaterial([REDACTED; 32])")
    }
}

impl KeyMaterial {
    /// Construct from a [`KeySource`].
    pub fn from_source(source: KeySource) -> anyhow::Result<Self> {
        match source {
            KeySource::Passphrase { phrase, salt } => {
                if salt.len() < 8 {
                    return Err(anyhow!(
                        "Argon2id salt must be at least 8 bytes (got {})",
                        salt.len()
                    ));
                }
                let params = Params::new(
                    ARGON2_MEMORY_KIB,
                    ARGON2_ITERATIONS,
                    ARGON2_PARALLELISM,
                    Some(KEY_BYTES),
                )
                .map_err(|e| anyhow!("argon2 params: {e}"))?;
                let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
                let mut key = Zeroizing::new([0u8; KEY_BYTES]);
                argon
                    .hash_password_into(phrase.as_bytes(), &salt, key.as_mut())
                    .map_err(|e| anyhow!("argon2 derive: {e}"))?;
                Ok(KeyMaterial(key))
            }
            KeySource::IdentityFile { identity_bytes } => {
                if identity_bytes.is_empty() {
                    return Err(anyhow!("identity file bytes empty"));
                }
                let mut hasher = Sha256::new();
                hasher.update(IDENTITY_KEY_DOMAIN);
                hasher.update(identity_bytes.as_slice());
                let digest = hasher.finalize();
                let mut key = Zeroizing::new([0u8; KEY_BYTES]);
                key.copy_from_slice(&digest);
                Ok(KeyMaterial(key))
            }
            KeySource::Raw(bytes) => Ok(KeyMaterial(Zeroizing::new(bytes))),
        }
    }

    /// Borrow the 32-byte key. Be careful not to copy this slice into a
    /// non-zeroizing container.
    pub fn as_bytes(&self) -> &[u8; KEY_BYTES] {
        &self.0
    }

    /// Lowercase hex form for SQLCipher's `PRAGMA key = "x'...'"`
    /// statement. Returns inside a [`Zeroizing`] so the hex form is
    /// also scrubbed.
    pub fn to_sqlcipher_hex(&self) -> Zeroizing<String> {
        // `hex::encode` is allocation-aware; the resulting String is
        // immediately wrapped in `Zeroizing` so it is scrubbed on drop.
        Zeroizing::new(hex::encode(self.0.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_key_round_trips() {
        let raw = [7u8; KEY_BYTES];
        let km = KeyMaterial::from_source(KeySource::Raw(raw)).unwrap();
        assert_eq!(km.as_bytes(), &raw);
    }

    #[test]
    fn identity_file_key_is_deterministic() {
        let bytes = Zeroizing::new(b"some libp2p key bytes".to_vec());
        let a = KeyMaterial::from_source(KeySource::IdentityFile {
            identity_bytes: bytes.clone(),
        })
        .unwrap();
        let b = KeyMaterial::from_source(KeySource::IdentityFile {
            identity_bytes: bytes,
        })
        .unwrap();
        assert_eq!(a.as_bytes(), b.as_bytes());
    }

    #[test]
    fn identity_file_key_changes_with_input() {
        let a = KeyMaterial::from_source(KeySource::IdentityFile {
            identity_bytes: Zeroizing::new(b"alpha".to_vec()),
        })
        .unwrap();
        let b = KeyMaterial::from_source(KeySource::IdentityFile {
            identity_bytes: Zeroizing::new(b"beta".to_vec()),
        })
        .unwrap();
        assert_ne!(a.as_bytes(), b.as_bytes());
    }

    #[test]
    fn empty_identity_file_rejected() {
        let r = KeyMaterial::from_source(KeySource::IdentityFile {
            identity_bytes: Zeroizing::new(Vec::new()),
        });
        assert!(r.is_err());
    }

    #[test]
    fn too_short_salt_rejected() {
        let r = KeyMaterial::from_source(KeySource::Passphrase {
            phrase: Zeroizing::new("secret".to_string()),
            salt: vec![0u8; 4],
        });
        assert!(r.is_err());
    }

    #[test]
    fn debug_does_not_leak_bytes() {
        let km = KeyMaterial::from_source(KeySource::Raw([0xAB; KEY_BYTES])).unwrap();
        let s = format!("{:?}", km);
        assert!(s.contains("REDACTED"));
        assert!(!s.contains("ab"));
    }

    #[test]
    fn hex_form_is_64_chars() {
        let km = KeyMaterial::from_source(KeySource::Raw([0xCD; KEY_BYTES])).unwrap();
        let hex = km.to_sqlcipher_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(&hex[..2], "cd");
    }
}
