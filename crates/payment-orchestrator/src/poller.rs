//! The detection path: poll every user's permanent deposit address, plus any still-open legacy
//! per-intent address, and credit what has arrived.
//!
//! There is no webhook any more, so this timer is not a latency optimisation over another path — it
//! is the only path, which is simpler to reason about than the previous arrangement where a webhook
//! could in principle reach a state the poller had to be kept able to reach too.
//!
//! Ordering inside a pass is deliberate: match BEFORE sweeping expiry, so a payment that landed
//! moments before the TTL is credited rather than raced into `expired` by our own timer.
//!
//! `poll_once` is two independent loops — permanent per-user addresses (a), then legacy per-intent
//! addresses (b) — followed by the expiry sweep and window close. Every stage runs every pass
//! regardless of whether an earlier one hit a snag, so one bad address or one bad row never starves
//! the rest.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::alerts::alert;
use crate::custody::{CustodyWatcher, DepositWatcher, ObservedTransfer};
use crate::deposits;

/// How long past expiry an address keeps being watched for a late payment.
///
/// Under per-address deposits this is far less load-bearing than the discriminator slot it replaced:
/// indices are never reused at all (migration 0007), so there is no slot to free and no cross-user
/// hazard in letting one go. What it still governs is how long we keep LOOKING, so a slow or
/// late-broadcast transfer is credited rather than silently stranded.
const WATCH_WINDOW_HOURS: i64 = 24;

/// Per-user addresses polled per pass — the budget `main.rs` hands to `TieredPoller`.
///
/// One TronGrid request per address is unavoidable: Tron cannot watch an xpub as a group, so derived
/// addresses have to be queried individually. Permanent addresses are never "closed" the way intents
/// were, so nothing shrinks this set on its own — see `due_addresses`, which is what turns this into
/// a rotating budget instead of an ever-growing one. A burst still has to degrade gracefully instead
/// of hammering an unkeyed endpoint into throttling, which looks exactly like "nobody is paying".
/// That failure mode has already cost a day of debugging once.
///
/// `pub`: `main.rs` builds `TieredPoller`'s budget from this number rather than a second guess.
pub const MAX_ADDRESSES_PER_PASS: i64 = 50;

/// Legacy per-intent rows polled per pass. Capped separately from `MAX_ADDRESSES_PER_PASS` so a
/// legacy backlog can never crowd out the per-user budget — stage has 28 such rows today and the set
/// only shrinks (new deposits go through the per-user address path exclusively). Hitting the cap is
/// LOGGED, never silent: a quietly truncated pass reads as "no payments found".
const MAX_LEGACY_INTENTS_PER_PASS: i64 = 50;

/// Record one observed transfer as its own deposit.
///
/// Returns `Ok(false)` when the transaction was already recorded, or when nothing actually moved
/// (`t.amount_usdt <= 0`). Neither is an error: a poll pass re-reads an address's recent history
/// every rotation, so the same transaction is seen many times, and TRON dust-poisoning sends 0-value
/// TRC-20 transfers routinely — `amount_usdt`/`received_usdt` both carry `CHECK (> 0)`, so crediting
/// one verbatim would be a recurring database error, not a real deposit. `uq_deposit_intents_tron_tx_id`
/// is what makes re-observation free.
///
/// `derivation_index` is the address's own (`deposit_addresses.derivation_index`), not derived from
/// the transfer. It is not optional: `treasury_bridge.rs` forwards it verbatim, and the treasury's
/// sweeper only ever selects `WHERE derivation_index IS NOT NULL` (`sweeper.rs`) — a credited deposit
/// that does not carry it is minted and then silently never swept.
///
/// The amount credited is what ARRIVED. There is no expected figure to reconcile against any more —
/// the user was never asked for one.
pub async fn credit_transfer(
    pool: &PgPool,
    user_pk: &str,
    clt_address: &str,
    derivation_index: i64,
    t: &ObservedTransfer,
) -> Result<bool, String> {
    if t.amount_usdt <= 0 {
        return Ok(false);
    }

    let done = sqlx::query(
        // client_key is NOT NULL and was the user's idempotency key when users created intents. The
        // chain creates them now, so the tx id IS the idempotency key — and it makes the pre-existing
        // UNIQUE (user_pk, client_key) a second guard behind uq_deposit_intents_tron_tx_id.
        // expires_at is NOT NULL and meaningless for an observed transfer; now() reads as "already
        // settled" rather than inventing a deadline nothing enforces. pay_amount_usdt (dropped in
        // migration 0008) is gone, and the address/derivation-index uniqueness (dropped in migration
        // 0011) no longer applies — nothing else this table still requires NOT NULL is missing here.
        "INSERT INTO deposit_intents
            (id, user_pk, clt_address, amount_usdt, amount_clt, status, client_key,
             deposit_address, tron_tx_id, received_usdt, expires_at, derivation_index)
         VALUES ($1, $2, $6, $3, $3, 'confirmed', $5, $4, $5, $3, now(), $7)
         ON CONFLICT (tron_tx_id) WHERE tron_tx_id IS NOT NULL DO NOTHING",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(user_pk)
    .bind(t.amount_usdt)
    .bind(&t.to)
    .bind(&t.tx_id)
    .bind(clt_address)
    .bind(derivation_index)
    .execute(pool)
    .await
    .map_err(|e| format!("crediting {}: {e}", t.tx_id))?;

    Ok(done.rows_affected() == 1)
}

/// Who owns a permanent address, if `to` is one, plus the derivation index `credit_transfer` must
/// carry so the treasury's sweeper can find it. `Ok(None)` means it is not a `deposit_addresses` row
/// at all — a legacy per-intent address, which loop (b) of `poll_once` owns instead.
async fn user_for_address(pool: &PgPool, to: &str) -> Result<Option<(String, String, i64)>, String> {
    sqlx::query_as::<_, (String, String, i64)>(
        "SELECT user_pk, clt_address, derivation_index FROM deposit_addresses WHERE address = $1",
    )
    .bind(to)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("resolving the owner of {to}: {e}"))
}

/// Loop (a): one pass over the permanent per-user addresses. `watcher.poll()` already selected which
/// addresses were due and stamped `last_polled_at` for all of them (`TieredPoller`) — this function
/// never learns how the transfers it is handed were found, which is the point of the seam.
///
/// A transfer to an address with no `deposit_addresses` row is not an error — it is a legacy
/// per-intent address, left to loop (b). A failure crediting one transfer must not stop the rest.
///
/// `usdt_contract` is re-checked here rather than trusted from upstream: `DepositWatcher` is
/// deliberately not address-scoped (see custody.rs's module docs), so a future implementation that
/// follows the USDT contract's Transfer events from a cursor and filters locally would have nothing
/// upstream of this function guaranteed to have already checked it.
pub async fn credit_from_addresses(
    pool: &PgPool,
    watcher: &dyn DepositWatcher,
    usdt_contract: &str,
) -> Result<(), String> {
    let transfers = watcher.poll().await?;
    for t in &transfers {
        if t.contract != usdt_contract {
            tracing::warn!(
                "poller: transfer {} to {} carries contract {} (expected {usdt_contract}) — skipped",
                t.tx_id,
                t.to,
                t.contract
            );
            continue;
        }
        match user_for_address(pool, &t.to).await {
            Ok(Some((user_pk, clt_address, derivation_index))) => {
                if let Err(e) = credit_transfer(pool, &user_pk, &clt_address, derivation_index, t).await {
                    tracing::error!("poller: failed to credit {}: {e}", t.tx_id);
                }
            }
            Ok(None) => {} // a legacy per-intent address; loop (b) owns it.
            Err(e) => tracing::error!("poller: {e}"),
        }
    }
    Ok(())
}

/// One pass: credit every transfer arriving at a permanent per-user address (a) or a still-open
/// legacy per-intent address (b), then expire and close. Neither loop's failure blocks the other,
/// and both are attempted — success or failure — before the expiry sweep and window close run.
pub async fn poll_once(
    pool: &PgPool,
    watcher: &dyn DepositWatcher,
    legacy: &dyn CustodyWatcher,
    usdt_contract: &str,
) {
    // (a) Permanent per-user addresses.
    if let Err(e) = credit_from_addresses(pool, watcher, usdt_contract).await {
        tracing::error!("poller: per-user address pass failed: {e}");
    }

    // (b) Legacy per-intent addresses — discriminator-era rows, which migration 0007 could not
    // backfill an address for, stay NULL and are skipped by due_intents; every other still-open
    // intent is watched here until its payment window closes. When due_intents returns nothing for
    // good, this whole block is dead code and can be deleted along with due_intents.
    let due = match due_intents(pool).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("poller: failed to select payable intents: {e}");
            Vec::new()
        }
    };
    if due.len() as i64 == MAX_LEGACY_INTENTS_PER_PASS {
        tracing::warn!(
            "poller: hit the {MAX_LEGACY_INTENTS_PER_PASS}-legacy-intent cap this pass — the legacy \
             set should only be shrinking; if it is not, something is stopping intents from \
             confirming or their windows from closing."
        );
    }

    for (id, address) in due {
        let transfers = match legacy.transfers_to(&address, None).await {
            Ok(t) => t,
            Err(e) => {
                // Transient by assumption: TronGrid down, throttled, or a blip. Nothing is persisted
                // and no intent is advanced or expired on the strength of an unread chain, which
                // would be indistinguishable from "nobody paid".
                alert(pool, "warn", "poller", &format!("custody fetch failed for {address}: {e}")).await;
                continue;
            }
        };

        // "Credit everything, cap nothing" applies here too: there is no expected amount to compare
        // against any more, so the first (earliest) unseen transfer settles the intent at its own
        // arrived amount. `Partial` — held, not credited, because it fell short of a promised figure
        // — no longer means anything, because nothing is promised.
        let Some(earliest) = transfers.iter().min_by_key(|t| t.block_timestamp) else {
            continue;
        };
        let tx_id = &earliest.tx_id;
        let received_usdt = earliest.amount_usdt;

        // Store the evidence FIRST. If this process dies here, the next pass reaches the same
        // conclusion and stores the same hash; transitioning first could leave a `confirmed` intent
        // with no evidence recorded, which the treasury's verifier would then have to resolve down
        // its weaker no-hash path.
        if let Err(e) = deposits::set_tron_tx_id(pool, id, tx_id).await {
            tracing::error!("poller: failed to store tx id for intent {id}: {e}");
            continue;
        }
        if let Err(e) = deposits::set_received_usdt(pool, id, received_usdt).await {
            tracing::error!("poller: failed to store received amount for intent {id}: {e}");
            continue;
        }
        match deposits::transition(pool, id, &["created", "invoiced", "paying", "expired"], "confirmed").await {
            Ok(true) => {
                tracing::info!("deposit {id} confirmed: {received_usdt} micro-USDT at {address} in {tx_id}")
            }
            Ok(false) => {} // already past `confirmed`; nothing to do.
            Err(e) => tracing::error!("poller: failed to confirm intent {id}: {e}"),
        }
    }

    sweep_expired(pool).await;
    close_stale_watch_windows(pool).await;
}

/// Legacy intents that can still take a payment, oldest first, capped.
///
/// `expired` is included on purpose: our expiry is a soft local timer and at par there is no FX risk
/// in honouring a late payment. An address stops being watched when its window closes, not when the
/// TTL lapses.
///
/// `deposit_address IS NOT NULL` is what makes this query "legacy": every row a permanent address
/// (Task 5) covers stays NULL here and is credited through loop (a) instead.
async fn due_intents(pool: &PgPool) -> Result<Vec<(uuid::Uuid, String)>, sqlx::Error> {
    sqlx::query_as::<_, (uuid::Uuid, String)>(
        "SELECT id, deposit_address FROM deposit_intents
         WHERE deposit_address IS NOT NULL
           AND NOT payment_window_closed
           AND status IN ('created', 'invoiced', 'paying', 'expired')
         ORDER BY created_at ASC
         LIMIT $1",
    )
    .bind(MAX_LEGACY_INTENTS_PER_PASS)
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
pub async fn run(
    pool: PgPool,
    watcher: Arc<dyn DepositWatcher>,
    legacy: Arc<dyn CustodyWatcher>,
    usdt_contract: String,
    poll_interval_secs: u64,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(poll_interval_secs));
    loop {
        interval.tick().await;
        poll_once(&pool, watcher.as_ref(), legacy.as_ref(), &usdt_contract).await;
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
