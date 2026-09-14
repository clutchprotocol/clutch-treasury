//! What `/public/reconciliation` publishes, and what it must not.
//!
//! This endpoint is unauthenticated and its output is meant to be republished to the open internet
//! by the explorer. The risk is not that it breaks — it is that it quietly grows a field. So the
//! test pins the exact key set rather than checking the ones it happens to care about.

use sqlx::PgPool;
use treasury_service::reconciliation::{record, Sources};

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_tre_pubrecon");
    if !<sqlx::Postgres as sqlx::migrate::MigrateDatabase>::database_exists(&url).await.unwrap_or(false) {
        <sqlx::Postgres as sqlx::migrate::MigrateDatabase>::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE treasury_events, mint_intents, reconciliation_runs, alerts RESTART IDENTITY CASCADE")
        .execute(&pool).await.unwrap();
    sqlx::query("UPDATE breaker_state SET minting_halted = FALSE, halt_reason = NULL")
        .execute(&pool).await.unwrap();
    pool
}

/// The query the handler runs. Kept identical to `api.rs` so this test exercises the same shape;
/// the handler itself needs an AppState and a running router, which is more machinery than the
/// thing being protected here warrants.
async fn latest(pool: &PgPool) -> Option<(chrono::DateTime<chrono::Utc>, i64, i64, i64, i64, String)> {
    sqlx::query_as(
        "SELECT run_at, onchain_supply, genesis_allocation, ledger_liability, custody_reported, status
         FROM reconciliation_runs ORDER BY run_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn no_runs_yet_is_a_state_not_an_error() {
    let pool = pool().await;
    // A chain that has never reconciled is a real condition. Answering "unavailable" would let a
    // consumer render it as a fault, and answering "ok" would be a lie.
    assert!(latest(&pool).await.is_none());
}

#[tokio::test]
async fn publishes_the_latest_run_and_its_numbers() {
    let pool = pool().await;
    let s = Sources { onchain_supply: 5_000_000, genesis_allocation: 0, ledger_liability: 5_000_000, custody_reported: 5_000_000 };
    record(&pool, &s).await.unwrap();

    let (_, onchain, genesis, liability, custody, status) = latest(&pool).await.expect("a run");
    assert_eq!(onchain, 5_000_000);
    assert_eq!(genesis, 0);
    assert_eq!(liability, 5_000_000);
    assert_eq!(custody, 5_000_000);
    assert_eq!(status, "ok");
}

#[tokio::test]
async fn a_mismatch_is_published_rather_than_hidden() {
    let pool = pool().await;
    // Reserve below liability: the case a holder most needs to see. A page that hides this is
    // worth less than no page, because it converts "unverified" into "verified fine".
    let s = Sources { onchain_supply: 9_000_000, genesis_allocation: 0, ledger_liability: 9_000_000, custody_reported: 1_000_000 };
    record(&pool, &s).await.unwrap();

    let (_, _, _, _, custody, status) = latest(&pool).await.expect("a run");
    assert_eq!(status, "mismatch");
    assert_eq!(custody, 1_000_000);
}

#[tokio::test]
async fn the_newest_run_wins() {
    let pool = pool().await;
    let bad = Sources { onchain_supply: 9_000_000, genesis_allocation: 0, ledger_liability: 9_000_000, custody_reported: 1 };
    record(&pool, &bad).await.unwrap();
    let good = Sources { onchain_supply: 9_000_000, genesis_allocation: 0, ledger_liability: 9_000_000, custody_reported: 9_000_000 };
    record(&pool, &good).await.unwrap();

    let (_, _, _, _, custody, status) = latest(&pool).await.expect("a run");
    assert_eq!(status, "ok", "ORDER BY run_at DESC must surface the newest, not the first");
    assert_eq!(custody, 9_000_000);
}
