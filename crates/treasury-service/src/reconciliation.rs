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
    pub custody_reported: i64,
    /// Plan C T5: a live TronGrid read of the custody address's USDT balance, independent of
    /// `custody_reported` (which is the LEDGER's custody balance). `None` when the TronGrid
    /// call itself failed — an early smell gets logged as `null`, not a synthetic zero that
    /// would read as "custody drained to nothing" and could confuse a human reading the report.
    pub trongrid_balance: Option<i64>,
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
    // trongrid_balance is a cross-check column ONLY — it plays no part in any branch below.
    // A mismatch against custody_reported is an early smell a human reads off the report,
    // never a halt condition (brief: "not wired into the breaker").
    let detail = json!({
        "onchain_supply": s.onchain_supply,
        "genesis_allocation": s.genesis_allocation,
        "treasury_minted": treasury_minted as i64,
        "ledger_liability": s.ledger_liability,
        "custody_reported": s.custody_reported,
        "trongrid_balance": s.trongrid_balance,
    });
    if treasury_minted > s.ledger_liability as i128 {
        ("mismatch", detail) // unbacked CLT on-chain
    } else if s.custody_reported < s.ledger_liability {
        ("mismatch", detail) // reserve below liability
    } else if treasury_minted < s.ledger_liability as i128 {
        ("over_backed_drift", detail) // plain burns — benign, logged
    } else {
        ("ok", detail)
    }
}

/// Record a judged run and trip the breaker on mismatch. Split from gathering so
/// tests drive it directly.
pub async fn record(pool: &PgPool, s: &Sources) -> Result<String, String> {
    let (status, detail) = judge(s);
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
        alert(pool, "warn", "reconciliation", &format!("over-backed drift: {}", detail)).await;
    }
    Ok(status.to_string())
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
    custody_reported: i64,
    genesis_allocation: u64,
    config: &AppConfig,
) -> Result<String, String> {
    let info = node.get_chain_info().await?;
    let balances = ledger::balances(pool).await.map_err(|e| e.to_string())?;
    let in_flight = in_flight_mint_amount(pool).await.map_err(|e| e.to_string())?;

    // Cross-check only (brief: "not a halt") — a TronGrid failure here must not stop
    // reconciliation itself from running and judging the other three sources. Logged as
    // `None`/`null` in `detail.trongrid_balance` rather than aborting the whole run.
    let client = TronClient::new(config.trongrid_url.clone(), config.trongrid_api_key.clone());
    let trongrid_balance = match client.get_custody_balance(&config.custody_tron_address, &config.usdt_contract).await {
        Ok(bal) => Some(bal),
        Err(e) => {
            tracing::warn!("reconciliation: trongrid custody balance read failed: {}", e);
            None
        }
    };

    record(
        pool,
        &Sources {
            onchain_supply: info.total_supply,
            genesis_allocation,
            ledger_liability: balances.clt_liability + in_flight,
            custody_reported,
            trongrid_balance,
        },
    )
    .await
}
