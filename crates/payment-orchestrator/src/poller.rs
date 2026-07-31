//! The detection path: poll each open intent's own deposit address and credit what has arrived.
//!
//! There is no webhook any more, so this timer is not a latency optimisation over another path — it
//! is the only path, which is simpler to reason about than the previous arrangement where a webhook
//! could in principle reach a state the poller had to be kept able to reach too.
//!
//! Ordering inside a pass is deliberate: match BEFORE sweeping expiry, so a payment that landed
//! moments before the TTL is credited rather than raced into `expired` by our own timer.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::alerts::alert;
use crate::custody::{evaluate_payment, CustodyWatcher, PaymentOutcome};
use crate::deposits;

/// How long past expiry an address keeps being watched for a late payment.
///
/// Under per-address deposits this is far less load-bearing than the discriminator slot it replaced:
/// indices are never reused at all (migration 0007), so there is no slot to free and no cross-user
/// hazard in letting one go. What it still governs is how long we keep LOOKING, so a slow or
/// late-broadcast transfer is credited rather than silently stranded.
const WATCH_WINDOW_HOURS: i64 = 24;

/// Addresses polled per pass.
///
/// One TronGrid request per address is unavoidable: Tron cannot watch an xpub as a group, so derived
/// addresses have to be queried individually. Only OPEN intents are polled, which bounds this by
/// in-flight deposits rather than by intents ever created — but a burst still has to degrade
/// gracefully instead of hammering an unkeyed endpoint into throttling, which looks exactly like
/// "nobody is paying". That failure mode has already cost a day of debugging once.
///
/// Hitting the cap is LOGGED, never silent: a quietly truncated pass reads as "no payments found".
const MAX_ADDRESSES_PER_PASS: i64 = 50;

/// One pass: poll each open deposit address, credit or flag what arrived, then expire and close.
pub async fn poll_once(pool: &PgPool, watcher: &dyn CustodyWatcher) {
    let due = match due_intents(pool).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("poller: failed to select payable intents: {e}");
            return;
        }
    };
    if due.len() as i64 == MAX_ADDRESSES_PER_PASS {
        tracing::warn!(
            "poller: hit the {MAX_ADDRESSES_PER_PASS}-address cap this pass — oldest open intents \
             are polled first and newer ones wait a tick. Sustained load here needs a TronGrid API \
             key and a higher cap."
        );
    }

    for (id, address, expected_amount_usdt) in due {
        let transfers = match watcher.transfers_to(&address).await {
            Ok(t) => t,
            Err(e) => {
                // Transient by assumption: TronGrid down, throttled, or a blip. Nothing is persisted
                // and no intent is advanced or expired on the strength of an unread chain, which
                // would be indistinguishable from "nobody paid".
                alert(pool, "warn", "poller", &format!("custody fetch failed for {address}: {e}")).await;
                continue;
            }
        };

        match evaluate_payment(&transfers, expected_amount_usdt) {
            PaymentOutcome::None => {}

            // Money arrived but is short. Deliberately NOT credited: crediting the expected amount
            // would mint CLT the deposit does not back. Held at `paying` so a second instalment can
            // still settle it, and flagged so a human knows funds are sitting there.
            //
            // Under amount-matching this case was inexpressible — a short payment matched nothing
            // and stranded with no record at all.
            PaymentOutcome::Partial { received_usdt } => {
                let _ = deposits::transition(pool, id, &["created", "invoiced", "expired"], "paying").await;
                alert(
                    pool,
                    "warn",
                    "poller",
                    &format!(
                        "deposit {id} underpaid: {received_usdt} of {expected_amount_usdt} micro-USDT \
                         at {address} — held, not credited"
                    ),
                )
                .await;
            }

            PaymentOutcome::Settled { tx_id, received_usdt } => {
                // Store the evidence FIRST. If this process dies here, the next pass reaches the
                // same conclusion and stores the same hash; transitioning first could leave a
                // `confirmed` intent with no evidence recorded, which the treasury's verifier would
                // then have to resolve down its weaker no-hash path.
                if let Err(e) = deposits::set_tron_tx_id(pool, id, &tx_id).await {
                    tracing::error!("poller: failed to store tx id for intent {id}: {e}");
                    continue;
                }
                match deposits::transition(pool, id, &["created", "invoiced", "paying", "expired"], "confirmed").await
                {
                    Ok(true) => {
                        tracing::info!("deposit {id} confirmed: {received_usdt} micro-USDT at {address} in {tx_id}")
                    }
                    Ok(false) => {} // already past `confirmed`; nothing to do.
                    Err(e) => tracing::error!("poller: failed to confirm intent {id}: {e}"),
                }
                if received_usdt > expected_amount_usdt {
                    alert(
                        pool,
                        "warn",
                        "poller",
                        &format!(
                            "deposit {id} overpaid: {received_usdt} vs {expected_amount_usdt} micro-USDT \
                             at {address} — credited what arrived"
                        ),
                    )
                    .await;
                }
            }
        }
    }

    sweep_expired(pool).await;
    close_stale_watch_windows(pool).await;
}

/// Intents that can still take a payment, oldest first, capped.
///
/// `expired` is included on purpose: our expiry is a soft local timer and at par there is no FX risk
/// in honouring a late payment. An address stops being watched when its window closes, not when the
/// TTL lapses.
///
/// `deposit_address IS NOT NULL` skips discriminator-era rows, which have no address to poll.
async fn due_intents(pool: &PgPool) -> Result<Vec<(uuid::Uuid, String, i64)>, sqlx::Error> {
    sqlx::query_as::<_, (uuid::Uuid, String, i64)>(
        "SELECT id, deposit_address, amount_usdt FROM deposit_intents
         WHERE deposit_address IS NOT NULL
           AND NOT payment_window_closed
           AND status IN ('created', 'invoiced', 'paying', 'expired')
         ORDER BY created_at ASC
         LIMIT $1",
    )
    .bind(MAX_ADDRESSES_PER_PASS)
    .fetch_all(pool)
    .await
}

/// Soft-expire anything past its pay-in window that has not moved on.
///
/// Deliberately only `created`/`invoiced`/`paying`: never touches
/// `confirmed`/`mint_requested`/`credited`/`failed`/`needs_manual`, all of which are further along
/// than a mere timeout should move them.
async fn sweep_expired(pool: &PgPool) {
    if let Err(e) = sqlx::query(
        "UPDATE deposit_intents SET status = 'expired', updated_at = now()
         WHERE status IN ('created', 'invoiced', 'paying') AND expires_at < now()",
    )
    .execute(pool)
    .await
    {
        tracing::error!("poller: failed to sweep expired intents: {e}");
    }
}

/// Stop watching addresses whose late-payment window has closed.
///
/// Never closes the window on a row a human is holding (`needs_manual`) — that money may still be
/// sitting at the address awaiting a decision, and a closed window means nobody is looking.
async fn close_stale_watch_windows(pool: &PgPool) {
    if let Err(e) = sqlx::query(
        "UPDATE deposit_intents
         SET payment_window_closed = TRUE, updated_at = now()
         WHERE NOT payment_window_closed
           AND status IN ('expired', 'failed')
           AND expires_at < now() - ($1 || ' hours')::interval",
    )
    .bind(WATCH_WINDOW_HOURS.to_string())
    .execute(pool)
    .await
    {
        tracing::error!("poller: failed to close stale watch windows: {e}");
    }
}

/// Spawned once from `main.rs`; runs forever on `poll_interval_secs`.
pub async fn run(pool: PgPool, watcher: Arc<dyn CustodyWatcher>, poll_interval_secs: u64) {
    let mut interval = tokio::time::interval(Duration::from_secs(poll_interval_secs));
    loop {
        interval.tick().await;
        poll_once(&pool, watcher.as_ref()).await;
    }
}
