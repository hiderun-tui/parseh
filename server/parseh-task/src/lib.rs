//! `parseh-task` — V0.2 Primitive 1 (Contribution Layer).
//!
//! Provides the signed task abstraction that other coordination primitives
//! ([`parseh-verify`], [`parseh-shared-state`]) build on. **No business
//! logic** lives here — this crate is types, serde, ed25519 signing,
//! content hashing, and tests.
//!
//! ## Wire format
//!
//! CBOR over libp2p `request-response` (`/parseh/job/2.0.0`) for direct
//! submission, and CBOR over gossipsub (`parseh.tasks.v1`) for
//! capability-fanout announcements. Each of the four core types —
//! [`JobSpec`], [`JobResult`], [`JobVerification`], [`JobOutcome`] — is
//! signed by its author and uniquely content-addressed via [`ContentHash`].
//!
//! The [`StateSyncRequest`] / [`StateSyncResponse`] pair (module
//! [`state_sync`]) carries the `/parseh/state-sync/1.0.0`
//! request-response protocol that closes the partition-recovery gap the
//! chaos harness surfaced. See the project notes.
//!
//! ## Signing convention
//!
//! The signature field on each top-level type is computed over the CBOR
//! encoding of the struct *with the signature field present but empty*
//! (i.e. zero-length byte string). To verify, a peer clears the signature
//! field, re-encodes, and checks the bytes. This is the same trick used
//! by Cosmos SDK transactions and avoids relying on a separate canonical
//! serialisation pass — `ciborium`'s output is already deterministic for
//! the value shapes we use here (no maps with non-string keys, no
//! floating-point variants except the well-defined `f64` in
//! `OutcomeVerdict::Valid`).
//!
//! ## What this crate intentionally does NOT do
//!
//! - It does not orchestrate verification (that is `parseh-verify`).
//! - It does not persist anything (that is `parseh-shared-state`).
//! - It does not publish on gossipsub (that is `parseh-miner`).
//! - It does not interpret `JobInputs.prompt_text` (that is the executor).
//!
//! See the project notes §3.1 and
//! the project notes §3.1.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod hash;
mod outcome;
mod result;
mod sign;
mod spec;
mod state_sync;
mod verification;

pub use hash::{content_hash, ContentHash};
pub use outcome::{JobOutcome, OutcomeVerdict};
pub use result::{JobResult, ResultMeta};
pub use sign::{sign_bytes, verify_bytes, verifying_key_from_bytes, SignError};
pub use spec::{JobInputs, JobKind, JobSpec};
pub use state_sync::{StateSyncRequest, StateSyncResponse, STATE_SYNC_HARD_CEILING};
pub use verification::{JobVerification, VerifierMethod, VerifierVerdict};

/// Crate version surfaced via `parseh_task::VERSION`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Wire-format version. Every top-level message embeds this. Mismatching
/// peers must drop messages they do not understand rather than mis-parsing.
pub const WIRE_VERSION: u32 = 1;

/// Maximum CBOR-encoded size of any individual top-level message.
///
/// Larger payloads use content-addressed sidechannels (IPFS / direct
/// request-response) and carry only the hash on gossipsub. See
/// the project notes §3.8
/// (DOS-via-large-payload).
pub const MAX_MESSAGE_SIZE_BYTES: usize = 1024 * 1024; // 1 MiB

/// CBOR-encode a serialisable value into a freshly-allocated `Vec`.
///
/// Re-exported so downstream crates do not have to depend on `ciborium`
/// directly just to round-trip the wire types.
pub fn to_cbor_bytes<T: serde::Serialize>(
    value: &T,
) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf)?;
    Ok(buf)
}

/// CBOR-decode a value from a byte slice.
pub fn from_cbor_bytes<T: for<'de> serde::Deserialize<'de>>(
    bytes: &[u8],
) -> Result<T, ciborium::de::Error<std::io::Error>> {
    ciborium::from_reader(bytes)
}
