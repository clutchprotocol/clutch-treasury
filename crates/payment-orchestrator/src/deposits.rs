//! Deposit intents: the record of "a user intends to pay us USDT" and the idempotency
//! that keeps one payment from being credited twice. This module owns the row-level
//! mechanics only — no HTTP, no Bitcart adapter (T3), no treasury calls. T2b's route
//! handler wires this together with `adapter.create_invoice` and the daily-headroom check.
//!
//! At par (1 USD = 1,000,000 CLT, USDT also 6 decimals) amount_usdt == amount_clt is an
//! integer identity — no rate arithmetic anywhere in this file.

use rand::seq::SliceRandom;
use serde::Serialize;
use sqlx::{Acquire, PgPool};
use uuid::Uuid;

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
            // response_body IS NULL: a previous attempt crashed mid-flow. It may have
            // already created a Bitcart invoice we never recorded — reusing its
            // pay_amount_usdt would risk two live invoices sharing one amount on the
            // shared custody address, exactly what the discriminator exists to prevent.
            // Take a fresh slot (full shuffled retry, same savepoint pattern as a brand
            // new insert) and let the orphan expire.
            let mut shuffled: Vec<i64> = (1..=DISCRIMINATOR_RANGE_END).collect();
            shuffled.shuffle(&mut rand::thread_rng());

            for d in shuffled {
                let candidate = amount_usdt + d;
                let mut attempt = tx.begin().await?;
                let row = sqlx::query_as::<_, DepositIntent>(&format!(
                    "UPDATE deposit_intents SET pay_amount_usdt = $1, updated_at = now()
                     WHERE id = $2 RETURNING {INTENT_COLS}"
                ))
                .bind(candidate)
                .bind(intent.id)
                .fetch_one(&mut *attempt)
                .await;

                match row {
                    Ok(resumed) => {
                        attempt.commit().await?;
                        tx.commit().await?;
                        return Ok(CreateOutcome::Created(resumed));
                    }
                    // This UPDATE targets a single row by primary key (`id`), so it can
                    // only ever collide with uq_active_pay_amount — the identity
                    // constraint doesn't reference `id` and `id` isn't changing.
                    Err(e) if unique_violation_constraint(&e).is_some() => {
                        attempt.rollback().await?;
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            tx.rollback().await?;
            Err(ApiError::NoDiscriminatorSlot)
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
