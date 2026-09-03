//! The derivation-index allocator, tested for the one property that matters: an index is NEVER
//! handed out twice.
//!
//! The signer derives spending keys from the index, so a duplicate index is two users with a claim
//! on one pot of money. A deposit address, by contrast, is now assigned to one user and may receive
//! multiple deposits over its lifetime — this is the normal case under the permanent-address model.
//! These tests exercise index uniqueness against real Postgres concurrency and real rollbacks
//! rather than reasoning about it.
//!
//! Same shared-database convention as the other `db_*.rs` files: a sibling database, because sqlx's
//! `_sqlx_migrations` table has no configurable name and two crates' migrators would corrupt each
//! other's history on one database.

use payment_orchestrator::deposits;
use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_orch_derivation");
    if !Postgres::database_exists(&url).await.unwrap_or(false) {
        Postgres::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE deposit_intents RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    // RESTART IDENTITY only resets sequences OWNED by the table, and this one is standalone — so it
    // survives the truncate. Without this reset the exhaustion test below (`setval` to the maximum)
    // poisons every other test in the file, which is exactly what happened the first time.
    sqlx::query("ALTER SEQUENCE deposit_derivation_index_seq RESTART")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

#[tokio::test]
async fn allocations_are_unique_and_strictly_increasing() {
    let pool = pool().await;
    let mut seen = Vec::new();
    for _ in 0..50 {
        seen.push(deposits::allocate_derivation_index(&pool).await.unwrap());
    }
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), seen.len(), "an index was handed out twice: {seen:?}");
    assert!(seen.windows(2).all(|w| w[1] > w[0]), "sequential calls must strictly increase: {seen:?}");
}

/// THE property. 64 concurrent allocators against one pool must produce 64 distinct indices.
/// `max(derivation_index) + 1` would fail this — two readers see the same max and both take it.
#[tokio::test]
async fn concurrent_allocations_never_collide() {
    let pool = pool().await;
    let mut tasks = Vec::new();
    for _ in 0..64 {
        let p = pool.clone();
        tasks.push(tokio::spawn(async move { deposits::allocate_derivation_index(&p).await.unwrap() }));
    }
    let mut got = Vec::new();
    for t in tasks {
        got.push(t.await.unwrap());
    }
    let mut uniq = got.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), 64, "concurrent allocation produced duplicates: {got:?}");
}

/// A rolled-back transaction must NOT return its index to the pool.
///
/// This is what makes sequences the right primitive and a table-max the wrong one: the burnt index
/// leaves a gap, which is harmless, whereas reissuing it would point a second intent at the first
/// one's address.
#[tokio::test]
async fn a_rolled_back_transaction_burns_its_index_rather_than_reusing_it() {
    let pool = pool().await;
    let before = deposits::allocate_derivation_index(&pool).await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let inside = sqlx::query_scalar::<_, i64>("SELECT nextval('deposit_derivation_index_seq')")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    let after = deposits::allocate_derivation_index(&pool).await.unwrap();
    assert!(inside > before, "index inside the transaction must advance past {before}, got {inside}");
    assert!(
        after > inside,
        "the rolled-back index {inside} must NOT be reissued — next was {after}"
    );
}

/// Legacy discriminator-era rows carry NULL for both columns, and Postgres allows many NULLs in a
/// unique index. Verified rather than assumed, because if it were not true the migration would fail
/// on any host with more than one historical intent.
#[tokio::test]
async fn multiple_legacy_rows_with_null_index_and_address_coexist() {
    let pool = pool().await;
    for key in ["legacy-a", "legacy-b", "legacy-c"] {
        sqlx::query(
            "INSERT INTO deposit_intents
                (id, user_pk, clt_address, amount_usdt, amount_clt, client_key, expires_at)
             VALUES ($1, 'pk', 'clt', 1000000, 1000000, $2, now() + interval '30 min')",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(key)
        .execute(&pool)
        .await
        .unwrap_or_else(|e| panic!("legacy row {key} must insert with NULL index/address: {e}"));
    }
    let n: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM deposit_intents WHERE derivation_index IS NULL AND deposit_address IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n.0, 3, "all three NULL-index rows must coexist");
}

/// The sequence is capped at the top of the BIP32 non-hardened range. Allocation must fail there,
/// rather than yielding an index `derive.rs` will refuse only after a row exists.
#[tokio::test]
async fn exhausting_the_sequence_fails_at_allocation_not_at_derivation() {
    let pool = pool().await;
    sqlx::query("SELECT setval('deposit_derivation_index_seq', 2147483647, true)")
        .execute(&pool)
        .await
        .unwrap();
    let err = deposits::allocate_derivation_index(&pool)
        .await
        .expect_err("past 2^31-1 there is no derivable index left");
    let msg = err.to_string();
    assert!(
        msg.contains("2147483647") || msg.to_lowercase().contains("reached maximum"),
        "the error should name sequence exhaustion, got: {msg}"
    );
}
