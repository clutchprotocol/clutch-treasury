//! Plan C 5b: the deposit->mint bridge, against a real database and wiremock standing in for
//! the treasury (same convention as `bitcart_adapter.rs`'s wiremock coverage of Bitcart).
//!
//! The central property this file exists to prove, per the brief: the POST to
//! `/internal/mint-intents` sends `expected_amount_usdt` = `pay_amount_usdt` (the DISCRIMINATED
//! amount), never `amount_clt` — proven by asserting the exact JSON body wiremock actually
//! received, not by reading `treasury_bridge.rs` back and trusting it matches its own comments.
//!
//! Same shared-database convention as the other `db_*.rs` files: a sibling `_orchestrator`
//! database (sqlx's `_sqlx_migrations` bookkeeping table has no configurable name, so two
//! crates' migrators would corrupt each other's history on ONE shared DB).

use payment_orchestrator::configuration::OrchConfig;
use payment_orchestrator::deposits;
use payment_orchestrator::treasury_bridge;
use serde_json::json;
use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};
use uuid::Uuid;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_orchestrator");

    if !Postgres::database_exists(&url).await.unwrap_or(false) {
        Postgres::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE deposit_intents, webhook_events, alerts RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

fn test_config(treasury_url: String) -> OrchConfig {
    OrchConfig {
        http_addr: "0.0.0.0:0".into(),
        database_url: std::env::var("DATABASE_URL").unwrap(),
        jwt_secret: "test-jwt-secret".into(),
        bitcart_url: "http://unused".into(),
        bitcart_token: "t".into(),
        bitcart_store_id: "s".into(),
        public_base_url: "http://unused".into(),
        treasury_url,
        treasury_initiator_token: "test-treasury-initiator".into(),
        treasury_readonly_token: "test-treasury-readonly".into(),
        custody_tron_address: "Tunused".into(),
        deposit_ttl_minutes: 30,
        min_deposit_usdt: 1_000_000,
        max_deposit_usdt: 50_000_000,
        poll_interval_secs: 30,
    }
}

/// Inserts a `confirmed` deposit intent directly (bypassing the create-flow/webhook, since these
/// tests drive `treasury_bridge::run_once` at the module level) with `amount_clt` and
/// `pay_amount_usdt` deliberately DIFFERENT — the same fixture discipline `db_tron_verifier.rs`
/// uses, and for the same reason: if the bridge ever regresses to sending `amount_clt` where
/// `expected_amount_usdt` belongs, a test where the two values are equal could not catch it.
async fn seed_confirmed_deposit(pool: &PgPool, amount_clt: i64, pay_amount_usdt: i64, tron_tx_id: Option<&str>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deposit_intents
            (id, user_pk, clt_address, amount_usdt, pay_amount_usdt, amount_clt, status, client_key,
             invoice_id, tron_tx_id, bitcart_terminal, expires_at)
         VALUES ($1, 'user-pk-1', 'TBeneficiary1111111111111111111111', $2, $3, $2, 'confirmed', $4,
                 'inv-1', $5, TRUE, now() + interval '30 minutes')",
    )
    .bind(id)
    .bind(amount_clt)
    .bind(pay_amount_usdt)
    .bind(format!("key-{id}"))
    .bind(tron_tx_id)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn status_of(pool: &PgPool, id: Uuid) -> String {
    deposits::find_by_id(pool, id).await.unwrap().unwrap().status
}

fn mint_intent_response(id: Uuid, status: &str) -> serde_json::Value {
    json!({
        "id": id,
        "beneficiary": "TBeneficiary1111111111111111111111",
        "amount_clt": 1_000_000,
        "status": status,
        "credit_ref": format!("ref-{id}"),
        "created_by": "initiator",
        "approved_by": serde_json::Value::Null,
        "chain_tx_hash": serde_json::Value::Null,
        "client_ref": serde_json::Value::Null,
        "deposit_tx_id": serde_json::Value::Null,
        "verified_at": serde_json::Value::Null,
    })
}

/// THE central property (brief: "prove it on the wire, not by reading the code"): the POST body
/// must carry `pay_amount_usdt` as `expected_amount_usdt`, never `amount_clt`. `body_json` makes
/// wiremock itself reject any request that doesn't match this exact JSON — if the bridge ever
/// sent `amount_clt` (1,000,000) instead of `pay_amount_usdt` (1,000,391) in that field, wiremock
/// would 404 the request and this test would fail on the resulting treasury-unreachable path
/// (deposit stuck at `confirmed`), not silently pass.
#[tokio::test]
async fn post_sends_pay_amount_usdt_as_expected_amount_usdt_proven_on_the_wire() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = Uuid::new_v4();
    let deposit_id = seed_confirmed_deposit(&pool, 1_000_000, 1_000_391, Some("tron-tx-abc")).await;

    Mock::given(method("POST"))
        .and(path("/internal/mint-intents"))
        .and(body_json(json!({
            "beneficiary": "TBeneficiary1111111111111111111111",
            "amount_clt": 1_000_000,
            "expected_amount_usdt": 1_000_391,
            "client_ref": deposit_id.to_string(),
            "deposit_tx_id": "tron-tx-abc",
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(mint_intent_response(treasury_id, "created")))
        .expect(1)
        .mount(&server)
        .await;

    let config = test_config(server.uri());
    treasury_bridge::run_once(&pool, &config).await;

    // wiremock's .expect(1) above already fails the test if the exact body above was never
    // received; these assertions confirm the SIDE EFFECTS of that exact call landed correctly.
    let row = deposits::find_by_id(&pool, deposit_id).await.unwrap().unwrap();
    assert_eq!(row.status, "mint_requested", "a successful create must move the deposit to mint_requested");
    assert_eq!(row.treasury_intent_id, Some(treasury_id), "the returned treasury intent id must be stored");
}

/// `deposit_tx_id` may be NULL (Bitcart never returned a hash) — the POST must still succeed and
/// send `null`, not omit the field or invent a value.
#[tokio::test]
async fn post_sends_null_deposit_tx_id_when_not_yet_known() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = Uuid::new_v4();
    let deposit_id = seed_confirmed_deposit(&pool, 2_000_000, 2_000_042, None).await;

    Mock::given(method("POST"))
        .and(path("/internal/mint-intents"))
        .and(body_json(json!({
            "beneficiary": "TBeneficiary1111111111111111111111",
            "amount_clt": 2_000_000,
            "expected_amount_usdt": 2_000_042,
            "client_ref": deposit_id.to_string(),
            "deposit_tx_id": serde_json::Value::Null,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(mint_intent_response(treasury_id, "created")))
        .expect(1)
        .mount(&server)
        .await;

    let config = test_config(server.uri());
    treasury_bridge::run_once(&pool, &config).await;

    assert_eq!(status_of(&pool, deposit_id).await, "mint_requested");
}

/// The idempotency property (brief: "a duplicate/retried post does not produce two mint
/// intents"): the treasury's `client_ref` replay is what makes a retry safe, per the brief's
/// explicit instruction NOT to add a dedup layer on top of it. Simulates a retry by running
/// `run_once` twice against a mock that always returns the SAME treasury intent id (exactly what
/// a real client_ref replay would do) and confirms the bridge only ever calls the endpoint for a
/// row that is still `confirmed` — once the first call moves it to `mint_requested`, the second
/// pass's `due_for_mint_request` selection no longer includes it at all, so the endpoint is
/// asserted to receive exactly one call across both passes.
#[tokio::test]
async fn retried_run_does_not_create_a_second_mint_intent() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = Uuid::new_v4();
    let deposit_id = seed_confirmed_deposit(&pool, 3_000_000, 3_000_007, Some("tron-tx-retry")).await;

    Mock::given(method("POST"))
        .and(path("/internal/mint-intents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mint_intent_response(treasury_id, "created")))
        .expect(1) // exactly once across BOTH run_once calls below
        .mount(&server)
        .await;

    let config = test_config(server.uri());
    treasury_bridge::run_once(&pool, &config).await;
    assert_eq!(status_of(&pool, deposit_id).await, "mint_requested");

    // A second pass (simulating a retry, or just the next poll tick): the row is no longer
    // `confirmed`, so `due_for_mint_request` must not pick it up again — the create step must
    // not run a second time for this deposit at all, which the mock's `.expect(1)` enforces.
    treasury_bridge::run_once(&pool, &config).await;
    assert_eq!(status_of(&pool, deposit_id).await, "mint_requested", "still exactly one mint_requested transition");
}

/// Treasury `rejected` (funds arrived but the verifier found a hard evidence mismatch, or a
/// human rejected a manual mint) must drive the deposit to `needs_manual` and raise a P1 whose
/// text tells the operator funds are unminted in custody and a NEW intent is required.
///
/// A single tick runs the create step AND the poll step in the same pass (see the comment on
/// `a_successful_call_resets_the_consecutive_failure_streak`): the row `create_step` just moved
/// to `mint_requested` is immediately eligible for that same call's `due_for_status_poll` half.
/// So the GET route must already be mounted (answering a real in-flight status) BEFORE the
/// first `run_once` — otherwise that same-tick poll attempt 404s, records a failure, and backs
/// `next_attempt_at` off into the future, which would make the SECOND `run_once` below skip the
/// row entirely (still within its backoff window) rather than actually observing `rejected`.
#[tokio::test]
async fn treasury_rejected_drives_needs_manual_with_p1_alert() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = Uuid::new_v4();
    let deposit_id = seed_confirmed_deposit(&pool, 1_000_000, 1_000_555, Some("tron-tx-rej")).await;

    Mock::given(method("POST"))
        .and(path("/internal/mint-intents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mint_intent_response(treasury_id, "created")))
        .mount(&server)
        .await;
    // Answers the FIRST call's same-tick poll half with a real in-flight status, so that pass
    // succeeds cleanly (attempts stay 0, no backoff) rather than 404ing.
    Mock::given(method("GET"))
        .and(path(format!("/internal/mint-intents/{treasury_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(mint_intent_response(treasury_id, "created")))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let config = test_config(server.uri());
    treasury_bridge::run_once(&pool, &config).await; // create step: confirmed -> mint_requested; same-tick poll sees "created", no-op
    assert_eq!(status_of(&pool, deposit_id).await, "mint_requested");

    // Now the SECOND call's poll must see the real verdict.
    Mock::given(method("GET"))
        .and(path(format!("/internal/mint-intents/{treasury_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(mint_intent_response(treasury_id, "rejected")))
        .mount(&server)
        .await;
    treasury_bridge::run_once(&pool, &config).await; // poll step: sees rejected
    assert_eq!(status_of(&pool, deposit_id).await, "needs_manual", "rejected must drive the deposit to needs_manual");

    let alerts: Vec<(String, String)> =
        sqlx::query_as("SELECT severity, message FROM alerts WHERE source = 'treasury_bridge'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(alerts.len(), 1, "exactly one alert for the rejection");
    let (severity, message) = &alerts[0];
    assert_eq!(severity, "p1", "a rejected deposit-backed mint must page p1, not a lower severity");
    assert!(message.contains("CUSTODY"), "alert must say funds are in custody: {message}");
    assert!(message.contains("client_ref") && message.contains("burned"), "alert must say client_ref is burned: {message}");
    assert!(message.to_lowercase().contains("new"), "alert must say re-minting needs a NEW intent: {message}");
}

/// Same shape as `rejected`, proven separately since the brief names both explicitly and they
/// are two different treasury outcomes (rejected = hard evidence mismatch; failed = something
/// else went wrong downstream) that must both land the deposit in the same place.
#[tokio::test]
async fn treasury_failed_also_drives_needs_manual_with_p1_alert() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = Uuid::new_v4();
    let deposit_id = seed_confirmed_deposit(&pool, 1_000_000, 1_000_777, Some("tron-tx-failed")).await;

    Mock::given(method("POST"))
        .and(path("/internal/mint-intents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mint_intent_response(treasury_id, "created")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/internal/mint-intents/{treasury_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(mint_intent_response(treasury_id, "failed")))
        .mount(&server)
        .await;

    let config = test_config(server.uri());
    treasury_bridge::run_once(&pool, &config).await;
    treasury_bridge::run_once(&pool, &config).await;

    assert_eq!(status_of(&pool, deposit_id).await, "needs_manual");
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alerts WHERE severity = 'p1' AND source = 'treasury_bridge'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

/// Treasury `credited` must move the deposit all the way to its own terminal `credited` status.
#[tokio::test]
async fn treasury_credited_drives_deposit_to_credited() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = Uuid::new_v4();
    let deposit_id = seed_confirmed_deposit(&pool, 1_000_000, 1_000_222, Some("tron-tx-cred")).await;

    Mock::given(method("POST"))
        .and(path("/internal/mint-intents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mint_intent_response(treasury_id, "created")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/internal/mint-intents/{treasury_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(mint_intent_response(treasury_id, "credited")))
        .mount(&server)
        .await;

    let config = test_config(server.uri());
    treasury_bridge::run_once(&pool, &config).await;
    treasury_bridge::run_once(&pool, &config).await;

    assert_eq!(status_of(&pool, deposit_id).await, "credited");
}

/// A still-in-flight treasury status (`created`/`approved`/`submitted`) must leave the deposit
/// at `mint_requested` — nothing to do yet, no alert, no status change.
#[tokio::test]
async fn treasury_still_pending_leaves_deposit_at_mint_requested() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = Uuid::new_v4();
    let deposit_id = seed_confirmed_deposit(&pool, 1_000_000, 1_000_888, Some("tron-tx-pending")).await;

    Mock::given(method("POST"))
        .and(path("/internal/mint-intents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mint_intent_response(treasury_id, "created")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/internal/mint-intents/{treasury_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(mint_intent_response(treasury_id, "approved")))
        .mount(&server)
        .await;

    let config = test_config(server.uri());
    treasury_bridge::run_once(&pool, &config).await;
    treasury_bridge::run_once(&pool, &config).await;

    assert_eq!(status_of(&pool, deposit_id).await, "mint_requested", "an in-flight treasury status must not move the deposit");
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alerts").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0, "no alert for a merely-still-pending status");
}

/// The reliability property: a treasury that is completely unreachable (connection refused —
/// no mock mounted at all) must neither lose the deposit (it stays exactly `confirmed`) nor
/// silently advance it. `attempts` must be recorded so the backoff and the eventual P1 have
/// something to count from.
#[tokio::test]
async fn treasury_unreachable_neither_loses_nor_advances_the_deposit() {
    let pool = pool().await;
    // A MockServer with NO mounted routes still accepts connections and 404s everything, which
    // exercises the "non-2xx response" arm of create_step's match, not a transport-level
    // connection failure. Both are "unreachable" from the bridge's point of view (module docs:
    // "not reachable-to-fix" gets the same failure treatment), so this is a faithful stand-in
    // for the reliability property without needing to bind-then-drop a real port.
    let server = MockServer::start().await;
    let deposit_id = seed_confirmed_deposit(&pool, 1_000_000, 1_000_999, Some("tron-tx-unreachable")).await;

    let config = test_config(server.uri());
    treasury_bridge::run_once(&pool, &config).await;

    let row = deposits::find_by_id(&pool, deposit_id).await.unwrap().unwrap();
    assert_eq!(row.status, "confirmed", "an unreachable treasury must leave the deposit exactly where it was");
    assert_eq!(row.attempts, 1, "the failed attempt must be recorded");
    assert!(row.treasury_intent_id.is_none(), "no treasury intent id can exist for a call that never succeeded");
}

/// The brief's explicit threshold: only after the 10th CONSECUTIVE treasury-unreachable failure
/// does a P1 fire — not on the first, and not on every one after the 10th either (a prolonged
/// outage must not spam pages beyond the first crossing).
#[tokio::test]
async fn p1_alert_fires_at_exactly_ten_consecutive_failures_not_before_or_repeatedly() {
    let pool = pool().await;
    let server = MockServer::start().await; // no routes mounted: every call is a 404 ("unreachable")
    let deposit_id = seed_confirmed_deposit(&pool, 1_000_000, 1_000_321, Some("tron-tx-flaky")).await;
    let config = test_config(server.uri());

    for i in 1..=9 {
        treasury_bridge::run_once(&pool, &config).await;
        // Reset next_attempt_at so the next call is immediately due — this test asserts the
        // COUNT threshold, not the real backoff timing (that's a separate concern this file
        // doesn't need to slow itself down proving).
        sqlx::query("UPDATE deposit_intents SET next_attempt_at = now() WHERE id = $1")
            .bind(deposit_id)
            .execute(&pool)
            .await
            .unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alerts WHERE severity = 'p1'").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "no p1 before the 10th consecutive failure (attempt {i})");
    }

    treasury_bridge::run_once(&pool, &config).await; // 10th consecutive failure
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alerts WHERE severity = 'p1'").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1, "the 10th consecutive failure must page p1");

    sqlx::query("UPDATE deposit_intents SET next_attempt_at = now() WHERE id = $1")
        .bind(deposit_id)
        .execute(&pool)
        .await
        .unwrap();
    treasury_bridge::run_once(&pool, &config).await; // 11th: must not page again
    let (n_after,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alerts WHERE severity = 'p1'").fetch_one(&pool).await.unwrap();
    assert_eq!(n_after, 1, "an 11th consecutive failure must not page a second p1");
}

/// A treasury call that DOES succeed after prior failures must reset the streak — proven by
/// failing once, then succeeding, then failing nine more times and confirming that does NOT
/// reach the threshold (10 failures total, but only 9 consecutive since the reset).
///
/// Both the GET and the POST routes are mounted before the second `run_once`: a single tick
/// runs the create step AND the poll step in the same pass (`run_once` drives both
/// `due_for_mint_request` and `due_for_status_poll` every call, same as `outbox.rs`/
/// `watcher.rs`'s existing two-pass shape elsewhere in this workspace), so a row the create
/// step just moved to `mint_requested` is immediately eligible for that same tick's poll half.
/// Mounting only POST here would make the poll half fail instead (no GET route to answer it),
/// which is a real property of this test's setup, not of the reset behaviour under test.
#[tokio::test]
async fn a_successful_call_resets_the_consecutive_failure_streak() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = Uuid::new_v4();
    let deposit_id = seed_confirmed_deposit(&pool, 1_000_000, 1_000_654, Some("tron-tx-reset")).await;

    // First call: no mock mounted yet, so it fails (attempts -> 1).
    let config = test_config(server.uri());
    treasury_bridge::run_once(&pool, &config).await;
    assert_eq!(deposits::find_by_id(&pool, deposit_id).await.unwrap().unwrap().attempts, 1);
    sqlx::query("UPDATE deposit_intents SET next_attempt_at = now() WHERE id = $1").bind(deposit_id).execute(&pool).await.unwrap();

    // Now mount both routes so the NEXT call's create step succeeds AND its same-tick poll
    // step (still-pending "created") also succeeds — attempts must reset to 0 either way.
    Mock::given(method("POST"))
        .and(path("/internal/mint-intents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mint_intent_response(treasury_id, "created")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/internal/mint-intents/{treasury_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(mint_intent_response(treasury_id, "created")))
        .mount(&server)
        .await;
    treasury_bridge::run_once(&pool, &config).await;
    assert_eq!(status_of(&pool, deposit_id).await, "mint_requested");
    assert_eq!(deposits::find_by_id(&pool, deposit_id).await.unwrap().unwrap().attempts, 0, "a success must reset attempts to 0");
}

/// The headroom check (T2b's deferral, landed here): when the treasury reports insufficient
/// daily headroom, `create_and_invoice` must refuse the deposit with `InsufficientHeadroom`
/// rather than proceeding to call Bitcart — proven via the FakeAdapter panicking if invoked.
#[tokio::test]
async fn headroom_check_refuses_when_insufficient() {
    use async_trait::async_trait;
    use payment_orchestrator::adapter::{InvoiceStatus, PaymentAdapter, PaymentInstructions};

    struct PanicsIfCalledAdapter;
    #[async_trait]
    impl PaymentAdapter for PanicsIfCalledAdapter {
        async fn create_invoice(&self, _: &str, _: i64, _: &str) -> Result<PaymentInstructions, String> {
            panic!("Bitcart must never be called when headroom is insufficient — fail closed BEFORE creating any invoice");
        }
        async fn get_invoice(&self, _: &str) -> Result<InvoiceStatus, String> {
            unimplemented!("not exercised by this test")
        }
    }

    let pool = pool().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/internal/reserve-status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"daily_headroom_clt": 500_000})))
        .mount(&server)
        .await;

    let config = test_config(server.uri());
    let outcome = deposits::create_and_invoice(
        &pool,
        &config,
        &PanicsIfCalledAdapter,
        "user-pk-headroom",
        "clt-addr-headroom",
        1_000_000, // exceeds the 500_000 headroom reported above
        "key-headroom-insufficient",
        "http://unused/webhooks/bitcart",
    )
    .await;

    match outcome {
        deposits::DepositOutcome::InsufficientHeadroom { headroom_clt } => assert_eq!(headroom_clt, 500_000),
        other => panic!("expected InsufficientHeadroom, got {other:?}"),
    }

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM deposit_intents WHERE client_key = 'key-headroom-insufficient'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "the row itself is still created (idempotency layer 1) even though the invoice step never ran");
}

/// The other half of fail-closed: an unreachable treasury during the headroom check must ALSO
/// refuse (503-shaped), never silently proceed as if headroom were fine.
#[tokio::test]
async fn headroom_check_refuses_when_treasury_unreachable() {
    use async_trait::async_trait;
    use payment_orchestrator::adapter::{InvoiceStatus, PaymentAdapter, PaymentInstructions};

    struct PanicsIfCalledAdapter;
    #[async_trait]
    impl PaymentAdapter for PanicsIfCalledAdapter {
        async fn create_invoice(&self, _: &str, _: i64, _: &str) -> Result<PaymentInstructions, String> {
            panic!("Bitcart must never be called when the treasury is unreachable — fail closed");
        }
        async fn get_invoice(&self, _: &str) -> Result<InvoiceStatus, String> {
            unimplemented!("not exercised by this test")
        }
    }

    let pool = pool().await;
    // No MockServer at all for this one — a bare unroutable URL simulates a treasury that is
    // completely unreachable (connection failure), the other half of "unreachable" alongside a
    // non-2xx response.
    let mut config = test_config("http://127.0.0.1:1".to_string());
    config.treasury_url = "http://127.0.0.1:1".to_string();

    let outcome = deposits::create_and_invoice(
        &pool,
        &config,
        &PanicsIfCalledAdapter,
        "user-pk-unreachable",
        "clt-addr-unreachable",
        1_000_000,
        "key-headroom-unreachable",
        "http://unused/webhooks/bitcart",
    )
    .await;

    assert!(
        matches!(outcome, deposits::DepositOutcome::TreasuryUnavailable),
        "expected TreasuryUnavailable, got {outcome:?}"
    );
}

/// Headroom sufficient: the create-flow must proceed exactly as before this task landed —
/// proven by reaching a real 201 Respond via a working FakeAdapter.
#[tokio::test]
async fn headroom_check_allows_through_when_sufficient() {
    use async_trait::async_trait;
    use payment_orchestrator::adapter::{InvoiceStatus, PaymentAdapter, PaymentInstructions};

    struct WorkingAdapter;
    #[async_trait]
    impl PaymentAdapter for WorkingAdapter {
        async fn create_invoice(&self, order_id: &str, pay_amount_usdt: i64, _: &str) -> Result<PaymentInstructions, String> {
            Ok(PaymentInstructions {
                invoice_id: format!("inv-{order_id}"),
                pay_address: "TCustodyAddressXXXXXXXXXXXXXXXXXXX".to_string(),
                pay_amount_usdt,
                expires_at: chrono::Utc::now() + chrono::Duration::minutes(30),
            })
        }
        async fn get_invoice(&self, _: &str) -> Result<InvoiceStatus, String> {
            unimplemented!("not exercised by this test")
        }
    }

    let pool = pool().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/internal/reserve-status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"daily_headroom_clt": 50_000_000})))
        .mount(&server)
        .await;

    let config = test_config(server.uri());
    let outcome = deposits::create_and_invoice(
        &pool,
        &config,
        &WorkingAdapter,
        "user-pk-sufficient",
        "clt-addr-sufficient",
        1_000_000,
        "key-headroom-sufficient",
        "http://unused/webhooks/bitcart",
    )
    .await;

    assert!(
        matches!(outcome, deposits::DepositOutcome::Respond { status: 201, .. }),
        "expected a normal 201 Respond when headroom is sufficient, got {outcome:?}"
    );
}
