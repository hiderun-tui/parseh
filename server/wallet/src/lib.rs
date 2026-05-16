//! `parseh-wallet` — PARSEH on-chain wallet primitives.
//!
//! Provides:
//!   - ed25519 keypair generation
//!   - bech32 PARSEH address derivation (HRP `parseh`)
//!   - tx signing + verification helpers
//!
//! Planned for later milestones:
//!   - encrypted on-disk persistence of signing keys
//!   - tx construction for MsgRegisterProvider / MsgSubmitJob / etc.
//!   - balance + tx-history queries against a chain RPC endpoint
//!   - NFC + QR primitives (used by the Hiderun mobile clients via UniFFI)

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;

use bech32::{Bech32, Hrp};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

/// A 32-byte address derived from an ed25519 public key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address(pub [u8; 32]);

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hrp = Hrp::parse(BECH32_HRP).expect("BECH32_HRP is a valid bech32 prefix");
        let encoded = bech32::encode::<Bech32>(hrp, &self.0)
            .expect("bech32 encoding of a 32-byte payload cannot fail");
        f.write_str(&encoded)
    }
}

/// Bech32 prefix used for human-readable PARSEH addresses (e.g. `parseh1…`).
pub const BECH32_HRP: &str = "parseh";

/// Wallet errors.
#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    /// Signing key is missing or could not be loaded.
    #[error("wallet: no signing key")]
    NoKey,
    /// Chain RPC returned an error.
    #[error("wallet: rpc: {0}")]
    Rpc(String),
    /// Bech32 decoding failed (bad string, bad checksum, etc.).
    #[error("wallet: bech32 decode: {0}")]
    Bech32(String),
    /// Bech32 string did not carry the expected payload length.
    #[error("wallet: invalid payload length: expected {expected}, got {got}")]
    InvalidLength {
        /// Expected number of bytes after decoding.
        expected: usize,
        /// Actual number of bytes decoded.
        got: usize,
    },
}

/// Wallet version string (compile-time crate version).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// An ed25519 keypair used to sign PARSEH chain transactions.
///
/// Wraps `ed25519_dalek::SigningKey`; the verifying (public) key is derived
/// on demand. The 32-byte verifying key is what gets encoded as a `parseh1…`
/// bech32 address.
pub struct Keypair {
    signing_key: SigningKey,
}

impl Keypair {
    /// Generate a fresh keypair using the OS CSPRNG.
    pub fn generate() -> Self {
        let mut rng = OsRng;
        let signing_key = SigningKey::generate(&mut rng);
        Self { signing_key }
    }

    /// Sign `msg` with this keypair, returning the 64-byte ed25519 signature.
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        let sig: Signature = self.signing_key.sign(msg);
        sig.to_bytes()
    }

    /// Verify a signature against a 32-byte verifying key + message.
    ///
    /// Returns `true` iff the signature is well-formed *and* valid for
    /// `(verifying_key, msg)`. All failure modes (bad key bytes, malformed
    /// signature, wrong message) collapse to `false`.
    pub fn verify(verifying_key: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
        let Ok(vk) = VerifyingKey::from_bytes(verifying_key) else {
            return false;
        };
        let signature = Signature::from_bytes(sig);
        vk.verify(msg, &signature).is_ok()
    }

    /// Return the 32-byte verifying (public) key for this keypair.
    pub fn public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    /// Encode the *public* key as a bech32 string with the given HRP.
    ///
    /// For PARSEH addresses, callers should pass [`BECH32_HRP`].
    pub fn to_bech32(&self, hrp: &str) -> String {
        let hrp = Hrp::parse(hrp).expect("caller-supplied HRP must be a valid bech32 prefix");
        let pk = self.public_key();
        bech32::encode::<Bech32>(hrp, &pk)
            .expect("bech32 encoding of a 32-byte payload cannot fail")
    }

    /// Decode a `parseh1…` bech32 string back into a 32-byte public key.
    ///
    /// The HRP is not checked against [`BECH32_HRP`] here — callers that
    /// care about the prefix should compare it separately. We do enforce
    /// that the decoded payload is exactly 32 bytes.
    pub fn from_bech32(s: &str) -> Result<[u8; 32], WalletError> {
        let (_hrp, data) =
            bech32::decode(s).map_err(|e| WalletError::Bech32(e.to_string()))?;
        let got = data.len();
        let bytes: [u8; 32] = data.try_into().map_err(|_| WalletError::InvalidLength {
            expected: 32,
            got,
        })?;
        Ok(bytes)
    }
}

impl fmt::Debug for Keypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately do not print the signing key bytes.
        f.debug_struct("Keypair")
            .field("public_key", &hex_short(&self.public_key()))
            .finish()
    }
}

fn hex_short(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(2 * 4 + 1);
    for b in &bytes[..4] {
        s.push_str(&format!("{:02x}", b));
    }
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_sign_verify() {
        let kp = Keypair::generate();
        let msg = b"hello world";
        let sig = kp.sign(msg);
        let pk = kp.public_key();
        assert!(Keypair::verify(&pk, msg, &sig));
    }

    #[test]
    fn wrong_message_fails_verify() {
        let kp = Keypair::generate();
        let sig = kp.sign(b"a");
        let pk = kp.public_key();
        assert!(!Keypair::verify(&pk, b"b", &sig));
    }

    #[test]
    fn wrong_key_fails_verify() {
        let kp_a = Keypair::generate();
        let kp_b = Keypair::generate();
        let msg = b"sign me";
        let sig = kp_a.sign(msg);
        let pk_b = kp_b.public_key();
        assert!(!Keypair::verify(&pk_b, msg, &sig));
    }

    #[test]
    fn roundtrip_bech32() {
        let kp = Keypair::generate();
        let pk = kp.public_key();
        let encoded = kp.to_bech32(BECH32_HRP);
        let decoded = Keypair::from_bech32(&encoded).expect("decode must succeed");
        assert_eq!(decoded, pk);
    }

    #[test]
    fn address_format() {
        // 32-byte payload in bech32 = 52 base32 chars; with HRP "parseh" (6)
        // + separator '1' (1) + checksum (6) → 65 chars total.
        let kp = Keypair::generate();
        let encoded = kp.to_bech32(BECH32_HRP);
        assert!(
            encoded.starts_with("parseh1"),
            "expected `parseh1` prefix, got {encoded}"
        );
        assert_eq!(
            encoded.len(),
            65,
            "expected 65 chars (parseh1 + 52 data + 6 checksum), got {} in {encoded}",
            encoded.len()
        );
    }

    #[test]
    fn invalid_bech32_returns_err() {
        assert!(Keypair::from_bech32("not-a-valid-address").is_err());
    }

    #[test]
    fn address_display_uses_bech32() {
        let kp = Keypair::generate();
        let addr = Address(kp.public_key());
        let s = addr.to_string();
        assert!(s.starts_with("parseh1"));
        let decoded = Keypair::from_bech32(&s).expect("Display output must round-trip");
        assert_eq!(decoded, kp.public_key());
    }
}
