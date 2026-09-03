use payment_orchestrator::addresses;
use payment_orchestrator::derive::AddressDeriver;
use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};

/// The published account xpub for the canonical test mnemonic — the same wallet derive.rs's own
/// tests pin, so these addresses are checkable against that table.
/// The fixture account xpub, copied verbatim from `src/derive.rs`'s own test module — the same
/// wallet whose addresses that file already pins, so these results are checkable against it.
const XPUB: &str = "xpub6D1AabNHCupeiLM65ZR9UStMhJ1vCpyV4XbZdyhMZBiJXALQtmn9p42VTQckoHVn8WNqS7dqnJokZHAHcHGoaQgmv8D45oNUKx6DZMNZBCd";

/// docker-compose.test.yml points every crate's DATABASE_URL at ONE shared database
/// (`treasury_test`) for simplicity — real dev/prod already gives each service its own
/// database (.env.example: treasury on 5433/treasury, orchestrator on 5434/orchestrator).
/// sqlx's migrator hardcodes a single unqualified `_sqlx_migrations` table with no
/// configurable name (sqlx-postgres 0.8.6 migrate.rs) — two crates' independent
/// `sqlx::migrate!` calls against the SAME database therefore corrupt each other's
/// migration history (VersionMismatch/VersionMissing) regardless of using separate
/// migrations directories. Deriving a sibling database name here restores the real
/// per-service isolation without touching the shared compose file or treasury-service.
async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    // Swap the last path segment (the database name) for a sibling name — the URL shape
    // is fixed (postgres://user:pass@host:port/dbname), so a plain string split is enough
    // and avoids pulling in a URL-parsing crate for one rename.
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_orch_addresses");

    if !Postgres::database_exists(&url).await.unwrap_or(false) {
        Postgres::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    // Each test file starts clean — order-independent tests. This crate's own database
    // now holds only this crate's own tables. deposit_addresses is included here (the
    // db_deposits.rs helper this was copied from predates that table and does not).
    sqlx::query("TRUNCATE deposit_intents, deposit_addresses RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

#[tokio::test]
async fn a_users_address_is_stable_across_calls() {
    // The whole point of the change: a user has ONE address, forever. If a second call derived a
    // second address, every deposit sent to the first would arrive somewhere nothing watches.
    let pool = pool().await;
    let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();

    let first = addresses::address_for_user(&pool, &deriver, "0xuser-a", "0xclt-a").await.unwrap();
    let second = addresses::address_for_user(&pool, &deriver, "0xuser-a", "0xclt-a").await.unwrap();

    assert_eq!(first, second);
}

#[tokio::test]
async fn two_users_get_different_addresses() {
    let pool = pool().await;
    let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();

    let a = addresses::address_for_user(&pool, &deriver, "0xuser-a", "0xclt-a").await.unwrap();
    let b = addresses::address_for_user(&pool, &deriver, "0xuser-b", "0xclt-b").await.unwrap();

    assert_ne!(a, b, "sharing an address between users would credit one user's deposit to another");
}

#[tokio::test]
async fn indexes_come_from_the_shared_sequence_and_never_repeat() {
    // Legacy per-intent addresses already hold issued indexes. Reusing one would hand a new user an
    // address a previous depositor was told to pay into.
    let pool = pool().await;
    let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();

    // Burn an index the way a legacy deposit would have.
    let burned: i64 = sqlx::query_scalar("SELECT nextval('deposit_derivation_index_seq')")
        .fetch_one(&pool).await.unwrap();

    addresses::address_for_user(&pool, &deriver, "0xuser-a", "0xclt-a").await.unwrap();
    let got: i64 = sqlx::query_scalar("SELECT derivation_index FROM deposit_addresses WHERE user_pk = '0xuser-a'")
        .fetch_one(&pool).await.unwrap();

    assert!(got > burned, "index {got} must be past the already-issued {burned}");
}

#[tokio::test]
async fn marking_hot_sets_a_future_window() {
    let pool = pool().await;
    let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();
    addresses::address_for_user(&pool, &deriver, "0xuser-a", "0xclt-a").await.unwrap();

    addresses::mark_hot(&pool, "0xuser-a", 24).await.unwrap();

    let hot: bool = sqlx::query_scalar("SELECT hot_until > now() FROM deposit_addresses WHERE user_pk = '0xuser-a'")
        .fetch_one(&pool).await.unwrap();
    assert!(hot);
}
