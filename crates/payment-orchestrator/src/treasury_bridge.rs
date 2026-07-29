//! The deposit→mint bridge (Plan C 5b): the only thing in this crate that crosses from the
//! public zone into the private one. Two steps, both driven off the `confirmed` deposit row
//! itself — spec §6 outbox semantics: that row IS the pending-operation row, written atomically
//! with the state change by `webhook.rs`'s `confirm_and_credit`, so there is no separate queue
//! to keep in sync.
//!
//! 1. `create_step`: POST `{treasury_url}/internal/mint-intents` with the **initiator** token.
//!    THE line that matters most in this file: `expected_amount_usdt` is
//!    `deposit_intents.pay_amount_usdt` — the DISCRIMINATED amount the user was actually told to
//!    pay — never `amount_clt` (what they merely asked to deposit). On the shared static Tron
//!    custody address the discriminated amount is the only thing telling one payer's transfer
//!    from another's; the treasury's TronGrid verifier matches on-chain transfers against
//!    exactly this value. Sending the wrong one lets a stranger's larger transfer satisfy this
//!    deposit and get ledgered as its backing — that was Critical `cb497e3`, fixed treasury-side;
//!    this worker is the sole producer of the value on the wire.
//!    `client_ref` (this deposit intent's own id) is the idempotency key: the treasury replays
//!    the existing intent (200) on a duplicate rather than creating a second one, so this file
//!    adds no dedup layer of its own — a lost-response retry is safe by construction via that
//!    mechanism alone.
//! 2. `poll_step`: GET `{treasury_url}/internal/mint-intents/:id` with the **readonly** token —
//!    `credited` → deposit `credited`; `rejected`/`failed` → deposit `needs_manual` + a P1 alert
//!    that says plainly that funds are in custody with no minted claim against them, and that
//!    re-minting is a NEW treasury intent (this `client_ref` is burned, never reusable) created
//!    by a human initiator referencing this deposit — so whoever is paged doesn't try to retry
//!    this bridge instead.
//!
//! Every status change goes through `deposits::transition`'s guarded from-sets, never a bare
//! UPDATE — it already absorbs out-of-order/repeated application as a no-op, same as every other
//! writer of `deposit_intents.status` in this crate.

use reqwest::Client;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::alerts::alert;
use crate::configuration::OrchConfig;
use crate::deposits::{self, DepositIntent};

/// Consecutive treasury-unreachable failures (on either step) before the brief's required P1 —
/// a treasury outage must not lose a deposit or spin the log, but it must also eventually page
/// a human if it doesn't recover.
const ALERT_AFTER_CONSECUTIVE_FAILURES: i32 = 10;

/// One pass: drive every `confirmed` deposit through the create step, then every
/// `mint_requested` deposit through the poll step. Each row's outcome is independent — one
/// row's treasury error is alerted/backed-off and does not stop the rest of the batch.
pub async fn run_once(pool: &PgPool, config: &OrchConfig) {
    let http = Client::new();
    for intent in deposits::due_for_mint_request(pool).await.unwrap_or_else(|e| {
        tracing::error!("treasury_bridge: failed to select due-for-mint-request deposits: {e}");
        Vec::new()
    }) {
        create_step(pool, config, &http, &intent).await;
    }
    for intent in deposits::due_for_status_poll(pool).await.unwrap_or_else(|e| {
        tracing::error!("treasury_bridge: failed to select due-for-status-poll deposits: {e}");
        Vec::new()
    }) {
        poll_step(pool, config, &http, &intent).await;
    }
}

/// POSTs a `confirmed` deposit to the treasury and, on success, stores the returned intent id
/// and moves the deposit to `mint_requested`. A treasury-unreachable failure records the
/// attempt and backs off — the row stays `confirmed` for the next tick, never lost.
async fn create_step(pool: &PgPool, config: &OrchConfig, http: &Client, intent: &DepositIntent) {
    let body = json!({
        "beneficiary": intent.clt_address,
        "amount_clt": intent.amount_clt,
        // THE line: the DISCRIMINATED pay amount, never amount_clt. See module docs.
        "expected_amount_usdt": intent.pay_amount_usdt,
        "client_ref": intent.id.to_string(),
        "deposit_tx_id": intent.tron_tx_id,
    });

    let resp = http
        .post(format!("{}/internal/mint-intents", config.treasury_url))
        .bearer_auth(&config.treasury_initiator_token)
        .json(&body)
        .send()
        .await;

    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            // A non-2xx here (e.g. the treasury's 400 for a missing expected_amount_usdt) is
            // not "unreachable" — it is the treasury telling us this request is malformed, and
            // retrying it unchanged would just fail identically forever. Treat it the same as a
            // transport failure for backoff/alerting purposes (this bridge has no way to fix the
            // request itself), but log the body so a human can see WHY.
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            tracing::error!("treasury_bridge: create for deposit {} rejected: {status} {text}", intent.id);
            record_failure_and_maybe_alert(pool, intent.id, "create").await;
            return;
        }
        Err(e) => {
            tracing::warn!("treasury_bridge: create for deposit {} unreachable: {e}", intent.id);
            record_failure_and_maybe_alert(pool, intent.id, "create").await;
            return;
        }
    };

    let treasury_intent: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("treasury_bridge: create response for deposit {} unparseable: {e}", intent.id);
            record_failure_and_maybe_alert(pool, intent.id, "create").await;
            return;
        }
    };
    let Some(treasury_id) = treasury_intent.get("id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok())
    else {
        tracing::error!("treasury_bridge: create response for deposit {} had no parseable id: {treasury_intent}", intent.id);
        record_failure_and_maybe_alert(pool, intent.id, "create").await;
        return;
    };

    if let Err(e) = deposits::set_treasury_intent_id(pool, intent.id, treasury_id).await {
        tracing::error!("treasury_bridge: failed to store treasury_intent_id for deposit {}: {e}", intent.id);
        record_failure_and_maybe_alert(pool, intent.id, "create").await;
        return;
    }
    let _ = deposits::reset_attempts(pool, intent.id).await;
    match deposits::transition(pool, intent.id, &["confirmed"], "mint_requested").await {
        Ok(_) => {} // false is a benign race (another tick already moved it) — treasury_intent_id is stored either way.
        Err(e) => tracing::error!("treasury_bridge: failed to transition deposit {} to mint_requested: {e}", intent.id),
    }
}

/// GETs a `mint_requested` deposit's treasury intent status and reacts:
/// - `credited` → deposit `credited`.
/// - `rejected`/`failed` → deposit `needs_manual` + a P1 alert naming the fact that funds are
///   unminted in custody and that re-minting needs a brand-new treasury intent.
/// - anything else (`created`/`approved`/`submitted`) → still in flight, nothing to do yet.
async fn poll_step(pool: &PgPool, config: &OrchConfig, http: &Client, intent: &DepositIntent) {
    let Some(treasury_id) = intent.treasury_intent_id else {
        // due_for_status_poll's own WHERE clause guarantees this is Some; unreachable in
        // practice, but never a panic over a money-path row.
        tracing::error!("treasury_bridge: deposit {} is mint_requested with no treasury_intent_id", intent.id);
        return;
    };

    let resp = http
        .get(format!("{}/internal/mint-intents/{treasury_id}", config.treasury_url))
        .bearer_auth(&config.treasury_readonly_token)
        .send()
        .await;

    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::warn!("treasury_bridge: poll for deposit {} (treasury intent {treasury_id}) returned {}", intent.id, r.status());
            record_failure_and_maybe_alert(pool, intent.id, "poll").await;
            return;
        }
        Err(e) => {
            tracing::warn!("treasury_bridge: poll for deposit {} (treasury intent {treasury_id}) unreachable: {e}", intent.id);
            record_failure_and_maybe_alert(pool, intent.id, "poll").await;
            return;
        }
    };

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("treasury_bridge: poll response for deposit {} unparseable: {e}", intent.id);
            record_failure_and_maybe_alert(pool, intent.id, "poll").await;
            return;
        }
    };
    let _ = deposits::reset_attempts(pool, intent.id).await;

    match body.get("status").and_then(|v| v.as_str()) {
        Some("credited") => {
            let _ = deposits::transition(pool, intent.id, &["mint_requested"], "credited").await;
        }
        Some(s @ ("rejected" | "failed")) => {
            let applied = deposits::transition(pool, intent.id, &["mint_requested"], "needs_manual").await;
            if matches!(applied, Ok(true)) {
                alert(
                    pool,
                    "p1",
                    "treasury_bridge",
                    &format!(
                        "deposit {} treasury mint intent {treasury_id} was {s} — the user's funds are \
                         SITTING IN CUSTODY WITH NO MINTED CLAIM against them. This deposit's client_ref \
                         is burned and CANNOT be reused; re-minting requires a human to create a BRAND-NEW \
                         treasury mint intent (human initiator) that references this deposit ({}) in its \
                         description. Do NOT retry this bridge — it will only replay the same {s} intent.",
                        intent.id, intent.id
                    ),
                )
                .await;
            }
        }
        // created/approved/submitted (or an unrecognised status): still in flight, or a status
        // this bridge doesn't need to react to. Nothing to do — the next tick polls again.
        _ => {}
    }
}

/// Shared failure path for both steps: bump `attempts`/backoff, and once the run crosses the
/// brief's 10-consecutive-failures threshold, page a P1 naming which step is stuck. Firing once
/// per crossing (not on every subsequent failure) keeps a prolonged outage from spamming pages
/// beyond the first one across the threshold.
async fn record_failure_and_maybe_alert(pool: &PgPool, id: Uuid, step: &str) {
    match deposits::record_attempt_failure(pool, id).await {
        Ok(attempts) if attempts == ALERT_AFTER_CONSECUTIVE_FAILURES => {
            alert(
                pool,
                "p1",
                "treasury_bridge",
                &format!(
                    "deposit {id}: {ALERT_AFTER_CONSECUTIVE_FAILURES} consecutive treasury-unreachable \
                     failures on the {step} step — treasury may be down. Deposit is untouched (still \
                     at its prior status) and will keep retrying with backoff."
                ),
            )
            .await;
        }
        Ok(_) => {}
        Err(e) => tracing::error!("treasury_bridge: failed to record attempt failure for deposit {id}: {e}"),
    }
}

/// Spawned once from `main.rs`, same shape as `poller::run`. `poll_interval_secs` is shared with
/// the Bitcart poller — no dedicated interval justified for a second outbound dependency at
/// pilot volume.
pub async fn run(pool: PgPool, config: OrchConfig, poll_interval_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_interval_secs));
    loop {
        interval.tick().await;
        run_once(&pool, &config).await;
    }
}
