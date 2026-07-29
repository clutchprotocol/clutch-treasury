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
//! TronGrid endpoint shapes below are ASSUMED from the plan doc and TronGrid's public,
//! widely-documented v1 API, NOT verified against a live call — flagged in the report per the
//! brief's instruction. The two calls:
//!   - `GET /v1/accounts/{address}/transactions/trc20?only_confirmed=true&contract_address={c}`
//!     — TRC-20 transfer history for the custody address. Used for BOTH the has-tx_id path
//!     (find and validate this specific transfer) and the fallback path (find any transfer
//!     that matches by amount when Bitcart never returned a hash).
//!   - `GET /v1/transactions/{tx_id}` — used only to confirm the transaction's confirmed depth
//!     when `deposit_tx_id` is known, since the trc20 list alone (per TronGrid docs) does not
//!     reliably expose a confirmations count.

use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::configuration::AppConfig;
use crate::ledger::alert;

/// A single TRC-20 transfer as TronGrid's trc20 endpoint reports it. Field names ASSUMED per
/// TronGrid's public v1 docs (`transaction_id`, `token_info.address`, `to`, `value` as a
/// decimal STRING in the token's base unit — USDT-TRC20 is 6 decimals, same scale as CLT at
/// par, so `value` parses directly as micro-USDT with no rate conversion). Never verified
/// against a live TronGrid call.
#[derive(Debug, Deserialize)]
struct Trc20Transfer {
    transaction_id: String,
    to: String,
    value: String,
    token_info: TokenInfo,
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

/// ASSUMED shape of `/v1/transactions/{id}` — used only for `ret[0].contractRet` (whether the
/// underlying contract call succeeded, TronGrid's usual place for this) is NOT what we need
/// here; what we need is confirmed depth, which TronGrid exposes via the separate
/// `/wallet/gettransactioninfobyid`-style `confirmed` boolean this endpoint's docs describe on
/// the top-level response. Modeled minimally — only the field this module reads.
#[derive(Debug, Deserialize)]
struct TronTransaction {
    #[serde(default)]
    confirmed: bool,
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
            .query(&[("only_confirmed", "true"), ("contract_address", usdt_contract)])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("trongrid trc20 list failed: {status} {text}"));
        }
        let parsed: Trc20TransfersResponse = resp.json().await.map_err(|e| e.to_string())?;
        Ok(parsed.data)
    }

    async fn transaction_confirmed(&self, tx_id: &str) -> Result<bool, String> {
        let url = format!("{}/v1/transactions/{}", self.base_url, tx_id);
        let resp = self
            .http
            .get(&url)
            .header("TRON-PRO-API-KEY", &self.api_key)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("trongrid get_transaction failed: {status} {text}"));
        }
        let parsed: TronTransaction = resp.json().await.map_err(|e| e.to_string())?;
        Ok(parsed.confirmed)
    }

    /// Reconciliation's cross-check read (spec's fourth-source stand-in until a PSP exists):
    /// the custody address's current TRC-20 USDT balance, straight from TronGrid rather than
    /// our own ledger. A mismatch against `custody_reported` is logged as an early smell, not
    /// wired into the breaker (brief: "not a halt").
    ///
    /// ASSUMED shape of `/v1/accounts/{address}` — `trc20` is documented as an array of
    /// single-key maps `{contract_address: balance_string}`; balance is base-unit decimal
    /// string, same 6-decimal scale as everywhere else in this file.
    pub async fn get_custody_balance(&self, custody_address: &str, usdt_contract: &str) -> Result<i64, String> {
        #[derive(Deserialize)]
        struct AccountResponse {
            #[serde(default)]
            data: Vec<AccountData>,
        }
        #[derive(Deserialize)]
        struct AccountData {
            #[serde(default)]
            trc20: Vec<std::collections::HashMap<String, String>>,
        }

        let url = format!("{}/v1/accounts/{}", self.base_url, custody_address);
        let resp = self
            .http
            .get(&url)
            .header("TRON-PRO-API-KEY", &self.api_key)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("trongrid get_account failed: {status} {text}"));
        }
        let parsed: AccountResponse = resp.json().await.map_err(|e| e.to_string())?;
        let balance = parsed
            .data
            .first()
            .and_then(|d| d.trc20.iter().find_map(|m| m.get(usdt_contract)))
            .map(|v| v.parse::<i64>().unwrap_or(0))
            .unwrap_or(0);
        Ok(balance)
    }
}

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
    custody_tron_address: &str,
    usdt_contract: &str,
    expected_amount_usdt: i64,
) -> Result<i64, String> {
    let recipient_ok = transfer.to == custody_tron_address;
    let contract_ok = transfer.token_info.address == usdt_contract;
    let observed_amount: i64 = transfer.value.parse().unwrap_or(0);
    let amount_ok = observed_amount >= expected_amount_usdt;

    if !recipient_ok {
        return Err(format!("transfer {} recipient '{}' != custody '{custody_tron_address}'", transfer.transaction_id, transfer.to));
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
    amount_clt: i64,
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
    let transfers = match client.trc20_transfers(&config.custody_tron_address, &config.usdt_contract).await {
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
            match check_transfer(transfer, confirmed, &config.custody_tron_address, &config.usdt_contract, intent.amount_clt) {
                Ok(observed) => Evidence::Pass { tx_id: tx_id.clone(), observed_amount_usdt: observed },
                Err(reason) if reason == "not yet confirmed" => Evidence::Transient(reason),
                Err(reason) => Evidence::HardMismatch(reason),
            }
        }
        None => {
            // Fallback: any transfer to custody at or above the expected amount. The
            // `only_confirmed=true` query param already restricts this list to confirmed
            // transfers, so there is no separate confirmed-depth call to make here.
            let candidate = transfers.iter().find(|t| {
                t.to == config.custody_tron_address
                    && t.token_info.address == config.usdt_contract
                    && t.value.parse::<i64>().unwrap_or(0) >= intent.amount_clt
            });
            match candidate {
                Some(transfer) => {
                    let observed = transfer.value.parse().unwrap_or(0);
                    Evidence::Pass { tx_id: transfer.transaction_id.clone(), observed_amount_usdt: observed }
                }
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
    let rows: Vec<(Uuid, i64, Option<String>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT id, amount_clt, deposit_tx_id, created_at FROM mint_intents
         WHERE status = 'created' AND client_ref IS NOT NULL
         ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let intents: Vec<DepositBackedIntent> = rows
        .into_iter()
        .map(|(id, amount_clt, deposit_tx_id, created_at)| DepositBackedIntent { id, amount_clt, deposit_tx_id, created_at })
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
                    None => match backfill_deposit_tx_id(pool, intent.id, &tx_id).await {
                        Ok(ok) => ok,
                        Err(e) => {
                            alert(pool, "warn", "tron_verifier", &format!("intent {}: backfill error: {e}", intent.id)).await;
                            false
                        }
                    },
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

    fn transfer(to: &str, contract: &str, value: &str) -> Trc20Transfer {
        Trc20Transfer {
            transaction_id: "tx1".to_string(),
            to: to.to_string(),
            value: value.to_string(),
            token_info: TokenInfo { address: contract.to_string() },
        }
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
