//! T2b end to end: the two public routes, exercised through the real router (auth header
//! parsing, JSON extraction, handler → `deposits::create_and_invoice` → `PaymentAdapter` →
//! CAS store), not just the underlying module functions (those are `db_deposits.rs`, T2).
//!
//! Same shared-database convention as `db_deposits.rs`: a sibling `_orchestrator` database
//! per the comment there (sqlx's `_sqlx_migrations` bookkeeping table has no configurable
//! name, so two crates' migrators would corrupt each other's history on ONE shared DB).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use jsonwebtoken::{encode, EncodingKey, Header};
use payment_orchestrator::configuration::OrchConfig;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Account xpub for the canonical public BIP39 all-"abandon" test mnemonic (m/44'/195'/0').
/// Public test material; never holds funds.
const TEST_XPUB: &str = "xpub6D1AabNHCupeiLM65ZR9UStMhJ1vCpyV4XbZdyhMZBiJXALQtmn9p42VTQckoHVn8WNqS7dqnJokZHAHcHGoaQgmv8D45oNUKx6DZMNZBCd";

/// `&'static` so call sites can pass it without creating a temporary that is dropped while
/// borrowed, and so the xpub is parsed once per test binary rather than per call.
fn test_deriver() -> &'static payment_orchestrator::derive::AddressDeriver {
    static D: std::sync::OnceLock<payment_orchestrator::derive::AddressDeriver> = std::sync::OnceLock::new();
    D.get_or_init(|| payment_orchestrator::derive::AddressDeriver::from_account_xpub(TEST_XPUB).unwrap())
}

const JWT_SECRET: &str = "test-jwt-secret";

/// `pay_address` is now read straight from config rather than returned by a payment
/// gateway, so the response must echo exactly this.
const TEST_CUSTODY: &str = "TQwgeRaDt4FSJSsncmFNcbMNTfFpjvjwFX";

/// Plan C 5b landed the daily-headroom check inside `create_and_invoice` — every deposit create
/// now GETs the treasury's `/internal/reserve-status` before ever calling Bitcart. This file's
/// tests are about the create-flow's idempotency/ownership/bounds properties (headroom itself is
/// covered separately in `db_bridge.rs`), so every test here needs a treasury double that just
/// answers with generous headroom and gets out of the way — same wiremock convention this crate
/// already uses for Bitcart coverage in `bitcart_adapter.rs`.
async fn mock_treasury_with_generous_headroom() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/internal/reserve-status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"daily_headroom_clt": 1_000_000_000})))
        .mount(&server)
        .await;
    server
}

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_orch_deposit_api");

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

fn test_config(treasury_url: String) -> OrchConfig {
    OrchConfig {
        http_addr: "0.0.0.0:0".into(),
        database_url: std::env::var("DATABASE_URL").unwrap(),
        jwt_secret: JWT_SECRET.into(),
        allowed_origins: "*".into(),
        treasury_url,
        treasury_initiator_token: "i".into(),
        treasury_readonly_token: "r".into(),
        custody_tron_address: TEST_CUSTODY.into(),
        deposit_account_xpub: TEST_XPUB.into(),
        trongrid_url: "http://localhost:0".to_string(),
        trongrid_api_key: "test-key".to_string(),
        usdt_contract: "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf".to_string(),
        deposit_ttl_minutes: 30,
        min_deposit_usdt: 1_000_000,
        max_deposit_usdt: 50_000_000,
        poll_interval_secs: 30,
        deposit_hot_window_hours: 24,
        // This file never exercises the redemption routes (Plan C T6) — off by default,
        // same as production, and unused otherwise.
        redemptions_enabled: false,
        min_redemption_clt: 1_000_000,
        max_redemption_clt: 50_000_000,
    }
}

#[derive(Serialize)]
struct Claims {
    pk: String,
    exp: usize,
}

fn bearer_for(pk: &str) -> String {
    let claims = Claims { pk: pk.to_string(), exp: (chrono::Utc::now().timestamp() + 3600) as usize };
    let token = encode(&Header::default(), &claims, &EncodingKey::from_secret(JWT_SECRET.as_bytes())).unwrap();
    format!("Bearer {token}")
}

fn router_with(pool: PgPool, config: OrchConfig) -> axum::Router {
    payment_orchestrator::api::router(pool, config, std::sync::Arc::new(payment_orchestrator::derive::AddressDeriver::from_account_xpub(TEST_XPUB).unwrap()))
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Core end-to-end replay property (spec §6, the reason this task exists): the SAME
/// Idempotency-Key + SAME body must return the ORIGINAL status and body on a second call —
/// not a fresh 201. (There is no longer any gateway call to double-invoke — the second
/// half of that original property is now structurally impossible.)
#[tokio::test]
async fn replay_same_key_same_body_returns_original_status_and_body() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri());
    let app = router_with(pool.clone(), config.clone());
    let auth = bearer_for("0xalice");

    let make_request = || {
        Request::builder()
            .method("POST")
            .uri("/api/v1/deposits")
            .header("authorization", auth.clone())
            .header("idempotency-key", "key-replay-1")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"clt_address":"clt1alice","amount_usdt":2000000}"#))
            .unwrap()
    };

    let first = app.clone().oneshot(make_request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::CREATED, "first call must create and return 201");
    let first_body = body_json(first).await;
    // pay_address is DERIVED per intent now, not read from config. Asserted against the row's own
    // derivation_index rather than a literal, so this also proves the address the user was told
    // matches the index the signer will later derive the sweeping key from — a mismatch there is
    // funds nobody can move.
    let (index, stored_address): (i64, String) = sqlx::query_as(
        "SELECT derivation_index, deposit_address FROM deposit_intents WHERE client_key = 'key-replay-1'",
    )
    .fetch_one(&pool)
    .await
    .expect("the intent must carry an index and an address");
    assert_eq!(
        first_body["pay_address"], stored_address,
        "the response must echo the address stored on the row"
    );
    assert_eq!(
        stored_address,
        test_deriver().address_at(u32::try_from(index).unwrap()).unwrap(),
        "stored address must be the one derived at the row's own index"
    );
    assert_ne!(
        first_body["pay_address"], TEST_CUSTODY,
        "must NOT be the shared custody address — that is the sweep destination, not a pay-in address"
    );

    // Second call: identical key, identical body.
    let second = app.oneshot(make_request()).await.unwrap();
    assert_eq!(
        second.status(),
        StatusCode::CREATED,
        "replay must return the ORIGINAL stored status (201), not a hardcoded/different code"
    );
    let second_body = body_json(second).await;
    assert_eq!(second_body, first_body, "replay must return the ORIGINAL stored body verbatim");
}

/// Same Idempotency-Key, DIFFERENT body — the client is confused about what it asked for;
/// must be refused with 409, never silently served the earlier intent's response.
#[tokio::test]
async fn same_key_different_body_returns_409() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri());
    let app = router_with(pool.clone(), config.clone());
    let auth = bearer_for("0xalice");

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .header("authorization", auth.clone())
                .header("idempotency-key", "key-conflict-1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"clt_address":"clt1alice","amount_usdt":2000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    // Same key, different amount_usdt this time.
    let second = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .header("authorization", auth)
                .header("idempotency-key", "key-conflict-1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"clt_address":"clt1alice","amount_usdt":3000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT, "same key + different body must be 409, not a silent replay");
}

/// "Key exists but still processing" (spec §6): a request that finds the row locked by a
/// concurrent in-flight create must get 409 with `Retry-After: 2`, not hang or 500.
///
/// Produced directly at the `deposits::create` layer (T2's own mechanism) rather than by
/// racing two real HTTP requests against a slow adapter — `FOR UPDATE SKIP LOCKED` is a
/// DB-transaction-level condition, and holding a transaction open across an `.await` on a
/// second concurrent axum request is exactly the kind of test-only plumbing that would
/// obscure what's actually being proven: that the ROUTE surfaces `StillProcessing` as 409 +
/// Retry-After correctly. So this test holds the lock with a raw `FOR UPDATE` transaction
/// (simulating "another writer's create() is mid-flight holding this row"), then drives the
/// real HTTP route against it.
#[tokio::test]
async fn retry_while_processing_returns_409_with_retry_after() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri());
    let app = router_with(pool.clone(), config.clone());
    let auth = bearer_for("0xbob");

    // First call creates the row for (user_pk="0xbob", client_key="key-stillproc-1").
    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .header("authorization", auth.clone())
                .header("idempotency-key", "key-stillproc-1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"clt_address":"clt1bob","amount_usdt":2000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    // Simulate a second writer's create() being mid-flight: open a transaction, take the
    // row lock `deposits::create` itself takes (`FOR UPDATE`), and hold it without
    // committing — exactly what a concurrent in-flight request would do while it's
    // between its own lock acquisition and its eventual commit/rollback.
    let mut locking_tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM deposit_intents WHERE user_pk = $1 AND client_key = $2 FOR UPDATE")
        .bind("0xbob")
        .bind("key-stillproc-1")
        .fetch_one(&mut *locking_tx)
        .await
        .unwrap();

    let retried = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .header("authorization", auth)
                .header("idempotency-key", "key-stillproc-1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"clt_address":"clt1bob","amount_usdt":2000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retried.status(), StatusCode::CONFLICT, "row locked by a concurrent create must be 409");
    assert_eq!(
        retried.headers().get("retry-after").map(|v| v.to_str().unwrap()),
        Some("2"),
        "StillProcessing must carry Retry-After: 2 per spec §6"
    );

    locking_tx.rollback().await.unwrap();
}

/// Owner check on `GET /api/v1/deposits/:id`: a valid JWT for a DIFFERENT user_pk than the
/// one that owns the intent must be rejected — not shown someone else's deposit status.
#[tokio::test]
async fn get_deposit_rejects_non_owner() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri());
    let app = router_with(pool.clone(), config.clone());

    let create_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .header("authorization", bearer_for("0xowner"))
                .header("idempotency-key", "key-owner-1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"clt_address":"clt1owner","amount_usdt":2000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let created = body_json(create_res).await;
    let id = created["id"].as_str().unwrap();

    // The rightful owner CAN read it.
    let owner_get = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/deposits/{id}"))
                .header("authorization", bearer_for("0xowner"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(owner_get.status(), StatusCode::OK, "the owner must be able to read their own intent");

    // A different authenticated user must NOT be able to read it.
    let intruder_get = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/deposits/{id}"))
                .header("authorization", bearer_for("0xintruder"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        intruder_get.status(),
        StatusCode::NOT_FOUND,
        "a caller whose pk does not own the intent must be rejected"
    );
}

/// Bounds enforcement reaches the route, not just the underlying `deposits::create` (that's
/// already covered at the module level in `db_deposits.rs::bounds_are_enforced`) — this
/// proves the HTTP layer surfaces it as 400 rather than 500 or a silent pass-through.
#[tokio::test]
async fn out_of_bounds_amount_returns_400() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri());
    let app = router_with(pool.clone(), config.clone());

    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .header("authorization", bearer_for("0xcarol"))
                .header("idempotency-key", "key-bounds-1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"clt_address":"clt1carol","amount_usdt":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "below min_deposit_usdt must be rejected at the route, not 500");
}

/// Missing `Idempotency-Key` header must be rejected outright — every other guarantee in
/// this file depends on that header existing.
#[tokio::test]
async fn missing_idempotency_key_returns_400() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri());
    let app = router_with(pool.clone(), config.clone());

    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .header("authorization", bearer_for("0xdave"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"clt_address":"clt1dave","amount_usdt":2000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

/// Missing/invalid JWT must be rejected before touching the DB or Bitcart at all.
#[tokio::test]
async fn missing_auth_returns_401() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri());
    let app = router_with(pool.clone(), config.clone());

    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .header("idempotency-key", "key-noauth-1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"clt_address":"clt1nobody","amount_usdt":2000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
