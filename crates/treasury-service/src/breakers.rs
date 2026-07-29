use sqlx::PgPool;
use uuid::Uuid;

use crate::configuration::AppConfig;
use crate::ledger::{self, alert};

#[derive(Debug)]
pub struct Denial {
    pub reason: String,
}

/// Approval-time gate: the intent is still `created`, so it is not yet counted in the
/// daily-cap sum (status IN approved|submitted|credited) — nothing to exclude.
pub async fn check_mint(pool: &PgPool, config: &AppConfig, amount_clt: i64) -> Result<(), Denial> {
    check_mint_inner(pool, config, amount_clt, None).await
}

/// Outbox-worker gate: re-checked AFTER the intent is already `approved`, so its own
/// amount is already inside the daily-cap sum. Excluding it here is what stops the
/// authoritative path from denying work it already authorised near the cap edge.
pub async fn check_mint_excluding(
    pool: &PgPool,
    config: &AppConfig,
    amount_clt: i64,
    intent_id: Uuid,
) -> Result<(), Denial> {
    check_mint_inner(pool, config, amount_clt, Some(intent_id)).await
}

/// The rolling 24h mint total that counts against `daily_mint_cap_clt` — every status past
/// `created` that represents a mint already authorised (approved/submitted/credited). Shared
/// by `check_mint_inner`'s plain gate and `/internal/reserve-status`'s `daily_headroom_clt`
/// (api.rs) so the two never compute this sum two different ways.
pub async fn daily_mint_total(pool: &PgPool) -> Result<i64, sqlx::Error> {
    let (day_total,): (i64,) = sqlx::query_as(
        // ::BIGINT — SUM(BIGINT) is NUMERIC, sqlx can't decode that into i64.
        "SELECT COALESCE(SUM(amount_clt), 0)::BIGINT FROM mint_intents
         WHERE status IN ('approved','submitted','credited')
           AND created_at > now() - interval '24 hours'",
    )
    .fetch_one(pool)
    .await?;
    Ok(day_total)
}

async fn check_mint_inner(
    pool: &PgPool,
    config: &AppConfig,
    amount_clt: i64,
    exclude_intent: Option<Uuid>,
) -> Result<(), Denial> {
    let deny = |reason: String| Denial { reason };

    let (halted, halt_reason): (bool, Option<String>) =
        sqlx::query_as("SELECT minting_halted, halt_reason FROM breaker_state")
            .fetch_one(pool)
            .await
            .map_err(|e| deny(format!("breaker read failed: {e}")))?;
    if halted {
        let r = format!("minting halted: {}", halt_reason.unwrap_or_default());
        alert(pool, "warn", "breakers", &r).await;
        return Err(deny(r));
    }

    if amount_clt > config.per_tx_mint_cap_clt {
        let r = format!(
            "amount {} exceeds per-transaction cap {} — escalate to multisig (post-pilot)",
            amount_clt, config.per_tx_mint_cap_clt
        );
        alert(pool, "warn", "breakers", &r).await;
        return Err(deny(r));
    }

    let day_total = match exclude_intent {
        None => daily_mint_total(pool).await,
        Some(id) => {
            sqlx::query_as::<_, (i64,)>(
                "SELECT COALESCE(SUM(amount_clt), 0)::BIGINT FROM mint_intents
                 WHERE status IN ('approved','submitted','credited')
                   AND created_at > now() - interval '24 hours'
                   AND id <> $1",
            )
            .bind(id)
            .fetch_one(pool)
            .await
            .map(|(t,)| t)
        }
    }
    .map_err(|e| deny(format!("cap read failed: {e}")))?;
    if day_total + amount_clt > config.daily_mint_cap_clt {
        let r = format!(
            "daily cap: {} + {} exceeds {}",
            day_total, amount_clt, config.daily_mint_cap_clt
        );
        alert(pool, "warn", "breakers", &r).await;
        return Err(deny(r));
    }

    let b = ledger::balances(pool).await.map_err(|e| deny(e.to_string()))?;
    let projected_liability = b.clt_liability + amount_clt;
    if projected_liability > 0 {
        let ratio_bps = (b.custody_usdt as i128 * 10_000) / projected_liability as i128;
        if ratio_bps < config.backing_halt_bps as i128 {
            // Auto-trip: below 100% backing nothing mints until a human investigates.
            let r = format!(
                "backing ratio {} bps below halt threshold {} — HALTING",
                ratio_bps, config.backing_halt_bps
            );
            let _ = sqlx::query(
                "UPDATE breaker_state SET minting_halted = TRUE, halt_reason = $1, updated_at = now()",
            )
            .bind(&r)
            .execute(pool)
            .await;
            alert(pool, "p1", "breakers", &r).await;
            return Err(deny(r));
        }
    }

    let last: Option<(String,)> = sqlx::query_as(
        "SELECT status FROM reconciliation_runs
         WHERE run_at > now() - interval '48 hours'
         ORDER BY run_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| deny(format!("reconciliation read failed: {e}")))?;
    match last {
        Some((status,)) if status != "mismatch" => Ok(()),
        Some(_) => {
            let r = "last reconciliation was a mismatch".to_string();
            alert(pool, "warn", "breakers", &r).await;
            Err(deny(r))
        }
        None => {
            let r = "no reconciliation run in 48h — refusing to mint blind".to_string();
            alert(pool, "warn", "breakers", &r).await;
            Err(deny(r))
        }
    }
}

pub async fn manual_halt(pool: &PgPool, reason: &str, actor: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE breaker_state SET minting_halted = TRUE, halt_reason = $1, updated_at = now()",
    )
    .bind(reason)
    .execute(pool)
    .await?;
    alert(pool, "warn", "breakers", &format!("MANUAL HALT by {actor}: {reason}")).await;
    Ok(())
}

pub async fn manual_resume(pool: &PgPool, actor: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE breaker_state SET minting_halted = FALSE, halt_reason = NULL, updated_at = now()",
    )
    .execute(pool)
    .await?;
    alert(pool, "warn", "breakers", &format!("MANUAL RESUME by {actor}")).await;
    Ok(())
}
