//! The detection path end to end: a transfer at an intent's OWN derived address drives it to
//! `confirmed`, with the evidence recorded.
//!
//! The watcher is faked so the chain can be scripted, but everything else is real — real Postgres,
//! real transitions, real guards. What these tests are actually protecting is attribution: under
//! per-address deposits the destination IS the payer's identity, so a transfer landing on the wrong
//! intent is money credited to the wrong user.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use payment_orchestrator::custody::{CustodyWatcher, DepositWatcher, ObservedTransfer, TronGridWatcher};
use payment_orchestrator::poller;
use serde_json::json;
use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const USDT: &str = "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf";
/// Fixture address for the four crediting tests below — a real-shaped Tron address, matching what
/// `seed_address` and `TronGridWatcher` both expect.
const ADDR: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";

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

/// `poll_once`'s loop (a) needs a `DepositWatcher` too. Returns every transfer this fake knows about
/// regardless of address (a real `DepositWatcher` is not address-scoped either, per custody.rs) and
/// fails the same way `CustodyWatcher::transfers_to` does when scripted to. None of `FakeChain`'s own
/// legacy-per-intent tests seed any `deposit_addresses` rows, so `credit_from_addresses` resolves no
/// owner for anything this returns and quietly leaves it to loop (b) — this is what R17's new test
/// (`a_per_user_poll_outage_does_not_stop_the_legacy_loop`) needs a real failure mode for.
#[async_trait]
impl DepositWatcher for FakeChain {
    async fn poll(&self) -> Result<Vec<ObservedTransfer>, String> {
        if self.fail {
            return Err("scripted TronGrid outage".into());
        }
        Ok(self.by_address.values().flatten().cloned().collect())
    }
}

fn transfer(tx: &str, to: &str, amount: i64, ts: i64) -> ObservedTransfer {
    ObservedTransfer { tx_id: tx.into(), amount_usdt: amount, to: to.into(), contract: USDT.into(), block_timestamp: ts }
}

/// Same shape as `transfer`, minus the timestamp: the four crediting tests below never depend on
/// ordering, only on identity (`tx_id`) and amount.
fn observed(tx_id: &str, to: &str, amount: i64) -> ObservedTransfer {
    ObservedTransfer { tx_id: tx_id.into(), amount_usdt: amount, to: to.into(), contract: USDT.into(), block_timestamp: 1_700_000_000_000 }
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
    poller::poll_once(&pool, &chain, &chain, USDT).await;

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
    poller::poll_once(&pool, &chain, &chain, USDT).await;

    assert_eq!(status_of(&pool, bob).await, "confirmed", "bob paid, bob confirms");
    assert_eq!(status_of(&pool, alice).await, "invoiced", "alice did not pay and must not advance");
    assert!(tx_of(&pool, alice).await.is_none(), "alice must carry no evidence");
}

// `an_underpayment_is_held_at_paying_and_never_credited` and
// `a_second_instalment_settles_a_previously_underpaid_intent` were deleted here (Task 7, Step 5b):
// both pinned `PaymentOutcome::Partial` — an intent held at `paying`, not credited, because a
// transfer fell short of the amount it was created with. "Credit everything, cap nothing" now
// applies to legacy addresses too, so the property they tested (short payments are NOT credited) is
// gone, not moved: the FIRST unseen transfer to a legacy address settles it at its own arrived
// amount, however small. There is nothing left to instalment toward.

/// "Credit everything, cap nothing" applies to legacy addresses too: an amount over what the intent
/// was originally created for still confirms, at the full arrived amount — there is no ceiling to
/// compare against any more. Renamed from `an_overpayment_confirms_and_is_flagged`: the "overpaid"
/// alert was expected-amount arithmetic (Task 7, Step 5b) and was deleted with it.
#[tokio::test]
async fn an_overpayment_confirms_at_the_full_arrived_amount() {
    let pool = pool().await;
    let addr = "TAddrOver";
    let id = seed(&pool, "k-over", addr, 2_000_000, "invoiced").await;

    let chain = FakeChain::with(addr, vec![transfer("tx-over", addr, 9_000_000, 100)]);
    poller::poll_once(&pool, &chain, &chain, USDT).await;

    assert_eq!(status_of(&pool, id).await, "confirmed");
    let received: i64 = sqlx::query_scalar("SELECT received_usdt FROM deposit_intents WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(received, 9_000_000, "credited at what arrived, not capped at what was originally asked for");
}

/// A soft-expired intent is still honoured: our TTL is a local timer, and at par a late payment
/// carries no FX risk. Losing it would be losing a user's money.
#[tokio::test]
async fn a_late_payment_to_an_expired_intent_still_confirms() {
    let pool = pool().await;
    let addr = "TAddrLate";
    let id = seed(&pool, "k-late", addr, 2_000_000, "expired").await;

    let chain = FakeChain::with(addr, vec![transfer("tx-late", addr, 2_000_000, 100)]);
    poller::poll_once(&pool, &chain, &chain, USDT).await;

    assert_eq!(status_of(&pool, id).await, "confirmed");
}

/// A TronGrid outage must never look like "nobody paid": nothing advances, nothing is lost, and the
/// failure is reported.
#[tokio::test]
async fn a_chain_outage_neither_advances_nor_loses_the_intent() {
    let pool = pool().await;
    let id = seed(&pool, "k-outage", "TAddrOutage", 2_000_000, "invoiced").await;

    let chain = FakeChain::failing();
    poller::poll_once(&pool, &chain, &chain, USDT).await;

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
    poller::poll_once(&pool, &chain, &chain, USDT).await;

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

    poller::poll_once(&pool, &chain, &chain, USDT).await;
    let after_first = status_of(&pool, id).await;
    let tx_first = tx_of(&pool, id).await;

    poller::poll_once(&pool, &chain, &chain, USDT).await;
    poller::poll_once(&pool, &chain, &chain, USDT).await;

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
    poller::poll_once(&pool, &chain, &chain, USDT).await;

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

// Task 7: crediting permanent per-user addresses. Each transfer is its own deposit — there is no
// expected amount to sum toward or cap against any more.

#[tokio::test]
async fn two_transfers_to_one_address_are_two_credits() {
    // The core of the change, and impossible to express under the uniqueness index dropped in
    // Task 3. One user, one address, two deposits.
    let pool = pool().await;
    seed_address(&pool, "0xuser-a", ADDR, None).await;
    let index: i64 = sqlx::query_scalar("SELECT derivation_index FROM deposit_addresses WHERE address = $1")
        .bind(ADDR)
        .fetch_one(&pool)
        .await
        .unwrap();

    let a = observed("tx-one", ADDR, 1_000_000);
    let b = observed("tx-two", ADDR, 2_500_000);
    assert!(poller::credit_transfer(&pool, "0xuser-a", "0xclt-a", index, &a).await.unwrap());
    assert!(poller::credit_transfer(&pool, "0xuser-a", "0xclt-a", index, &b).await.unwrap());

    let (rows, total, off_par, wrong_index): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), COALESCE(SUM(received_usdt),0)::BIGINT,
                count(*) FILTER (WHERE amount_clt <> received_usdt OR amount_usdt <> received_usdt),
                count(*) FILTER (WHERE derivation_index IS DISTINCT FROM $2)
         FROM deposit_intents WHERE deposit_address = $1",
    )
    .bind(ADDR)
    .bind(index)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 2);
    assert_eq!(total, 3_500_000, "each transfer credited in full — credit everything, cap nothing");
    // Task 6 retired `amount_clt_equals_amount_usdt_at_par` with the create() flow; the par rule
    // (1 micro-USDT = 1 CLT base unit) now lives in credit_transfer's INSERT, so it is pinned here.
    assert_eq!(off_par, 0, "amount_clt and amount_usdt must both equal what arrived");
    // R17: credit_transfer used to silently omit derivation_index — every such row got minted
    // (treasury_bridge.rs forwards it verbatim) and then never swept (sweeper.rs filters on it being
    // set), with nothing in the suite noticing a NULL.
    assert_eq!(wrong_index, 0, "both credited rows must carry the address's own derivation_index");
}

#[tokio::test]
async fn the_same_transfer_seen_twice_is_one_credit() {
    // A poll pass re-reads an address's recent history every rotation, so re-observation is the
    // normal case, not an edge case. Identity is the transaction.
    let pool = pool().await;
    seed_address(&pool, "0xuser-a", ADDR, None).await;
    let t = observed("tx-one", ADDR, 1_000_000);

    assert!(poller::credit_transfer(&pool, "0xuser-a", "0xclt-a", 0, &t).await.unwrap());
    assert!(
        !poller::credit_transfer(&pool, "0xuser-a", "0xclt-a", 0, &t).await.unwrap(),
        "the second sighting creates nothing"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM deposit_intents WHERE deposit_address = $1")
        .bind(ADDR)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn a_one_cent_deposit_is_credited_in_full() {
    // "Credit everything, cap nothing" — there is no floor. Dust costs more TRX to sweep than it is
    // worth, but that lands on the sweep threshold and the fee account, never on the user.
    let pool = pool().await;
    seed_address(&pool, "0xuser-a", ADDR, None).await;

    poller::credit_transfer(&pool, "0xuser-a", "0xclt-a", 0, &observed("tx-dust", ADDR, 10_000)).await.unwrap();

    let got: i64 = sqlx::query_scalar("SELECT received_usdt FROM deposit_intents WHERE tron_tx_id = 'tx-dust'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(got, 10_000);
}

/// `TieredPoller::poll` itself had no coverage before this: without "stamp `last_polled_at` even on
/// failure", one broken address would be re-polled every pass forever while the others starved
/// behind it — exactly the failure mode Task 5's per-pass budget exists to prevent. Covered here,
/// where `poll_once`'s loop (a) (`credit_from_addresses`) finally wires `TieredPoller` in for real,
/// against a real (mocked) TronGrid rather than `FakeChain`.
#[tokio::test]
async fn a_failing_address_is_still_stamped_and_does_not_block_the_others() {
    let pool = pool().await;
    let addr_a = "TAddrFailingA";
    let addr_b = "TAddrWorkingB";
    seed_address(&pool, "0xuser-a", addr_a, None).await;
    seed_address(&pool, "0xuser-b", addr_b, None).await;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{addr_a}/transactions/trc20")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{addr_b}/transactions/trc20")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "transaction_id": "tx-b",
                "to": addr_b,
                "value": "500000",
                "token_info": { "address": USDT },
                "type": "Transfer",
                "block_timestamp": 1_700_000_000_000i64,
            }]
        })))
        .mount(&server)
        .await;

    let raw: Arc<dyn CustodyWatcher> = Arc::new(TronGridWatcher::new(server.uri(), "test-key".into(), USDT.into()));
    let tiered = poller::TieredPoller { pool: pool.clone(), inner: raw, budget: 2 };

    let result = poller::credit_from_addresses(&pool, &tiered, USDT).await;
    assert!(result.is_ok(), "A's failure must be logged, not propagated: {result:?}");

    let credited: i64 = sqlx::query_scalar("SELECT count(*) FROM deposit_intents WHERE tron_tx_id = 'tx-b'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(credited, 1, "B's transfer must be credited");

    let stamped: i64 = sqlx::query_scalar("SELECT count(*) FROM deposit_addresses WHERE last_polled_at IS NOT NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stamped, 2, "both A and B must be stamped even though A's fetch failed");
}

// R17 review fixes.

/// TRON dust-poisoning sends 0-value TRC-20 transfers routinely — this must be a no-op, not the
/// recurring `CHECK (amount_usdt > 0)` violation it would be if credited verbatim.
#[tokio::test]
async fn a_zero_value_transfer_is_ignored_not_an_error() {
    let pool = pool().await;
    seed_address(&pool, "0xuser-a", ADDR, None).await;

    assert!(
        !poller::credit_transfer(&pool, "0xuser-a", "0xclt-a", 0, &observed("tx-dust-zero", ADDR, 0))
            .await
            .unwrap(),
        "a zero-value transfer must not be credited"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM deposit_intents WHERE tron_tx_id = 'tx-dust-zero'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

/// Defence in depth for the `DepositWatcher` seam: it is deliberately not address-scoped (custody.rs
/// module docs), so a future cursor-based implementation that filters locally has nothing upstream of
/// `credit_from_addresses` guaranteed to have already checked the contract.
#[tokio::test]
async fn a_transfer_of_another_token_is_not_credited() {
    let pool = pool().await;
    seed_address(&pool, "0xuser-a", ADDR, None).await;

    let wrong_token = ObservedTransfer {
        tx_id: "tx-wrong-token".into(),
        amount_usdt: 1_000_000,
        to: ADDR.into(),
        contract: "TWrongTokenContract111111111111111".into(),
        block_timestamp: 1_700_000_000_000,
    };
    let chain = FakeChain::with(ADDR, vec![wrong_token]);

    let result = poller::credit_from_addresses(&pool, &chain, USDT).await;
    assert!(result.is_ok());

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM deposit_intents WHERE tron_tx_id = 'tx-wrong-token'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "a transfer in the wrong contract must never be credited");
}

/// The property that is the whole reason `poll_once` stays `()`-returning instead of propagating
/// loop (a)'s `Result` with a bare `?`: a per-user `DepositWatcher` outage must not stop loop (b)
/// from reaching the legacy intents behind it.
#[tokio::test]
async fn a_per_user_poll_outage_does_not_stop_the_legacy_loop() {
    let pool = pool().await;
    let addr = "TAddrLegacyDuringOutage";
    let id = seed(&pool, "k-legacy-outage", addr, 2_000_000, "invoiced").await;

    let failing = FakeChain::failing();
    let healthy = FakeChain::with(addr, vec![transfer("tx-legacy", addr, 2_000_000, 100)]);

    poller::poll_once(&pool, &failing, &healthy, USDT).await;

    assert_eq!(status_of(&pool, id).await, "confirmed", "a per-user outage must not block the legacy loop");
}
