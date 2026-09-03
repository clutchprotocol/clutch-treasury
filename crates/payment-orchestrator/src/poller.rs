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

use async_trait::async_trait;
use sqlx::PgPool;

use crate::alerts::alert;
use crate::custody::{evaluate_payment, CustodyWatcher, DepositWatcher, ObservedTransfer, PaymentOutcome};
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
        // A later task replaces this whole body with TieredPoller/due_addresses; for now the
        // per-intent query is unchanged and simply passes the new bound as unset.
        let transfers = match watcher.transfers_to(&address, None).await {
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
                // What arrived is what gets credited, so it has to be persisted before the intent
                // advances. Previously this figure was logged and dropped: the bridge then sent the
                // REQUESTED amount, and an overpayment left the difference in the treasury with
                // nothing recording that we owed it.
                if let Err(e) = deposits::set_received_usdt(pool, id, received_usdt).await {
                    tracing::error!("poller: failed to store received amount for intent {id}: {e}");
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
                    // Says "recorded", not "credited": crediting happens downstream in the bridge
                    // and the treasury's caps can still refuse it. The old wording claimed the
                    // excess had been credited while nothing carried the figure anywhere at all,
                    // so the one alert that could have caught the shortfall asserted it was fine.
                    alert(
                        pool,
                        "warn",
                        "poller",
                        &format!(
                            "deposit {id} overpaid: {received_usdt} vs {expected_amount_usdt} micro-USDT \
                             at {address} — recorded as received; the credit is for the full amount"
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

/// One address due for polling this pass.
pub struct DueAddress {
    pub user_pk: String,
    pub address: String,
    pub last_polled_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Hot addresses first, then ONE cold tier — everyone else, oldest-polled (and never-polled) first.
/// `COALESCE(hot_until > now(), false)` is load-bearing: a bare `(hot_until > now()) DESC` sorts
/// TRUE / FALSE / NULL as three tiers, so an address that was hot once and has since expired would
/// permanently outrank one that was never hot, regardless of `last_polled_at`. The COALESCE folds
/// "expired hot" and "never hot" into the same false/cold bucket, which is why `NULLS LAST` on that
/// column is no longer needed.
///
/// The LIMIT is the whole cost control: permanent addresses never stop being watched, so without a
/// per-pass budget the request count grows with every user who has ever existed. With it, cost per
/// pass is constant and the cold rotation period is simply (addresses / budget) * poll_interval — a
/// number an operator can be told rather than discover.
pub async fn due_addresses(pool: &PgPool, budget: i64) -> Result<Vec<DueAddress>, String> {
    sqlx::query_as::<_, (String, String, Option<chrono::DateTime<chrono::Utc>>)>(
        "SELECT user_pk, address, last_polled_at FROM deposit_addresses
         ORDER BY COALESCE(hot_until > now(), false) DESC, last_polled_at ASC NULLS FIRST
         LIMIT $1",
    )
    .bind(budget)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|(user_pk, address, last_polled_at)| DueAddress { user_pk, address, last_polled_at })
            .collect()
    })
    .map_err(|e| format!("selecting due addresses: {e}"))
}

/// The `DepositWatcher` this deployment runs: poll a bounded slice of addresses per pass, hot first.
///
/// Owns the tier state — it stamps `last_polled_at` for every address it polled, whether or not
/// anything arrived, because an address that is never stamped is re-polled every pass forever and
/// the cold rotation never advances.
pub struct TieredPoller {
    pub pool: PgPool,
    pub inner: Arc<dyn CustodyWatcher>,
    pub budget: i64,
}

#[async_trait]
impl DepositWatcher for TieredPoller {
    async fn poll(&self) -> Result<Vec<ObservedTransfer>, String> {
        let due = due_addresses(&self.pool, self.budget).await?;
        let mut found = Vec::new();

        for a in &due {
            // Only transfers since we last looked, minus an hour of overlap. Permanent addresses
            // otherwise re-fetch their entire history every rotation. The overlap is free: a
            // transfer landing between the query and the stamp is re-observed next pass, and
            // credit_transfer is idempotent on tron_tx_id. Epoch MILLISECONDS, per ObservedTransfer.
            let since = a.last_polled_at.map(|t| (t - chrono::Duration::hours(1)).timestamp_millis());
            match self.inner.transfers_to(&a.address, since).await {
                Ok(mut ts) => found.append(&mut ts),
                // One unreadable address must not abort the pass: the others are still due, and a
                // TronGrid blip on one address would otherwise stall every deposit behind it.
                Err(e) => tracing::warn!("polling {}: {e}", a.address),
            }
        }

        let polled: Vec<String> = due.iter().map(|a| a.address.clone()).collect();
        sqlx::query("UPDATE deposit_addresses SET last_polled_at = now() WHERE address = ANY($1)")
            .bind(&polled)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("stamping last_polled_at: {e}"))?;

        Ok(found)
    }
}
