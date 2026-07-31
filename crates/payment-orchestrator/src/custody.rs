//! Watching the custody address directly, which is what replaced Bitcart.
//!
//! # Why Bitcart is gone
//!
//! Bitcart could never have detected our deposits. Its TRX daemon attributes a payment by the
//! SENDER's address:
//!
//! ```text
//! if tx.from_addr in self.wallets[wallet].request_addresses:      # genericprocessor.py
//!     process_new_payment(tx.from_addr, ...)
//! ```
//!
//! `request_addresses` is only populated by `set_request_address(req, address)`, so a request is
//! detectable only once the payer's Tron address has been registered against it in advance. Our
//! model is the inverse: one shared static custody address, payers unknown until they pay, and
//! the amount discriminator as the identity. Nothing configurable reconciles those.
//!
//! Reproduced in isolation against Nile with a private daemon: it synced to the chain head, it
//! read the custody balance correctly (4.001474 -> 4.124930 USDT across a marked payment), it
//! emitted `new_block` events past the block containing the transfer — and produced zero payment
//! events, leaving the request at `sent_amount: "0.000000"`. Per-invoice addresses would not have
//! helped either: `TRX_ACCOUNT_PATH = "m/44'/195'/0'/0/0"` is a fixed path, one address per
//! wallet, and the matcher keys on the sender regardless of which address receives.
//!
//! # What this does instead
//!
//! Reads confirmed TRC-20 transfers into the custody address from TronGrid and matches them to
//! intents on the EXACT discriminated amount. That is the same evidence rule `treasury-service`'s
//! `tron_verifier` already applies as the approver — this crate is now the detector for it rather
//! than trusting a third party that was matching on something else entirely.
//!
//! One list fetch serves a whole poll pass, not one call per intent. There is no TronGrid API key
//! on stage, and unkeyed TronGrid throttles hard — N calls per tick would reintroduce a failure
//! that looks exactly like "no payments are arriving".

use async_trait::async_trait;
use serde::Deserialize;

/// A confirmed inbound TRC-20 transfer, reduced to the fields that decide attribution.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservedTransfer {
    pub tx_id: String,
    /// Base units. USDT-TRC20 is 6 decimals, the same scale as micro-USD at par, so this is
    /// directly comparable to `pay_amount_usdt` with no rate conversion.
    pub amount_usdt: i64,
    pub to: String,
    pub contract: String,
    /// Epoch MILLISECONDS, verified against the live endpoint (same field `tron_verifier` reads).
    pub block_timestamp: i64,
}

#[async_trait]
pub trait CustodyWatcher: Send + Sync {
    /// Confirmed transfers into the custody address, most recent first. One call per poll pass.
    async fn recent_transfers(&self) -> Result<Vec<ObservedTransfer>, String>;
}

/// EXACT match, never `>=`.
///
/// The fractional tail IS the payer's identity on a shared address, so a near-miss is a different
/// payment, not this one. `>=` would let a larger unrelated transfer satisfy a smaller intent and
/// credit the wrong user — the defect class that produced commit cb497e3 in the verifier, which
/// matched `>= amount_clt` and let the discriminator never reach the treasury at all.
///
/// `uq_active_pay_amount` guarantees at most one *active* intent per amount, so a match here is
/// unambiguous. Ties are still resolved oldest-first for determinism rather than relying on
/// TronGrid's ordering.
pub fn match_exact<'a>(
    transfers: &'a [ObservedTransfer],
    expected_amount_usdt: i64,
) -> Option<&'a ObservedTransfer> {
    transfers
        .iter()
        .filter(|t| t.amount_usdt == expected_amount_usdt)
        .min_by_key(|t| t.block_timestamp)
}

pub struct TronGridWatcher {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    custody_address: String,
    usdt_contract: String,
}

/// TronGrid caps `limit` at 200 and defaults it to 20. Ask for the max: a busy custody address
/// with more than one page of recent transfers could otherwise push a legitimate deposit off the
/// only page we look at, and the intent would sit unmatched until it expired.
const PAGE_LIMIT: &str = "200";

/// Only this TRC-20 event kind moves value. An `Approval` carries a `to` and a `value` too, so
/// without this check an approval could satisfy an amount match with nothing having moved.
const TRANSFER_EVENT: &str = "Transfer";

impl TronGridWatcher {
    pub fn new(base_url: String, api_key: String, custody_address: String, usdt_contract: String) -> Self {
        Self { http: reqwest::Client::new(), base_url, api_key, custody_address, usdt_contract }
    }
}

#[derive(Debug, Deserialize)]
struct Trc20Row {
    transaction_id: String,
    to: String,
    /// Decimal STRING in base units — verified against the live endpoint. Parsed as an integer;
    /// a float here would reintroduce the precision loss the integer peg exists to eliminate.
    value: String,
    token_info: TokenInfo,
    #[serde(default, rename = "type")]
    event_type: String,
    #[serde(default)]
    block_timestamp: i64,
}

#[derive(Debug, Deserialize)]
struct TokenInfo {
    address: String,
}

#[derive(Debug, Deserialize)]
struct Trc20Response {
    #[serde(default)]
    data: Vec<Trc20Row>,
}

#[async_trait]
impl CustodyWatcher for TronGridWatcher {
    async fn recent_transfers(&self) -> Result<Vec<ObservedTransfer>, String> {
        let url = format!("{}/v1/accounts/{}/transactions/trc20", self.base_url, self.custody_address);
        let resp = self
            .http
            .get(&url)
            .header("TRON-PRO-API-KEY", &self.api_key)
            .query(&[
                // Confirmed only. This is also the confirmation gate: TronGrid excludes
                // transfers from blocks that are not yet irreversible, so a transfer appearing
                // here has the confirmed depth the verifier separately re-checks.
                ("only_confirmed", "true"),
                ("contract_address", self.usdt_contract.as_str()),
                ("limit", PAGE_LIMIT),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("trongrid trc20 list failed: {status} {text}"));
        }
        let parsed: Trc20Response = resp.json().await.map_err(|e| e.to_string())?;
        if parsed.data.len() >= PAGE_LIMIT.parse::<usize>().unwrap_or(usize::MAX) {
            tracing::warn!(
                "trongrid returned a full page ({PAGE_LIMIT}) of custody transfers — older ones may \
                 be truncated; paginate via meta.links.next if this becomes routine"
            );
        }
        Ok(rows_to_transfers(parsed.data, &self.custody_address, &self.usdt_contract))
    }
}

/// Filter and convert, kept pure so the parsing rules are testable without HTTP.
///
/// Re-checks recipient and contract even though the query already filters by contract: this is
/// the money path, the filter is a remote system's promise, and a mismatch here would attribute a
/// stranger's transfer to one of our intents.
fn rows_to_transfers(rows: Vec<Trc20Row>, custody: &str, contract: &str) -> Vec<ObservedTransfer> {
    rows.into_iter()
        .filter(|r| r.event_type == TRANSFER_EVENT)
        .filter(|r| r.to == custody)
        .filter(|r| r.token_info.address == contract)
        .filter_map(|r| {
            // An unparseable amount is dropped, not defaulted to 0: a 0 would compare equal to
            // nothing and silently vanish, but it would also pollute the unattributed-payment
            // alert with a phantom zero-value transfer.
            let amount = r.value.parse::<i64>().ok()?;
            Some(ObservedTransfer {
                tx_id: r.transaction_id,
                amount_usdt: amount,
                to: r.to,
                contract: r.token_info.address,
                block_timestamp: r.block_timestamp,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUSTODY: &str = "TQwgeRaDt4FSJSsncmFNcbMNTfFpjvjwFX";
    const USDT: &str = "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf";

    fn row(tx: &str, to: &str, contract: &str, value: &str, event: &str, ts: i64) -> Trc20Row {
        Trc20Row {
            transaction_id: tx.into(),
            to: to.into(),
            value: value.into(),
            token_info: TokenInfo { address: contract.into() },
            event_type: event.into(),
            block_timestamp: ts,
        }
    }

    fn transfer(tx: &str, amount: i64, ts: i64) -> ObservedTransfer {
        ObservedTransfer {
            tx_id: tx.into(),
            amount_usdt: amount,
            to: CUSTODY.into(),
            contract: USDT.into(),
            block_timestamp: ts,
        }
    }

    #[test]
    fn exact_amount_matches() {
        let ts = vec![transfer("a", 1_000_133, 10), transfer("b", 1_000_219, 20)];
        assert_eq!(match_exact(&ts, 1_000_219).unwrap().tx_id, "b");
    }

    /// The whole point of the discriminator: one micro-unit off is a DIFFERENT payment.
    #[test]
    fn near_miss_is_not_a_match() {
        let ts = vec![transfer("a", 1_000_133, 10)];
        assert!(match_exact(&ts, 1_000_132).is_none());
        assert!(match_exact(&ts, 1_000_134).is_none());
    }

    /// A larger transfer must never satisfy a smaller intent — that is cb497e3's defect class,
    /// and on a shared custody address it credits the wrong user.
    #[test]
    fn a_larger_transfer_never_satisfies_a_smaller_intent() {
        let ts = vec![transfer("whale", 50_000_000, 10)];
        assert!(match_exact(&ts, 1_000_133).is_none());
    }

    /// Underpayment must not match either: it is not this deposit, it is an unattributed payment.
    #[test]
    fn underpayment_does_not_match() {
        let ts = vec![transfer("short", 999_999, 10)];
        assert!(match_exact(&ts, 1_000_133).is_none());
    }

    #[test]
    fn duplicate_amounts_resolve_oldest_first_deterministically() {
        let ts = vec![transfer("newer", 1_000_133, 999), transfer("older", 1_000_133, 5)];
        assert_eq!(match_exact(&ts, 1_000_133).unwrap().tx_id, "older");
    }

    #[test]
    fn empty_list_matches_nothing() {
        assert!(match_exact(&[], 1_000_133).is_none());
    }

    #[test]
    fn approval_events_are_dropped() {
        let rows = vec![row("appr", CUSTODY, USDT, "1000133", "Approval", 1)];
        assert!(rows_to_transfers(rows, CUSTODY, USDT).is_empty(), "an Approval moves no value");
    }

    /// A missing `type` must fail closed rather than be assumed to be a Transfer.
    #[test]
    fn absent_event_type_is_dropped() {
        let rows = vec![row("none", CUSTODY, USDT, "1000133", "", 1)];
        assert!(rows_to_transfers(rows, CUSTODY, USDT).is_empty());
    }

    #[test]
    fn transfers_to_another_address_are_dropped() {
        let rows = vec![row("other", "TSomeoneElseAddressXXXXXXXXXXXXXXX", USDT, "1000133", "Transfer", 1)];
        assert!(rows_to_transfers(rows, CUSTODY, USDT).is_empty());
    }

    /// Defence in depth: the query filters by contract, but a remote system's filter is a promise,
    /// not a guarantee, and the wrong token would credit CLT against a worthless deposit.
    #[test]
    fn transfers_of_another_token_are_dropped() {
        let rows = vec![row("wrongtok", CUSTODY, "TXLAQ63Xg1NAzckPwKHvzw7CSEmLMEqcdj", "1000133", "Transfer", 1)];
        assert!(rows_to_transfers(rows, CUSTODY, USDT).is_empty());
    }

    #[test]
    fn unparseable_amount_is_dropped_not_zeroed() {
        let rows = vec![row("bad", CUSTODY, USDT, "1.000133", "Transfer", 1)];
        assert!(rows_to_transfers(rows, CUSTODY, USDT).is_empty(), "base units are integers");
    }

    #[test]
    fn a_good_row_survives_with_every_field_intact() {
        let rows = vec![row("good", CUSTODY, USDT, "1000133", "Transfer", 1785465765000)];
        let out = rows_to_transfers(rows, CUSTODY, USDT);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], transfer("good", 1_000_133, 1785465765000));
    }
}
