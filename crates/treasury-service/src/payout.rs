use sqlx::PgPool;
use uuid::Uuid;

use crate::configuration::AppConfig;
use crate::ledger::alert;

/// What the signer reported for one payout.
///
/// The division that matters is `Refused` vs `Ambiguous`, and it is not a stylistic one: `Refused`
/// means the signer told us it did not broadcast, so retrying is free. `Ambiguous` means we do not
/// know, and a TRC-20 transfer has no memo to dedupe against, so retrying risks paying twice for a
/// burn that only happened once. Never widen `Refused` to cover a case you are not certain about.
#[derive(Debug, PartialEq)]
pub enum PayoutReply {
    Paid { tx_id: String },
    FloatDry { float_address: String, have_usdt: i64, need_usdt: i64 },
    CapExceeded { limit_usdt: i64 },
    /// The float was topped up with TRX and the transfer has not happened yet. Retryable.
    NeedsTrx,
    /// The signer answered, and its answer proves nothing was broadcast. Retryable.
    Refused(String),
    /// No usable answer. MAY have broadcast. Not retryable by any automation.
    Ambiguous(String),
}

/// The signer boundary, as a trait so the worker is testable without a live service or real keys —
/// same reasoning as `SweepSigner` in sweeper.rs.
#[async_trait::async_trait]
pub trait PayoutSigner: Send + Sync {
    async fn pay(&self, intent_id: Uuid, to: &str, amount_usdt: i64) -> PayoutReply;
}

/// The real signer, over HTTP. Modelled on `sweeper::HttpSigner` — same shape, same reasoning.
pub struct HttpPayoutSigner {
    pub http: reqwest::Client,
    pub base_url: String,
    pub token: String,
}

#[async_trait::async_trait]
impl PayoutSigner for HttpPayoutSigner {
    async fn pay(&self, intent_id: Uuid, to: &str, amount_usdt: i64) -> PayoutReply {
        let resp = self
            .http
            .post(format!("{}/internal/payout", self.base_url))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "intent_id": intent_id.to_string(),
                "to": to,
                "amount_usdt": amount_usdt,
            }))
            .send()
            .await;

        let body: serde_json::Value = match resp {
            Ok(r) if r.status().is_success() => match r.json().await {
                Ok(v) => v,
                // Success status, unreadable body: the signer may well have broadcast.
                Err(e) => return PayoutReply::Ambiguous(format!("unreadable signer response: {e}")),
            },
            // 400 is the signer rejecting the request shape before doing anything. Every other
            // status could have followed a broadcast, so it is ambiguous, not refused.
            Ok(r) if r.status() == reqwest::StatusCode::BAD_REQUEST => {
                return PayoutReply::Refused("signer rejected the request as malformed".into())
            }
            Ok(r) => return PayoutReply::Ambiguous(format!("signer returned {}", r.status())),
            // Connection refused and DNS failures are safe, but a timeout is not distinguishable
            // here from a request that landed. Treat the whole class as ambiguous.
            Err(e) => return PayoutReply::Ambiguous(format!("signer unreachable or timed out: {e}")),
        };

        match body["status"].as_str() {
            Some("paid") => match body["tx_id"].as_str() {
                Some(tx) => PayoutReply::Paid { tx_id: tx.to_string() },
                // Claimed success without naming the transaction. It may have broadcast and we
                // cannot point at it, which is the definition of ambiguous.
                None => PayoutReply::Ambiguous("signer reported paid with no tx_id".into()),
            },
            Some("float_dry") => PayoutReply::FloatDry {
                float_address: body["float_address"].as_str().unwrap_or("unknown").to_string(),
                have_usdt: body["have_usdt"].as_i64().unwrap_or(0),
                need_usdt: body["need_usdt"].as_i64().unwrap_or(0),
            },
            Some("cap_exceeded") => {
                PayoutReply::CapExceeded { limit_usdt: body["limit_usdt"].as_i64().unwrap_or(0) }
            }
            Some("needs_trx") => PayoutReply::NeedsTrx,
            // An unknown status from a newer signer might describe a broadcast this version does
            // not understand. Ambiguous, never Refused.
            other => PayoutReply::Ambiguous(format!("unrecognised signer status {other:?}")),
        }
    }
}

/// Pays each due `payout_pending` intent against its ALREADY-CONFIRMED burn.
///
/// Burn first, payout second, always — `watcher::confirm_burn` is the sole path into
/// `payout_pending`, so nothing here can pay before the chain leg is final.
///
/// The halted breaker gates this too, not just minting: a treasury that stopped minting because its
/// books disagree must not ship money out the other door either.
///
/// Each intent is CLAIMED (`payout_submitted`, committed) before the signer is called, so a crash
/// mid-call leaves a state that is visibly in-flight rather than one that looks retryable. Only a
/// reply that PROVES no broadcast returns it to `payout_pending`. Everything else stays claimed and
/// alerts: orphaning a burn is the one outcome this function must never produce, and paying one
/// burn twice is the mirror-image sin.
pub async fn drain_once(
    pool: &PgPool,
    config: &AppConfig,
    signer: &dyn PayoutSigner,
) -> Result<u32, String> {
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

    let mut day_total = daily_payout_total(pool).await.map_err(|e| e.to_string())?;
    let mut processed = 0u32;

    for (intent_id, payout_address, amount_clt) in rows {
        if day_total + amount_clt > config.daily_payout_cap_clt {
            tracing::warn!(%intent_id, day_total, cap = config.daily_payout_cap_clt,
                "daily payout cap reached; remaining intents wait for the window to roll");
            break;
        }

        // CLAIM FIRST. Committed before the call, so a crash between here and the reply is
        // indistinguishable from a lost response — which is correct, because it is one.
        let claimed = sqlx::query(
            "UPDATE redemption_intents SET status = 'payout_submitted', updated_at = now()
             WHERE id = $1 AND status = 'payout_pending'",
        )
        .bind(intent_id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
        if claimed.rows_affected() == 0 {
            // Another worker took it between the SELECT and here.
            continue;
        }

        // 1:1 CLT<->USDT base units at par — spread/fee modelling is an orchestrator concern.
        match signer.pay(intent_id, &payout_address, amount_clt).await {
            PayoutReply::Paid { tx_id } => {
                sqlx::query("UPDATE redemption_intents SET payout_ref = $2, updated_at = now() WHERE id = $1")
                    .bind(intent_id)
                    .bind(&tx_id)
                    .execute(pool)
                    .await
                    .map_err(|e| e.to_string())?;
                day_total += amount_clt;
                processed += 1;
            }
            // Proven non-broadcast: hand it back for a later pass.
            reply @ (PayoutReply::FloatDry { .. }
            | PayoutReply::CapExceeded { .. }
            | PayoutReply::NeedsTrx
            | PayoutReply::Refused(_)) => {
                sqlx::query(
                    "UPDATE redemption_intents SET status = 'payout_pending', updated_at = now() WHERE id = $1",
                )
                .bind(intent_id)
                .execute(pool)
                .await
                .map_err(|e| e.to_string())?;
                alert(pool, "p1", "payout",
                    &format!("redemption {intent_id}: payout refused ({reply:?}), returned to payout_pending")).await;
            }
            // May or may not have broadcast. Stays claimed, forever, until a human resolves it.
            PayoutReply::Ambiguous(msg) => {
                alert(pool, "p1", "payout", &format!(
                    "redemption {intent_id}: payout outcome UNKNOWN ({msg}). Left payout_submitted and \
                     NOT retried — retrying could pay this burn twice. Check the payout float's \
                     outbound transfers for a transfer of {amount_clt} to {payout_address}, then \
                     either set payout_ref to that tx and let confirmation finish it, or return the \
                     intent to payout_pending."
                )).await;
            }
        }
    }
    Ok(processed)
}

/// The rolling 24h payout total against `daily_payout_cap_clt`. Counts every status at or past
/// submission, mirroring `breakers::daily_mint_total` — an in-flight payout is spent budget.
async fn daily_payout_total(pool: &PgPool) -> Result<i64, sqlx::Error> {
    let (total,): (i64,) = sqlx::query_as(
        // ::BIGINT — SUM(BIGINT) is NUMERIC, sqlx can't decode that into i64.
        "SELECT COALESCE(SUM(amount_clt), 0)::BIGINT FROM redemption_intents
         WHERE status IN ('payout_submitted','paid')
           AND updated_at > now() - interval '24 hours'",
    )
    .fetch_one(pool)
    .await?;
    Ok(total)
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
