use std::collections::HashSet;
use std::sync::Mutex;

use sqlx::PgPool;
use uuid::Uuid;

use crate::configuration::AppConfig;
use crate::gasfree_rail::Trace;
use crate::ledger::{alert, alert_once};
use crate::tron_verifier::TronClient;

/// What the signer reported for one payout.
///
/// The division that matters is `Refused` vs `Ambiguous`, and it is not a stylistic one: `Refused`
/// means the signer told us it did not broadcast, so retrying is free. `Ambiguous` means we do not
/// know, and a TRC-20 transfer has no memo to dedupe against, so retrying risks paying twice for a
/// burn that only happened once. Never widen `Refused` to cover a case you are not certain about.
#[derive(Debug, Clone, PartialEq)]
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
    /// A GasFree permit paying this redemption is with the relay. Not paid until its transfer from
    /// the float is found confirmed on chain (`confirm_gasfree_payouts_once`).
    Submitted { trace_id: String, nonce: u64, deadline: u64 },
    /// The relay refused a signed permit with one of the refusals its docs list as pre-execution
    /// checks. That is the relay's word, not proof: the permit stays valid until `deadline`, so
    /// nothing is signed before then, and after it the chain decides.
    RelayRefused { reason: String, nonce: u64, deadline: u64 },
    /// The GasFree float has never made a transfer, so its next one would also pay its activation.
    /// Provably nothing was signed. Redemptions wait for the one-time activation (spec §4).
    FloatNotActive { float_address: String },
}

/// The signer boundary, as a trait so the worker is testable without a live service or real keys —
/// same reasoning as `SweepSigner` in sweeper.rs.
#[async_trait::async_trait]
pub trait PayoutSigner: Send + Sync {
    async fn pay(&self, intent_id: Uuid, to: &str, amount_usdt: i64) -> PayoutReply;

    /// The relay's record of a GasFree permit. Only used to find its transaction; the chain decides
    /// whether that paid the redemption.
    async fn trace(&self, _trace_id: &str) -> Result<Trace, String> {
        Err("this signer cannot read GasFree traces".into())
    }

    /// The plain address that owns the payout float (`2/0`), and the GasFree float it owns. The
    /// float's nonce on the GasFree controller is its owner's.
    async fn float_owner(&self) -> Result<(String, Option<String>), String> {
        Err("this signer cannot name the float's owner".into())
    }
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
            // A GasFree permit is with the relay. Without its trace id, nonce and deadline nothing
            // here could follow it, so it is not a clear answer.
            Some("submitted") => match (body["trace_id"].as_str(), body["nonce"].as_u64(), body["deadline"].as_u64()) {
                (Some(trace_id), Some(nonce), Some(deadline)) => {
                    PayoutReply::Submitted { trace_id: trace_id.to_string(), nonce, deadline }
                }
                _ => PayoutReply::Ambiguous(format!("signer reported submitted without the permit's trace id, nonce and deadline: {body}")),
            },
            Some("float_not_active") => PayoutReply::FloatNotActive {
                float_address: body["float_address"].as_str().unwrap_or("unknown").to_string(),
            },
            // Without a nonce and a deadline: the signer proved this happened before it ever
            // attempted a broadcast (a bad recipient, a key derivation failure, a TronGrid read that
            // never got a response) — see sweep.rs's `PayoutOutcome::Refused`. With both: the relay
            // refused a signed GasFree permit (`PayoutOutcome::RelayRefused`). With one of the two,
            // the answer is not one the signer gives, so it is not a clear one.
            Some("refused") => {
                let reason = body["reason"].as_str().unwrap_or("signer reported refused with no reason").to_string();
                match (body["nonce"].as_u64(), body["deadline"].as_u64()) {
                    (None, None) => PayoutReply::Refused(reason),
                    (Some(nonce), Some(deadline)) => PayoutReply::RelayRefused { reason, nonce, deadline },
                    _ => PayoutReply::Ambiguous(format!("signer reported a refusal with half a permit: {body}")),
                }
            }
            // An unknown status from a newer signer might describe a broadcast this version does
            // not understand. Ambiguous, never Refused.
            other => PayoutReply::Ambiguous(format!("unrecognised signer status {other:?}")),
        }
    }

    async fn trace(&self, trace_id: &str) -> Result<Trace, String> {
        crate::gasfree_rail::fetch_trace(&self.http, &self.base_url, &self.token, trace_id).await
    }

    async fn float_owner(&self) -> Result<(String, Option<String>), String> {
        let resp = self
            .http
            .get(format!("{}/internal/xpub", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("signer unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("signer returned {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| format!("unreadable signer response: {e}"))?;
        let owner = body["payout_address"]
            .as_str()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| format!("the signer named no payout address: {body}"))?;
        Ok((owner.to_string(), body["payout_gasfree_address"].as_str().map(str::to_string)))
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

/// Same shape and reasoning as `over_cap_alerted` — de-dupes a live condition, not a record, so
/// re-alerting after a restart is correct — but for `drain_once`'s proven-non-broadcast arm
/// (`FloatDry` / `CapExceeded` / `NeedsTrx` / `Refused`) instead of the over-cap one.
///
/// `outbox_poll_ms` is 2 seconds, so a dry float — the NORMAL state until the rollout funds it —
/// re-claims and re-refuses the same intent every single pass. Without this, that inserts a fresh
/// P1 row and burns two TronGrid calls every 2 seconds, which buries the ambiguous-payout P1s the
/// whole design depends on someone actually reading.
fn payout_refused_alerted() -> &'static Mutex<HashSet<Uuid>> {
    static ALERTED: std::sync::OnceLock<Mutex<HashSet<Uuid>>> = std::sync::OnceLock::new();
    ALERTED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Same shape again, for `confirm_payouts_once`'s confirmed-but-failed arm. A `REVERT` /
/// `OUT_OF_ENERGY` transfer never leaves `payout_submitted` on its own — nothing here retries it —
/// so without this dedupe it would re-alert on every confirmation pass forever, exactly the noise
/// `payout_refused_alerted` exists to avoid on the submission side.
fn failed_transfer_alerted() -> &'static Mutex<HashSet<Uuid>> {
    static ALERTED: std::sync::OnceLock<Mutex<HashSet<Uuid>>> = std::sync::OnceLock::new();
    ALERTED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// The signer refuses `APP_GASFREE_DEADLINE_SECS` above 600, so no payout permit it signs is valid
/// for longer than this after it was sent.
const LONGEST_PERMIT_SECS: i64 = 600;

/// After a permit's deadline the controller refuses it. Five minutes past it, a read that shows the
/// float's nonce unmoved is taken as "it never ran", and the redemption is paid again. That is the
/// one decision here that would pay a burn twice if the read were stale, so the margin is far
/// longer than any TronGrid node should lag, and still short enough that a refused redemption is
/// paid again within minutes.
const DEADLINE_GRACE_SECS: i64 = 300;

/// A signer paying by GasFree permit behind a treasury with GasFree off: nothing here settles those
/// payouts, and the float may have paid them.
async fn gasfree_off(pool: &PgPool) {
    alert_once(
        pool,
        "p1",
        "payout",
        "the signer pays redemptions by GasFree permit, but this treasury has GasFree off (APP_GASFREE_NETWORK is \
         not set), so it never settles them: each stays payout_submitted. Do not return them to payout_pending; \
         the float may have paid them. Give both services the same GasFree settings.",
        chrono::Duration::hours(1),
    )
    .await;
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
/// an orphan by omission, exactly as bad as one by crash. Only the breaker read, the initial
/// SELECT, and the `daily_payout_total` read still propagate via `?`: nothing is claimed yet at
/// that point, so there is nothing to lose.
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

    // GasFree: one payout permit alive at a time. While a permit may still run, a second would carry
    // the same nonce, and after a refusal the chain could no longer say which of the two ran. So
    // nothing is signed until every permit is settled by `confirm_gasfree_payouts_once`, or, for one
    // whose nonce is unknown, until it can no longer run. On the TRX rail no row carries a permit.
    let (permit_in_doubt,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM redemption_intents
                         WHERE status = 'payout_submitted' AND payout_ref IS NULL
                           AND (payout_permit_nonce IS NOT NULL OR payout_permit_deadline > $1))",
    )
    .bind(chrono::Utc::now().timestamp())
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;
    if permit_in_doubt {
        return Ok(0);
    }

    // Both amounts. `amount_clt` is the burn — it is what the caps are measured in, and what the
    // user gave up. `payout_amount_usdt` is the quote stored at creation, and it is the only
    // number that may reach the signer.
    let rows: Vec<(Uuid, String, i64, i64)> = sqlx::query_as(
        "SELECT id, payout_address, amount_clt, payout_amount_usdt FROM redemption_intents
         WHERE status = 'payout_pending' ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut day_total = daily_payout_total(pool).await.map_err(|e| e.to_string())?;
    let mut processed = 0u32;

    for (intent_id, payout_address, amount_clt, payout_amount_usdt) in rows {
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

        // The quote stored at creation, never `amount_clt` and never recomputed from config here.
        // The two are equal only when no fee is set. The caps above deliberately stay measured in
        // `amount_clt`: the gross is the larger number, so capping on it can only ever let less
        // money out than the cap allows, which is the safe direction to be wrong in.
        match signer.pay(intent_id, &payout_address, payout_amount_usdt).await {
            PayoutReply::Paid { tx_id } => {
                // Charge the budget now, before the write below can fail: the float has already
                // paid out either way, same as the Ambiguous arm's identical reasoning. Charging
                // it only after a successful write undercounts today's spend by exactly the
                // amount that just left through a bookkeeping failure.
                day_total += amount_clt;
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
                processed += 1;
            }
            PayoutReply::Submitted { trace_id, nonce, deadline } => {
                if config.gasfree.is_none() {
                    gasfree_off(pool).await;
                }
                // Counted against today's budget from the next pass on: `daily_payout_total` counts
                // every `payout_submitted` row, and this pass ends here.
                if let Err(e) = sqlx::query(
                    "UPDATE redemption_intents
                        SET payout_trace_id = $2, payout_permit_nonce = $3, payout_permit_deadline = $4, updated_at = now()
                      WHERE id = $1",
                )
                .bind(intent_id)
                .bind(&trace_id)
                .bind(nonce as i64)
                .bind(deadline as i64)
                .execute(pool)
                .await
                {
                    alert(pool, "p1", "payout", &format!(
                        "redemption {intent_id}: GasFree permit {trace_id} (nonce {nonce}, valid until {deadline}) is \
                         with the relay, but recording it failed ({e}). Left payout_submitted with no payout_ref: find \
                         the float's transfer for that trace and set payout_ref by hand, or, after the deadline, return \
                         it to payout_pending if the float's nonce is still {nonce}."
                    )).await;
                }
                processed += 1;
                break; // one permit at a time
            }
            PayoutReply::RelayRefused { reason, nonce, deadline } => {
                if config.gasfree.is_none() {
                    gasfree_off(pool).await;
                }
                // Refused on the relay's word. The signed permit stays valid until its deadline, so it
                // may still pay: the row stays payout_submitted, which `daily_payout_total` counts from
                // the next pass on.
                match sqlx::query(
                    "UPDATE redemption_intents
                        SET payout_trace_id = NULL, payout_permit_nonce = $2, payout_permit_deadline = $3, updated_at = now()
                      WHERE id = $1",
                )
                .bind(intent_id)
                .bind(nonce as i64)
                .bind(deadline as i64)
                .execute(pool)
                .await
                {
                    Ok(_) => alert(pool, "warn", "payout", &format!(
                        "redemption {intent_id}: {reason}. The signed permit stays valid until {deadline}, so nothing \
                         is paid before then; after it, the float's nonce says whether it ran."
                    )).await,
                    Err(e) => alert(pool, "p1", "payout", &format!(
                        "redemption {intent_id}: {reason}, and recording the refused permit (nonce {nonce}, valid until \
                         {deadline}) failed ({e}). Left payout_submitted: after the deadline, return it to \
                         payout_pending by hand if the float's nonce is still {nonce}."
                    )).await,
                }
                break; // one permit at a time
            }
            PayoutReply::FloatNotActive { float_address } => {
                if let Err(e) = sqlx::query(
                    "UPDATE redemption_intents SET status = 'payout_pending', updated_at = now()
                     WHERE id = $1 AND status = 'payout_submitted'",
                )
                .bind(intent_id)
                .execute(pool)
                .await
                {
                    alert(pool, "p1", "payout", &format!(
                        "redemption {intent_id}: the signer proved nothing was signed (the GasFree float is not \
                         activated), but returning it to payout_pending failed ({e}). It carries no payout_ref, so it \
                         is safe to move back by hand."
                    )).await;
                }
                alert_once(
                    pool,
                    "warn",
                    "payout",
                    &format!(
                        "redemptions are not available yet: the GasFree float {float_address} has never made a transfer, \
                         so its next one would also pay its activation. Its one-time activation comes first."
                    ),
                    chrono::Duration::hours(1),
                )
                .await;
                break; // every redemption would get the same answer
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
                // NeedsTrx is not a refusal and must not page anyone. The float pays energy for
                // its own transfer, so an empty one gets topped up from the fee account and the
                // NEXT pass sends the USDT — the same two-pass shape the sweeper has, where the
                // equivalent outcome (`Funded`) is a log line and nothing more. The first real
                // redemption on stage raised a P1 this way and then paid normally one pass later,
                // which is exactly how an alert stops meaning anything.
                //
                // The other three in this arm do deserve one: a dry float and an exceeded cap both
                // need an operator, and a Refused is a real refusal.
                if matches!(reply, PayoutReply::NeedsTrx) {
                    tracing::info!(
                        "redemption {intent_id}: float topped up with TRX, payout goes out on the next pass"
                    );
                } else if payout_refused_alerted().lock().unwrap().insert(intent_id) {
                    // Deduped: a dry float re-claims and re-refuses this same intent every pass, and
                    // an alert per pass buries the ambiguous-payout P1s a human actually needs to see.
                    alert(pool, "p1", "payout",
                        &format!("redemption {intent_id}: payout refused ({reply:?}), returned to payout_pending")).await;
                }
            }
            // May or may not have broadcast. Stays claimed, forever, until a human resolves it.
            PayoutReply::Ambiguous(msg) => {
                // Counts against today's budget: it might have spent real float capacity, and
                // daily_payout_total counts every payout_submitted row as spent from the next
                // pass onward regardless — this just makes the CURRENT pass agree with that.
                day_total += amount_clt;
                // On the GasFree rail a permit may be with the relay. It cannot run past the longest
                // deadline the signer signs, so no other permit is signed before then.
                let gasfree_payouts = config.gasfree.as_ref().is_some_and(|s| s.rail);
                // The same margin the settlement uses, so the time named below is when the hold ends.
                let hold_until = chrono::Utc::now().timestamp() + LONGEST_PERMIT_SECS + DEADLINE_GRACE_SECS;
                if gasfree_payouts {
                    if let Err(e) = sqlx::query("UPDATE redemption_intents SET payout_permit_deadline = $2 WHERE id = $1")
                        .bind(intent_id)
                        .bind(hold_until)
                        .execute(pool)
                        .await
                    {
                        tracing::error!(%intent_id, "could not hold the float after an unclear GasFree payout: {e}");
                    }
                }
                // A GasFree permit may still be with the relay. Returning the intent before that permit
                // can no longer run lets the next pass sign the next nonce and pay the same burn again.
                let not_before = if gasfree_payouts {
                    format!(
                        " A GasFree permit may still run until {hold_until} (unix seconds): do not return this \
                         intent to payout_pending before then."
                    )
                } else {
                    String::new()
                };
                alert(pool, "p1", "payout", &format!(
                    "redemption {intent_id}: payout outcome UNKNOWN ({msg}). Left payout_submitted \
                     and NOT retried — retrying could pay this burn twice. Claimed at {claimed_at}: \
                     check the payout float ({float}) for an outbound USDT transfer of {payout_amount_usdt} \
                     (micro-USDT: the quoted net, below the {amount_clt} burned when a fee is set) to {payout_address} around that time. Found it? Set \
                     payout_ref to that tx hash — confirm_payouts_once will pick it up from there. \
                     Found nothing? Return the intent to payout_pending by hand.{not_before}",
                    float = config.payout_float_address
                )).await;
                if gasfree_payouts {
                    break; // one permit at a time
                }
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
///
/// Confirmation is two questions, not one: `transaction_confirmed` (is it in an irreversible
/// block) gates `transfer_succeeded` (did it actually move value). A TRC-20 transfer that ran out
/// of energy or reverted answers the first `true` and the second `false` — it is in the block and
/// paid nothing. Only both `true` writes `paid` and the ledger event; the first true and second
/// false is a dead end that alerts a human and is never retried automatically.
///
/// Each intent's own failure is logged (or alerted, for the one case where money already moved)
/// and the loop continues rather than using `?` — one intent's DB error must not head-of-line
/// block confirmation for every intent after it in this pass.
pub async fn confirm_payouts_once(pool: &PgPool, client: &TronClient) -> Result<u32, String> {
    // The NET, because what this writes to the ledger is how much USDT left the float.
    // `burn_redeemed` has already dropped liability by the full burn; recording the gross here
    // too would understate the reserve by the fee on every redemption, and a reserve reported
    // below liability is the one condition that halts minting.
    let rows: Vec<(Uuid, i64, String)> = sqlx::query_as(
        "SELECT id, payout_amount_usdt, payout_ref FROM redemption_intents
         WHERE status = 'payout_submitted' AND payout_ref IS NOT NULL ORDER BY updated_at",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut confirmed = 0u32;
    for (intent_id, payout_amount_usdt, payout_ref) in rows {
        match client.transaction_confirmed(&payout_ref).await {
            Ok(true) => match client.transfer_succeeded(&payout_ref).await {
                Ok(true) => {
                    if let Err(e) = pay_intent(pool, intent_id, payout_amount_usdt, &payout_ref).await {
                        // The transfer is proven successful on chain; only our own bookkeeping
                        // failed. Safe to retry — pay_intent's UPDATE and its ON CONFLICT DO
                        // NOTHING insert are both idempotent, and this row still matches the
                        // SELECT above, so the next pass picks it up on its own.
                        alert(pool, "p1", "payout", &format!(
                            "redemption {intent_id}: tx {payout_ref} confirmed successful on \
                             chain but recording it as paid failed ({e}). Safe to retry — the \
                             next confirmation pass will pick it up automatically."
                        )).await;
                        continue;
                    }
                    confirmed += 1;
                }
                // In an irreversible block, but the transfer itself moved nothing —
                // OUT_OF_ENERGY, REVERT, etc. The float never paid out: this must never become
                // `paid` and must never be retried automatically (drain_once already spent this
                // intent's one signer call; the chain's answer is already terminal). Only a human
                // resolves it from here.
                Ok(false) => {
                    if failed_transfer_alerted().lock().unwrap().insert(intent_id) {
                        let reason = client
                            .payout_contract_ret(&payout_ref)
                            .await
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| "unknown".to_string());
                        alert(pool, "p1", "payout", &format!(
                            "redemption {intent_id}: payout tx {payout_ref} is confirmed on chain \
                             but did NOT succeed (contractRet={reason}). No USDT left the float. \
                             Left payout_submitted and NOT retried automatically — check the tx on \
                             chain, then either move this intent back to payout_pending by hand to \
                             retry, or resolve it another way."
                        )).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(%intent_id, %payout_ref, "could not check payout success: {e}");
                }
            },
            // Not yet mined. Nothing to do; the next pass looks again.
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(%intent_id, %payout_ref, "could not check payout confirmation: {e}");
            }
        }
    }
    Ok(confirmed)
}

/// Settles GasFree payouts (spec §4, §5).
///
/// A permit returns a trace id. The relay's record of it names the transaction, and the payout is
/// paid only when that transaction's transfer — from the float, to the redeemer, of exactly the
/// quoted amount — is confirmed on chain. After a permit's deadline, the float's nonce says whether
/// it can have run: still at the permit's nonce, it never did, and the redemption goes back to be
/// paid again.
pub async fn confirm_gasfree_payouts_once(
    pool: &PgPool,
    config: &AppConfig,
    settings: &gasfree::Settings,
    client: &TronClient,
    signer: &dyn PayoutSigner,
) -> Result<u32, String> {
    let rows: Vec<(Uuid, String, i64, Option<String>, i64, i64, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT id, payout_address, payout_amount_usdt, payout_trace_id, payout_permit_nonce,
                payout_permit_deadline, payout_submitted_at
         FROM redemption_intents
         WHERE status = 'payout_submitted' AND payout_ref IS NULL AND payout_permit_nonce IS NOT NULL
         ORDER BY payout_submitted_at",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut paid = 0u32;
    for (intent_id, to, amount, trace_id, nonce, deadline, submitted_at) in rows {
        // The relay's record names the transaction; the chain says whether it paid THIS redemption.
        if let Some(trace_id) = &trace_id {
            match signer.trace(trace_id).await {
                Ok(Trace { txn_hash: Some(hash), .. }) => {
                    // Ten minutes before the claim, for clock skew between this host and the chain.
                    let since_ms = (submitted_at - chrono::Duration::minutes(10)).timestamp_millis();
                    match client
                        .confirmed_transfer(&hash, &config.payout_float_address, &to, &config.usdt_contract, amount, since_ms)
                        .await
                    {
                        Ok(true) => {
                            // A transaction already recorded as another redemption's payment cannot pay
                            // this one too. Only the relay tied it to this permit; the chain says it paid
                            // someone else. Not taken: after the deadline the float's nonce decides.
                            match paid_by_another(pool, intent_id, &hash).await {
                                Ok(None) => {
                                    match pay_intent(pool, intent_id, amount, &hash).await {
                                        Ok(()) => paid += 1,
                                        Err(e) => {
                                            alert(pool, "p1", "payout", &format!(
                                                "redemption {intent_id}: GasFree payout {hash} is confirmed on chain, but \
                                                 recording it as paid failed ({e}). Safe to retry: the next pass picks it up."
                                            )).await;
                                        }
                                    }
                                    continue;
                                }
                                Ok(Some(other)) => {
                                    alert_once(
                                        pool,
                                        "p1",
                                        "payout",
                                        &format!(
                                            "redemption {intent_id}: the relay names transaction {hash} as its payout, but \
                                             that transaction already paid redemption {other}. It is not taken as this \
                                             one's payment; after the permit's deadline the float's nonce decides."
                                        ),
                                        chrono::Duration::hours(1),
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    tracing::warn!(%intent_id, %hash, "could not check whether the transaction already paid another redemption: {e}");
                                    continue;
                                }
                            }
                        }
                        Ok(false) => {} // not confirmed yet, or not a transfer that pays this redemption
                        Err(e) => {
                            tracing::warn!(%intent_id, %hash, "could not read the float's transfers: {e}");
                            continue;
                        }
                    }
                }
                Ok(_) => {} // no transaction yet
                Err(e) => tracing::warn!(%intent_id, %trace_id, "the relay's record of the payout permit is unreadable: {e}"),
            }
        }

        // Until its deadline, and a margin for the last block, the permit may still run.
        if chrono::Utc::now().timestamp() <= deadline + DEADLINE_GRACE_SECS {
            continue;
        }
        let owner = match signer.float_owner().await {
            Ok((owner, float)) if float.as_deref() == Some(config.payout_float_address.as_str()) => owner,
            Ok((_, float)) => {
                alert_once(
                    pool,
                    "p1",
                    "payout",
                    &format!(
                        "the signer's GasFree float is {float:?}, but PAYOUT_FLOAT_ADDRESS is {}: the reserve counts a \
                         different float than the one redemptions are paid from, and GasFree payouts cannot be settled \
                         until the two agree",
                        config.payout_float_address
                    ),
                    chrono::Duration::hours(1),
                )
                .await;
                continue;
            }
            Err(e) => {
                tracing::warn!(%intent_id, "could not ask the signer for the float's owner: {e}");
                continue;
            }
        };
        let chain_nonce = match client.gasfree_nonce(settings.chain.controller, &owner).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(%intent_id, "the float's nonce is unreadable: {e}");
                continue;
            }
        };
        if chain_nonce <= nonce as u64 {
            // It can no longer run, and it did not: the redemption is paid again on a later pass.
            match sqlx::query(
                "UPDATE redemption_intents
                    SET status = 'payout_pending', payout_trace_id = NULL, payout_permit_nonce = NULL,
                        payout_permit_deadline = NULL, updated_at = now()
                  WHERE id = $1 AND status = 'payout_submitted' AND payout_ref IS NULL",
            )
            .bind(intent_id)
            .execute(pool)
            .await
            {
                Ok(_) => tracing::info!(%intent_id, "GasFree payout permit (nonce {nonce}) expired without running; paid again on a later pass"),
                Err(e) => tracing::error!(%intent_id, "could not return an expired GasFree payout to payout_pending: {e}"),
            }
        } else {
            // One permit at a time, so the nonce moved because this permit ran, or because something
            // else spent from the float (its activation, or a hand-made transfer). No transfer paying
            // this redemption was found either way, so it may be paid, and must not be paid again.
            alert(pool, "p1", "payout", &format!(
                "redemption {intent_id}: the GasFree float's nonce moved past this payout's permit (nonce {nonce}), but \
                 no confirmed transfer of {amount} micro-USDT from the float to {to} was found{}. It may have been paid. \
                 Left payout_submitted and NOT retried: find the float's transfer to {to} and set payout_ref, or return \
                 it to payout_pending by hand.",
                trace_id.as_deref().map(|t| format!(" (trace {t})")).unwrap_or_default()
            )).await;
            // A human's now: no longer a permit this service settles, or one that holds the float.
            if let Err(e) = sqlx::query("UPDATE redemption_intents SET payout_permit_nonce = NULL WHERE id = $1")
                .bind(intent_id)
                .execute(pool)
                .await
            {
                tracing::error!(%intent_id, "could not hand the GasFree payout to a human: {e}");
            }
        }
    }
    Ok(paid)
}

/// The redemption, other than `intent_id`, that `tx_id` is already recorded as paying.
async fn paid_by_another(pool: &PgPool, intent_id: Uuid, tx_id: &str) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM redemption_intents WHERE payout_ref = $1 AND id <> $2 LIMIT 1")
        .bind(tx_id)
        .bind(intent_id)
        .fetch_optional(pool)
        .await
}

/// The rolling 24h payout total against `daily_payout_cap_clt`. Counts every status at or past
/// submission — an in-flight payout is spent budget — keyed on `payout_submitted_at`, the moment
/// each claim happened.
///
/// NOT `updated_at`, which is wrong in both directions: `pay_intent`'s later confirmation write
/// touches it too, so a day-old payout re-enters TODAY's budget the instant it confirms; an
/// `Ambiguous` row that nothing ever touches again just sits at its claim time and ages out of the
/// window despite possibly having spent real float capacity. `payout_submitted_at` is set by the
/// claim UPDATE below and touched by nothing else — confirmation and an Ambiguous outcome both
/// leave it alone. (A refused intent later RE-claimed on a subsequent pass DOES get re-stamped —
/// correctly: a new claim spends new budget, not the same claim again.) What actually mirrors how
/// `breakers::daily_mint_total` keys its window on `mint_intents.created_at` is that no write
/// other than the claim itself ever moves it.
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
async fn pay_intent(pool: &PgPool, intent_id: Uuid, amount_usdt: i64, payout_ref: &str) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO treasury_events (kind, amount_clt, amount_usdt, intent_id, chain_tx_hash, description)
         VALUES ('custody_withdrawal', 0, $1, $2, NULL, 'redemption payout')
         ON CONFLICT (intent_id, kind) WHERE intent_id IS NOT NULL DO NOTHING",
    )
    .bind(amount_usdt)
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
