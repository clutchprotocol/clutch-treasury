//! The detection path. One TronGrid fetch per pass, matched against every intent still able to
//! take a payment.
//!
//! This replaced a Bitcart-refetch loop. There is no webhook any more and nothing to reduce
//! latency against, so this timer is not an "also" path — it is the only path, which is simpler to
//! reason about than the previous arrangement where a webhook could in principle reach a state the
//! poller had to be kept able to reach too.
//!
//! Ordering inside a pass is deliberate: match BEFORE sweeping expiry, so a payment that landed
//! moments before the TTL is credited rather than raced into `expired` by our own timer.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::alerts::alert;
use crate::custody::{match_exact, CustodyWatcher, ObservedTransfer};
use crate::deposits;

/// How long after an intent stops being payable its discriminator slot stays reserved.
///
/// The slot cannot be freed the moment we stop crediting: a payer whose transaction is already in
/// flight would land on an amount a DIFFERENT user has since been allocated, and on a shared
/// custody address the amount is the only thing telling them apart — cross-user misattribution,
/// the invariant migration 0003 exists to protect. Held for a full day so a late or slow payment
/// is still identifiable by a human reconciling custody.
const SLOT_HOLD_HOURS: i64 = 24;

/// One pass: fetch confirmed custody transfers once, credit exact matches, close stale payment
/// windows, and report anything that arrived for no intent at all.
pub async fn poll_once(pool: &PgPool, watcher: &dyn CustodyWatcher) {
    let transfers = match watcher.recent_transfers().await {
        Ok(t) => t,
        Err(e) => {
            // Transient by assumption — TronGrid down, throttled, or a network blip. Nothing is
            // persisted from a failed fetch and no intent is advanced or expired on the strength
            // of an unread chain, which would be indistinguishable from "nobody paid".
            alert(pool, "warn", "poller", &format!("custody fetch failed: {e}")).await;
            return;
        }
    };

    match_due_intents(pool, &transfers).await;
    sweep_expired(pool).await;
    close_stale_payment_windows(pool).await;
    report_unattributed(pool, &transfers).await;
}

/// Credit every payable intent whose exact discriminated amount is present on chain.
async fn match_due_intents(pool: &PgPool, transfers: &[ObservedTransfer]) {
    let due = match sqlx::query_as::<_, (uuid::Uuid, i64)>(
        // `expired` is included on purpose: our expiry is a soft local timer, and at par there is
        // no FX risk in honouring a late payment. It stops being creditable when the payment
        // window closes below, not when the TTL lapses.
        "SELECT id, pay_amount_usdt FROM deposit_intents
         WHERE status IN ('created', 'invoiced', 'paying', 'expired')
           AND NOT payment_window_closed",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("poller: failed to select payable intents: {e}");
            return;
        }
    };

    for (id, pay_amount_usdt) in due {
        let Some(observed) = match_exact(transfers, pay_amount_usdt) else { continue };

        // Store the hash first. If the transition below fails or this process dies here, the next
        // pass re-matches the same transfer and re-stores the same hash — whereas transitioning
        // first could leave a `confirmed` intent with no evidence recorded, which the verifier
        // would then have to resolve down its weaker no-hash path.
        if let Err(e) = deposits::set_tron_tx_id(pool, id, &observed.tx_id).await {
            tracing::error!("poller: failed to store tx id for intent {id}: {e}");
            continue;
        }

        // Guarded from-set, so a row that has already moved past this is a silent no-op rather
        // than being walked backwards.
        match deposits::transition(pool, id, &["created", "invoiced", "paying", "expired"], "confirmed").await {
            Ok(true) => {
                tracing::info!(
                    "deposit {id} confirmed: {} micro-USDT observed in {}",
                    observed.amount_usdt,
                    observed.tx_id
                );
            }
            Ok(false) => {} // already past `confirmed`; nothing to do.
            Err(e) => tracing::error!("poller: failed to confirm intent {id}: {e}"),
        }
    }
}

/// Soft-expire anything past its pay-in window that has not moved on.
///
/// Deliberately only `created`/`invoiced`/`paying`: this must never touch
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

/// Release discriminator slots whose payment window has closed.
///
/// This is what Bitcart's terminal status used to decide, now driven by a clock we own rather
/// than a third party's opinion about an invoice we can no longer create. Never closes the window on a row a human is holding
/// (`needs_manual`) — that money may still be sitting in custody awaiting a decision, and freeing
/// its amount would let a later user be allocated it.
async fn close_stale_payment_windows(pool: &PgPool) {
    if let Err(e) = sqlx::query(
        "UPDATE deposit_intents
         SET payment_window_closed = TRUE, updated_at = now()
         WHERE NOT payment_window_closed
           AND status IN ('expired', 'failed')
           AND expires_at < now() - ($1 || ' hours')::interval",
    )
    .bind(SLOT_HOLD_HOURS.to_string())
    .execute(pool)
    .await
    {
        tracing::error!("poller: failed to close stale payment windows: {e}");
    }
}

/// Alert on money that arrived for no intent.
///
/// Nothing watched for this before, and it is the exact shape of the loss found during the first
/// real deposit run: a confirmed payment of a discriminated amount whose intent had expired, left
/// stranded in custody with no automatic recovery and nothing reporting it. The reserve
/// cross-check that should have noticed was independently returning zero. Two silent failures
/// lining up is how a paying user gets nothing and nobody finds out.
///
/// Reported, never auto-credited: attributing an unmatched payment would be a guess, and guessing
/// on the mint path is what the discriminator exists to avoid.
async fn report_unattributed(pool: &PgPool, transfers: &[ObservedTransfer]) {
    for t in transfers {
        // Any intent at all, in any state — a transfer matching a long-credited intent is that
        // deposit, not an orphan.
        let known = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM deposit_intents WHERE pay_amount_usdt = $1",
        )
        .bind(t.amount_usdt)
        .fetch_one(pool)
        .await;

        match known {
            Ok(0) => {
                // Deduplicated by the alerts table's own uniqueness on message, so a standing
                // orphan does not re-page every tick.
                alert(
                    pool,
                    "warn",
                    "poller",
                    &format!(
                        "unattributed custody payment: {} micro-USDT in tx {} matches no deposit intent \
                         — funds are in custody with no claim against them",
                        t.amount_usdt, t.tx_id
                    ),
                )
                .await;
            }
            Ok(_) => {}
            Err(e) => tracing::error!("poller: failed to check attribution for {}: {e}", t.tx_id),
        }
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
