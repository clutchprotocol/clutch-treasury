//! TronLink direct top-ups (`direct.rs`): transfers to the pay-in address are credited by sender,
//! and only to a user who asked first. The chain is scripted; Postgres is real.
//!
//! What these protect is the same as in db_poller.rs, attribution: crediting a transfer to anyone but
//! the holder of its sending key, or crediting one a human may already have refunded.

use std::sync::Mutex;

use async_trait::async_trait;
use payment_orchestrator::custody::{CustodyWatcher, ObservedTransfer};
use payment_orchestrator::direct;
use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};

const USDT: &str = "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf";
/// The pay-in address (the float). Any valid-looking TRON address will do.
const PAY_TO: &str = "TQwgeRaDt4FSJSsncmFNcbMNTfFpjvjwFX";
/// A user whose TRON twin is `USER_TRON` (the all-"abandon" index-0 address, see direct.rs's tests).
const USER: &str = "0xc8599111f29c1e1e061265b4af93ea1f274ad78a";
const USER_TRON: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";
/// Somebody else entirely: an exchange's hot wallet, say.
const STRANGER: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_orch_direct");
    if !Postgres::database_exists(&url).await.unwrap_or(false) {
        Postgres::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query(
        "TRUNCATE deposit_intents, alerts, deposit_addresses, direct_payers, direct_watch, direct_unattributed
         RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

struct FakeChain {
    transfers: Vec<ObservedTransfer>,
    fail: bool,
    asked: Mutex<u32>,
}

impl FakeChain {
    fn with(transfers: Vec<ObservedTransfer>) -> Self {
        Self { transfers, fail: false, asked: Mutex::new(0) }
    }
}

#[async_trait]
impl CustodyWatcher for FakeChain {
    async fn transfers_to(&self, address: &str, _min: Option<i64>) -> Result<Vec<ObservedTransfer>, String> {
        *self.asked.lock().unwrap() += 1;
        if self.fail {
            return Err("scripted TronGrid outage".into());
        }
        assert_eq!(address, PAY_TO, "only the pay-in address is read");
        Ok(self.transfers.clone())
    }
}

/// `ts` in epoch milliseconds; `now_ms()` + something lands after any registration made in the test.
fn transfer(tx: &str, from: &str, amount: i64, ts: i64) -> ObservedTransfer {
    ObservedTransfer {
        tx_id: tx.into(),
        amount_usdt: amount,
        from: from.into(),
        to: PAY_TO.into(),
        contract: USDT.into(),
        block_timestamp: ts,
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn credited(pool: &PgPool) -> Vec<(String, String, i64, Option<String>, Option<String>, Option<i64>)> {
    sqlx::query_as(
        "SELECT user_pk, tron_tx_id, received_usdt, direct_from, deposit_address, derivation_index
         FROM deposit_intents WHERE status = 'confirmed' ORDER BY tron_tx_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn alerts(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM alerts WHERE source = 'direct_topup'").fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn registering_gives_the_tron_address_of_the_same_key() {
    let pool = pool().await;
    assert_eq!(direct::register_payer(&pool, USER, USER).await.unwrap(), USER_TRON);
    // A second call is the same answer, and keeps the first row.
    assert_eq!(direct::register_payer(&pool, USER, USER).await.unwrap(), USER_TRON);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM direct_payers").fetch_one(&pool).await.unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn a_transfer_from_a_registered_payer_is_credited_to_them_once() {
    let pool = pool().await;
    direct::register_payer(&pool, USER, USER).await.unwrap();
    let chain = FakeChain::with(vec![transfer("tx-1", USER_TRON, 30_000_000, now_ms() + 1_000)]);

    direct::credit_direct(&pool, &chain, PAY_TO, USDT).await.unwrap();
    direct::credit_direct(&pool, &chain, PAY_TO, USDT).await.unwrap(); // seen again next pass

    assert_eq!(
        credited(&pool).await,
        vec![(USER.into(), "tx-1".into(), 30_000_000, Some(USER_TRON.into()), None, None)],
        "one deposit, to the payer, with no deposit address or index: nothing is swept"
    );
    assert_eq!(alerts(&pool).await, 0);
}

#[tokio::test]
async fn a_stranger_is_never_credited_and_is_reported_once() {
    let pool = pool().await;
    direct::register_payer(&pool, USER, USER).await.unwrap();
    let chain = FakeChain::with(vec![transfer("tx-ex", STRANGER, 50_000_000, now_ms() + 1_000)]);

    direct::credit_direct(&pool, &chain, PAY_TO, USDT).await.unwrap();
    direct::credit_direct(&pool, &chain, PAY_TO, USDT).await.unwrap();

    assert!(credited(&pool).await.is_empty());
    assert_eq!(alerts(&pool).await, 1, "one alert per transfer, not one per pass");
}

#[tokio::test]
async fn a_payment_before_asking_stays_unattributed_even_after_asking() {
    let pool = pool().await;
    let chain = FakeChain::with(vec![transfer("tx-early", USER_TRON, 20_000_000, now_ms() - 60_000)]);

    // Paid before pressing the button: reported, not credited.
    direct::credit_direct(&pool, &chain, PAY_TO, USDT).await.unwrap();
    // Now they ask. The old transfer may already have been refunded by hand, so it must not mint.
    direct::register_payer(&pool, USER, USER).await.unwrap();
    direct::credit_direct(&pool, &chain, PAY_TO, USDT).await.unwrap();

    assert!(credited(&pool).await.is_empty());
    assert_eq!(alerts(&pool).await, 1);
}

#[tokio::test]
async fn sweeps_from_our_own_deposit_addresses_are_ignored_quietly() {
    let pool = pool().await;
    let deposit_address = "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx";
    sqlx::query(
        "INSERT INTO deposit_addresses (user_pk, derivation_index, address, clt_address) VALUES ('0xother', 7, $1, '0xother')",
    )
    .bind(deposit_address)
    .execute(&pool)
    .await
    .unwrap();
    let chain = FakeChain::with(vec![transfer("tx-sweep", deposit_address, 99_000_000, now_ms() + 1_000)]);

    direct::credit_direct(&pool, &chain, PAY_TO, USDT).await.unwrap();

    assert!(credited(&pool).await.is_empty());
    assert_eq!(alerts(&pool).await, 0);
    let unattributed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM direct_unattributed").fetch_one(&pool).await.unwrap();
    assert_eq!(unattributed, 0);
}

#[tokio::test]
async fn dust_from_a_stranger_is_recorded_without_an_alert() {
    let pool = pool().await;
    let chain = FakeChain::with(vec![transfer("tx-dust", STRANGER, 1, now_ms())]);
    direct::credit_direct(&pool, &chain, PAY_TO, USDT).await.unwrap();
    assert!(credited(&pool).await.is_empty());
    assert_eq!(alerts(&pool).await, 0);
}

#[tokio::test]
async fn another_token_or_destination_is_ignored() {
    let pool = pool().await;
    direct::register_payer(&pool, USER, USER).await.unwrap();
    let mut wrong_token = transfer("tx-tok", USER_TRON, 5_000_000, now_ms() + 1_000);
    wrong_token.contract = "TXLAQ63Xg1NAzckPwKHvzw7CSEmLMEqcdj".into();
    let mut elsewhere = transfer("tx-to", USER_TRON, 5_000_000, now_ms() + 1_000);
    elsewhere.to = STRANGER.into();
    let chain = FakeChain::with(vec![wrong_token, elsewhere]);

    direct::credit_direct(&pool, &chain, PAY_TO, USDT).await.unwrap();
    assert!(credited(&pool).await.is_empty());
}

#[tokio::test]
async fn an_unread_chain_does_not_move_the_watermark() {
    let pool = pool().await;
    let chain = FakeChain { transfers: vec![], fail: true, asked: Mutex::new(0) };
    assert!(direct::credit_direct(&pool, &chain, PAY_TO, USDT).await.is_err());
    let watermark: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_polled_at FROM direct_watch WHERE pay_to = $1")
            .bind(PAY_TO)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(watermark, None);
}
