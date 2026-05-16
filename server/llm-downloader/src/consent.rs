//! Consent token — a zero-sized type passed from a UI confirmation step
//! into the download function.
//!
//! [`Consent`] cannot be constructed without going through [`Consent::obtain`],
//! which is async to give the caller a place to await a UI dialog. The inner
//! `()` field is private to this module, so no other crate (and no other
//! module in this crate) can mint a `Consent` value out of thin air.
//!
//! ```ignore
//! let consent = Consent::obtain(|| async { show_dialog_and_return_user_choice() }).await?;
//! download_model(&spec, consent, None).await?;
//! ```

/// Proof-of-consent token. Zero bytes at runtime; carries weight only at the
/// type level: every network call in this crate demands one.
#[derive(Debug, Clone, Copy)]
pub struct Consent(());

impl Consent {
    /// Run the caller's consent prompt and return a [`Consent`] iff the user
    /// agreed.
    ///
    /// The caller must obtain user consent (via UI dialog or interactive CLI
    /// prompt) inside `prompt`. The closure returns `true` to grant consent
    /// and `false` to deny it. The async signature lets the UI dialog be
    /// awaited.
    ///
    /// Returns [`ConsentDenied`] when the user declines.
    pub async fn obtain<F, Fut>(prompt: F) -> Result<Self, ConsentDenied>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        if prompt().await {
            Ok(Self(()))
        } else {
            Err(ConsentDenied)
        }
    }
}

/// Error returned by [`Consent::obtain`] when the user declines.
#[derive(Debug, Clone, Copy)]
pub struct ConsentDenied;

impl std::fmt::Display for ConsentDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "user denied consent for LLM model download")
    }
}

impl std::error::Error for ConsentDenied {}
