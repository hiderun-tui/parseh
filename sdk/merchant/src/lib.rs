//! `parseh-merchant` — merchant SDK stub.
//!
//! Goal in V2: a brick-and-mortar shop or an online merchant should be
//! able to call `Invoice::new(amount_irr, "Coffee + croissant")` and get
//! back a QR code + NFC NDEF payload + REST callback URL. When the
//! customer pays from Hiderun, a webhook fires on the merchant side.
//!
//! Today: type stubs only.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// SDK version surface, mirrors `parseh-sdk`.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A merchant invoice ready to render as a QR code or NFC payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invoice {
    /// Merchant chain address (bech32). Funds settle here.
    pub merchant: String,
    /// Amount in microPARSEH.
    pub amount_micro_parseh: u64,
    /// Human description displayed in the customer's wallet.
    pub memo: String,
    /// Unix timestamp at which the invoice expires.
    pub expires_at: u64,
    /// Stable invoice id for webhook correlation.
    pub invoice_id: String,
}

impl Invoice {
    /// Construct an invoice. In V2 this calls a price oracle for IRR/USD → PARSEH.
    pub fn new(merchant: impl Into<String>, amount_micro_parseh: u64, memo: impl Into<String>) -> Self {
        Self {
            merchant: merchant.into(),
            amount_micro_parseh,
            memo: memo.into(),
            expires_at: 0,
            invoice_id: "stub".into(),
        }
    }
}
