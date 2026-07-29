//! Payment-gateway boundary. `PaymentAdapter` is the seam a second gateway would implement
//! later without touching the deposit flow (T2b) — same reasoning as `ChainSigner` in
//! clutch-chain being the KMS migration path, one implementor today, swappable later.
//!
//! `BitcartAdapter` is pinned to Bitcart 0.10.3.0's shape. Two constraints from the choice
//! of Bitcart (watch-only custody, so the box can't move funds) shape this file:
//!
//! 1. Tron watch-only has no xpub-style per-invoice address derivation, so every invoice
//!    shares the one static custody address and Bitcart matches payments by AMOUNT. This
//!    adapter's job is only to forward the exact `pay_amount_usdt` it's given (T2's
//!    discriminator already made that amount unique) — it must never compute or perturb it.
//! 2. Bitcart's IPN webhook is `{"id", "status"}`, unsigned, and dropped (not retried) on
//!    delivery failure. That means `get_invoice` is the only trustworthy read of an
//!    invoice's state; T4's webhook handler treats a webhook as a wake-up ping and always
//!    refetches through `get_invoice` rather than trusting the payload.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct PaymentInstructions {
    pub invoice_id: String,
    pub pay_address: String, // the static custody Tron address
    pub pay_amount_usdt: i64, // micro-USDT
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InvoiceState {
    Pending,
    Paid,      // tx seen, unconfirmed
    Confirmed, // bitcart 'confirmed' or 'complete'
    Expired,
    Invalid,
    Refunded,
    PaidPartial,
    PaidOver,
    FailedConfirm,    // bitcart exception: money moved, confirmation failed
    Unknown(String), // forward-compat: any unmapped status — NEVER silently treat as benign
}

#[derive(Debug, Clone)]
pub struct InvoiceStatus {
    pub state: InvoiceState,
    pub tron_tx_id: Option<String>, // from payments[].tx_hash when present
}

#[async_trait]
pub trait PaymentAdapter: Send + Sync {
    async fn create_invoice(
        &self,
        order_id: &str,
        pay_amount_usdt: i64,
        notification_url: &str,
    ) -> Result<PaymentInstructions, String>;
    async fn get_invoice(&self, invoice_id: &str) -> Result<InvoiceStatus, String>;
}

/// Build Bitcart's decimal `price` string by integer arithmetic only — a float here would
/// reintroduce exactly the precision loss the integer micro-USD peg exists to eliminate.
/// `n` is micro-USDT (6 decimals, same scale as CLT at par).
pub fn usdt_decimal_string(n: i64) -> String {
    format!("{}.{:06}", n / 1_000_000, n % 1_000_000)
}

pub struct BitcartAdapter {
    pub http: reqwest::Client,
    pub base_url: String,
    pub token: String,
    pub store_id: String,
    /// Matches `OrchConfig::deposit_ttl_minutes` — passed explicitly on every
    /// `create_invoice` call because Bitcart's own default expiry window must not diverge
    /// from our advertised pay-in window (a mismatch would let invoices expire on Bitcart's
    /// side mid-window, which interacts badly with T2's amount-slot reservation).
    pub deposit_ttl_minutes: i64,
}

/// Bitcart 0.10.x invoice response shape (pinned; re-verify against the deployed instance's
/// Swagger per docs/bitcart.md before going live). Fields this adapter doesn't read are left
/// off rather than modeled — `#[serde(default)]` / `Option` everywhere else so an unexpected
/// absence deserializes instead of hard-failing.
#[derive(Debug, Deserialize)]
struct BitcartInvoice {
    id: String,
    status: String,
    #[serde(default)]
    exception_status: Option<String>,
    #[serde(default)]
    payments: Vec<BitcartPayment>,
    // Present on create; not needed on get_invoice, but harmless to ignore if absent.
    #[serde(default)]
    payment_address: Option<String>,
    #[serde(default)]
    expiration_seconds: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct BitcartPayment {
    #[serde(default)]
    tx_hash: Option<String>,
}

#[async_trait]
impl PaymentAdapter for BitcartAdapter {
    async fn create_invoice(
        &self,
        order_id: &str,
        pay_amount_usdt: i64,
        notification_url: &str,
    ) -> Result<PaymentInstructions, String> {
        let body = json!({
            "price": usdt_decimal_string(pay_amount_usdt),
            "store_id": self.store_id,
            "order_id": order_id,
            "notification_url": notification_url,
            // Explicit expiration in seconds, matching deposit_ttl_minutes — Bitcart's own
            // default window must not diverge from our advertised pay-in window (see struct
            // doc comment). Field name per the brief; pin against the deployed instance's
            // Swagger in docs/bitcart.md (T7) before going live.
            "expiration": self.deposit_ttl_minutes * 60,
        });

        let resp = self
            .http
            .post(format!("{}/invoices", self.base_url))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("bitcart create_invoice failed: {status} {text}"));
        }

        let invoice: BitcartInvoice = resp.json().await.map_err(|e| e.to_string())?;
        let pay_address = invoice
            .payment_address
            .ok_or_else(|| "bitcart response missing payment_address".to_string())?;

        Ok(PaymentInstructions {
            invoice_id: invoice.id,
            pay_address,
            pay_amount_usdt,
            expires_at: Utc::now()
                + chrono::Duration::seconds(invoice.expiration_seconds.unwrap_or(0)),
        })
    }

    /// The only trustworthy read of an invoice's state (see module docs) — T4's webhook
    /// handler treats the IPN payload as a wake-up ping and always calls this rather than
    /// trusting the webhook body.
    async fn get_invoice(&self, invoice_id: &str) -> Result<InvoiceStatus, String> {
        let resp = self
            .http
            .get(format!("{}/invoices/{}", self.base_url, invoice_id))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("bitcart get_invoice failed: {status} {text}"));
        }

        let invoice: BitcartInvoice = resp.json().await.map_err(|e| e.to_string())?;
        let tron_tx_id = invoice.payments.iter().find_map(|p| p.tx_hash.clone());

        Ok(InvoiceStatus {
            state: map_status(&invoice.status, invoice.exception_status.as_deref()),
            tron_tx_id,
        })
    }
}

/// Exception sub-status wins over the main status when both are present: a `complete` +
/// `paid_over` invoice is an overpayment that must surface for a refund, not read as an
/// exact payment just because the main status alone would say `Confirmed`.
fn map_status(status: &str, exception_status: Option<&str>) -> InvoiceState {
    if let Some(exc) = exception_status {
        match exc {
            "paid_partial" => return InvoiceState::PaidPartial,
            "paid_over" => return InvoiceState::PaidOver,
            "failed_confirm" => return InvoiceState::FailedConfirm,
            "" => {} // no exception — fall through to the main status
            other => return InvoiceState::Unknown(other.to_string()),
        }
    }
    match status {
        "pending" => InvoiceState::Pending,
        "paid" => InvoiceState::Paid,
        "confirmed" | "complete" => InvoiceState::Confirmed,
        "expired" => InvoiceState::Expired,
        "invalid" => InvoiceState::Invalid,
        "refunded" => InvoiceState::Refunded,
        other => InvoiceState::Unknown(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_string_boundary_cases() {
        assert_eq!(usdt_decimal_string(1), "0.000001");
        assert_eq!(usdt_decimal_string(5_000_173), "5.000173");
        assert_eq!(usdt_decimal_string(50_000_000), "50.000000");
        assert_eq!(usdt_decimal_string(0), "0.000000");
    }

    #[test]
    fn status_mapping_covers_every_state() {
        assert_eq!(map_status("pending", None), InvoiceState::Pending);
        assert_eq!(map_status("paid", None), InvoiceState::Paid);
        assert_eq!(map_status("confirmed", None), InvoiceState::Confirmed);
        assert_eq!(map_status("complete", None), InvoiceState::Confirmed);
        assert_eq!(map_status("expired", None), InvoiceState::Expired);
        assert_eq!(map_status("invalid", None), InvoiceState::Invalid);
        assert_eq!(map_status("refunded", None), InvoiceState::Refunded);
    }

    #[test]
    fn exception_sub_statuses_map_independently() {
        assert_eq!(map_status("paid", Some("paid_partial")), InvoiceState::PaidPartial);
        assert_eq!(map_status("paid", Some("paid_over")), InvoiceState::PaidOver);
        assert_eq!(map_status("confirmed", Some("failed_confirm")), InvoiceState::FailedConfirm);
    }

    /// The precedence rule by name: a `complete` invoice with an overpayment must map to
    /// PaidOver, not Confirmed — otherwise the surplus is silently treated as an exact
    /// payment and never flagged for refund.
    #[test]
    fn exception_sub_status_wins_over_main_status() {
        assert_eq!(map_status("complete", Some("paid_over")), InvoiceState::PaidOver);
        assert_eq!(map_status("complete", Some("paid_partial")), InvoiceState::PaidPartial);
        assert_eq!(map_status("confirmed", Some("failed_confirm")), InvoiceState::FailedConfirm);
    }

    /// An unrecognised status must surface as Unknown(raw), never fall through to a benign
    /// default like Pending — a future Bitcart status must be visible to a human, not read
    /// as "still waiting" while funds sit in custody.
    #[test]
    fn unmapped_status_becomes_unknown_not_benign() {
        assert_eq!(
            map_status("some_future_status", None),
            InvoiceState::Unknown("some_future_status".to_string())
        );
        assert_ne!(map_status("some_future_status", None), InvoiceState::Pending);
    }

    #[test]
    fn no_exception_status_falls_through_to_main() {
        assert_eq!(map_status("paid", Some("")), InvoiceState::Paid);
    }
}
