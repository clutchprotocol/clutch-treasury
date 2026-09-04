use clutch_chain::tx::intent_ref;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow)]
pub struct MintIntent {
    pub id: Uuid,
    pub beneficiary: String,
    pub amount_clt: i64,
    pub status: String,
    pub credit_ref: String,
    pub created_by: String,
    pub approved_by: Option<String>,
    pub chain_tx_hash: Option<String>,
    pub client_ref: Option<String>,
    pub deposit_tx_id: Option<String>,
    pub verified_at: Option<chrono::DateTime<chrono::Utc>>,
    /// What the depositor was told to PAY on-chain: `amount_clt` plus the orchestrator's
    /// per-intent discriminator. On the shared static custody address this is the only thing
    /// telling one user's payment from another's, so it — never `amount_clt` — is what the
    /// verifier matches transfers against. Required for deposit-backed intents by a DB CHECK.
    pub expected_amount_usdt: Option<i64>,
    /// The address this deposit was expected at. Mandatory for deposit-backed intents
    /// (migration 0004's CHECK); `None` only for Plan B's human-created ones.
    pub deposit_address: Option<String>,
    /// BIP32 index the address was derived at — what the sweeper names the key by.
    pub derivation_index: Option<i64>,
}

const MINT_COLS: &str = "id, beneficiary, amount_clt, status, credit_ref, created_by, approved_by, \
    chain_tx_hash, client_ref, deposit_tx_id, verified_at, expected_amount_usdt, deposit_address, derivation_index";

/// `client_ref` is the orchestrator's idempotency key (Plan C T5) — `None` for Plan B's
/// direct/manual mint intents, `Some(deposit_intent_id)` for deposit-backed ones the
/// tron_verifier will pick up. `deposit_tx_id` may be known already (Bitcart returned a
/// hash) or backfilled later by the verifier's fallback match; either way it is NEVER the
/// sole authority for a second create — see `find_by_client_ref` for the replay path, which
/// the caller (api.rs) must check before calling this, exactly as Plan C's brief specifies
/// ("existing client_ref -> 200 with the existing intent, else create").
pub async fn create_mint_intent(
    pool: &PgPool,
    beneficiary: &str,
    amount_clt: i64,
    created_by: &str,
    client_ref: Option<&str>,
    deposit_tx_id: Option<&str>,
    expected_amount_usdt: Option<i64>,
    deposit_address: Option<String>,
    derivation_index: Option<i64>,
) -> Result<MintIntent, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query_as::<_, MintIntent>(&format!(
        "INSERT INTO mint_intents
            (id, beneficiary, amount_clt, credit_ref, created_by, client_ref, deposit_tx_id, expected_amount_usdt,
             deposit_address, derivation_index)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING {MINT_COLS}"
    ))
    .bind(id)
    .bind(beneficiary)
    .bind(amount_clt)
    .bind(intent_ref(&id.to_string()))
    .bind(created_by)
    .bind(client_ref)
    .bind(deposit_tx_id)
    .bind(expected_amount_usdt)
    .bind(deposit_address)
    .bind(derivation_index)
    .fetch_one(pool)
    .await
}

/// The replay lookup api.rs's create-intent handler needs: a duplicate `client_ref` returns
/// the ORIGINAL intent rather than erroring or creating a second row (spec: "duplicate
/// client_ref create replays instead of duplicating"). `client_ref` is UNIQUE in the schema,
/// so this can only ever return zero or one row.
pub async fn find_by_client_ref(pool: &PgPool, client_ref: &str) -> Result<Option<MintIntent>, sqlx::Error> {
    sqlx::query_as::<_, MintIntent>(&format!("SELECT {MINT_COLS} FROM mint_intents WHERE client_ref = $1"))
        .bind(client_ref)
        .fetch_optional(pool)
        .await
}

/// Plan C 5b: the bridge worker's status-poll lookup (`GET /internal/mint-intents/:id`,
/// readonly token) — it has to check what became of the intent it created, and there was no
/// route to read one by id before this.
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<MintIntent>, sqlx::Error> {
    sqlx::query_as::<_, MintIntent>(&format!("SELECT {MINT_COLS} FROM mint_intents WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Approval + outbox row in ONE db transaction with a row lock — the state
/// transition and the work item are atomic; retries can't double-enqueue
/// (unique intent_id on chain_outbox is the backstop).
pub async fn approve_mint_intent(
    pool: &PgPool,
    id: Uuid,
    approved_by: &str,
) -> Result<MintIntent, String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let intent = sqlx::query_as::<_, MintIntent>(&format!(
        "SELECT {MINT_COLS} FROM mint_intents WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| e.to_string())?
    .ok_or("intent not found")?;

    // `needs_manual` is approvable again on purpose: it is where an intent lands when it
    // was over the per-transaction cap, and the way out is a human raising the cap and
    // approving it again. Every other status is either already past approval or terminal.
    if intent.status != "created" && intent.status != "needs_manual" {
        return Err(format!(
            "intent is '{}', only 'created' or 'needs_manual' can be approved",
            intent.status
        ));
    }
    // The DB CHECK also enforces this; checking here gives a readable error.
    if intent.created_by == approved_by {
        return Err("four-eyes: approver must differ from initiator".to_string());
    }

    let updated = sqlx::query_as::<_, MintIntent>(&format!(
        "UPDATE mint_intents SET status = 'approved', approved_by = $2, updated_at = now()
         WHERE id = $1 RETURNING {MINT_COLS}"
    ))
    .bind(id)
    .bind(approved_by)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    // A re-approval from `needs_manual` finds its old outbox row closed as `failed`;
    // reopen it rather than fail the UNIQUE(intent_id) insert. Attempts reset: the earlier
    // ones were spent against a cap that has since been raised, and carrying them would
    // fail the row early.
    sqlx::query(
        "INSERT INTO chain_outbox (intent_id) VALUES ($1)
         ON CONFLICT (intent_id) DO UPDATE
         SET status = 'pending', attempts = 0, next_attempt_at = now(), last_error = NULL",
    )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(updated)
}

#[derive(Debug, sqlx::FromRow)]
pub struct RedemptionIntent {
    pub id: Uuid,
    pub redeemer_address: String,
    pub payout_address: String,
    pub amount_clt: i64,
    pub status: String,
    pub redemption_ref: String,
    pub burn_tx_hash: Option<String>,
    /// The Tron transaction that paid the redeemer, once one has been broadcast. `None`
    /// until then, and still `None` on a redemption that never gets paid.
    pub payout_ref: Option<String>,
}

/// Plan C T6's status read. Without it the orchestrator's `GET /api/v1/redemptions/:id` could only
/// serve the status captured at creation time — so a user polling their own redemption would see
/// `created` forever, including after the payout had already happened. There was no route to read
/// one redemption intent before this.
pub async fn find_redemption_by_id(pool: &PgPool, id: Uuid) -> Result<Option<RedemptionIntent>, sqlx::Error> {
    sqlx::query_as::<_, RedemptionIntent>(
        "SELECT id, redeemer_address, payout_address, amount_clt, status, redemption_ref, burn_tx_hash, payout_ref
         FROM redemption_intents WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn create_redemption_intent(
    pool: &PgPool,
    redeemer_address: &str,
    payout_address: &str,
    amount_clt: i64,
) -> Result<RedemptionIntent, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query_as::<_, RedemptionIntent>(
        "INSERT INTO redemption_intents (id, redeemer_address, payout_address, amount_clt, redemption_ref)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id, redeemer_address, payout_address, amount_clt, status, redemption_ref, burn_tx_hash, payout_ref",
    )
    .bind(id)
    .bind(redeemer_address)
    .bind(payout_address)
    .bind(amount_clt)
    .bind(intent_ref(&id.to_string()))
    .fetch_one(pool)
    .await
}
