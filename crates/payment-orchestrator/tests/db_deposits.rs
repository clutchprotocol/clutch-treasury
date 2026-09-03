//! `deposits::transition`'s guarded-transition properties. The row this exercises against no
//! longer comes from the create flow — Task 6 deleted the amount-bearing, idempotency-keyed
//! create flow this file used to test end to end — so the fixture below inserts a row directly.
//! `transition` itself is untouched and still live: `poller.rs` and `treasury_bridge.rs` both
//! drive rows through it, and the legacy per-intent loop keeps calling it after Task 7.
//!
//! Same shared-database convention as the other `db_*.rs` files: a sibling `_orchestrator`
//! database (sqlx's `_sqlx_migrations` bookkeeping table has no configurable name, so two
//! crates' migrators would corrupt each other's history on ONE shared DB).

use payment_orchestrator::deposits;
use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};
use uuid::Uuid;

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_orch_deposits");

    if !Postgres::database_exists(&url).await.unwrap_or(false) {
        Postgres::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE deposit_intents RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

/// Seeds a `deposit_intents` row directly — the create flow that used to seed one here is gone
/// (Task 6). Status defaults to `created` (the column's own DEFAULT), which is all either test
/// below needs as a starting point.
async fn seed_intent(pool: &PgPool, user_pk: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deposit_intents (id, user_pk, clt_address, amount_usdt, amount_clt, client_key, expires_at)
         VALUES ($1, $2, 'clt1address', 2000000, 2000000, $3, now() + interval '30 minutes')",
    )
    .bind(id)
    .bind(user_pk)
    .bind(format!("key-{id}"))
    .execute(pool)
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn transition_allows_expired_to_confirmed_late_honour() {
    let pool = pool().await;
    let id = seed_intent(&pool, "0xalice0000000000000000000000000000000001").await;

    assert!(deposits::transition(&pool, id, &["created", "invoiced", "paying"], "expired").await.unwrap());

    // Late-but-genuine payment: expired -> confirmed is legal (no FX risk at par).
    let applied = deposits::transition(&pool, id, &["paying", "invoiced", "expired"], "confirmed")
        .await
        .unwrap();
    assert!(applied, "expired -> confirmed must be a legal late-honour transition");

    let row = deposits::find_by_id(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "confirmed");
}

#[tokio::test]
async fn transition_refuses_confirmed_to_failed() {
    let pool = pool().await;
    let id = seed_intent(&pool, "0xalice0000000000000000000000000000000001").await;

    assert!(deposits::transition(&pool, id, &["created", "invoiced", "paying"], "confirmed").await.unwrap());

    // Out-of-order webhook: a 'failed' arriving after 'confirmed' must be absorbed, not applied.
    let applied = deposits::transition(&pool, id, &["created", "invoiced", "paying"], "failed")
        .await
        .unwrap();
    assert!(!applied, "confirmed -> failed must be refused (out-of-order webhook, spec §6)");

    let row = deposits::find_by_id(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "confirmed", "status must remain confirmed, not regress to failed");
}
