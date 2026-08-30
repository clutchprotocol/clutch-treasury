//! Plan C T5: the independent on-chain confirmation that a deposit-backed mint intent's
//! USDT actually arrived (spec §7.1) — and, on that evidence alone, the automated approver.
//! Four-eyes here is: initiator `'orchestrator'` (the bridge worker that created the intent,
//! 5b), approver `'tron-verifier'` (this module) — the `four_eyes` DB CHECK holds because
//! those two strings differ, same mechanism Plan B's human flow relies on.
//!
//! The one distinction this whole file exists to get right: a HARD evidence mismatch (wrong
//! recipient, wrong token contract, amount below expected) is real data saying this deposit is
//! not what it claims — reject and alert. A TRANSIENT failure (TronGrid error, timeout, tx not
//! yet confirmed) is OUR infrastructure failing, not the user's deposit — leave the intent
//! `created` so the next poll tick retries it; never reject on an outage. Conflating these
//! either mints unbacked CLT or throws away a real user's real money.
//!
//! TronGrid shapes for the trc20 list are VERIFIED against a live call to
//! `api.trongrid.io` (T7): `transaction_id`, `to`, `value` (decimal STRING in base units),
//! `token_info.address`, `type`, and `block_timestamp` (epoch MILLISECONDS) all confirmed
//! present with those exact names.
//!
//! Every endpoint below is now verified against a real confirmed TRC-20 transfer on Nile. Two
//! previously carried an ASSUMED note and both turned out to be wrong — see the doc comments on
//! `SolidityTransaction` and `get_custody_balance` for what each one silently did. The lesson
//! worth keeping: an assumption about an external API is not a small comment, it is an untested
//! branch on the money path.
//!
//! The three calls:
//!   - `GET /v1/accounts/{address}/transactions/trc20?only_confirmed=true&contract_address={c}`
//!     — TRC-20 transfer history for the custody address. Used for BOTH the has-tx_id path
//!     (find and validate this specific transfer) and the fallback path (find any transfer
//!     that matches by amount when Bitcart never returned a hash).
//!   - `POST /walletsolidity/gettransactionbyid` — confirmed depth for a known `deposit_tx_id`.
//!     The solidity node only serves irreversible blocks, so presence is the proof.
//!   - `POST /wallet/triggerconstantcontract` calling `balanceOf(address)` — the custody
//!     reserve read for reconciliation's cross-check.

use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::configuration::AppConfig;
use crate::ledger::alert;

/// A single TRC-20 transfer as TronGrid's trc20 endpoint reports it. Field names VERIFIED
/// against a live response (T7), not inferred: `value` really is a decimal STRING in the
/// token's base unit — USDT-TRC20 is 6 decimals, the same scale as CLT at par, so it parses
/// directly as micro-USDT with no rate conversion.
#[derive(Debug, Deserialize)]
struct Trc20Transfer {
    transaction_id: String,
    to: String,
    value: String,
    token_info: TokenInfo,
    /// The TRC-20 event kind. Only `"Transfer"` moves value — an `Approval` event carries a
    /// `value` and a `to` as well, so without this check one could satisfy the amount match
    /// without a single token having moved. Empty (missing field) fails the check, closed.
    #[serde(default, rename = "type")]
    event_type: String,
    /// VERIFIED against the live endpoint: epoch MILLISECONDS (e.g. 1785358407000), which is what
    /// the fallback window compares against via `timestamp_millis()`. Only the fallback reads it,
    /// to bound how far back an unclaimed transfer can be swept up; a missing field defaults to 0,
    /// which puts the transfer outside every window and so fails closed.
    #[serde(default)]
    block_timestamp: i64,
}

#[derive(Debug, Deserialize)]
struct TokenInfo {
    address: String,
}

#[derive(Debug, Deserialize)]
struct Trc20TransfersResponse {
    #[serde(default)]
    data: Vec<Trc20Transfer>,
}

/// Response of `POST /walletsolidity/gettransactionbyid`, modeled minimally.
///
/// The SOLIDITY node only serves blocks that are already irreversible (≈19 blocks, 2/3+1 SR
/// confirmations), so a transaction being present here IS the confirmed-depth proof. When it is
/// not yet final the node answers `{}` — hence `Option`, which is how "not confirmed yet" is
/// told apart from a transport failure.
///
/// This replaces `GET /v1/transactions/{id}` and its `confirmed` boolean. That endpoint and that
/// field never existed: the module previously documented them as ASSUMED, and a live call
/// against Nile returns **HTTP 404**. Since a 404 became `Err` and every `Err` here maps to
/// `Evidence::Transient`, the has-tx_id path could never reach `Pass` — every deposit with a
/// known hash retried until the 24h stuck-sweep handed it to a human. Verified against a real
/// confirmed TRC-20 transfer on Nile before this change.
#[derive(Debug, Deserialize)]
struct SolidityTransaction {
    /// Absent when the transaction is not yet in an irreversible block.
    #[serde(default, rename = "txID")]
    tx_id: Option<String>,
}

pub struct TronClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl TronClient {
    pub fn new(base_url: String, api_key: String) -> Self {
        Self { http: reqwest::Client::new(), base_url, api_key }
    }

    async fn trc20_transfers(&self, custody_address: &str, usdt_contract: &str) -> Result<Vec<Trc20Transfer>, String> {
        let url = format!("{}/v1/accounts/{}/transactions/trc20", self.base_url, custody_address);
        let resp = self
            .http
            .get(&url)
            .header("TRON-PRO-API-KEY", &self.api_key)
            .query(&[
                ("only_confirmed", "true"),
                ("contract_address", usdt_contract),
                // TronGrid defaults `limit` to 20 (max 200). Leaving it unset meant a custody
                // address with more than 20 recent USDT transfers could push a legitimate
                // deposit off the only page we look at — the intent would then never find its
                // evidence, stay Transient forever, and age into manual review. Verified against
                // the live endpoint's documented parameters.
                ("limit", TRC20_PAGE_LIMIT),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("trongrid trc20 list failed: {status} {text}"));
        }
        let parsed: Trc20TransfersResponse = resp.json().await.map_err(|e| e.to_string())?;
        // ponytail: single page only. If it comes back FULL we may be truncating, so say so
        // rather than let a missed deposit look like "not on chain yet". Paginating via
        // `meta.links.next` is the fix if a busy custody address ever makes this routine.
        if parsed.data.len() >= TRC20_PAGE_LIMIT.parse::<usize>().unwrap_or(usize::MAX) {
            tracing::warn!(
                "trongrid trc20 list returned a full page ({TRC20_PAGE_LIMIT}) for {custody_address} — \
                 older transfers in the window may be truncated; consider paginating"
            );
        }
        Ok(parsed.data)
    }

    /// Is `tx_id` in an irreversible block?
    ///
    /// Asks the SOLIDITY node, which by definition only serves finalised blocks — presence is the
    /// proof. Deliberately a different endpoint from `trc20_transfers`' `only_confirmed=true`, so
    /// this stays a genuinely independent second observation rather than the same read twice.
    ///
    /// Returns `Ok(false)` (not `Err`) when the node answers `{}`: not-yet-final is a normal
    /// waiting state that should surface as "not yet confirmed", while `Err` is reserved for our
    /// infrastructure failing. The caller maps both to `Transient`, but only one of them is worth
    /// alerting about.
    pub async fn transaction_confirmed(&self, tx_id: &str) -> Result<bool, String> {
        let url = format!("{}/walletsolidity/gettransactionbyid", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("TRON-PRO-API-KEY", &self.api_key)
            .json(&serde_json::json!({ "value": tx_id }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("trongrid gettransactionbyid failed: {status} {text}"));
        }
        let parsed: SolidityTransaction = resp.json().await.map_err(|e| e.to_string())?;
        // Compare the echoed id rather than merely checking presence: a response for some other
        // transaction must not be read as confirmation of this one. Tron hex ids are
        // case-insensitive, hence the eq_ignore_ascii_case.
        Ok(parsed.tx_id.is_some_and(|got| got.eq_ignore_ascii_case(tx_id)))
    }

    /// Reconciliation's cross-check read (spec's fourth-source stand-in until a PSP exists):
    /// the custody address's current TRC-20 USDT balance, straight from TronGrid rather than
    /// our own ledger. A mismatch against `custody_reported` is logged as an early smell, not
    /// wired into the breaker (brief: "not a halt").
    ///
    /// The custody address's USDT balance, read by calling `balanceOf(address)` on the token
    /// contract.
    ///
    /// This used to read `GET /v1/accounts/{address}` and pick the contract out of a `trc20`
    /// array. That endpoint answers `{"data":[]}` for an address Tron has never *created*, and
    /// receiving only TRC-20 tokens does NOT create an account — only receiving TRX does. A
    /// receive-only custody address is therefore absent from the account index no matter how much
    /// USDT it holds, and the old chain ended `.unwrap_or(0)`, so it returned a confident `Ok(0)`.
    ///
    /// Verified on Nile against this exact custody address: `/v1/accounts/{addr}` returned
    /// `{"data":[]}` while `balanceOf` returned 2.000699 USDT — two real payments the reserve
    /// cross-check would have reported as zero reserve.
    ///
    /// `balanceOf` asks the token's own ledger, so account activation is irrelevant.
    ///
    /// Custody + every unswept deposit address + the payout float.
    ///
    /// Reading only the main address would report a reserve near zero while deposits sit on derived
    /// addresses awaiting a sweep. That is not a halt risk — `judge` keys on the LEDGER's
    /// `custody_reported`, and `trongrid_balance` is a cross-check column that plays no part in any
    /// branch — but a fourth source that is permanently wrong is worse than one that is absent:
    /// people stop reading it, and then disbelieve it on the day it is right.
    ///
    /// The float is counted for the same reason, with a sharper failure mode: it is funded FROM
    /// custody, so leaving it out means the first top-up reads as the reserve shrinking, and
    /// reconciliation halts minting over money that never left.
    ///
    /// `float_address` is a separate parameter rather than another entry in `unswept_addresses` so a
    /// failure reading it is attributed to the float, not misreported as a deposit problem.
    ///
    /// A failure on ANY address fails the whole sum. A partial total would understate the reserve
    /// and look exactly like a shortfall, which is the one direction a reserve figure must never
    /// silently err in.
    pub async fn get_reserve_balance(
        &self,
        main_address: &str,
        unswept_addresses: &[String],
        float_address: &str,
        usdt_contract: &str,
    ) -> Result<i64, String> {
        let mut total = self.get_custody_balance(main_address, usdt_contract).await?;
        for addr in unswept_addresses {
            let bal = self
                .get_custody_balance(addr, usdt_contract)
                .await
                .map_err(|e| format!("unswept deposit address {addr}: {e}"))?;
            // Saturating: a corrupt balance must not wrap the reserve into something small.
            total = total.saturating_add(bal);
        }
        let float = self
            .get_custody_balance(float_address, usdt_contract)
            .await
            .map_err(|e| format!("payout float {float_address}: {e}"))?;
        Ok(total.saturating_add(float))
    }

    pub async fn get_custody_balance(&self, custody_address: &str, usdt_contract: &str) -> Result<i64, String> {
        #[derive(Deserialize)]
        struct ConstantCallResponse {
            #[serde(default)]
            constant_result: Vec<String>,
        }

        let url = format!("{}/wallet/triggerconstantcontract", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("TRON-PRO-API-KEY", &self.api_key)
            .json(&serde_json::json!({
                // A constant call executes nothing and costs nothing, so owner can be the address
                // being queried; no key is involved and no state changes.
                "owner_address": custody_address,
                "contract_address": usdt_contract,
                "function_selector": "balanceOf(address)",
                "parameter": abi_encode_address(custody_address)?,
                "visible": true,
            }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("trongrid balanceOf failed: {status} {text}"));
        }
        let parsed: ConstantCallResponse = resp.json().await.map_err(|e| e.to_string())?;
        // Err, never Ok(0), when the call produced no result. Conflating "could not read the
        // reserve" with "the reserve is empty" is the whole bug being fixed here: one is an
        // outage, the other is an emergency, and they must not look alike.
        let word = parsed
            .constant_result
            .first()
            .ok_or_else(|| "trongrid balanceOf returned no constant_result".to_string())?;
        let trimmed = word.trim_start_matches('0');
        if trimmed.is_empty() {
            return Ok(0); // a genuine zero balance: all 64 nibbles were '0'
        }
        // A uint256 above i64::MAX cannot be a balance we can represent; refuse rather than
        // truncate a reserve figure into something plausible-looking.
        i64::from_str_radix(trimmed, 16)
            .map_err(|_| format!("balanceOf returned an unrepresentable uint256: 0x{word}"))
    }
}

/// Left-pad a Tron base58check address into the 32-byte ABI word `balanceOf(address)` expects.
///
/// Decodes with `with_check(Some(0x41))`, so a mistyped custody address fails the double-SHA256
/// checksum here instead of silently querying the balance of some other account — the same
/// reasoning as `payment_orchestrator::redemptions::is_valid_tron_address`.
fn abi_encode_address(address: &str) -> Result<String, String> {
    let bytes = bs58::decode(address)
        .with_check(Some(TRON_ADDRESS_VERSION))
        .into_vec()
        .map_err(|e| format!("custody address failed base58check: {e}"))?;
    if bytes.len() != DECODED_LEN_WITH_VERSION {
        return Err(format!("custody address decoded to {} bytes, want {DECODED_LEN_WITH_VERSION}", bytes.len()));
    }
    // Drop the 0x41 version byte: the ABI wants the bare 20-byte address, right-aligned in 32.
    Ok(format!("{:0>64}", hex::encode(&bytes[1..])))
}

/// Tron address version byte (base58check payload's first byte).
const TRON_ADDRESS_VERSION: u8 = 0x41;
/// Version byte (1) + address (20), the length `bs58` hands back once the 4-byte checksum is
/// stripped. `bs58` verifies checksum and version but not total length.
const DECODED_LEN_WITH_VERSION: usize = 21;

/// TronGrid caps `limit` at 200 and defaults it to 20. Ask for the max.
const TRC20_PAGE_LIMIT: &str = "200";

/// Only this TRC-20 event kind actually moves value.
const TRC20_TRANSFER_EVENT: &str = "Transfer";

/// Why a hard mismatch rejects and a transient failure never does (module doc's central
/// distinction, made into a type so the worker can't blur the two in a match arm).
#[derive(Debug)]
enum Evidence {
    /// All four conditions held: recipient, contract, amount, confirmed depth. Carries the
    /// OBSERVED amount (includes the discriminator + any overpay surplus) — the ledger must
    /// record this, not `amount_clt` (brief: ledgering the intended amount would corrupt the
    /// reconciliation cross-check).
    Pass { tx_id: String, observed_amount_usdt: i64 },
    /// Real data says this deposit is not what it claims. `rejected` + alert, never retried.
    HardMismatch(String),
    /// Our infrastructure, not the user's deposit. Leave `created`, retry next poll tick.
    Transient(String),
}

/// Pure evidence check against one candidate transfer, given the already-fetched confirmed
/// depth for it (confirmed depth is a separate TronGrid call from the transfer list, so it's
/// threaded in rather than fetched inside — keeps this testable with canned data, no IO).
///
/// All four conditions are checked unconditionally (no early-return short-circuit that could
/// skip one under some input) — a pass requires every one to hold.
fn check_transfer(
    transfer: &Trc20Transfer,
    confirmed: bool,
    deposit_address: &str,
    usdt_contract: &str,
    expected_amount_usdt: i64,
) -> Result<i64, String> {
    let recipient_ok = transfer.to == deposit_address;
    let contract_ok = transfer.token_info.address == usdt_contract;
    let observed_amount: i64 = transfer.value.parse().unwrap_or(0);
    let amount_ok = observed_amount >= expected_amount_usdt;
    let is_transfer = transfer.event_type == TRC20_TRANSFER_EVENT;

    if !is_transfer {
        return Err(format!(
            "transfer {} is a '{}' event, not a {TRC20_TRANSFER_EVENT} — no value moved",
            transfer.transaction_id, transfer.event_type
        ));
    }
    if !recipient_ok {
        return Err(format!(
            "transfer {} recipient '{}' != this intent's deposit address '{deposit_address}'",
            transfer.transaction_id, transfer.to
        ));
    }
    if !contract_ok {
        return Err(format!(
            "transfer {} token contract '{}' != expected usdt contract '{usdt_contract}'",
            transfer.transaction_id, transfer.token_info.address
        ));
    }
    if !amount_ok {
        return Err(format!(
            "transfer {} amount {observed_amount} below expected {expected_amount_usdt}",
            transfer.transaction_id
        ));
    }
    if !confirmed {
        return Err("not yet confirmed".to_string()); // caller maps this one to Transient, not HardMismatch
    }
    Ok(observed_amount)
}

struct DepositBackedIntent {
    id: Uuid,
    /// The discriminated pay amount, NOT `amount_clt`. `amount_clt` is deliberately absent from
    /// this struct: every user depositing the same round number shares it, so matching a transfer
    /// against it approves an intent on whoever's money happened to land first. Keeping it out of
    /// reach means no code path here can make that mistake again.
    expected_amount_usdt: Option<i64>,
    /// The address THIS deposit was expected at. Read from the intent, never from config: the
    /// verifier is the approver, and approving on evidence gathered at an address of its own
    /// choosing would defeat the point of the four-eyes split.
    deposit_address: Option<String>,
    deposit_tx_id: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// Evaluate one intent's evidence. Two paths per the plan:
/// - `deposit_tx_id` present: find that exact transfer in the trc20 list, check confirmed
///   depth via `/v1/transactions/{id}`.
/// - `deposit_tx_id` NULL (Bitcart's response lacked the hash): fallback match on the same
///   trc20 list — a transfer to custody of at least the expected amount. On match the caller
///   backfills `deposit_tx_id`.
async fn evaluate(
    client: &TronClient,
    config: &AppConfig,
    intent: &DepositBackedIntent,
) -> Evidence {
    // A deposit-backed intent with no expected pay amount cannot be verified against anything
    // trustworthy (migration 0003's CHECK plus api.rs's 400 should make this unreachable).
    // Transient, not HardMismatch: this is our own data missing, not evidence against the user's
    // deposit — the stuck sweep escalates it to a human rather than rejecting real money.
    let Some(expected_amount_usdt) = intent.expected_amount_usdt else {
        return Evidence::Transient(format!(
            "intent {} is deposit-backed but has no expected_amount_usdt — unverifiable",
            intent.id
        ));
    };

    // Same rule as expected_amount_usdt: deposit-backed with no address is unverifiable, and
    // Transient rather than HardMismatch because it is OUR data missing, not evidence against the
    // user's money. Migration 0004's CHECK plus api.rs's 400 should make this unreachable for new
    // rows; pre-per-address rows land here and escalate to a human via the stuck sweep.
    let Some(deposit_address) = intent.deposit_address.clone() else {
        return Evidence::Transient(format!(
            "intent {} is deposit-backed but has no deposit_address — unverifiable",
            intent.id
        ));
    };

    let transfers = match client.trc20_transfers(&deposit_address, &config.usdt_contract).await {
        Ok(t) => t,
        Err(e) => return Evidence::Transient(format!("trongrid trc20 list: {e}")),
    };

    match &intent.deposit_tx_id {
        Some(tx_id) => {
            let Some(transfer) = transfers.iter().find(|t| &t.transaction_id == tx_id) else {
                // Not visible yet in the confirmed-only list — could be genuinely absent
                // (wrong hash — a hard mismatch) or just not yet confirmed by Tron (transient).
                // We cannot distinguish "wrong" from "not yet visible" from absence alone, and
                // the brief's central rule is to never let ambiguity resolve to a rejection of
                // a real deposit — so an unmatched hash is treated as transient and retried.
                // A hash that is genuinely wrong then also converges on the 24h p1 stuck-sweep,
                // which puts a human in the loop rather than an automatic reject.
                return Evidence::Transient(format!("deposit_tx_id '{tx_id}' not found in confirmed transfer list yet"));
            };
            let confirmed = match client.transaction_confirmed(tx_id).await {
                Ok(c) => c,
                Err(e) => return Evidence::Transient(format!("trongrid get_transaction: {e}")),
            };
            match check_transfer(transfer, confirmed, &deposit_address, &config.usdt_contract, expected_amount_usdt) {
                Ok(observed) => Evidence::Pass { tx_id: tx_id.clone(), observed_amount_usdt: observed },
                Err(reason) if reason == "not yet confirmed" => Evidence::Transient(reason),
                Err(reason) => Evidence::HardMismatch(reason),
            }
        }
        None => {
            // Fallback, used when the detector recorded no tx hash. Per-address deposits make
            // this path materially STRONGER than it was.
            //
            // It used to require an EXACT amount match, because on one shared custody address the
            // amount was the only thing separating payers and `>=` would have matched a different
            // user's larger deposit. An address has exactly one intended payer, so `>=` is now
            // safe — and an overpayment with no hash verifies instead of ageing into manual review.
            //
            // The time bound is KEPT even though derivation indices are never reused. It costs
            // nothing and it still bounds the blast radius of an address being reused by mistake,
            // whether by a restored-from-mnemonic signer scanning old indices or by a future bug.
            // Removing a cheap guard because the current design makes it redundant is how the
            // redundancy stops being there when the design shifts again.
            //
            // `only_confirmed=true` already restricts this list to confirmed transfers, so no
            // separate depth call is needed here.
            let window = chrono::Duration::hours(config.deposit_match_window_hours);
            let earliest_ms = (intent.created_at - window).timestamp_millis();
            let candidate = transfers.iter().find(|t| {
                t.event_type == TRC20_TRANSFER_EVENT
                    && t.to == deposit_address
                    && t.token_info.address == config.usdt_contract
                    && t.value.parse::<i64>().ok().is_some_and(|v| v >= expected_amount_usdt)
                    && t.block_timestamp >= earliest_ms
            });
            match candidate {
                // Ledger the OBSERVED amount, not the expected one — an overpayment recorded as
                // the intended figure would build a permanent gap into the reconciliation
                // cross-check against custody.
                Some(transfer) => Evidence::Pass {
                    tx_id: transfer.transaction_id.clone(),
                    observed_amount_usdt: transfer.value.parse::<i64>().unwrap_or(expected_amount_usdt),
                },
                // No matching transfer yet is exactly "the deposit hasn't been observed
                // on-chain yet" — transient by construction, not a mismatch. Nothing here
                // is real evidence against the deposit; it is absence of evidence so far.
                None => Evidence::Transient("no matching confirmed transfer found yet".to_string()),
            }
        }
    }
}

/// Approve + `verified_at` + outbox row + `ledger::append_event("custody_deposit", ...)` in
/// ONE transaction (brief's exactly-once requirement). `WHERE status = 'created'` on the
/// UPDATE is what makes a rerun after a crash safe: if a prior run already flipped this intent
/// to `approved` (crashed AFTER commit, e.g. mid-outbox-processing later), this UPDATE affects
/// zero rows and the function returns Ok(false) — no second `approved_by` write, no second
/// outbox row, no second ledger event. If the crash was BEFORE commit, the whole transaction
/// never happened and this rerun performs the one real attempt. There is no window where a
/// rerun can observe a half-committed state, because all four writes share one transaction.
async fn approve_and_ledger(
    pool: &PgPool,
    intent_id: Uuid,
    observed_amount_usdt: i64,
    tx_id: &str,
) -> Result<bool, String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

    let updated = sqlx::query(
        "UPDATE mint_intents
         SET status = 'approved', approved_by = 'tron-verifier', verified_at = now(), updated_at = now()
         WHERE id = $1 AND status = 'created'",
    )
    .bind(intent_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    if updated.rows_affected() == 0 {
        // Already approved by a prior run (or otherwise no longer `created`) — rerun-safe
        // no-op. Roll back rather than commit an empty transaction; either is harmless here,
        // but rollback makes "nothing happened" true of the DB log too.
        tx.rollback().await.map_err(|e| e.to_string())?;
        return Ok(false);
    }

    sqlx::query("INSERT INTO chain_outbox (intent_id) VALUES ($1)")
        .bind(intent_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    // Custody enters the ledger ONLY here — independent verification is what makes the
    // backing-ratio breaker meaningful (brief). OBSERVED amount, not amount_clt: includes the
    // discriminator and any overpay surplus, so the ledger agrees with real custody and
    // backing sits slightly above par, not exactly at it.
    sqlx::query(
        "INSERT INTO treasury_events (kind, amount_clt, amount_usdt, intent_id, chain_tx_hash, description)
         VALUES ('custody_deposit', 0, $1, $2, $3, 'TronGrid-verified deposit')
         ON CONFLICT (intent_id, kind) WHERE intent_id IS NOT NULL DO NOTHING",
    )
    .bind(observed_amount_usdt)
    .bind(intent_id)
    .bind(tx_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(true)
}

/// Hard mismatch: `rejected` + alert. `WHERE status = 'created'` for the same rerun-safety
/// reason as approval — a mismatch found twice (e.g. the stuck-sweep and a normal pass both
/// touching an old intent) must not alert twice as if it were a new event, though duplicate
/// alerts here cost nothing worse than noise (unlike a duplicate custody event, which corrupts
/// a real number). Guarded anyway for consistency with the approve path.
async fn reject_and_alert(pool: &PgPool, intent_id: Uuid, reason: &str) -> Result<(), String> {
    let result = sqlx::query("UPDATE mint_intents SET status = 'rejected', updated_at = now() WHERE id = $1 AND status = 'created'")
        .bind(intent_id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    if result.rows_affected() == 1 {
        alert(pool, "warn", "tron_verifier", &format!("mint intent {intent_id} rejected: {reason}")).await;
    }
    Ok(())
}

/// Backfills `deposit_tx_id` for the fallback-match path, but must lose gracefully to the
/// unique index (brief's gap-closing requirement): if another intent's backfill already
/// claimed this `deposit_tx_id`, that is a HARD mismatch for THIS intent — the transfer is
/// real, but it already backs a different mint, so this intent's claim on it is false
/// evidence, not an infrastructure hiccup. Returns Ok(true) if the backfill (and by extension
/// this intent's approval) may proceed, Ok(false) if it lost the race and was rejected.
async fn backfill_deposit_tx_id(pool: &PgPool, intent_id: Uuid, tx_id: &str) -> Result<bool, String> {
    let result = sqlx::query("UPDATE mint_intents SET deposit_tx_id = $1, updated_at = now() WHERE id = $2")
        .bind(tx_id)
        .bind(intent_id)
        .execute(pool)
        .await;
    match result {
        Ok(_) => Ok(true),
        Err(e) => {
            let is_uq_deposit_tx = e
                .as_database_error()
                .and_then(|d| d.constraint())
                .map(|c| c == "uq_mint_intents_deposit_tx")
                .unwrap_or(false);
            if is_uq_deposit_tx {
                reject_and_alert(
                    pool,
                    intent_id,
                    &format!("deposit_tx_id '{tx_id}' already claimed by another mint intent — one transfer cannot back two mints"),
                )
                .await?;
                Ok(false)
            } else {
                Err(e.to_string())
            }
        }
    }
}

/// Every `created` intent with `client_ref IS NOT NULL` (deposit-backed — Plan B's manual
/// intents have `client_ref IS NULL` and are never touched by this worker) that has sat
/// unresolved past a threshold gets an alert. Threshold-gated so a healthy 2-minute-old
/// intent doesn't page anyone every poll tick; `warn` at 30 minutes, `p1` at 24 hours per the
/// brief. Pure age check against `created_at` — no new column needed, since "reschedule with
/// backoff" for this worker IS the poll loop itself: a transient failure simply leaves the
/// intent `created`, and the next tick retries it automatically.
async fn stuck_intent_sweep(pool: &PgPool, intents: &[DepositBackedIntent]) {
    let now = chrono::Utc::now();
    for intent in intents {
        let age = now - intent.created_at;
        if age > chrono::Duration::hours(24) {
            alert(pool, "p1", "tron_verifier", &format!("mint intent {} unresolved for over 24h", intent.id)).await;
        } else if age > chrono::Duration::minutes(30) {
            alert(pool, "warn", "tron_verifier", &format!("mint intent {} unresolved for over 30m", intent.id)).await;
        }
    }
}

/// One pass: fetch due intents, evaluate each, act on the verdict. Returns the count
/// approved. Errors from the DB fetch itself propagate (nothing to evaluate); errors from
/// evaluating or acting on ONE intent are alerted and skipped rather than aborting the batch.
pub async fn verify_once(pool: &PgPool, config: &AppConfig) -> Result<u32, String> {
    let client = TronClient::new(config.trongrid_url.clone(), config.trongrid_api_key.clone());
    let rows: Vec<(Uuid, Option<i64>, Option<String>, Option<String>, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(
            "SELECT id, expected_amount_usdt, deposit_address, deposit_tx_id, created_at FROM mint_intents
             WHERE status = 'created' AND client_ref IS NOT NULL
             ORDER BY created_at",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;

    let intents: Vec<DepositBackedIntent> = rows
        .into_iter()
        .map(|(id, expected_amount_usdt, deposit_address, deposit_tx_id, created_at)| DepositBackedIntent {
            id,
            expected_amount_usdt,
            deposit_address,
            deposit_tx_id,
            created_at,
        })
        .collect();

    stuck_intent_sweep(pool, &intents).await;

    let mut approved = 0u32;
    for intent in &intents {
        match evaluate(&client, config, intent).await {
            Evidence::Pass { tx_id, observed_amount_usdt } => {
                // Backfill first when this intent didn't already know its tx_id — losing the
                // race here (brief's gap) must reject THIS intent, never approve on a transfer
                // another intent already claimed.
                let may_approve = match &intent.deposit_tx_id {
                    Some(_) => true,
                    None => {
                        // Approving off the fallback match means we never got a tx hash from the
                        // payment processor and identified the payment by amount and timing
                        // alone. That is sound but strictly weaker than a hash, so it is worth an
                        // operator seeing every time it happens — a run of these means Bitcart
                        // stopped returning hashes, which is worth knowing before it is the only
                        // evidence behind a large share of the reserve.
                        alert(
                            pool,
                            "warn",
                            "tron_verifier",
                            &format!(
                                "intent {} verified via fallback amount match (no tx hash from processor); \
                                 claimed transfer {tx_id}",
                                intent.id
                            ),
                        )
                        .await;
                        match backfill_deposit_tx_id(pool, intent.id, &tx_id).await {
                            Ok(ok) => ok,
                            Err(e) => {
                                alert(pool, "warn", "tron_verifier", &format!("intent {}: backfill error: {e}", intent.id)).await;
                                false
                            }
                        }
                    }
                };
                if !may_approve {
                    continue;
                }
                match approve_and_ledger(pool, intent.id, observed_amount_usdt, &tx_id).await {
                    Ok(true) => approved += 1,
                    Ok(false) => {} // already approved by a prior run — rerun-safe no-op
                    Err(e) => {
                        alert(pool, "p1", "tron_verifier", &format!("intent {}: approval write failed: {e}", intent.id)).await;
                    }
                }
            }
            Evidence::HardMismatch(reason) => {
                if let Err(e) = reject_and_alert(pool, intent.id, &reason).await {
                    alert(pool, "p1", "tron_verifier", &format!("intent {}: reject write failed: {e}", intent.id)).await;
                }
            }
            // Deliberately no status write, no alert beyond what the stuck-sweep already
            // covers by age — this is the "never reject on an outage" guarantee made
            // structural: the only two arms above that touch `status` are Pass and
            // HardMismatch. Transient falls through to nothing but a debug log, and the
            // intent stays `created` for the next poll tick to retry.
            Evidence::Transient(reason) => {
                tracing::debug!(intent_id = %intent.id, reason, "tron_verifier: transient, retrying next tick");
            }
        }
    }
    Ok(approved)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real Nile custody address this was reproduced against, and its 20-byte body.
    const NILE_CUSTODY: &str = "TQwgeRaDt4FSJSsncmFNcbMNTfFpjvjwFX";

    /// `balanceOf(address)` takes the bare 20-byte address right-aligned in a 32-byte word. Get
    /// the padding or the version-byte strip wrong and the call silently reads a DIFFERENT
    /// account's balance — a wrong reserve figure, not an error.
    #[test]
    fn abi_encodes_the_custody_address_into_one_right_aligned_word() {
        let word = abi_encode_address(NILE_CUSTODY).expect("valid Tron address must encode");
        assert_eq!(word.len(), 64, "an ABI word is exactly 32 bytes / 64 hex chars, got: {word}");
        assert!(word.starts_with(&"0".repeat(24)), "20-byte address must be right-aligned: {word}");
        // The 0x41 version byte must NOT survive into the word.
        let body = &word[24..];
        assert_eq!(body.len(), 40);
        let expected = bs58::decode(NILE_CUSTODY).with_check(Some(0x41)).into_vec().unwrap();
        assert_eq!(body, hex::encode(&expected[1..]), "body must be the address without the 0x41 prefix");
    }

    /// A single mistyped character must fail the checksum here rather than encode into a valid-
    /// looking word for some other account.
    #[test]
    fn mistyped_custody_address_is_refused_not_encoded() {
        // Flip one MIDDLE character to a definitely-different base58 char. The first version of
        // this test replaced the last character with 'X' — which this address already ends in, so
        // it rebuilt the identical string, encoded fine, and asserted nothing.
        let mut chars: Vec<char> = NILE_CUSTODY.chars().collect();
        chars[10] = if chars[10] == 'a' { 'b' } else { 'a' };
        let bad: String = chars.into_iter().collect();
        assert_ne!(bad, NILE_CUSTODY, "the mutation must actually change the address");

        let err = abi_encode_address(&bad).expect_err("a bad checksum must not encode");
        assert!(err.contains("base58check"), "the error must name the checksum, got: {err}");
    }

    #[test]
    fn empty_address_is_refused() {
        assert!(abi_encode_address("").is_err());
    }

    fn transfer(to: &str, contract: &str, value: &str) -> Trc20Transfer {
        Trc20Transfer {
            transaction_id: "tx1".to_string(),
            to: to.to_string(),
            value: value.to_string(),
            token_info: TokenInfo { address: contract.to_string() },
            event_type: TRC20_TRANSFER_EVENT.to_string(),
            // Only the fallback match reads this; these unit tests exercise check_transfer,
            // which is the known-hash path.
            block_timestamp: 0,
        }
    }

    /// An `Approval` event carries a `value` and a `to` just like a Transfer does, but moves no
    /// tokens. Accepting one would approve a mint against a deposit that never arrived.
    #[test]
    fn approval_event_is_rejected_even_with_perfect_recipient_token_and_amount() {
        let mut t = transfer(CUSTODY, USDT, "1000000");
        t.event_type = "Approval".to_string();
        let err = check_transfer(&t, true, CUSTODY, USDT, 1_000_000).expect_err("an Approval must not pass");
        assert!(err.contains("Approval"), "the rejection must name the event kind, got: {err}");
    }

    /// A missing `type` must fail closed rather than being treated as a Transfer.
    #[test]
    fn absent_event_type_is_rejected() {
        let mut t = transfer(CUSTODY, USDT, "1000000");
        t.event_type = String::new();
        assert!(check_transfer(&t, true, CUSTODY, USDT, 1_000_000).is_err(), "an absent type must fail closed");
    }

    const CUSTODY: &str = "TCustodyAddressXXXXXXXXXXXXXXXXXXX";
    const USDT: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";

    #[test]
    fn all_four_conditions_hold_passes() {
        let t = transfer(CUSTODY, USDT, "1000000");
        assert_eq!(check_transfer(&t, true, CUSTODY, USDT, 1_000_000), Ok(1_000_000));
    }

    #[test]
    fn overpayment_still_passes_at_or_above_expected() {
        let t = transfer(CUSTODY, USDT, "1500000");
        assert_eq!(check_transfer(&t, true, CUSTODY, USDT, 1_000_000), Ok(1_500_000));
    }

    #[test]
    fn wrong_recipient_rejects() {
        let t = transfer("TSomeoneElse", USDT, "1000000");
        assert!(check_transfer(&t, true, CUSTODY, USDT, 1_000_000).is_err());
    }

    #[test]
    fn wrong_token_contract_rejects_even_with_right_amount() {
        // A worthless token with the right recipient and amount must not pass.
        let t = transfer(CUSTODY, "TWorthlessTokenContract", "1000000");
        assert!(check_transfer(&t, true, CUSTODY, USDT, 1_000_000).is_err());
    }

    #[test]
    fn amount_below_expected_rejects() {
        let t = transfer(CUSTODY, USDT, "999999");
        assert!(check_transfer(&t, true, CUSTODY, USDT, 1_000_000).is_err());
    }

    #[test]
    fn not_yet_confirmed_is_the_transient_marker_not_a_silent_pass() {
        let t = transfer(CUSTODY, USDT, "1000000");
        let err = check_transfer(&t, false, CUSTODY, USDT, 1_000_000).unwrap_err();
        assert_eq!(err, "not yet confirmed", "caller matches this exact string to route to Transient, not HardMismatch");
    }
}
