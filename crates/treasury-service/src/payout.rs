use sqlx::PgPool;
use uuid::Uuid;

use crate::ledger::alert;

/// Boundary for the outbound USDT leg. `StubRail` is the only implementor today; the real
/// Tron TRC-20 rail (transaction construction, a hot payout key in KMS, TRX energy costs)
/// is Plan C follow-on research, not this task. Kept as a trait anyway — same migration-path
/// reasoning as `ChainSigner` in clutch-chain: one implementor now, a swappable seam later.
/// See docs/keys.md — the payout key is a THIRD key, distinct from mint and from custody,
/// and does not exist yet.
#[async_trait::async_trait]
pub trait PayoutRail: Send + Sync {
    async fn send_usdt(&self, to_address: &str, amount_usdt: i64) -> Result<String, String>;
}

pub struct StubRail;

#[async_trait::async_trait]
impl PayoutRail for StubRail {
    async fn send_usdt(&self, to_address: &str, amount_usdt: i64) -> Result<String, String> {
        let payout_ref = format!("stub:{}", Uuid::new_v4());
        tracing::info!(to_address, amount_usdt, payout_ref, "StubRail: fake USDT payout recorded");
        Ok(payout_ref)
    }
}

/// Picks due `payout_pending` intents and pays each against its ALREADY-CONFIRMED burn.
/// Burn first, payout second, always — this worker only ever sees intents whose burn is
/// already ledgered (`watcher::confirm_burn` is the sole path into `payout_pending`), so
/// there is no code path here that pays before the chain leg is final.
///
/// The halted breaker state gates this too, not just minting: a treasury that stopped
/// minting because its books disagree must not ship money out the other door either.
///
/// A failed `send_usdt` NEVER un-burns (there is no such operation) — it alerts P1 and
/// leaves the intent `payout_pending` for retry or manual multisig intervention. Orphaning
/// a burn (no payout, no alert) is the one outcome this function must never produce.
pub async fn drain_once(pool: &PgPool, rail: &dyn PayoutRail) -> Result<u32, String> {
    let (halted, halt_reason): (bool, Option<String>) =
        sqlx::query_as("SELECT minting_halted, halt_reason FROM breaker_state")
            .fetch_one(pool)
            .await
            .map_err(|e| e.to_string())?;
    if halted {
        tracing::warn!(halt_reason, "payouts blocked: treasury is halted");
        return Ok(0);
    }

    let rows: Vec<(Uuid, String, i64)> = sqlx::query_as(
        "SELECT id, payout_address, amount_clt FROM redemption_intents
         WHERE status = 'payout_pending' ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut processed = 0u32;
    for (intent_id, payout_address, amount_clt) in rows {
        // 1:1 CLT<->USDT base units at par — spread/fee modelling is an orchestrator
        // concern (Plan C), not this stub.
        match rail.send_usdt(&payout_address, amount_clt).await {
            Ok(payout_ref) => {
                pay_intent(pool, intent_id, amount_clt, &payout_ref).await?;
                processed += 1;
            }
            Err(e) => {
                // Retryable: leave `payout_pending` exactly as-is. The burn already
                // happened and is final; this failure can only cost us a retry, never
                // orphan the user's money.
                alert(
                    pool,
                    "p1",
                    "payout",
                    &format!("redemption {intent_id}: payout failed, staying payout_pending for retry: {e}"),
                )
                .await;
            }
        }
    }
    Ok(processed)
}

/// Same shape as `watcher::confirm_burn`'s ledger write: one atomic transaction inserting
/// the `treasury_events` row and flipping intent status together (rather than calling
/// `ledger::append_event`, which only takes a bare `&PgPool` and can't join this
/// transaction).
async fn pay_intent(pool: &PgPool, intent_id: Uuid, amount_clt: i64, payout_ref: &str) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO treasury_events (kind, amount_clt, amount_usdt, intent_id, chain_tx_hash, description)
         VALUES ('custody_withdrawal', 0, $1, $2, NULL, 'redemption payout')
         ON CONFLICT (intent_id, kind) WHERE intent_id IS NOT NULL DO NOTHING",
    )
    .bind(amount_clt)
    .bind(intent_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query(
        "UPDATE redemption_intents SET status = 'paid', payout_ref = $2, updated_at = now() WHERE id = $1",
    )
    .bind(intent_id)
    .bind(payout_ref)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}
