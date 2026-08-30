use sqlx::PgPool;
use uuid::Uuid;

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
