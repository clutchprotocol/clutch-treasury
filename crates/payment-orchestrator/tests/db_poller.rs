//! The detection path end to end: a transfer at an intent's OWN derived address drives it to
//! `confirmed`, with the evidence recorded.
//!
//! The watcher is faked so the chain can be scripted, but everything else is real — real Postgres,
//! real transitions, real guards. What these tests are actually protecting is attribution: under
//! per-address deposits the destination IS the payer's identity, so a transfer landing on the wrong
//! intent is money credited to the wrong user.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use payment_orchestrator::custody::{CustodyWatcher, ObservedTransfer};
use payment_orchestrator::poller;
use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};
use uuid::Uuid;

const USDT: &str = "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf";

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_orch_poller");
    if !Postgres::database_exists(&url).await.unwrap_or(false) {
        Postgres::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    // deposit_addresses is included here (Task 5 test helper seed_address writes to it):
    // otherwise its rows outlive the test that created them while the sequence below is reset
    // to 0 every time, and the next test's first insert collides with a leftover row's
    // derivation_index — same reasoning as db_addresses.rs's own pool() helper.
    sqlx::query("TRUNCATE deposit_intents, alerts, deposit_addresses RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER SEQUENCE deposit_derivation_index_seq RESTART").execute(&pool).await.unwrap();
    pool
}

/// A scripted chain: address -> transfers. Records which addresses were asked about, so a test can
/// assert the poller does NOT go looking at addresses it has no business polling.
#[derive(Default)]
struct FakeChain {
    by_address: HashMap<String, Vec<ObservedTransfer>>,
    asked: Mutex<Vec<String>>,
    fail: bool,
}

impl FakeChain {
    fn with(address: &str, transfers: Vec<ObservedTransfer>) -> Self {
        let mut by_address = HashMap::new();
        by_address.insert(address.to_string(), transfers);
        Self { by_address, asked: Mutex::new(Vec::new()), fail: false }
    }
    fn failing() -> Self {
        Self { fail: true, ..Default::default() }
    }
}

#[async_trait]
impl CustodyWatcher for FakeChain {
    async fn transfers_to(
        &self,
        address: &str,
        _min_timestamp_ms: Option<i64>,
    ) -> Result<Vec<ObservedTransfer>, String> {
        self.asked.lock().unwrap().push(address.to_string());
        if self.fail {
            return Err("scripted TronGrid outage".into());
        }
        Ok(self.by_address.get(address).cloned().unwrap_or_default())
    }
}

fn transfer(tx: &str, to: &str, amount: i64, ts: i64) -> ObservedTransfer {
    ObservedTransfer { tx_id: tx.into(), amount_usdt: amount, to: to.into(), contract: USDT.into(), block_timestamp: ts }
}

/// Insert an intent directly, with its own address — the create path is covered elsewhere; these
/// tests are about detection.
async fn seed(pool: &PgPool, key: &str, address: &str, amount: i64, status: &str) -> Uuid {
    let id = Uuid::new_v4();
    let index = sqlx::query_scalar::<_, i64>("SELECT nextval('deposit_derivation_index_seq')")
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO deposit_intents
            (id, user_pk, clt_address, amount_usdt, amount_clt, client_key, status,
             expires_at, derivation_index, deposit_address)
         VALUES ($1, 'pk', 'clt', $2, $2, $3, $4, now() + interval '30 min', $5, $6)",
    )
    .bind(id)
    .bind(amount)
    .bind(key)
    .bind(status)
    .bind(index)
    .bind(address)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Insert a permanent deposit address row directly — the derive/issue path is covered elsewhere.
/// `hot_hours: Some(n)` marks it hot for `n` hours from now; `None` leaves it cold.
async fn seed_address(pool: &PgPool, user_pk: &str, address: &str, hot_hours: Option<i64>) {
    let index = sqlx::query_scalar::<_, i64>("SELECT nextval('deposit_derivation_index_seq')")
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO deposit_addresses (user_pk, derivation_index, address, clt_address, hot_until)
         VALUES ($1, $2, $3, 'clt', $4)",
    )
    .bind(user_pk)
    .bind(index)
    .bind(address)
    .bind(hot_hours.map(|h| chrono::Utc::now() + chrono::Duration::hours(h)))
    .execute(pool)
    .await
    .unwrap();
}

async fn status_of(pool: &PgPool, id: Uuid) -> String {
    sqlx::query_scalar::<_, String>("SELECT status FROM deposit_intents WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn tx_of(pool: &PgPool, id: Uuid) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT tron_tx_id FROM deposit_intents WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_transfer_at_the_intents_own_address_confirms_it_and_records_the_evidence() {
    let pool = pool().await;
    let addr = "TAddrAlice";
    let id = seed(&pool, "k-alice", addr, 2_000_000, "invoiced").await;

    let chain = FakeChain::with(addr, vec![transfer("tx-alice", addr, 2_000_000, 100)]);
    poller::poll_once(&pool, &chain).await;

    assert_eq!(status_of(&pool, id).await, "confirmed");
    assert_eq!(tx_of(&pool, id).await.as_deref(), Some("tx-alice"), "the evidence hash must be stored");
}

/// THE attribution property. Two intents, two addresses, one payment. Only the intent whose own
/// address received it may advance — anything else is crediting a stranger.
#[tokio::test]
async fn a_payment_credits_only_the_intent_that_owns_that_address() {
    let pool = pool().await;
    let alice_addr = "TAddrAlice";
    let bob_addr = "TAddrBob";
    let alice = seed(&pool, "k-a", alice_addr, 2_000_000, "invoiced").await;
    let bob = seed(&pool, "k-b", bob_addr, 2_000_000, "invoiced").await;

    // Same amount, so amount alone could not tell them apart — the address must.
    let chain = FakeChain::with(bob_addr, vec![transfer("tx-bob", bob_addr, 2_000_000, 100)]);
    poller::poll_once(&pool, &chain).await;

    assert_eq!(status_of(&pool, bob).await, "confirmed", "bob paid, bob confirms");
    assert_eq!(status_of(&pool, alice).await, "invoiced", "alice did not pay and must not advance");
    assert!(tx_of(&pool, alice).await.is_none(), "alice must carry no evidence");
}

#[tokio::test]
async fn an_underpayment_is_held_at_paying_and_never_credited() {
    let pool = pool().await;
    let addr = "TAddrShort";
    let id = seed(&pool, "k-short", addr, 2_000_000, "invoiced").await;

    let chain = FakeChain::with(addr, vec![transfer("tx-short", addr, 1_999_999, 100)]);
    poller::poll_once(&pool, &chain).await;

    assert_eq!(status_of(&pool, id).await, "paying", "short of expected — held, not confirmed");
    let alerts: i64 = sqlx::query_scalar("SELECT count(*) FROM alerts WHERE message LIKE '%underpaid%'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alerts, 1, "an underpayment must be flagged, not silently parked");
}

#[tokio::test]
async fn a_second_instalment_settles_a_previously_underpaid_intent() {
    let pool = pool().await;
    let addr = "TAddrTwoPart";
    let id = seed(&pool, "k-two", addr, 2_000_000, "invoiced").await;

    let first = FakeChain::with(addr, vec![transfer("tx-1", addr, 1_200_000, 100)]);
    poller::poll_once(&pool, &first).await;
    assert_eq!(status_of(&pool, id).await, "paying");

    let both = FakeChain::with(
        addr,
        vec![transfer("tx-1", addr, 1_200_000, 100), transfer("tx-2", addr, 800_000, 200)],
    );
    poller::poll_once(&pool, &both).await;
    assert_eq!(status_of(&pool, id).await, "confirmed", "the sum now covers the expected amount");
    assert_eq!(tx_of(&pool, id).await.as_deref(), Some("tx-1"), "evidence names the EARLIEST transfer");
}

#[tokio::test]
async fn an_overpayment_confirms_and_is_flagged() {
    let pool = pool().await;
    let addr = "TAddrOver";
    let id = seed(&pool, "k-over", addr, 2_000_000, "invoiced").await;

    let chain = FakeChain::with(addr, vec![transfer("tx-over", addr, 9_000_000, 100)]);
    poller::poll_once(&pool, &chain).await;

    assert_eq!(status_of(&pool, id).await, "confirmed");
    let alerts: i64 = sqlx::query_scalar("SELECT count(*) FROM alerts WHERE message LIKE '%overpaid%'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alerts, 1, "a surplus must be flagged for reconciliation");
}

/// A soft-expired intent is still honoured: our TTL is a local timer, and at par a late payment
/// carries no FX risk. Losing it would be losing a user's money.
#[tokio::test]
async fn a_late_payment_to_an_expired_intent_still_confirms() {
    let pool = pool().await;
    let addr = "TAddrLate";
    let id = seed(&pool, "k-late", addr, 2_000_000, "expired").await;

    let chain = FakeChain::with(addr, vec![transfer("tx-late", addr, 2_000_000, 100)]);
    poller::poll_once(&pool, &chain).await;

    assert_eq!(status_of(&pool, id).await, "confirmed");
}

/// A TronGrid outage must never look like "nobody paid": nothing advances, nothing is lost, and the
/// failure is reported.
#[tokio::test]
async fn a_chain_outage_neither_advances_nor_loses_the_intent() {
    let pool = pool().await;
    let id = seed(&pool, "k-outage", "TAddrOutage", 2_000_000, "invoiced").await;

    poller::poll_once(&pool, &FakeChain::failing()).await;

    assert_eq!(status_of(&pool, id).await, "invoiced", "an unread chain must not change state");
    let alerts: i64 = sqlx::query_scalar("SELECT count(*) FROM alerts WHERE message LIKE '%custody fetch failed%'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(alerts >= 1, "an outage must be surfaced");
}

/// Already-credited and human-held rows must not be polled at all — re-crediting is the one thing
/// that must be impossible, and a `needs_manual` row is someone's open decision.
#[tokio::test]
async fn terminal_and_flagged_intents_are_not_polled() {
    let pool = pool().await;
    let credited = seed(&pool, "k-done", "TAddrDone", 2_000_000, "credited").await;
    let manual = seed(&pool, "k-manual", "TAddrManual", 2_000_000, "needs_manual").await;
    let open = seed(&pool, "k-open", "TAddrOpen", 2_000_000, "invoiced").await;

    let chain = FakeChain::with("TAddrOpen", vec![transfer("tx-open", "TAddrOpen", 2_000_000, 100)]);
    poller::poll_once(&pool, &chain).await;

    let asked = chain.asked.lock().unwrap().clone();
    assert!(asked.contains(&"TAddrOpen".to_string()), "the open intent must be polled");
    assert!(!asked.contains(&"TAddrDone".to_string()), "a credited intent must never be re-polled");
    assert!(!asked.contains(&"TAddrManual".to_string()), "a human-held intent must not be auto-resolved");
    assert_eq!(status_of(&pool, credited).await, "credited");
    assert_eq!(status_of(&pool, manual).await, "needs_manual");
    assert_eq!(status_of(&pool, open).await, "confirmed");
}

/// Re-running a pass over an already-confirmed intent must be a no-op, not a second credit. The
/// poller has no memory between passes, so idempotence here is what makes it safe to run forever.
#[tokio::test]
async fn re_polling_a_confirmed_intent_changes_nothing() {
    let pool = pool().await;
    let addr = "TAddrIdem";
    let id = seed(&pool, "k-idem", addr, 2_000_000, "invoiced").await;
    let chain = FakeChain::with(addr, vec![transfer("tx-idem", addr, 2_000_000, 100)]);

    poller::poll_once(&pool, &chain).await;
    let after_first = status_of(&pool, id).await;
    let tx_first = tx_of(&pool, id).await;

    poller::poll_once(&pool, &chain).await;
    poller::poll_once(&pool, &chain).await;

    assert_eq!(status_of(&pool, id).await, after_first, "status must not drift on re-poll");
    assert_eq!(tx_of(&pool, id).await, tx_first, "the first-seen evidence hash must stand");
}

/// Discriminator-era rows have no address, so there is nothing to poll and the poller must skip
/// them rather than error or query a NULL.
#[tokio::test]
async fn legacy_rows_without_an_address_are_skipped() {
    let pool = pool().await;
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deposit_intents
            (id, user_pk, clt_address, amount_usdt, amount_clt, client_key, expires_at)
         VALUES ($1, 'pk', 'clt', 2000000, 2000000, 'k-legacy', now() + interval '30 min')",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    let chain = FakeChain::default();
    poller::poll_once(&pool, &chain).await;

    assert!(chain.asked.lock().unwrap().is_empty(), "a row with no address must not be polled");
    assert_eq!(status_of(&pool, id).await, "created");
}

#[tokio::test]
async fn hot_addresses_are_polled_before_cold_ones_and_the_budget_is_respected() {
    // Cost per PASS is what is bounded, not cost per address — otherwise a permanent address per
    // user means one TronGrid request per user who ever existed, every pass.
    let pool = pool().await;
    seed_address(&pool, "0xcold-1", "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH", None).await;
    seed_address(&pool, "0xcold-2", "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", None).await;
    seed_address(&pool, "0xhot", "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx", Some(24)).await;

    let due = payment_orchestrator::poller::due_addresses(&pool, 2).await.unwrap();

    assert_eq!(due.len(), 2, "the per-pass budget is a hard cap");
    assert_eq!(due[0].address, "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx", "hot first");
}

#[tokio::test]
async fn cold_addresses_rotate_oldest_polled_first() {
    let pool = pool().await;
    seed_address(&pool, "0xa", "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH", None).await;
    seed_address(&pool, "0xb", "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", None).await;
    sqlx::query("UPDATE deposit_addresses SET last_polled_at = now() WHERE user_pk = '0xa'")
        .execute(&pool).await.unwrap();

    let due = payment_orchestrator::poller::due_addresses(&pool, 1).await.unwrap();

    assert_eq!(due[0].user_pk, "0xb", "never-polled sorts ahead of just-polled");
}

/// Regression test for the three-tier bug in `due_addresses`'s ORDER BY. Under the OLD query
/// (`(hot_until > now()) DESC NULLS LAST, last_polled_at ASC NULLS FIRST`): A's `hot_until` is an
/// hour in the past, so `hot_until > now()` evaluates to FALSE; B's `hot_until` is NULL (never hot),
/// so the same expression evaluates to NULL. Boolean `DESC` ranks TRUE, then FALSE, then NULL —
/// `NULLS LAST` only guarantees NULL does not beat TRUE, it does NOT merge FALSE and NULL into one
/// tier — so A's FALSE outranks B's NULL regardless of `last_polled_at`, and A (polled a minute ago)
/// would be selected over B (never polled). Under the FIX, `COALESCE(hot_until > now(), false)` maps
/// both A and B to `false` on that first key, so they land in the same cold tier ordered by
/// `last_polled_at ASC NULLS FIRST` — B's NULL sorts before A's one-minute-old timestamp, so B wins.
#[tokio::test]
async fn expired_hot_addresses_rejoin_the_cold_rotation_instead_of_outranking_it() {
    let pool = pool().await;
    // A: previously hot, now expired — hot_until an hour in the past — polled a minute ago.
    seed_address(&pool, "0xa-expired-hot", "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH", Some(-1)).await;
    sqlx::query(
        "UPDATE deposit_addresses SET last_polled_at = now() - interval '1 minute' \
         WHERE user_pk = '0xa-expired-hot'",
    )
    .execute(&pool)
    .await
    .unwrap();
    // B: never hot, never polled.
    seed_address(&pool, "0xb-never-hot", "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", None).await;

    let due = payment_orchestrator::poller::due_addresses(&pool, 1).await.unwrap();

    assert_eq!(due.len(), 1, "the per-pass budget is a hard cap");
    assert_eq!(
        due[0].address, "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK",
        "a never-hot, never-polled address must not be starved by one that was hot once and expired"
    );
}
