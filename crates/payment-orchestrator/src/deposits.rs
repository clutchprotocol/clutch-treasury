//! Deposit intents: the record of "a user intends to pay us USDT" and the idempotency
//! that keeps one payment from being credited twice. This module owns the row-level
//! mechanics and (below, `create_and_invoice`) the create-flow orchestration — no HTTP
//! itself (that's `api.rs`), but it does call the `PaymentAdapter` (T3) to turn a fresh
//! intent into a live Bitcart invoice. No treasury calls: the daily-headroom check needs
//! the treasury's `reserve-status` to expose `daily_headroom_clt`, which doesn't exist yet
//! (lands in T5, which already touches treasury-service) — bounds checks only for now.
//!
//! At par (1 USD = 1,000,000 CLT, USDT also 6 decimals) amount_usdt == amount_clt is an
//! integer identity — no rate arithmetic anywhere in this file.

use rand::seq::SliceRandom;
use serde::Serialize;
use serde_json::json;
use sqlx::{Acquire, PgPool};
use uuid::Uuid;

use crate::adapter::PaymentAdapter;
use crate::configuration::OrchConfig;

const DISCRIMINATOR_RANGE_END: i64 = 999;

#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct DepositIntent {
    pub id: Uuid,
    pub user_pk: String,
    pub clt_address: String,
    pub amount_usdt: i64,
    pub pay_amount_usdt: i64,
    pub amount_clt: i64,
    pub status: String,
    pub client_key: String,
    pub invoice_id: Option<String>,
    pub tron_tx_id: Option<String>,
    pub response_status: Option<i16>,
    pub response_body: Option<serde_json::Value>,
    pub bitcart_terminal: bool,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

const INTENT_COLS: &str = "id, user_pk, clt_address, amount_usdt, pay_amount_usdt, amount_clt, \
    status, client_key, invoice_id, tron_tx_id, response_status, response_body, bitcart_terminal, expires_at";

#[derive(Debug)]
pub enum CreateOutcome {
    /// Fresh row (or crash-resume of one) ready for the caller to invoice.
    Created(DepositIntent),
    /// Same key + same body + a previous attempt already stored a response: hand it back
    /// verbatim, original status included — spec §6 requires the original status code, not
    /// just the original body.
    Replay { status: i16, body: serde_json::Value },
    /// Same key + a different body: refuse, don't guess which request was "real".
    Conflict,
    /// Key exists and is locked by a concurrent in-flight create (FOR UPDATE SKIP LOCKED
    /// found nothing while the row is known to exist) — caller should 409 + Retry-After.
    StillProcessing,
}

#[derive(Debug)]
pub enum ApiError {
    /// amount_usdt outside [min_deposit_usdt, max_deposit_usdt].
    OutOfBounds { min: i64, max: i64 },
    /// All 999 discriminator slots for this amount are occupied by other active intents.
    NoDiscriminatorSlot,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Db(e)
    }
}

/// Postgres default-generated name for the table's plain `UNIQUE (user_pk, client_key)`.
const IDEMPOTENCY_KEY_CONSTRAINT: &str = "deposit_intents_user_pk_client_key_key";

fn unique_violation_constraint(e: &sqlx::Error) -> Option<&str> {
    let db_err = e.as_database_error()?;
    if db_err.code().as_deref() != Some("23505") {
        return None;
    }
    db_err.constraint()
}

/// Same request body means same deposit ask — clt_address and amount_usdt are the fields
/// that define what the user asked for; user_pk + client_key already selected this row.
fn same_body(intent: &DepositIntent, clt_address: &str, amount_usdt: i64) -> bool {
    intent.clt_address == clt_address && intent.amount_usdt == amount_usdt
}

/// Idempotency layers 1 + the discriminator: look up `(user_pk, client_key)`, and either
/// replay a stored response, reject a body mismatch, resume a crashed attempt with a fresh
/// discriminator, or insert a brand new intent — one function, spec §6's semantics exactly.
pub async fn create(
    pool: &PgPool,
    config: &OrchConfig,
    user_pk: &str,
    clt_address: &str,
    amount_usdt: i64,
    client_key: &str,
) -> Result<CreateOutcome, ApiError> {
    if amount_usdt < config.min_deposit_usdt || amount_usdt > config.max_deposit_usdt {
        return Err(ApiError::OutOfBounds { min: config.min_deposit_usdt, max: config.max_deposit_usdt });
    }

    let mut tx = pool.begin().await?;

    let locked = sqlx::query_as::<_, DepositIntent>(&format!(
        "SELECT {INTENT_COLS} FROM deposit_intents WHERE user_pk = $1 AND client_key = $2 FOR UPDATE SKIP LOCKED"
    ))
    .bind(user_pk)
    .bind(client_key)
    .fetch_optional(&mut *tx)
    .await?;

    let existing = match locked {
        Some(row) => Some(row),
        None => {
            // SKIP LOCKED found nothing — either no row exists, or one exists and another
            // writer holds its lock. Distinguish with a plain unlocked read.
            let unlocked: Option<(Uuid,)> =
                sqlx::query_as("SELECT id FROM deposit_intents WHERE user_pk = $1 AND client_key = $2")
                    .bind(user_pk)
                    .bind(client_key)
                    .fetch_optional(&mut *tx)
                    .await?;
            tx.rollback().await?;
            return Ok(match unlocked {
                Some(_) => CreateOutcome::StillProcessing,
                None => match insert_new(pool, config, user_pk, clt_address, amount_usdt, client_key).await? {
                    Some(intent) => CreateOutcome::Created(intent),
                    // The unlocked check above raced a concurrent insert between the two
                    // reads; the unique (user_pk, client_key) index means that insert now
                    // either won (fresh row, nothing to allocate against) or we lost the
                    // race entirely. Either way the caller should retry the same request.
                    None => CreateOutcome::StillProcessing,
                },
            });
        }
    };

    let Some(intent) = existing else { unreachable!() };

    if !same_body(&intent, clt_address, amount_usdt) {
        tx.rollback().await?;
        return Ok(CreateOutcome::Conflict);
    }

    match (&intent.response_status, &intent.response_body) {
        (Some(status), Some(body)) => {
            tx.rollback().await?;
            Ok(CreateOutcome::Replay { status: *status, body: body.clone() })
        }
        _ => {
            // response_body IS NULL: a previous attempt died between `create` and
            // `store_invoice` (a crash, or an adapter error/timeout where Bitcart may
            // still have created the invoice). Resume on the SAME pay_amount_usdt.
            //
            // Reusing the amount is what keeps this safe, not what makes it risky. This
            // row is the only holder of that slot under uq_active_pay_amount, and the
            // schema's invariant is that a slot stays reserved until Bitcart itself can
            // no longer match a payment to it. Moving the row to a fresh slot would
            // release the old amount while a possibly-live orphan invoice still carries
            // it, letting a LATER, DIFFERENT intent be allocated that amount — the
            // cross-user misattribution on the shared static custody address that the
            // discriminator exists to prevent.
            //
            // Two live invoices at one amount for THIS SAME intent is not that hazard:
            // both carry this intent's order_id, this user, this credit, and whichever
            // one Bitcart matches, `store_invoice`'s compare-and-set plus the per-invoice
            // webhook event key still credit exactly once. A second genuine payment at
            // the same amount strands on the `transition` guards and lands in
            // needs_manual — funds held and flagged, never paid to a stranger.
            tx.rollback().await?;
            Ok(CreateOutcome::Created(intent))
        }
    }
}

/// Brand new `(user_pk, client_key)`: allocate a discriminator and insert. `ON CONFLICT`
/// on the (user_pk, client_key) unique constraint means a concurrent request beat us to
/// this exact key — the caller re-checks and treats it as StillProcessing/retry rather
/// than us guessing at the winner's state here.
async fn insert_new(
    pool: &PgPool,
    config: &OrchConfig,
    user_pk: &str,
    clt_address: &str,
    amount_usdt: i64,
    client_key: &str,
) -> Result<Option<DepositIntent>, ApiError> {
    let mut tx = pool.begin().await?;
    let id = Uuid::new_v4();
    let expires_at = chrono::Utc::now() + chrono::Duration::minutes(config.deposit_ttl_minutes);

    let mut shuffled: Vec<i64> = (1..=DISCRIMINATOR_RANGE_END).collect();
    shuffled.shuffle(&mut rand::thread_rng());

    for d in shuffled {
        let pay_amount = amount_usdt + d;
        // SAVEPOINT per attempt: a failed INSERT poisons the rest of the enclosing
        // transaction in Postgres (every later statement errors "current transaction is
        // aborted" until rollback), so retrying the shuffled range needs a nested
        // transaction to roll back to, not just `continue` inside one `tx`.
        let mut attempt = tx.begin().await?;
        let row = sqlx::query_as::<_, DepositIntent>(&format!(
            "INSERT INTO deposit_intents
                (id, user_pk, clt_address, amount_usdt, pay_amount_usdt, amount_clt, client_key, expires_at)
             VALUES ($1, $2, $3, $4, $5, $4, $6, $7)
             RETURNING {INTENT_COLS}"
        ))
        .bind(id)
        .bind(user_pk)
        .bind(clt_address)
        .bind(amount_usdt)
        .bind(pay_amount)
        .bind(client_key)
        .bind(expires_at)
        .fetch_one(&mut *attempt)
        .await;

        match row {
            Ok(intent) => {
                attempt.commit().await?;
                tx.commit().await?;
                return Ok(Some(intent));
            }
            Err(e) => match unique_violation_constraint(&e) {
                // Lost a genuine create-vs-create race for this exact idempotency key —
                // not a discriminator collision. Retrying the amount loop would just fail
                // 999 more times against the same (user_pk, client_key) row; stop now and
                // let the caller re-run create(), which will see the now-committed row and
                // resolve Replay/Conflict/StillProcessing correctly (brief: "Insert race
                // resolved by the unique index (ON CONFLICT catch -> re-fetch -> compare)").
                Some(c) if c == IDEMPOTENCY_KEY_CONSTRAINT => {
                    attempt.rollback().await?;
                    tx.rollback().await?;
                    return Ok(None);
                }
                // uq_active_pay_amount (or any other unique index on this table): a
                // different amount collision, keep iterating the shuffled range.
                Some(_) => {
                    attempt.rollback().await?;
                    continue;
                }
                None => return Err(e.into()),
            },
        }
    }
    tx.rollback().await?;
    Err(ApiError::NoDiscriminatorSlot)
}

/// Idempotency layer 4: Bitcart has no server-side order_id dedup, so this compare-and-set
/// on invoice_id is what makes "store the created invoice" exactly-once. Zero rows affected
/// means a concurrent writer already stored a different invoice first; that writer's
/// response is canonical, so the caller should re-fetch and replay it rather than treat
/// this as an error.
pub async fn store_invoice(
    pool: &PgPool,
    id: Uuid,
    invoice_id: &str,
    response_status: i16,
    response_body: &serde_json::Value,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE deposit_intents SET invoice_id=$1, response_status=$2, response_body=$3, status='invoiced', updated_at=now()
         WHERE id=$4 AND invoice_id IS NULL",
    )
    .bind(invoice_id)
    .bind(response_status)
    .bind(response_body)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<DepositIntent>, sqlx::Error> {
    sqlx::query_as::<_, DepositIntent>(&format!("SELECT {INTENT_COLS} FROM deposit_intents WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// The webhook/poller lookup key (T4): Bitcart's IPN and `get_invoice` both key on
/// `invoice_id`, never on our own `id`. An invoice_id no row holds returns `None` — the
/// caller's job (spam resistance: store nothing, call nothing for an unknown id).
pub async fn find_by_invoice_id(pool: &PgPool, invoice_id: &str) -> Result<Option<DepositIntent>, sqlx::Error> {
    sqlx::query_as::<_, DepositIntent>(&format!("SELECT {INTENT_COLS} FROM deposit_intents WHERE invoice_id = $1"))
        .bind(invoice_id)
        .fetch_optional(pool)
        .await
}

/// Sets `bitcart_terminal = TRUE` — the ONLY thing that frees the discriminator slot
/// (`uq_active_pay_amount`'s `WHERE ... AND NOT bitcart_terminal` clause). Deliberately
/// unconditional (no status guard): terminality is a fact about the Bitcart invoice, not
/// about our own state machine, and setting it twice is harmless.
pub async fn mark_bitcart_terminal(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE deposit_intents SET bitcart_terminal = TRUE, updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Records the on-chain tx id once Bitcart's refetch surfaces one (`Confirmed`/`PaidOver`).
/// `WHERE tron_tx_id IS NULL` keeps the first-seen hash rather than letting a later refetch
/// (e.g. the poller re-confirming an already-confirmed intent) overwrite it.
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

/// HTTP-shaped result of the create-flow (T2b): `api.rs` matches this directly onto a
/// status code + optional header, keeping this module free of any `axum` dependency.
#[derive(Debug)]
pub enum DepositOutcome {
    /// Status + body to return VERBATIM — either a genuine 201 just created, or an
    /// idempotent replay of an earlier attempt's stored response (spec §6: replay must
    /// return the ORIGINAL status, not a fresh 201).
    Respond { status: u16, body: serde_json::Value },
    /// Same key, different body — 409, no `Retry-After` (the client's confusion isn't a
    /// timing issue, it needs a new key or the same body it used before).
    Conflict,
    /// Key exists and a concurrent create holds its lock — 409 + `Retry-After: 2`.
    StillProcessing,
    /// amount_usdt outside configured bounds.
    OutOfBounds { min: i64, max: i64 },
    /// Bitcart call or DB write failed — caller 5xx's without leaking internals.
    Failed(String),
}

/// The create-flow orchestration (T2b): routes `deposits::create`'s outcome (idempotency
/// layer 1 + the discriminator, Task 2) through `adapter.create_invoice` and the
/// compare-and-set store (idempotency layer 4) — the two mechanisms meeting the wire.
///
/// Ordering that keeps money safe: the intent row is created BEFORE calling Bitcart, and
/// `store_invoice`'s compare-and-set runs AFTER — so a crash between the two leaves an
/// orphan invoice (accepted; see `deposits::create`'s resume branch), never a lost intent
/// row with no corresponding invoice attempt recorded.
pub async fn create_and_invoice(
    pool: &PgPool,
    config: &OrchConfig,
    adapter: &dyn PaymentAdapter,
    user_pk: &str,
    clt_address: &str,
    amount_usdt: i64,
    client_key: &str,
    notification_url: &str,
) -> DepositOutcome {
    // ponytail: daily-headroom check omitted here — it needs the treasury's
    // reserve-status to expose daily_headroom_clt, which treasury-service doesn't
    // implement yet (lands in T5, which already touches that service). Bounds checks
    // (min_deposit_usdt..=max_deposit_usdt, enforced inside deposits::create) are the
    // only cap for now.
    let outcome = match create(pool, config, user_pk, clt_address, amount_usdt, client_key).await {
        Ok(o) => o,
        Err(ApiError::OutOfBounds { min, max }) => return DepositOutcome::OutOfBounds { min, max },
        Err(ApiError::NoDiscriminatorSlot) => {
            return DepositOutcome::Failed("no discriminator slot available for this amount".into())
        }
        Err(ApiError::Db(e)) => return DepositOutcome::Failed(e.to_string()),
    };

    let intent = match outcome {
        CreateOutcome::Replay { status, body } => return DepositOutcome::Respond { status: status as u16, body },
        CreateOutcome::Conflict => return DepositOutcome::Conflict,
        CreateOutcome::StillProcessing => return DepositOutcome::StillProcessing,
        CreateOutcome::Created(intent) => intent,
    };

    // Bitcart has no server-side order_id dedup (module docs, adapter.rs module docs) —
    // the intent id is unique per row regardless of how many times this call is retried,
    // so passing it as order_id is informational only; store_invoice's CAS below is what
    // actually makes this exactly-once, not Bitcart's own idempotency.
    let instructions = match adapter
        .create_invoice(&intent.id.to_string(), intent.pay_amount_usdt, notification_url)
        .await
    {
        Ok(i) => i,
        Err(e) => return DepositOutcome::Failed(e),
    };

    let body = json!({
        "id": intent.id,
        "pay_address": instructions.pay_address,
        "pay_amount_usdt": instructions.pay_amount_usdt,
        "expires_at": instructions.expires_at,
        "status": "invoiced",
    });

    match store_invoice(pool, intent.id, &instructions.invoice_id, 201, &body).await {
        Ok(true) => DepositOutcome::Respond { status: 201, body },
        // Lost the compare-and-set: another writer already stored a (possibly different)
        // invoice for this same intent row first. Their invoice is canonical — re-fetch
        // and replay it rather than pretend ours won.
        Ok(false) => match find_by_id(pool, intent.id).await {
            Ok(Some(row)) => match (row.response_status, row.response_body) {
                (Some(status), Some(body)) => DepositOutcome::Respond { status: status as u16, body },
                _ => DepositOutcome::Failed("lost invoice CAS but winner has no stored response".into()),
            },
            Ok(None) => DepositOutcome::Failed("intent vanished after losing invoice CAS".into()),
            Err(e) => DepositOutcome::Failed(e.to_string()),
        },
        Err(e) => DepositOutcome::Failed(e.to_string()),
    }
}
