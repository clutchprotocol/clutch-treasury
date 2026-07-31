//! Deposit intents: the record of "a user intends to pay us USDT" and the idempotency
//! that keeps one payment from being credited twice. This module owns the row-level
//! mechanics and (below, `create_and_invoice`) the create-flow orchestration — no HTTP
//! itself (that's `api.rs`). It used to call a `PaymentAdapter` to turn a fresh intent into a
//! live Bitcart invoice; there is no gateway any more (see `custody.rs`), so the pay address and
//! window are read straight from config. It still calls (T2b's deferral, landed in 5b) the treasury's
//! `reserve-status` for the daily-headroom check. The bridge worker that crosses into the
//! treasury's private zone to actually request a mint lives in `treasury_bridge.rs`, not here —
//! this module only owns the row and its guarded transitions.
//!
//! At par (1 USD = 1,000,000 CLT, USDT also 6 decimals) amount_usdt == amount_clt is an
//! integer identity — no rate arithmetic anywhere in this file.

use serde::Serialize;
use serde_json::json;
use sqlx::{Acquire, PgPool};
use uuid::Uuid;

use crate::configuration::OrchConfig;
use crate::derive::AddressDeriver;

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
}

const INTENT_COLS: &str = "id, user_pk, clt_address, amount_usdt, amount_clt, \
    status, client_key, invoice_id, tron_tx_id, response_status, response_body, payment_window_closed, \
    derivation_index, deposit_address, expires_at, \
    treasury_intent_id, attempts, next_attempt_at";

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
    Db(sqlx::Error),
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Db(e)
    }
}

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

/// Idempotency layer 1: look up `(user_pk, client_key)`, and either replay a stored response,
/// reject a body mismatch, resume a crashed attempt (on its EXISTING derived address), or insert a
/// brand new intent — one function, spec §6's semantics exactly.
pub async fn create(
    pool: &PgPool,
    config: &OrchConfig,
    deriver: &AddressDeriver,
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
                None => match insert_new(pool, config, deriver, user_pk, clt_address, amount_usdt, client_key).await? {
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
            // `store_invoice`. Resume on the SAME derived address.
            //
            // Reusing the amount is what keeps this safe, not what makes it risky. This
            // row is the only holder of that slot under uq_active_pay_amount, and the
            // schema's invariant is that a slot stays reserved until we ourselves can
            // no longer match a payment to it. Moving the row to a fresh slot would
            // release the old amount while a possibly-live orphan invoice still carries
            // it, letting a LATER, DIFFERENT intent be allocated that amount — the
            // cross-user misattribution on the shared static custody address that the
            // discriminator exists to prevent.
            //
            // The address dimension is simpler and safer: the row already holds its
            // derivation_index and deposit_address, so a resume reuses the SAME address by
            // construction — there is nothing to re-allocate and no way to hand this user a
            // second address while they are paying the first.
            //
            // Two live invoices at one amount for THIS SAME intent is not that hazard:
            // both carry this intent's order_id, this user, this credit, and whichever
            // one we match against, `store_invoice`'s compare-and-set plus the per-invoice
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
    deriver: &AddressDeriver,
    user_pk: &str,
    clt_address: &str,
    amount_usdt: i64,
    client_key: &str,
) -> Result<Option<DepositIntent>, ApiError> {
    let mut tx = pool.begin().await?;
    let id = Uuid::new_v4();
    let expires_at = chrono::Utc::now() + chrono::Duration::minutes(config.deposit_ttl_minutes);

    // Allocate and derive BEFORE the insert, and once — not per discriminator attempt. The address
    // is written in the same statement as the row, so a row can never exist without the address
    // users were told to pay. A retry below re-uses this same index: the index is not what
    // collides, and burning a fresh one per attempt would waste the sequence for no gain.
    //
    // A subsequently-failed insert burns this index. Gaps are harmless; see migration 0007.
    let derivation_index = allocate_derivation_index(pool).await?;
    let index_u32 = u32::try_from(derivation_index)
        .map_err(|_| ApiError::Db(sqlx::Error::Protocol(format!(
            "derivation index {derivation_index} does not fit u32; sequence MAXVALUE should have prevented this"
        ))))?;
    let deposit_address = deriver.address_at(index_u32).map_err(|e| {
        ApiError::Db(sqlx::Error::Protocol(format!("deriving address at index {derivation_index}: {e}")))
    })?;

    // One insert, no retry loop. The amount can no longer collide with anything: identity is the
    // derived address, and that is unique by construction (migration 0007's sequence) rather than
    // by searching a 999-wide space for a free slot.
    let mut attempt = tx.begin().await?;
    let row = sqlx::query_as::<_, DepositIntent>(&format!(
        "INSERT INTO deposit_intents
            (id, user_pk, clt_address, amount_usdt, amount_clt, client_key, expires_at,
             derivation_index, deposit_address)
         VALUES ($1, $2, $3, $4, $4, $5, $6, $7, $8)
         RETURNING {INTENT_COLS}"
    ))
    .bind(id)
    .bind(user_pk)
    .bind(clt_address)
    .bind(amount_usdt)
    .bind(client_key)
    .bind(expires_at)
    .bind(derivation_index)
    .bind(&deposit_address)
    .fetch_one(&mut *attempt)
    .await;

    match row {
        Ok(intent) => {
            attempt.commit().await?;
            tx.commit().await?;
            Ok(Some(intent))
        }
        Err(e) => match unique_violation_constraint(&e) {
            // Lost a genuine create-vs-create race for this exact idempotency key. Stop and let the
            // caller re-run create(), which will see the now-committed row and resolve
            // Replay/Conflict/StillProcessing correctly (brief: "Insert race resolved by the unique
            // index (ON CONFLICT catch -> re-fetch -> compare)").
            Some(c) if c == IDEMPOTENCY_KEY_CONSTRAINT => {
                attempt.rollback().await?;
                tx.rollback().await?;
                Ok(None)
            }
            // Any OTHER unique violation is now a genuine bug, not a slot collision to retry
            // around. uq_deposit_derivation_index or uq_deposit_address firing here means an index
            // was reissued or an address reused — the one thing that must never happen silently, so
            // it surfaces as an error rather than being swallowed by a retry loop.
            Some(_) | None => {
                attempt.rollback().await?;
                tx.rollback().await?;
                Err(e.into())
            }
        },
    }
}

/// Idempotency layer 4: nothing upstream dedupes for us, so this compare-and-set
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

/// Historical lookup key: the webhook and per-invoice refetch both keyed on
/// `invoice_id`, never on our own `id`. An invoice_id no row holds returns `None` — the
/// caller's job (spam resistance: store nothing, call nothing for an unknown id).
pub async fn find_by_invoice_id(pool: &PgPool, invoice_id: &str) -> Result<Option<DepositIntent>, sqlx::Error> {
    sqlx::query_as::<_, DepositIntent>(&format!("SELECT {INTENT_COLS} FROM deposit_intents WHERE invoice_id = $1"))
        .bind(invoice_id)
        .fetch_optional(pool)
        .await
}

/// Sets `payment_window_closed = TRUE` — the ONLY thing that frees the discriminator slot
/// (`uq_active_pay_amount`'s `WHERE ... AND NOT payment_window_closed` clause). Deliberately
/// unconditional (no status guard): a closed payment window is a fact about the clock, not
/// about our own state machine, and setting it twice is harmless.
pub async fn mark_payment_window_closed(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE deposit_intents SET payment_window_closed = TRUE, updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Records the on-chain tx id once the custody poller matches a transfer to this intent.
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
         WHERE status = 'mint_requested' AND treasury_intent_id IS NOT NULL AND next_attempt_at <= now()
         ORDER BY created_at"
    ))
    .fetch_all(pool)
    .await
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
    /// The daily-headroom check (T2b's deferral, now landed in 5b): the treasury could not be
    /// reached to ask. Fail closed — 503 + Retry-After, never proceed on an unanswered question
    /// about whether we could actually mint against this deposit.
    TreasuryUnavailable,
    /// The daily-headroom check answered, and today's remaining mint headroom is below this
    /// amount — a clear 4xx, not a 503: asking again immediately won't help, tomorrow's headroom
    /// (or a smaller amount) will.
    InsufficientHeadroom { headroom_clt: i64 },
    /// A DB write failed — caller 5xx's without leaking internals.
    Failed(String),
}

/// T2b's deferred daily-headroom check, landed here now that treasury-service's
/// `/internal/reserve-status` exposes `daily_headroom_clt` (T5a). `GET`s it with the readonly
/// token and refuses the deposit when headroom is short — **fail closed** either way a question
/// goes unanswered: an unreachable treasury and an insufficient headroom both refuse rather than
/// let the deposit proceed. Taking a user's money when we cannot mint against it is strictly
/// worse than turning the deposit away up front (they can retry later; funds already stranded in
/// custody need a human) — the availability coupling this creates (a treasury outage now also
/// blocks new deposit creation, not just deposit progress) is the accepted tradeoff.
async fn check_headroom(config: &OrchConfig, amount_usdt: i64) -> Result<(), DepositOutcome> {
    let resp = reqwest::Client::new()
        .get(format!("{}/internal/reserve-status", config.treasury_url))
        .bearer_auth(&config.treasury_readonly_token)
        .send()
        .await
        .map_err(|_| DepositOutcome::TreasuryUnavailable)?;

    if !resp.status().is_success() {
        return Err(DepositOutcome::TreasuryUnavailable);
    }
    let body: serde_json::Value = resp.json().await.map_err(|_| DepositOutcome::TreasuryUnavailable)?;
    let headroom_clt = body
        .get("daily_headroom_clt")
        .and_then(|v| v.as_i64())
        .ok_or(DepositOutcome::TreasuryUnavailable)?;

    // At par (module docs), amount_usdt is the amount_clt this deposit will eventually ask the
    // treasury to mint — comparable directly against headroom_clt with no rate conversion.
    if headroom_clt < amount_usdt {
        return Err(DepositOutcome::InsufficientHeadroom { headroom_clt });
    }
    Ok(())
}

/// The create-flow orchestration (T2b): routes `deposits::create`'s outcome (idempotency
/// layer 1 + the discriminator, Task 2) through `adapter.create_invoice` and the
/// compare-and-set store (idempotency layer 4) — the two mechanisms meeting the wire.
///
/// Ordering that keeps money safe: the intent row is created BEFORE any pay instructions are
/// handed out, and
/// `store_invoice`'s compare-and-set runs AFTER — so a crash between the two leaves an
/// orphan invoice (accepted; see `deposits::create`'s resume branch), never a lost intent
/// row with no corresponding invoice attempt recorded.
pub async fn create_and_invoice(
    pool: &PgPool,
    config: &OrchConfig,
    deriver: &AddressDeriver,
    user_pk: &str,
    clt_address: &str,
    amount_usdt: i64,
    client_key: &str,
) -> DepositOutcome {
    let outcome = match create(pool, config, deriver, user_pk, clt_address, amount_usdt, client_key).await {
        Ok(o) => o,
        Err(ApiError::OutOfBounds { min, max }) => return DepositOutcome::OutOfBounds { min, max },
        Err(ApiError::Db(e)) => return DepositOutcome::Failed(e.to_string()),
    };

    let intent = match outcome {
        CreateOutcome::Replay { status, body } => return DepositOutcome::Respond { status: status as u16, body },
        CreateOutcome::Conflict => return DepositOutcome::Conflict,
        CreateOutcome::StillProcessing => return DepositOutcome::StillProcessing,
        CreateOutcome::Created(intent) => intent,
    };

    // Headroom is checked here, AFTER create()'s idempotency resolution and BEFORE ever calling
    // handing out pay instructions: a replay/resume of an already-`Created` row must not re-refuse on today's
    // headroom (the row already exists; this may be a resume of a genuinely earlier attempt),
    // and no invoice/custody exposure has been created yet for a brand-new row, so refusing here
    // costs nothing but the row itself (soft-expires like any other unpaid intent).
    if let Err(outcome) = check_headroom(config, intent.amount_usdt).await {
        return outcome;
    }

    // No external gateway to call. Every value the old Bitcart adapter contributed here is a
    // local fact: the pay address is the one static custody address from config, the window is
    // our own TTL (already applied by `create`), and the reference is the intent's own id.
    //
    // That removes a whole failure mode. This step used to be able to fail with the intent row
    // already committed, leaving a `created` row with no invoice for the poller to catch up on;
    // now the only thing between the row and the response is a DB write.
    //
    // `store_invoice`'s CAS below is still what makes this exactly-once — that was never Bitcart's
    // idempotency doing the work, and it is unchanged.
    let invoice_ref = intent.id.to_string();

    // From the ROW, never re-derived here. A crash-resumed row already carries the address its
    // user was told to pay; deriving again on this path would be the per-address analogue of
    // a32a101 — re-issuing an identity a payer may already be sending to.
    //
    // `None` means a discriminator-era row that predates per-intent addresses. Refuse rather than
    // mint it a fresh address: the user was told a shared custody address, and quietly moving the
    // goalposts could credit a payment that never arrives at the new one.
    let Some(pay_address) = intent.deposit_address.clone() else {
        return DepositOutcome::Failed(format!(
            "intent {} predates per-intent deposit addresses and cannot be resumed",
            intent.id
        ));
    };

    let body = json!({
        "id": intent.id,
        "pay_address": pay_address,
        // Key kept for wire compatibility with the demo app; it still means "the amount to
        // pay". The value is now the plain amount — there is no discriminator to add, and the
        // address identifies the payer, so this is a minimum rather than an exact figure.
        "pay_amount_usdt": intent.amount_usdt,
        "expires_at": intent.expires_at,
        "status": "invoiced",
    });

    match store_invoice(pool, intent.id, &invoice_ref, 201, &body).await {
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
