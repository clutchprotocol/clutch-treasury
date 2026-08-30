use clutch_chain::node_client::NodeClient;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;

use crate::configuration::AppConfig;
use crate::ledger::{self, alert};
use crate::tron_verifier::TronClient;

/// The four inputs `judge` compares. Spec §5 wants FOUR records to agree daily: chain supply,
/// treasury ledger, custody balances, and PSP/gateway settlement reports. This is three —
/// there is no `psp_reported` field because no payment gateway exists yet (the Bitcart on-ramp
/// is a later plan). When it lands, its settlement figure joins this struct as a fourth source;
/// until then, three records are the complete set this service can check.
#[derive(Debug)]
pub struct Sources {
    pub onchain_supply: u64,
    pub genesis_allocation: u64,
    pub ledger_liability: i64,
    /// The reserve, read from chain: the treasury address plus every address still holding an
    /// unswept deposit, summed over the USDT contract.
    ///
    /// This used to be `config.custody_stub_balance_usdt`, a hand-maintained number that defaulted
    /// to 0 — so the first credited deposit made `custody_reported < ledger_liability` true and
    /// halted minting on a system that was over-collateralized a hundred times over. The figure
    /// that proved it was fine was computed in the same function and deliberately excluded from
    /// the decision. There is no stub any more; if the reserve cannot be read, no run is recorded.
    ///
    /// Units line up exactly: USDT has 6 decimals and 1 USD is 1,000,000 CLT, so one micro-USDT is
    /// one CLT and this compares directly against `ledger_liability`.
    pub custody_reported: i64,
}

/// Pure judgement — testable without IO. Spec §5: every confirmed intent has exactly
/// one on-chain credit; any under-backing is a P1, not a dashboard metric.
///
/// Known accepted false positive: a mint that is on-chain but not yet watcher-credited
/// makes `treasury_minted > ledger_liability` for the few blocks between submission and
/// the ledger recording the credit. A reconciliation run landing in that window would
/// otherwise P1-halt on healthy behaviour. `run_once` mitigates this by adding the sum of
/// `submitted`-status mint intent amounts to `ledger_liability` before calling `judge`.
pub fn judge(s: &Sources) -> (&'static str, serde_json::Value) {
    let treasury_minted = s.onchain_supply as i128 - s.genesis_allocation as i128;
    let detail = json!({
        "onchain_supply": s.onchain_supply,
        "genesis_allocation": s.genesis_allocation,
        "treasury_minted": treasury_minted as i64,
        "ledger_liability": s.ledger_liability,
        "custody_reported": s.custody_reported,
    });
    if treasury_minted > s.ledger_liability as i128 {
        ("mismatch", detail) // unbacked CLT on-chain
    } else if s.custody_reported < s.ledger_liability {
        ("mismatch", detail) // reserve below liability
    } else if treasury_minted < s.ledger_liability as i128 {
        // Benign ONLY while transient. A mint moves chain supply and liability together, and so
        // does a burn, so in a settled system these two figures are equal — a gap means one side
        // recorded something the other has not yet. That is ordinary for a few seconds.
        //
        // A gap that PERSISTS is CLT the ledger counts as issued which does not exist on chain:
        // someone is owed money they do not hold. `record` escalates on persistence; this arm
        // cannot tell the difference on its own because it sees a single run.
        ("over_backed_drift", detail)
    } else {
        ("ok", detail)
    }
}

/// Record a judged run and trip the breaker on mismatch. Split from gathering so
/// tests drive it directly.
pub async fn record(pool: &PgPool, s: &Sources) -> Result<String, String> {
    let (status, detail) = judge(s);
    // Read the previous drifting run BEFORE inserting this one, or the lookup finds this very run
    // and every first sighting reports itself as persistent.
    let prior_gap = previous_drift_gap(pool).await;
    sqlx::query(
        "INSERT INTO reconciliation_runs
         (onchain_supply, genesis_allocation, ledger_liability, custody_reported, status, detail)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(s.onchain_supply as i64)
    .bind(s.genesis_allocation as i64)
    .bind(s.ledger_liability)
    .bind(s.custody_reported)
    .bind(status)
    .bind(&detail)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    if status == "mismatch" {
        sqlx::query(
            "UPDATE breaker_state SET minting_halted = TRUE,
             halt_reason = 'reconciliation mismatch', updated_at = now()",
        )
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
        alert(pool, "p1", "reconciliation", &format!("MISMATCH: {}", detail)).await;
    } else if status == "over_backed_drift" {
        // Transient drift is a timing artifact and a warn. The same gap two runs running is not:
        // it means minted CLT the ledger recorded never reached the chain, or was destroyed after
        // it did — which is precisely what a chain reset does. Stage lost a $10 mint that way and
        // this arm reported it as benign, because a single run cannot tell a race from a loss.
        let gap = s.ledger_liability as i128 - (s.onchain_supply as i128 - s.genesis_allocation as i128);
        let persistent = prior_gap.is_some_and(|prev| prev >= gap);
        if persistent {
            alert(
                pool,
                "p1",
                "reconciliation",
                &format!(
                    "PERSISTENT under-issuance of {gap} CLT: the ledger counts more as issued than \
                     the chain holds, across consecutive runs. Not a timing race — a mint the ledger \
                     recorded is missing on chain. Detail: {detail}"
                ),
            )
            .await;
        } else {
            alert(pool, "warn", "reconciliation", &format!("over-backed drift: {}", detail)).await;
        }
    }
    Ok(status.to_string())
}

/// The under-issuance gap from the PREVIOUS run, if that run also drifted.
///
/// `None` when there is no prior run or it was not a drift — either way this run's gap has not been
/// seen before, so it is treated as transient. Read from `detail` rather than recomputed: those are
/// the figures that run actually judged.
async fn previous_drift_gap(pool: &PgPool) -> Option<i128> {
    // Turbofish on query_as, not an annotation on the whole chain: fetch_optional already returns
    // the Option, so annotating the result as Option<(Value,)> asks sqlx to decode a row INTO an
    // Option and does not compile.
    let row = sqlx::query_as::<_, (serde_json::Value,)>(
        "SELECT detail FROM reconciliation_runs
         WHERE status = 'over_backed_drift' ORDER BY run_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()?;
    let d = row.0;
    let liability = d.get("ledger_liability")?.as_i64()? as i128;
    let minted = d.get("treasury_minted")?.as_i64()? as i128;
    Some(liability - minted)
}

/// Sum of `submitted`-status mint intents: on-chain but not yet watcher-credited into
/// `ledger_liability`. See the false-positive note on `judge`.
async fn in_flight_mint_amount(pool: &PgPool) -> Result<i64, sqlx::Error> {
    let (sum,): (Option<i64>,) = sqlx::query_as(
        "SELECT SUM(amount_clt)::BIGINT FROM mint_intents WHERE status = 'submitted'",
    )
    .fetch_one(pool)
    .await?;
    Ok(sum.unwrap_or(0))
}

pub async fn run_once(
    pool: &PgPool,
    node: &Arc<NodeClient>,
    genesis_allocation: u64,
    config: &AppConfig,
) -> Result<String, String> {
    let info = node.get_chain_info().await?;
    let balances = ledger::balances(pool).await.map_err(|e| e.to_string())?;
    let in_flight = in_flight_mint_amount(pool).await.map_err(|e| e.to_string())?;

    let client = TronClient::new(config.trongrid_url.clone(), config.trongrid_api_key.clone());
    // Every address that still holds an unswept deposit. Read from our OWN rows — the orchestrator's
    // database is not reachable from here, and mint_intents.deposit_address is the record of which
    // addresses this service has ever approved a deposit at.
    let unswept: Vec<String> = sqlx::query_scalar(
        "SELECT deposit_address FROM mint_intents
         WHERE deposit_address IS NOT NULL AND swept_at IS NULL AND status IN ('approved', 'submitted', 'credited')",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_else(|e| {
        // Not fatal: the sum below just covers fewer addresses, and this is a cross-check.
        tracing::warn!("reconciliation: could not list unswept deposit addresses: {e}");
        Vec::new()
    });

    // An unreadable reserve aborts the run rather than recording one.
    //
    // Not a synthetic zero, which would read as "custody drained to nothing" and halt minting on a
    // TronGrid hiccup. Not a recorded `error` row either: the mint gate accepts any status that is
    // not `mismatch`, so an error row would license minting against a reserve nobody verified.
    //
    // Returning Err leaves NO row, which is the honest state — the run did not happen. main.rs
    // retries in 30 seconds, and if the outage persists the existing "no reconciliation run in 48h
    // — refusing to mint blind" gate stops minting on its own.
    let custody_reported = client
        .get_reserve_balance(
            &config.custody_tron_address,
            &unswept,
            &config.payout_float_address,
            &config.usdt_contract,
        )
        .await
        .map_err(|e| format!("reserve balance unreadable, not recording a run: {e}"))?;

    record(
        pool,
        &Sources {
            onchain_supply: info.total_supply,
            genesis_allocation,
            ledger_liability: balances.clt_liability + in_flight,
            custody_reported,
        },
    )
    .await
}
