//! The reliability path (T4 brief): Bitcart's IPN is unsigned and never retried, so the
//! webhook (`webhook.rs`) is only latency reduction — every state it can reach must also be
//! reachable by this poller alone, on a plain timer, with no dependency on Bitcart having
//! delivered anything. It calls the exact same `apply_invoice_update` the webhook does, so
//! that property holds by construction rather than by keeping two code paths in sync by hand.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::adapter::PaymentAdapter;
use crate::webhook::apply_invoice_update;

/// `webhook_events` rows exist for idempotency-layer-2 dedup on recent deliveries; nothing
/// reads one 30 days later, so this bounds the table instead of growing it forever.
const WEBHOOK_EVENT_RETENTION_DAYS: i64 = 30;

/// `created` intents this old with an invoice on file are as stuck as `invoiced` ones — this
/// only matters for a create-flow that stored `invoice_id` without also flipping `status` to
/// `invoiced` (shouldn't happen given `store_invoice`'s single UPDATE, but the brief calls out
/// this set explicitly as a belt-and-braces catch-up).
const STUCK_CREATED_MINUTES: i64 = 2;

/// One poll pass: refetch every non-terminal invoiced/paying/late-expired intent, soft-expire
/// anything past `expires_at`, and sweep old webhook_events. Runs on `poll_interval_secs.`
pub async fn poll_once(pool: &PgPool, adapter: &dyn PaymentAdapter) {
    for invoice_id in due_invoice_ids(pool).await {
        apply_invoice_update(pool, adapter, &invoice_id).await;
    }
    sweep_expired(pool).await;
    sweep_old_webhook_events(pool).await;
}

/// Selects `invoice_id`s for every intent this poll pass must refetch:
/// - `invoiced` / `paying`: normal in-flight polling.
/// - `expired` with `bitcart_terminal = FALSE`: the late-payment window (Decisions block —
///   our soft expiry does not mean Bitcart's invoice is dead; refetch until Bitcart says so).
/// - `created` older than `STUCK_CREATED_MINUTES` that somehow already has an invoice_id.
/// - `needs_manual` / `failed` with `bitcart_terminal = FALSE`: NOT to change their status —
///   `apply_invoice_update`'s from-sets deliberately never move a `needs_manual` row, so the
///   human's flag stands. These are polled purely so `bitcart_terminal` eventually gets set,
///   which is what releases the discriminator slot those rows are holding (migration 0003).
///   Without this, an underpaid deposit would reserve its amount forever.
async fn due_invoice_ids(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT invoice_id FROM deposit_intents
         WHERE invoice_id IS NOT NULL
           AND (
             status IN ('invoiced', 'paying')
             OR (status IN ('expired', 'needs_manual', 'failed') AND NOT bitcart_terminal)
             OR (status = 'created' AND created_at < now() - ($1 || ' minutes')::interval)
           )",
    )
    .bind(STUCK_CREATED_MINUTES.to_string())
    .fetch_all(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::error!("poller: failed to select due invoices: {e}");
        Vec::new()
    })
}

/// Soft-expire anything past its pay-in window that hasn't already moved on. Deliberately
/// only `created`/`invoiced`/`paying` — `expired` is idempotent (no-op to re-set), and this
/// must never touch `confirmed`/`mint_requested`/`credited`/`failed`/`needs_manual`, all of
/// which are further along than a mere timeout should ever move them.
async fn sweep_expired(pool: &PgPool) {
    let result = sqlx::query(
        "UPDATE deposit_intents SET status = 'expired', updated_at = now()
         WHERE status IN ('created', 'invoiced', 'paying') AND expires_at < now()",
    )
    .execute(pool)
    .await;
    if let Err(e) = result {
        tracing::error!("poller: failed to sweep expired intents: {e}");
    }
}

async fn sweep_old_webhook_events(pool: &PgPool) {
    let result = sqlx::query(
        "DELETE FROM webhook_events WHERE received_at < now() - ($1 || ' days')::interval",
    )
    .bind(WEBHOOK_EVENT_RETENTION_DAYS.to_string())
    .execute(pool)
    .await;
    if let Err(e) = result {
        tracing::error!("poller: failed to sweep old webhook_events: {e}");
    }
}

/// Spawned once from `main.rs`. Runs forever on `poll_interval_secs` — no backoff/jitter here
/// (unlike T5's treasury-bridge worker, this loop has no outbound dependency that fails as a
/// unit; each intent's refetch failure is handled and logged individually inside
/// `apply_invoice_update`, so a bad interval only delays the next attempt by one tick).
pub async fn run(pool: PgPool, adapter: Arc<dyn PaymentAdapter>, poll_interval_secs: u64) {
    let mut interval = tokio::time::interval(Duration::from_secs(poll_interval_secs));
    loop {
        interval.tick().await;
        poll_once(&pool, adapter.as_ref()).await;
    }
}
