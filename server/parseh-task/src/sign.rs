//! Ed25519 signing helpers, kept minimal on purpose.
//!
//! All four wire types implement the same pattern: sign the CBOR
//! encoding of the struct with the `signature` field empty, then write
//! the resulting bytes into the field. Verification mirrors the process
//! — clear, re-encode, verify.
//!
//! We deliberately do **not** wrap key material here. Callers pass in
//! `&SigningKey` / `&VerifyingKey` directly because in the wider stack
//! these keys are owned by `parseh-miner` (loaded from disk on startup)
//! and there is no value in re-wrapping them inside this leaf crate.

use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey, SigningKey};
use thiserror::Error;

/// Errors surfaced by [`verify_bytes`] and the higher-level
/// `verify_signature()` methods on the wire types.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum SignError {
    /// The signature bytes parsed as a `Signature`, but the cryptographic
    /// check failed against the supplied public key / message.
    #[error("verify failed: {0}")]
    Verify(String),
    /// The signature byte slice was not 64 bytes long, or otherwise not
    /// a valid ed25519 signature serialisation.
    #[error("invalid signature bytes")]
    InvalidSignatureBytes,
    /// The public key byte slice was not 32 bytes long, or otherwise not
    /// a valid ed25519 verifying key serialisation.
    #[error("invalid public key bytes")]
    InvalidPubkeyBytes,
}

/// Sign `bytes` with `signing_key`, returning the 64-byte signature.
///
/// Pure function — no I/O, no allocation beyond the return value.
pub fn sign_bytes(signing_key: &SigningKey, bytes: &[u8]) -> [u8; 64] {
    signing_key.sign(bytes).to_bytes()
}

/// Verify a 64-byte signature `sig_bytes` over `bytes` against
/// `verifying_key`. Returns `Ok(())` on success, [`SignError`] on
/// failure.
pub fn verify_bytes(
    verifying_key: &VerifyingKey,
    bytes: &[u8],
    sig_bytes: &[u8],
) -> Result<(), SignError> {
    let sig =
        Signature::from_slice(sig_bytes).map_err(|_| SignError::InvalidSignatureBytes)?;
    verifying_key
        .verify(bytes, &sig)
        .map_err(|e| SignError::Verify(e.to_string()))
}

/// Parse a 32-byte public-key slice into a `VerifyingKey`, mapping the
/// failure into a `SignError`. Provided as a convenience for callers
/// that hold pubkeys as `Vec<u8>` (e.g. from CBOR-decoded wire types).
pub fn verifying_key_from_bytes(bytes: &[u8]) -> Result<VerifyingKey, SignError> {
    let arr: &[u8; 32] = bytes.try_into().map_err(|_| SignError::InvalidPubkeyBytes)?;
    VerifyingKey::from_bytes(arr).map_err(|_| SignError::InvalidPubkeyBytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    #[test]
    fn sign_then_verify_succeeds() {
        let k = fresh_key();
        let msg = b"PARSEH integration test";
        let sig = sign_bytes(&k, msg);
        verify_bytes(&k.verifying_key(), msg, &sig).expect("verify");
    }

    #[test]
    fn verify_fails_when_message_changes() {
        let k = fresh_key();
        let sig = sign_bytes(&k, b"original");
        let err = verify_bytes(&k.verifying_key(), b"tampered", &sig).unwrap_err();
        assert!(matches!(err, SignError::Verify(_)));
    }

    #[test]
    fn verify_fails_when_signature_truncated() {
        let k = fresh_key();
        let sig = sign_bytes(&k, b"msg");
        let truncated = &sig[..32];
        let err = verify_bytes(&k.verifying_key(), b"msg", truncated).unwrap_err();
        assert_eq!(err, SignError::InvalidSignatureBytes);
    }

    #[test]
    fn verify_fails_when_signed_by_a_different_key() {
        let signer = fresh_key();
        let imposter = fresh_key();
        let sig = sign_bytes(&signer, b"msg");
        let err = verify_bytes(&imposter.verifying_key(), b"msg", &sig).unwrap_err();
        assert!(matches!(err, SignError::Verify(_)));
    }

    #[test]
    fn verifying_key_from_bytes_rejects_short_input() {
        let err = verifying_key_from_bytes(&[0u8; 16]).unwrap_err();
        assert_eq!(err, SignError::InvalidPubkeyBytes);
    }
}
