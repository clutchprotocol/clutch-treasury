//! BEP-20 top-ups (`bsc_watch.rs`): confirmed Transfers to the BSC pay-in address are credited by
//! sender, and only to a user who asked first. The chain is scripted; Postgres is real.

use std::sync::Mutex;

use async_trait::async_trait;
use payment_orchestrator::bsc_watch::{self, BscChain};
use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};

const USDT: &str = "0x337610d27c682e347c9cd60bd4b3b107c9d34ddd";
const PAY_IN: &str = "0x2222222222222222222222222222222222222222";
const USER: &str = "0xc8599111f29c1e1e061265b4af93ea1f274ad78a";
/// An exchange's hot wallet, say.
const STRANGER: &str = "0x1111111111111111111111111111111111111111";
const E18: u128 = 1_000_000_000_000_000_000;

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_orch_bsc");
    if !Postgres::database_exists(&url).await.unwrap_or(false) {
        Postgres::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE deposit_intents, deposit_addresses, alerts, bsc_payers, bsc_watch, bsc_unattributed RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

fn settings() -> bsc::Settings {
    bsc::Settings {
        chain_id: 97,
        rpc_url: "http://unused".into(),
        usdt_contract: USDT.into(),
        usdt_decimals: 18,
        pay_in_address: PAY_IN.into(),
        confirmations: 15,
    }
}

/// A chain at `head`, whose blocks are all stamped `block_seconds`, holding `transfers`. It answers a
/// range read with the transfers inside it, and records the ranges asked for.
struct FakeChain {
    head: u64,
    block_seconds: u64,
    transfers: Vec<bsc::Transfer>,
    ranges: Mutex<Vec<(u64, u64)>>,
    fail: bool,
}

impl FakeChain {
    fn new(head: u64, block_seconds: u64, transfers: Vec<bsc::Transfer>) -> Self {
        Self { head, block_seconds, transfers, ranges: Mutex::new(Vec::new()), fail: false }
    }
}

#[async_trait]
impl BscChain for FakeChain {
    async fn head(&self) -> Result<u64, String> {
        Ok(self.head)
    }
    async fn transfers_to(&self, contract: &str, to: &str, from_block: u64, to_block: u64) -> Result<Vec<bsc::Transfer>, String> {
        assert_eq!((contract, to), (USDT, PAY_IN));
        self.ranges.lock().unwrap().push((from_block, to_block));
        if self.fail {
            return Err("scripted node outage".into());
        }
        Ok(self.transfers.iter().filter(|t| t.to == to && (from_block..=to_block).contains(&t.block_number)).cloned().collect())
    }
    async fn transfers_to_any(&self, contract: &str, tos: &[String], from_block: u64, to_block: u64) -> Result<Vec<bsc::Transfer>, String> {
        assert_eq!(contract, USDT);
        assert!(!tos.is_empty() && tos.len() <= 100);
        Ok(self.transfers.iter().filter(|t| tos.contains(&t.to) && (from_block..=to_block).contains(&t.block_number)).cloned().collect())
    }
    async fn block_timestamp(&self, _number: u64) -> Result<u64, String> {
        Ok(self.block_seconds)
    }
}

fn transfer(tx: u8, block: u64, from: &str, value: u128) -> bsc::Transfer {
    bsc::Transfer {
        tx_hash: format!("0x{}", format!("{tx:02x}").repeat(32)),
        log_index: 0,
        block_number: block,
        contract: USDT.into(),
        from: from.into(),
        to: PAY_IN.into(),
        value,
    }
}

fn later() -> u64 {
    chrono::Utc::now().timestamp() as u64 + 60
}

async fn deposits(pool: &PgPool) -> Vec<(String, String, i64, String, Option<String>)> {
    sqlx::query_as("SELECT clt_address, status, received_usdt, network, direct_from FROM deposit_intents ORDER BY created_at")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn p1_alerts(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM alerts WHERE severity = 'p1' AND source = 'bsc_topup'").fetch_one(pool).await.unwrap()
}

async fn cursor(pool: &PgPool) -> Option<i64> {
    sqlx::query_scalar("SELECT last_block FROM bsc_watch").fetch_optional(pool).await.unwrap()
}

#[tokio::test]
async fn a_registered_users_transfer_is_credited_once_as_a_bsc_deposit() {
    let pool = pool().await;
    bsc_watch::register_payer(&pool, USER, USER).await.unwrap();
    // 12.5000004 USDT, 50 blocks deep.
    let chain = FakeChain::new(1000, later(), vec![transfer(1, 950, USER, 12 * E18 + E18 / 2 + 400_000_000_000)]);

    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();
    assert_eq!(cursor(&pool).await, Some(985), "up to the head less the confirmations");
    // The same range again (a restart that lost the cursor, say) credits nothing new.
    sqlx::query("DELETE FROM bsc_watch").execute(&pool).await.unwrap();
    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();

    assert_eq!(deposits(&pool).await, vec![(USER.into(), "confirmed".into(), 12_500_000, "bsc".into(), Some(USER.into()))]);
    assert_eq!(p1_alerts(&pool).await, 0);
}

#[tokio::test]
async fn a_transfer_not_yet_confirmed_waits() {
    let pool = pool().await;
    bsc_watch::register_payer(&pool, USER, USER).await.unwrap();
    let chain = FakeChain::new(1000, later(), vec![transfer(2, 990, USER, 5 * E18)]);
    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();
    assert!(deposits(&pool).await.is_empty(), "10 blocks deep, 15 wanted");

    let chain = FakeChain::new(1005, later(), vec![transfer(2, 990, USER, 5 * E18)]);
    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();
    assert_eq!(deposits(&pool).await.len(), 1);
}

#[tokio::test]
async fn a_stranger_or_a_payment_sent_before_asking_is_reported_once_and_never_credited() {
    let pool = pool().await;
    let chain = FakeChain::new(1000, later(), vec![transfer(3, 950, STRANGER, 100 * E18), transfer(4, 951, USER, 20 * E18)]);
    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();
    assert_eq!(p1_alerts(&pool).await, 2);

    // The user asks only now; their earlier payment stays a human's to settle.
    bsc_watch::register_payer(&pool, USER, USER).await.unwrap();
    sqlx::query("DELETE FROM bsc_watch").execute(&pool).await.unwrap();
    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();
    assert!(deposits(&pool).await.is_empty());
    assert_eq!(p1_alerts(&pool).await, 2, "alerted once");

    // A transfer whose block predates the registration is not credited either.
    let early = FakeChain::new(2000, 1_000_000, vec![transfer(5, 1950, USER, 20 * E18)]);
    sqlx::query("UPDATE bsc_watch SET last_block = 1900").execute(&pool).await.unwrap();
    bsc_watch::watch_once(&pool, &early, &settings()).await.unwrap();
    assert!(deposits(&pool).await.is_empty());
}

#[tokio::test]
async fn dust_is_recorded_without_paging_and_two_senders_in_one_transaction_are_nobodys() {
    let pool = pool().await;
    bsc_watch::register_payer(&pool, USER, USER).await.unwrap();
    let chain = FakeChain::new(
        1000,
        later(),
        vec![transfer(6, 950, STRANGER, E18 / 100), transfer(7, 951, USER, 5 * E18), transfer(7, 951, STRANGER, 5 * E18)],
    );
    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();
    assert!(deposits(&pool).await.is_empty());
    let unattributed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bsc_unattributed").fetch_one(&pool).await.unwrap();
    assert_eq!(unattributed, 2);
    assert_eq!(p1_alerts(&pool).await, 1, "only the 10 USDT batch pages anyone");
}

#[tokio::test]
async fn ranges_are_bounded_and_an_outage_advances_nothing() {
    let pool = pool().await;
    sqlx::query("INSERT INTO bsc_watch (pay_in, last_block) VALUES ($1, 100)").bind(PAY_IN).execute(&pool).await.unwrap();
    let chain = FakeChain::new(10_000, later(), vec![]);
    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();
    assert_eq!(*chain.ranges.lock().unwrap(), vec![(101, 600)]);
    assert_eq!(cursor(&pool).await, Some(600));

    let mut down = FakeChain::new(10_000, later(), vec![]);
    down.fail = true;
    assert!(bsc_watch::watch_once(&pool, &down, &settings()).await.is_err());
    assert_eq!(cursor(&pool).await, Some(600));
}

/// Two users' own BSC deposit addresses (spec 2026-10-29), at TRON indexes 7 and 8.
const ADDR_A: &str = "0x9858effd232b4033e47d90003d41ec34ecaeda94";
const ADDR_B: &str = "0x6fac4d18c912343bf86fa7049364dd4e424ab9c0";
const USER_B: &str = "0x00000000000000000000000000000000000000b2";

async fn give_addresses(pool: &PgPool) {
    for (user, index, tron, bsc) in [(USER, 7i64, "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH", ADDR_A), (USER_B, 8, "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", ADDR_B)] {
        sqlx::query(
            "INSERT INTO deposit_addresses (user_pk, derivation_index, address, clt_address, bsc_address) VALUES ($1, $2, $3, $1, $4)",
        )
        .bind(user)
        .bind(index)
        .bind(tron)
        .bind(bsc)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn to(mut t: bsc::Transfer, address: &str) -> bsc::Transfer {
    t.to = address.into();
    t
}

async fn address_deposits(pool: &PgPool) -> Vec<(String, i64, Option<String>, Option<i64>, Option<String>, String)> {
    sqlx::query_as(
        "SELECT clt_address, received_usdt, deposit_address, derivation_index, direct_from, network FROM deposit_intents ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn anything_sent_to_a_users_own_address_is_theirs_whoever_sent_it() {
    let pool = pool().await;
    give_addresses(&pool).await;
    // An exchange withdrawal, in two Transfers of one transaction, nobody having asked first.
    let chain = FakeChain::new(1000, later(), vec![to(transfer(9, 950, STRANGER, 3 * E18), ADDR_A), to(transfer(9, 950, STRANGER, 2 * E18), ADDR_A)]);
    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();
    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();
    assert_eq!(
        address_deposits(&pool).await,
        vec![(USER.into(), 5_000_000, Some(ADDR_A.into()), Some(7), None, "bsc".into())]
    );
    assert_eq!(p1_alerts(&pool).await, 0);
}

#[tokio::test]
async fn the_sweep_of_a_deposit_address_is_not_a_top_up() {
    let pool = pool().await;
    give_addresses(&pool).await;
    let chain = FakeChain::new(1000, later(), vec![transfer(10, 950, ADDR_A, 5 * E18)]);
    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();
    assert!(address_deposits(&pool).await.is_empty());
    assert_eq!(p1_alerts(&pool).await, 0, "no 'unknown sender' page for our own sweep");
}

#[tokio::test]
async fn one_transaction_paying_two_users_credits_one_and_pages_for_the_other() {
    let pool = pool().await;
    give_addresses(&pool).await;
    let chain = FakeChain::new(1000, later(), vec![to(transfer(11, 950, STRANGER, 4 * E18), ADDR_A), to(transfer(11, 950, STRANGER, 6 * E18), ADDR_B)]);
    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();
    bsc_watch::watch_once(&pool, &chain, &settings()).await.unwrap();
    assert_eq!(address_deposits(&pool).await.len(), 1);
    assert_eq!(p1_alerts(&pool).await, 1, "once");
}
