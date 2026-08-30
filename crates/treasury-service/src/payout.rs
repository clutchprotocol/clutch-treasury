use std::collections::HashSet;
use std::sync::Mutex;

use sqlx::PgPool;
use uuid::Uuid;

use crate::configuration::AppConfig;
use crate::ledger::alert;
use crate::tron_verifier::TronClient;

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

/// IDs already P1-alerted for exceeding the daily cap on their own. Same shape as outbox.rs's
/// `STALE_ALERTED`: de-duplicates a live condition rather than recording anything, so a restart
/// re-alerting is correct and expected, not a bug to fix.
///
/// Unlike `STALE_ALERTED` this never needs to clear mid-process: `daily_payout_cap_clt` is loaded
/// once at startup and never changes while this process runs, so an intent that exceeds it keeps
/// exceeding it for the rest of this process's life. The only way an id stops mattering is leaving
/// `payout_pending` entirely, and a handful of stale UUIDs sitting unused in this set forever costs
/// nothing worth guarding against.
///
/// `HashSet::new()` needs `RandomState`'s runtime entropy, so it cannot seed a `static` directly
/// the way `AtomicBool::new(false)` can — `OnceLock` is this codebase's existing answer to that
/// (see `test_deriver` in payment-orchestrator's db_bridge.rs).
fn over_cap_alerted() -> &'static Mutex<HashSet<Uuid>> {
    static ALERTED: std::sync::OnceLock<Mutex<HashSet<Uuid>>> = std::sync::OnceLock::new();
    ALERTED.get_or_init(|| Mutex::new(HashSet::new()))
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
///
/// Every write between "claimed" and "outcome recorded" alerts on failure instead of propagating
/// `?`. A `?` there would abort the whole pass and leave THIS intent claimed with nobody told —
/// an orphan by omission, exactly as bad as one by crash. Only the breaker read and the initial
/// SELECT still propagate: nothing is claimed yet at that point, so there is nothing to lose.
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
        // Unpayable under the current cap, permanently — nothing in this codebase caps a
        // redemption's size at creation. Checked BEFORE the cumulative test below: without this,
        // `ORDER BY created_at` would let one such intent `break` the pass forever and wedge every
        // intent behind it in line. Skip it instead; it consumes no budget and blocks nobody.
        if amount_clt > config.daily_payout_cap_clt {
            if over_cap_alerted().lock().unwrap().insert(intent_id) {
                alert(pool, "p1", "payout", &format!(
                    "redemption {intent_id}: amount {amount_clt} (CLT base units) alone exceeds \
                     daily_payout_cap_clt ({cap}); it can never be paid under the current cap and \
                     will not block any other intent. Raise the cap or resolve this intent by hand.",
                    cap = config.daily_payout_cap_clt
                )).await;
            }
            continue;
        }
        if day_total + amount_clt > config.daily_payout_cap_clt {
            tracing::warn!(%intent_id, day_total, cap = config.daily_payout_cap_clt,
                "daily payout cap reached; remaining intents wait for the window to roll");
            break;
        }

        // CLAIM FIRST. Committed before the call, so a crash between here and the reply is
        // indistinguishable from a lost response — which is correct, because it is one.
        let claimed: Option<(chrono::DateTime<chrono::Utc>,)> = match sqlx::query_as(
            "UPDATE redemption_intents SET status = 'payout_submitted', payout_submitted_at = now(),
                 updated_at = now()
             WHERE id = $1 AND status = 'payout_pending'
             RETURNING payout_submitted_at",
        )
        .bind(intent_id)
        .fetch_optional(pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                // The UPDATE can commit server-side with only the acknowledgement lost
                // (connection reset, failover) — indistinguishable here from a clean failure.
                // Bailing the whole pass via `?` would silently orphan this intent if it did
                // commit: claimed, signer never called, nobody told. Alert and move on instead —
                // the rest of the pass is unaffected.
                alert(pool, "p1", "payout", &format!(
                    "redemption {intent_id}: claim UPDATE errored ({e}). If it committed anyway \
                     this intent is now payout_submitted with no signer call made — check its \
                     status and payout_ref before assuming it is untouched."
                )).await;
                continue;
            }
        };
        let claimed_at = match claimed {
            Some((t,)) => t,
            // Another worker took it between the SELECT and here.
            None => continue,
        };

        // 1:1 CLT<->USDT base units at par — spread/fee modelling is an orchestrator concern.
        match signer.pay(intent_id, &payout_address, amount_clt).await {
            PayoutReply::Paid { tx_id } => {
                if let Err(e) = sqlx::query(
                    "UPDATE redemption_intents SET payout_ref = $2, updated_at = now() WHERE id = $1",
                )
                .bind(intent_id)
                .bind(&tx_id)
                .execute(pool)
                .await
                {
                    // Money already left the float. tx_id is the ONLY record of which transfer
                    // paid this burn — confirm_payouts_once finds it solely by payout_ref — so
                    // losing this write loses that link entirely. It goes into the alert
                    // (`alert` does a tracing::error! before its own insert, so the tx id
                    // survives even if the alerts-table write also fails) rather than through
                    // `?`, which would discard tx_id outright.
                    alert(pool, "p1", "payout", &format!(
                        "redemption {intent_id}: signer paid tx {tx_id} but recording payout_ref \
                         failed ({e}). The transfer already happened — find {tx_id} on chain and \
                         set payout_ref by hand, or confirm_payouts_once can never find it."
                    )).await;
                    continue;
                }
                day_total += amount_clt;
                processed += 1;
            }
            // Proven non-broadcast: hand it back for a later pass.
            reply @ (PayoutReply::FloatDry { .. }
            | PayoutReply::CapExceeded { .. }
            | PayoutReply::NeedsTrx
            | PayoutReply::Refused(_)) => {
                if let Err(e) = sqlx::query(
                    "UPDATE redemption_intents SET status = 'payout_pending', updated_at = now()
                     WHERE id = $1 AND status = 'payout_submitted'",
                )
                .bind(intent_id)
                .execute(pool)
                .await
                {
                    // Same reasoning as the claim-write failure above: `?` here would abandon a
                    // proven-safe-to-retry intent claimed with nobody told, which is strictly
                    // worse than the state it already looks like (indistinguishable from
                    // Ambiguous to anyone who does not read this log).
                    alert(pool, "p1", "payout", &format!(
                        "redemption {intent_id}: signer proved no broadcast ({reply:?}) but \
                         returning it to payout_pending failed ({e}). It carries no payout_ref, \
                         so it is safe to move back to payout_pending by hand."
                    )).await;
                    continue;
                }
                alert(pool, "p1", "payout",
                    &format!("redemption {intent_id}: payout refused ({reply:?}), returned to payout_pending")).await;
            }
            // May or may not have broadcast. Stays claimed, forever, until a human resolves it.
            PayoutReply::Ambiguous(msg) => {
                // Counts against today's budget: it might have spent real float capacity, and
                // daily_payout_total counts every payout_submitted row as spent from the next
                // pass onward regardless — this just makes the CURRENT pass agree with that.
                day_total += amount_clt;
                alert(pool, "p1", "payout", &format!(
                    "redemption {intent_id}: payout outcome UNKNOWN ({msg}). Left payout_submitted \
                     and NOT retried — retrying could pay this burn twice. Claimed at {claimed_at}: \
                     check the payout float ({float}) for an outbound USDT transfer of {amount_clt} \
                     (CLT base units, 1:1 par) to {payout_address} around that time. Found it? Set \
                     payout_ref to that tx hash — confirm_payouts_once will pick it up from there. \
                     Found nothing? Return the intent to payout_pending by hand.",
                    float = config.payout_float_address
                )).await;
            }
        }
    }
    Ok(processed)
}

/// Moves `payout_submitted` intents whose transfer is confirmed on chain to `paid`, writing the
/// ledger event in the same transaction.
///
/// Separate from `drain_once` because submission and confirmation happen at different times: the
/// transfer needs Tron confirmations, and holding a request open across them would stall the whole
/// drain for one intent.
///
/// An intent with no `payout_ref` is skipped, never confirmed and never failed — that is the
/// ambiguous state, and only a human puts a tx id on it or sends it back.
pub async fn confirm_payouts_once(pool: &PgPool, client: &TronClient) -> Result<u32, String> {
    let rows: Vec<(Uuid, i64, String)> = sqlx::query_as(
        "SELECT id, amount_clt, payout_ref FROM redemption_intents
         WHERE status = 'payout_submitted' AND payout_ref IS NOT NULL ORDER BY updated_at",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut confirmed = 0u32;
    for (intent_id, amount_clt, payout_ref) in rows {
        match client.transaction_confirmed(&payout_ref).await {
            Ok(true) => {
                pay_intent(pool, intent_id, amount_clt, &payout_ref).await?;
                confirmed += 1;
            }
            // Not yet mined. Nothing to do; the next pass looks again.
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(%intent_id, %payout_ref, "could not check payout confirmation: {e}");
            }
        }
    }
    Ok(confirmed)
}

/// The rolling 24h payout total against `daily_payout_cap_clt`. Counts every status at or past
/// submission — an in-flight payout is spent budget — keyed on `payout_submitted_at`, the moment
/// each claim happened.
///
/// NOT `updated_at`, which is wrong in both directions: `pay_intent`'s later confirmation write
/// touches it too, so a day-old payout re-enters TODAY's budget the instant it confirms; an
/// `Ambiguous` row that nothing ever touches again just sits at its claim time and ages out of the
/// window despite possibly having spent real float capacity. `payout_submitted_at` is set once, by
/// the claim UPDATE below, and never again — genuinely immutable, which is what actually mirrors
/// how `breakers::daily_mint_total` keys its window on `mint_intents.created_at`.
async fn daily_payout_total(pool: &PgPool) -> Result<i64, sqlx::Error> {
    let (total,): (i64,) = sqlx::query_as(
        // ::BIGINT — SUM(BIGINT) is NUMERIC, sqlx can't decode that into i64.
        "SELECT COALESCE(SUM(amount_clt), 0)::BIGINT FROM redemption_intents
         WHERE status IN ('payout_submitted','paid')
           AND payout_submitted_at > now() - interval '24 hours'",
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
