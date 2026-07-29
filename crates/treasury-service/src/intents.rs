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
}

const MINT_COLS: &str =
    "id, beneficiary, amount_clt, status, credit_ref, created_by, approved_by, chain_tx_hash";

pub async fn create_mint_intent(
    pool: &PgPool,
    beneficiary: &str,
    amount_clt: i64,
    created_by: &str,
) -> Result<MintIntent, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query_as::<_, MintIntent>(&format!(
        "INSERT INTO mint_intents (id, beneficiary, amount_clt, credit_ref, created_by)
         VALUES ($1, $2, $3, $4, $5) RETURNING {MINT_COLS}"
    ))
    .bind(id)
    .bind(beneficiary)
    .bind(amount_clt)
    .bind(intent_ref(&id.to_string()))
    .bind(created_by)
    .fetch_one(pool)
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

    if intent.status != "created" {
        return Err(format!("intent is '{}', only 'created' can be approved", intent.status));
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

    sqlx::query("INSERT INTO chain_outbox (intent_id) VALUES ($1)")
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
         RETURNING id, redeemer_address, payout_address, amount_clt, status, redemption_ref, burn_tx_hash",
    )
    .bind(id)
    .bind(redeemer_address)
    .bind(payout_address)
    .bind(amount_clt)
    .bind(intent_ref(&id.to_string()))
    .fetch_one(pool)
    .await
}
