//! Deposit intents: the row shape for a user's credited (or in-flight) USDT payment, and the
//! guarded transitions that carry it from `confirmed` through the treasury mint bridge. Nothing
//! in this module creates a row any more — the amount-bearing, idempotency-keyed create flow
//! this file used to own is gone along with the endpoint that drove it (see `api.rs`); what
//! replaces it (crediting an observed on-chain transfer) is a later change. This module now only
//! owns the row and the transitions `poller.rs` and `treasury_bridge.rs` drive it through.
//!
//! At par (1 USD = 1,000,000 CLT, USDT also 6 decimals) amount_usdt == amount_clt is an
//! integer identity — no rate arithmetic anywhere in this file.

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct DepositIntent {
    pub id: Uuid,
    pub user_pk: String,
    pub clt_address: String,
    pub amount_usdt: i64,
    pub amount_clt: i64,
    pub status: String,
    pub client_key: String,
    pub invoice_id: Option<String>,
    pub tron_tx_id: Option<String>,
    pub response_status: Option<i16>,
    pub response_body: Option<serde_json::Value>,
    pub payment_window_closed: bool,
    /// BIP32 non-hardened index this intent's deposit address was derived at (`<account>/0/index`).
    /// `None` only on discriminator-era rows that predate per-intent addresses.
    pub derivation_index: Option<i64>,
    /// The Tron address derived at `derivation_index` — this intent's own receive address, not a
    /// shared custody address. `None` only on legacy rows.
    pub deposit_address: Option<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// Set once `treasury_bridge`'s POST to `/internal/mint-intents` succeeds (migration 0004)
    /// — the id the bridge polls `GET /internal/mint-intents/:id` against. `None` until then.
    pub treasury_intent_id: Option<Uuid>,
    /// Consecutive treasury-unreachable failures on whichever step (create or poll) is
    /// currently live for this row — reset to 0 on any successful call, alerted at 10.
    pub attempts: i32,
    /// Jittered-backoff gate: the bridge's scan only picks up a row once `now() >=
    /// next_attempt_at`, so a failed attempt doesn't get retried on every single poll tick.
    pub next_attempt_at: chrono::DateTime<chrono::Utc>,
    /// What actually arrived on chain, which is what gets credited — `amount_usdt` is only what the
    /// user asked for, and the two differ whenever someone overpays.
    ///
    /// `None` until settled, and on rows that predate the column. Callers must fall back to
    /// `amount_usdt` for those rather than treating a missing figure as zero.
    pub received_usdt: Option<i64>,
    /// When this row was created. `due_for_mint_request`/`due_for_status_poll` have always
    /// ordered by this column in raw SQL without needing it back as a field; `recent_for_user`'s
    /// history view is the first caller that has to show it, which is why it joins `INTENT_COLS`
    /// only now.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

const INTENT_COLS: &str = "id, user_pk, clt_address, amount_usdt, amount_clt, \
    status, client_key, invoice_id, tron_tx_id, response_status, response_body, payment_window_closed, \
    derivation_index, deposit_address, expires_at, \
    treasury_intent_id, attempts, next_attempt_at, received_usdt, created_at";

/// Claim the next BIP32 derivation index for a deposit address.
///
/// `nextval` is the entire safety argument. It never returns a value twice — not across concurrent
/// transactions, and not when the caller's transaction later rolls back, because sequences are
/// deliberately non-transactional. Two intents sharing an index would share a deposit ADDRESS, and
/// the signer derives spending keys from the index, so that is two users with a claim on one pot of
/// money.
///
/// Called OUTSIDE the insert transaction, on purpose: the address has to be derived from the index
/// before the row can be written, and a row must never exist without its address. The cost is that
/// a subsequently-failed insert burns an index. Gaps are harmless; reuse is not. Do not be tempted
/// to reclaim one.
pub async fn allocate_derivation_index(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT nextval('deposit_derivation_index_seq')")
        .fetch_one(pool)
        .await
}

pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<DepositIntent>, sqlx::Error> {
    sqlx::query_as::<_, DepositIntent>(&format!("SELECT {INTENT_COLS} FROM deposit_intents WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// The caller's own deposits, newest first, excluding `expired` legacy intents. Scoped by
/// `user_pk` — the only thing standing between one user's deposit history and another's, so it is
/// not optional and not a filter applied after the fact: the WHERE clause IS the access control
/// here, same as everywhere else in this file that a caller-scoped row matters.
///
/// `status <> 'expired'` excludes pre-permanent-address legacy intents: an expired row is an
/// invoice nobody ever paid, not a deposit that happened, so it has no business in a deposit
/// history. Excluded here, in SQL, rather than filtered by the caller after the fetch — with
/// `LIMIT` below, a post-hoc filter would still spend the cap on rows the user is never shown,
/// silently crowding out real deposits (stage currently carries 33 such rows).
pub async fn recent_for_user(pool: &PgPool, user_pk: &str, limit: i64) -> Result<Vec<DepositIntent>, sqlx::Error> {
    sqlx::query_as::<_, DepositIntent>(&format!(
        "SELECT {INTENT_COLS} FROM deposit_intents \
         WHERE user_pk = $1 AND status <> 'expired' ORDER BY created_at DESC LIMIT $2"
    ))
    .bind(user_pk)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Records the on-chain tx id once the custody poller matches a transfer to this intent.
/// `WHERE tron_tx_id IS NULL` keeps the first-seen hash rather than letting a later refetch
/// (e.g. the poller re-confirming an already-confirmed intent) overwrite it.
/// Record what the chain actually paid to this intent's address.
///
/// Guarded on IS NULL so a later pass over an already-settled intent cannot rewrite the figure the
/// credit was based on. A second transfer arriving after settlement is a separate matter for a
/// human, not something that silently changes what we believe we owe.
pub async fn set_received_usdt(pool: &PgPool, id: Uuid, received: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE deposit_intents SET received_usdt = $2, updated_at = now()
         WHERE id = $1 AND received_usdt IS NULL",
    )
    .bind(id)
    .bind(received)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn set_tron_tx_id(pool: &PgPool, id: Uuid, tron_tx_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE deposit_intents SET tron_tx_id = $1, updated_at = now() WHERE id = $2 AND tron_tx_id IS NULL")
        .bind(tron_tx_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Guarded status transition. Out-of-order webhook events are absorbed here by construction:
/// a `failed` arriving after `confirmed` simply doesn't match any `from` set that includes
/// `confirmed`'s current value the way this call names it, so it's a no-op `Ok(false)`, not
/// an error and not a regression.
pub async fn transition(pool: &PgPool, id: Uuid, from: &[&str], to: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE deposit_intents SET status = $1, updated_at = now() WHERE id = $2 AND status = ANY($3)",
    )
    .bind(to)
    .bind(id)
    .bind(from)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// `treasury_bridge`'s (5b) equivalent of `set_tron_tx_id`: records the treasury's own intent
/// id once the create POST succeeds. `WHERE treasury_intent_id IS NULL` is a CAS, not a mere
/// convenience — it makes storing the id idempotent against a retried/duplicated call the same
/// way `set_tron_tx_id` does, keeping the first-seen id rather than letting a later replay
/// response (same value, since `client_ref` replay returns the SAME treasury intent) clobber it.
pub async fn set_treasury_intent_id(pool: &PgPool, id: Uuid, treasury_intent_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE deposit_intents SET treasury_intent_id = $1, updated_at = now() WHERE id = $2 AND treasury_intent_id IS NULL",
    )
    .bind(treasury_intent_id)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Base and cap for the bridge's jittered backoff (`record_attempt_failure`) — same shape as an
/// exponential-with-ceiling schedule, kept as two plain constants rather than a config knob
/// since nothing outside this crate needs to tune it.
const BACKOFF_BASE_SECS: i64 = 5;
const BACKOFF_MAX_SECS: i64 = 300;

/// A treasury-unreachable failure on this row's currently-live step (create or poll — never
/// both at once, see the column's doc comment): bump `attempts` and push `next_attempt_at` out
/// by a jittered exponential backoff capped at `BACKOFF_MAX_SECS`, so a treasury outage spaces
/// retries out instead of spinning the log on every poll tick. Returns the new attempt count —
/// the caller alerts once it crosses the brief's 10-consecutive-failures threshold.
pub async fn record_attempt_failure(pool: &PgPool, id: Uuid) -> Result<i32, sqlx::Error> {
    let (attempts,): (i32,) = sqlx::query_as(
        "UPDATE deposit_intents SET attempts = attempts + 1, updated_at = now() WHERE id = $1 RETURNING attempts",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;

    // Cap the EXPONENT, not just the final product: `attempts` is an unbounded counter (a
    // treasury outage lasting days at a 30s poll interval reaches the hundreds), and
    // `BASE * 2^attempts` would overflow i64 — and Rust panics on overflow in debug builds,
    // which is exactly the rig this runs in — long before `.min(BACKOFF_MAX_SECS)` gets a
    // chance to clamp it. Once 2^n alone exceeds the cap, growing n further changes nothing.
    let capped_exponent = attempts.max(0).min(BACKOFF_MAX_SECS.ilog2() as i32 + 1) as u32;
    let backoff_secs = (BACKOFF_BASE_SECS * 2i64.pow(capped_exponent)).min(BACKOFF_MAX_SECS);
    let jitter_secs = {
        use rand::Rng;
        rand::thread_rng().gen_range(0..=backoff_secs / 2)
    };
    sqlx::query("UPDATE deposit_intents SET next_attempt_at = now() + ($1 || ' seconds')::interval WHERE id = $2")
        .bind((backoff_secs + jitter_secs).to_string())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(attempts)
}

/// Resets the failure count after a successful treasury call — the next failure starts counting
/// from zero again rather than carrying over a prior outage's tally.
pub async fn reset_attempts(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE deposit_intents SET attempts = 0, updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Rows due for the bridge's create step: `confirmed` deposits whose backoff window has
/// elapsed. The `confirmed` row itself IS the pending-operation row (spec §6 outbox semantics —
/// it was written atomically with the state change by `webhook.rs`'s `confirm_and_credit`), so
/// there is no separate queue to scan.
pub async fn due_for_mint_request(pool: &PgPool) -> Result<Vec<DepositIntent>, sqlx::Error> {
    sqlx::query_as::<_, DepositIntent>(&format!(
        "SELECT {INTENT_COLS} FROM deposit_intents WHERE status = 'confirmed' AND next_attempt_at <= now() ORDER BY created_at"
    ))
    .fetch_all(pool)
    .await
}

/// Rows due for the bridge's status-poll step: `mint_requested` deposits (a treasury intent id
/// was already stored) whose backoff window has elapsed.
pub async fn due_for_status_poll(pool: &PgPool) -> Result<Vec<DepositIntent>, sqlx::Error> {
    sqlx::query_as::<_, DepositIntent>(&format!(
        "SELECT {INTENT_COLS} FROM deposit_intents
         WHERE status IN ('mint_requested', 'needs_manual') AND treasury_intent_id IS NOT NULL
           AND next_attempt_at <= now()
         ORDER BY created_at"
    ))
    .fetch_all(pool)
    .await
}

/// Pushes a deposit's next treasury status poll out by `hours`. Used once the treasury intent is
/// terminal (`rejected`/`failed`): the row stays pollable so a later resolution is noticed, but one
/// look a day is plenty for a state that only a human changes.
pub async fn defer_poll(pool: &PgPool, id: Uuid, hours: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE deposit_intents SET next_attempt_at = now() + ($2 * interval '1 hour') WHERE id = $1",
    )
    .bind(id)
    .bind(hours)
    .execute(pool)
    .await
    .map(|_| ())
}
