//! Watching per-intent deposit addresses, which is what replaced Bitcart.
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
//! detectable only once the payer's Tron address has been registered against it in advance — which
//! we cannot know. Reproduced in isolation against Nile with a private daemon: it synced to the
//! chain head, read balances correctly, emitted `new_block` events past the block containing a
//! marked transfer, and produced zero payment events. Per-invoice addresses would not have fixed
//! it either, because `TRX_ACCOUNT_PATH` is a fixed single-address derivation path and the matcher
//! keys on the sender regardless of which address receives.
//!
//! # How matching works now
//!
//! Each user has ONE permanent derived address (`derive.rs`, `deposit_addresses`), issued once and
//! reused for every deposit they make — not one address per intent any more. The destination still
//! identifies the payer, which remains a strictly better identity than the amount discriminator it
//! replaced: no 999-slot ceiling, no cross-user collision risk from a freed slot, and a payer who
//! rounds their amount is still correctly attributed.
//!
//! # The unavoidable cost
//!
//! TronGrid can only list TRC-20 transfers PER ADDRESS, and Tron has no way to watch an xpub as a
//! group — derived addresses are unrelated on-chain. So this is one request per address being
//! watched, not one per poll pass as it was under a single shared custody address.
//!
//! What keeps that bounded now is `poller::due_addresses`'s per-pass budget, not the open-intent
//! count: every address is permanent and stays watched for as long as its user might ever deposit
//! again, so cost is capped by how many addresses are polled each pass — hot ones first, the rest
//! rotating cold — rather than by how many deposits happen to be in flight. The reason to bound it
//! at all is unchanged: an unkeyed TronGrid throttles hard, and a throttled watcher is
//! indistinguishable from "nobody is paying."

use async_trait::async_trait;
use serde::Deserialize;

/// A confirmed inbound TRC-20 transfer, reduced to the fields that decide attribution.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservedTransfer {
    pub tx_id: String,
    /// Base units. USDT-TRC20 is 6 decimals, the same scale as micro-USD at par, so this compares
    /// directly against the intent's expected amount with no rate conversion.
    pub amount_usdt: i64,
    pub to: String,
    pub contract: String,
    /// Epoch MILLISECONDS, verified against the live endpoint.
    pub block_timestamp: i64,
}

#[async_trait]
pub trait CustodyWatcher: Send + Sync {
    /// Confirmed TRC-20 transfers into ONE address, optionally bounded below by
    /// `min_timestamp_ms` (epoch milliseconds). `None` fetches unbounded, same as before this
    /// parameter existed.
    async fn transfers_to(
        &self,
        address: &str,
        min_timestamp_ms: Option<i64>,
    ) -> Result<Vec<ObservedTransfer>, String>;
}

/// Everything credit-worthy observed since the last call.
///
/// Deliberately NOT address-oriented, unlike `CustodyWatcher`. A future implementation that follows
/// the USDT contract's Transfer events from a stored cursor cannot express itself as
/// `transfers_to(address)` — it asks for everything since a point in time and filters locally. With
/// the seam here instead, that implementation drops in without the credit path learning about it.
#[async_trait]
pub trait DepositWatcher: Send + Sync {
    async fn poll(&self) -> Result<Vec<ObservedTransfer>, String>;
}

pub struct TronGridWatcher {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    usdt_contract: String,
}

/// TronGrid caps `limit` at 200 and defaults it to 20. Under permanent addresses an address
/// accumulates transfers for its whole life, so this can no longer rely on a handful-per-address
/// premise to stay headroom — what keeps a page from truncating is `min_timestamp` bounding each
/// fetch to what has arrived since the last poll, which is exactly what `TieredPoller` passes. The
/// default of 20 is still small enough to truncate a pathological case, and a truncated page reads
/// as a missing payment.
const PAGE_LIMIT: &str = "200";

/// Only this TRC-20 event kind moves value. An `Approval` carries a `to` and a `value` too, so
/// without this check an approval could satisfy an amount check with nothing having moved.
const TRANSFER_EVENT: &str = "Transfer";

impl TronGridWatcher {
    pub fn new(base_url: String, api_key: String, usdt_contract: String) -> Self {
        Self { http: reqwest::Client::new(), base_url, api_key, usdt_contract }
    }
}

#[derive(Debug, Deserialize)]
struct Trc20Row {
    transaction_id: String,
    to: String,
    /// Decimal STRING in base units — verified against the live endpoint. Parsed as an integer; a
    /// float here would reintroduce the precision loss the integer peg exists to eliminate.
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
    async fn transfers_to(
        &self,
        address: &str,
        min_timestamp_ms: Option<i64>,
    ) -> Result<Vec<ObservedTransfer>, String> {
        let url = format!("{}/v1/accounts/{}/transactions/trc20", self.base_url, address);
        let mut req = self
            .http
            .get(&url)
            .header("TRON-PRO-API-KEY", &self.api_key)
            .query(&[
                // Confirmed only. This doubles as the confirmation gate: TronGrid excludes
                // transfers from blocks that are not yet irreversible, so anything appearing here
                // already has the depth the treasury's verifier separately re-checks.
                ("only_confirmed", "true"),
                ("contract_address", self.usdt_contract.as_str()),
                ("limit", PAGE_LIMIT),
            ]);
        if let Some(ts) = min_timestamp_ms {
            // Lower bound only — TronGrid still returns newest-first up to `limit`. Bounding here
            // is what keeps a permanent address's page from growing with its whole lifetime.
            req = req.query(&[("min_timestamp", ts.to_string())]);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("trongrid trc20 list for {address} failed: {status} {text}"));
        }
        let parsed: Trc20Response = resp.json().await.map_err(|e| e.to_string())?;
        if parsed.data.len() >= PAGE_LIMIT.parse::<usize>().unwrap_or(usize::MAX) {
            tracing::warn!("trongrid returned a full page of transfers for {address} — older ones may be truncated");
        }
        Ok(rows_to_transfers(parsed.data, address, &self.usdt_contract))
    }
}

/// Filter and convert, kept pure so the parsing rules are testable without HTTP.
///
/// Re-checks the recipient even though the request was scoped to that address, and the contract
/// even though the query filters by it. Both are a remote system's promise about a money path; a
/// mismatch would attribute a stranger's transfer, or a worthless token, to one of our intents.
fn rows_to_transfers(rows: Vec<Trc20Row>, expected_to: &str, contract: &str) -> Vec<ObservedTransfer> {
    rows.into_iter()
        .filter(|r| r.event_type == TRANSFER_EVENT)
        .filter(|r| r.to == expected_to)
        .filter(|r| r.token_info.address == contract)
        .filter_map(|r| {
            // An unparseable amount is dropped, not defaulted to 0: a phantom zero would pollute
            // the sum's provenance while contributing nothing.
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

    const ADDR: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";
    const OTHER: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";
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

    fn t(tx: &str, amount: i64, ts: i64) -> ObservedTransfer {
        ObservedTransfer { tx_id: tx.into(), amount_usdt: amount, to: ADDR.into(), contract: USDT.into(), block_timestamp: ts }
    }

    #[test]
    fn approval_events_are_dropped() {
        let rows = vec![row("appr", ADDR, USDT, "1000000", "Approval", 1)];
        assert!(rows_to_transfers(rows, ADDR, USDT).is_empty(), "an Approval moves no value");
    }

    #[test]
    fn absent_event_type_is_dropped() {
        let rows = vec![row("none", ADDR, USDT, "1000000", "", 1)];
        assert!(rows_to_transfers(rows, ADDR, USDT).is_empty());
    }

    /// The request is scoped to one address, but a response naming a different recipient must still
    /// be discarded — otherwise a stranger's transfer could be attributed to this intent.
    #[test]
    fn a_transfer_to_a_different_address_is_dropped() {
        let rows = vec![row("elsewhere", OTHER, USDT, "1000000", "Transfer", 1)];
        assert!(rows_to_transfers(rows, ADDR, USDT).is_empty());
    }

    #[test]
    fn a_transfer_of_another_token_is_dropped() {
        let rows = vec![row("wrongtok", ADDR, "TXLAQ63Xg1NAzckPwKHvzw7CSEmLMEqcdj", "1000000", "Transfer", 1)];
        assert!(rows_to_transfers(rows, ADDR, USDT).is_empty());
    }

    #[test]
    fn a_non_integer_amount_is_dropped() {
        let rows = vec![row("bad", ADDR, USDT, "1.000000", "Transfer", 1)];
        assert!(rows_to_transfers(rows, ADDR, USDT).is_empty(), "base units are integers");
    }

    #[test]
    fn a_good_row_survives_with_every_field_intact() {
        let rows = vec![row("good", ADDR, USDT, "1000000", "Transfer", 1785465765000)];
        let out = rows_to_transfers(rows, ADDR, USDT);
        assert_eq!(out, vec![t("good", 1_000_000, 1785465765000)]);
    }
}
