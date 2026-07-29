use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct Balances {
    pub clt_liability: i64,
    pub custody_usdt: i64,
}

pub async fn append_event(
    pool: &PgPool,
    kind: &str,
    amount_clt: i64,
    amount_usdt: i64,
    intent_id: Option<Uuid>,
    chain_tx_hash: Option<&str>,
    description: &str,
) -> Result<i64, sqlx::Error> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO treasury_events (kind, amount_clt, amount_usdt, intent_id, chain_tx_hash, description)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(kind)
    .bind(amount_clt)
    .bind(amount_usdt)
    .bind(intent_id)
    .bind(chain_tx_hash)
    .bind(description)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn balances(pool: &PgPool) -> Result<Balances, sqlx::Error> {
    sqlx::query_as::<_, Balances>("SELECT clt_liability, custody_usdt FROM ledger_balances")
        .fetch_one(pool)
        .await
}

pub async fn alert(pool: &PgPool, severity: &str, source: &str, message: &str) {
    tracing::error!(source, severity, "{}", message);
    let _ = sqlx::query("INSERT INTO alerts (severity, source, message) VALUES ($1, $2, $3)")
        .bind(severity)
        .bind(source)
        .bind(message)
        .execute(pool)
        .await;
}
