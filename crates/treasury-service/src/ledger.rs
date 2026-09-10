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

/// `alert` for a condition that is CHECKED far more often than it CHANGES.
///
/// Silent if the same source has already recorded this exact message within `within`.
///
/// Some of these checks run on every worker pass. The verifier's stuck-intent sweep runs about
/// every three seconds and has no memory of having already said this, so one deposit that took a
/// few minutes to resolve on stage wrote four identical rows and four identical log lines; one
/// that takes the full day before it escalates to p1 would write tens of thousands. A genuine
/// fault raised in the middle of that is invisible, which is the actual cost — the rows are
/// merely permanent. Nothing prunes this table, it has no resolved flag, and `metrics` gauges
/// severity by counting it.
///
/// A window rather than "only ever once": a condition still true an hour later is worth repeating,
/// and for a stuck intent this repetition is the only heartbeat there is. Deduplicating on the
/// message means an alert that names the intent still speaks once per intent.
///
/// Not race-free, and does not need to be. The workers that call this are single loops, so two
/// passes cannot interleave here; a duplicate would cost one redundant row, which is the thing
/// this is reducing rather than a correctness property anything depends on.
pub async fn alert_once(pool: &PgPool, severity: &str, source: &str, message: &str, within: chrono::Duration) {
    let cutoff = chrono::Utc::now() - within;
    let already: Option<(i32,)> =
        sqlx::query_as("SELECT 1 FROM alerts WHERE source = $1 AND message = $2 AND created_at > $3 LIMIT 1")
            .bind(source)
            .bind(message)
            .bind(cutoff)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);
    // A failed read falls through to alerting. Losing an alert to a database blip is the worse
    // half of this trade: the point of the guard is volume, not suppression.
    if already.is_some() {
        return;
    }
    alert(pool, severity, source, message).await;
}
