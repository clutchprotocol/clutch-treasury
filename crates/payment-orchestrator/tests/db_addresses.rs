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

#[tokio::test]
async fn two_simultaneous_first_calls_settle_on_one_address() {
    // The race the ON CONFLICT + re-read design exists for. Both callers pass the initial
    // existence check as None, both derive, both INSERT; exactly one wins and BOTH must return the
    // winner's address. A SELECT-then-INSERT implementation returns two different addresses here,
    // and money sent to the loser is watched by nothing.
    //
    // Why this is deterministic enough to be worth having: #[tokio::test] (no `flavor` arg,
    // confirmed nowhere in this crate) runs on the current-thread runtime, and tokio::join! polls
    // both futures on that one thread, interleaving at .await points. Both `address_for_user`
    // calls reach their first .await — the `existing()` read — before either is polled again, so
    // both genuinely observe None rather than one running to completion before the other starts.
    //
    // This does not prove the absence of every possible race — a real multi-threaded or
    // multi-process race is not reproduced here — but it WOULD fail against a SELECT-then-INSERT
    // implementation, which is the specific bug ON CONFLICT + re-read exists to prevent. That is
    // the bar this test clears.
    let pool = pool().await;
    let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();

    let (a, b) = tokio::join!(
        addresses::address_for_user(&pool, &deriver, "0xracer", "0xclt-r"),
        addresses::address_for_user(&pool, &deriver, "0xracer", "0xclt-r"),
    );
    let (a, b) = (a.unwrap(), b.unwrap());

    assert_eq!(a, b, "both callers must see the same persisted address");

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM deposit_addresses WHERE user_pk = '0xracer'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(rows, 1, "exactly one row, whichever caller won");

    let stored: String = sqlx::query_scalar("SELECT address FROM deposit_addresses WHERE user_pk = '0xracer'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(a, stored, "the returned address is the stored one, not a losing derivation");
}
