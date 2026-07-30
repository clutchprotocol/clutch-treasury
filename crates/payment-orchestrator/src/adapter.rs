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
    /// The currency the invoice is DENOMINATED in, and it must be the payment token itself
    /// (e.g. `USDT`) — never a fiat code.
    ///
    /// This is not cosmetic. Omitting it makes Bitcart fall back to the store's
    /// `default_currency`, and a live 0.10.3.0 run showed exactly what that costs: a price of
    /// `5.000173` under a USD store was stored as `5.00` and converted at a 0.33 rate into
    /// `15.152039` of the payment token. Two separate disasters in one:
    ///
    ///   1. The 6-decimal discriminator — the ONLY thing distinguishing two payers on the shared
    ///      custody address — is rounded away, so every deposit of the same dollar figure becomes
    ///      the same invoice.
    ///   2. A rate is applied at all, which is precisely the arithmetic the integer par peg
    ///      exists to eliminate.
    ///
    /// Denominated in the token, the same request returns price `5.000173` at rate `1.000000` —
    /// exact, and no conversion. Keep it that way.
    pub invoice_currency: String,
}

/// Bitcart invoice response shape, VERIFIED against `api/schemas/invoices.py`'s `DisplayInvoice`
/// and `api/models.py` at tag `0.10.3.0` (not assumed — three of these field names were wrong
/// before that check; see docs/bitcart.md).
///
/// Fields this adapter doesn't read are left off rather than modeled, and everything optional is
/// `#[serde(default)]` so an unexpected absence deserializes instead of hard-failing.
#[derive(Debug, Deserialize)]
struct BitcartInvoice {
    id: String,
    status: String,
    #[serde(default)]
    exception_status: Option<String>,
    /// `DisplayInvoice.payments` — each entry is `PaymentMethod::to_payment_dict`, i.e. every
    /// PaymentMethod column. `payment_address` lives HERE, per-method; there is no top-level one.
    #[serde(default)]
    payments: Vec<BitcartPayment>,
    /// `DisplayInvoice.tx_hashes` — top-level, PLURAL, and an array. PaymentMethod has no
    /// `tx_hash` column at all, so this is the only place an on-chain hash appears.
    #[serde(default)]
    tx_hashes: Vec<String>,
    #[serde(default)]
    expiration_seconds: Option<i64>,
    /// What the invoice ASKED for, and what was actually RECEIVED. Both kept as raw JSON numbers
    /// or strings and parsed by integer arithmetic — never through f64, which would reintroduce
    /// the precision loss the micro-unit peg exists to remove.
    ///
    /// These exist so the adapter can cross-check `exception_status` instead of trusting it. A
    /// live instance was observed reporting `paid_over` for a payment of 2.0 against a 5.000203
    /// invoice; believing that label alone credits the full intended amount for an underpayment,
    /// which is unbacked CLT.
    #[serde(default)]
    price: Option<serde_json::Value>,
    #[serde(default)]
    sent_amount: Option<serde_json::Value>,
}

/// Parse a decimal money value into integer micro-units, without floats.
///
/// Accepts what Bitcart actually emits for these fields: a JSON string (`"5.000173"`) or a bare
/// JSON number (`6.5`, `0.0`). In both cases the textual form is parsed digit by digit, so no f64
/// ever touches the value. More than 6 decimal places truncates rather than rounds — deliberately
/// downward, so a cross-check can never over-credit on a rounding artifact.
fn decimal_to_micros(v: &serde_json::Value) -> Option<i64> {
    let s = match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.chars().all(|c| c.is_ascii_digit()) || !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let whole: i64 = if int_part.is_empty() { 0 } else { int_part.parse().ok()? };
    let mut frac: i64 = 0;
    for i in 0..6 {
        frac = frac * 10 + frac_part.as_bytes().get(i).map_or(0, |b| i64::from(b - b'0'));
    }
    let total = whole.checked_mul(1_000_000)?.checked_add(frac)?;
    Some(if neg { -total } else { total })
}

#[derive(Debug, Deserialize)]
struct BitcartPayment {
    /// A `PaymentMethod` column, surfaced through `to_payment_dict`.
    #[serde(default)]
    payment_address: Option<String>,
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
            // Denominate in the token, never the store's fiat default — see `invoice_currency`.
            // Without this the discriminator is rounded away and a conversion rate is applied.
            "currency": self.invoice_currency,
            // MINUTES, not seconds. Verified against 0.10.3.0: `CreateInvoice.expiration: int`
            // and `Invoice.expiration_seconds` is a property returning `expiration * 60`.
            // Sending seconds here made a 30-minute window into 30 HOURS — Bitcart would keep
            // matching payments to a dead invoice long after our own expires_at, and hold the
            // discriminator slot 60x longer than intended (slots only free on Bitcart
            // terminality, and there are 999 per base amount).
            "expiration": self.deposit_ttl_minutes,
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
        // `payment_address` is per-payment-method, never top-level. With `BITCART_CRYPTOS=trx`
        // there is exactly one method, so the first entry is the Tron custody address — but take
        // it explicitly rather than assuming, and fail closed if the list is empty: no address
        // means we have nothing to tell the user to pay, and inventing one loses their money.
        let pay_address = invoice
            .payments
            .into_iter()
            .find_map(|p| p.payment_address)
            .ok_or_else(|| "bitcart response had no payments[].payment_address".to_string())?;

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
        // Top-level `tx_hashes`, not `payments[].tx_hash` (PaymentMethod has no such column).
        // Only take it when there is exactly ONE: with trx-only there is one payment method, so
        // several hashes means several transfers landed against this invoice, and picking one
        // would hand the verifier a hash that doesn't account for the full amount. Leaving it
        // None routes the deposit through T5a's exact-amount fallback instead, which is the
        // conservative path and alerts.
        let tron_tx_id = match invoice.tx_hashes.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        };

        let mut state = map_status(&invoice.status, invoice.exception_status.as_deref());

        // Cross-check the label against the numbers. `exception_status` is Bitcart's opinion;
        // `price` vs `sent_amount` is what actually moved. A live instance was seen reporting
        // `paid_over` for 2.0 received against a 5.000203 invoice, and PaidOver credits the full
        // INTENDED amount — so believing the label alone mints CLT that no deposit backs.
        //
        // Only ever downgrades. If the numbers say short-paid, the state becomes PaidPartial and
        // goes to a human; a genuine overpayment (sent >= asked) keeps whatever Bitcart said.
        // Unparseable or absent numbers leave the label untouched — this guard adds safety, it
        // must not invent a verdict from missing data.
        if matches!(state, InvoiceState::Confirmed | InvoiceState::PaidOver) {
            if let (Some(asked), Some(sent)) = (
                invoice.price.as_ref().and_then(decimal_to_micros),
                invoice.sent_amount.as_ref().and_then(decimal_to_micros),
            ) {
                if sent < asked {
                    tracing::warn!(
                        invoice_id = %invoice.id,
                        asked_micros = asked,
                        sent_micros = sent,
                        reported = ?state,
                        "bitcart reported a fully-paid state but sent_amount is BELOW price — \
                         treating as PaidPartial so it goes to manual review instead of crediting"
                    );
                    state = InvoiceState::PaidPartial;
                }
            }
        }

        Ok(InvoiceStatus { state, tron_tx_id })
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
            // "none" is what a live 0.10.3.0 actually sends on a healthy invoice — a literal
            // string, not null and not "". The model types it `str | None`, which is why this
            // was written expecting empty/absent; only a real call showed otherwise.
            //
            // Getting this wrong was total: every ordinary invoice mapped to Unknown("none"),
            // and T4 routes Unknown to needs_manual + P1. Every deposit would have been parked
            // for a human and nothing would ever have been credited.
            "none" | "" => {} // no exception — fall through to the main status
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
