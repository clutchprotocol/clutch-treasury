//! The two public deposit routes, exercised through the real router (auth header parsing, JSON
//! extraction, handler → `addresses::address_for_user`/`mark_hot`). Task 6 retired the
//! amount-bearing, idempotency-keyed create flow this file used to cover — and `db_deposits.rs`,
//! which covered its module functions directly, along with it. What remains: a POST that hands
//! back a stable address, and an owner-checked GET on an existing intent.
//!
//! Same shared-database convention as the other `db_*.rs` files: a sibling `_orchestrator`
//! database (sqlx's `_sqlx_migrations` bookkeeping table has no configurable name, so two
//! crates' migrators would corrupt each other's history on ONE shared DB).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use jsonwebtoken::{encode, EncodingKey, Header};
use payment_orchestrator::configuration::OrchConfig;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Account xpub for the canonical public BIP39 all-"abandon" test mnemonic (m/44'/195'/0').
/// Public test material; never holds funds.
const TEST_XPUB: &str = "xpub6D1AabNHCupeiLM65ZR9UStMhJ1vCpyV4XbZdyhMZBiJXALQtmn9p42VTQckoHVn8WNqS7dqnJokZHAHcHGoaQgmv8D45oNUKx6DZMNZBCd";

const JWT_SECRET: &str = "test-jwt-secret";

/// `pay_address` is now read straight from config rather than returned by a payment
/// gateway, so the response must echo exactly this.
const TEST_CUSTODY: &str = "TQwgeRaDt4FSJSsncmFNcbMNTfFpjvjwFX";

/// Unused by the deposit route itself since Task 6 — the address handler never calls the
/// treasury at all, headroom is checked later, at mint time (`treasury_bridge.rs`), not at
/// address-issuance time. Kept anyway: `test_config` still requires a `treasury_url`, and
/// standing up a double that answers cleanly is simpler than special-casing an unused one.
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

/// Seeds a `deposit_intents` row directly. The create-flow that used to seed one through the API
/// is gone (Task 6 — POST now hands back an address, not an intent), and this test is only about
/// the GET owner check, not about how a row comes to exist.
async fn seed_intent(pool: &PgPool, user_pk: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deposit_intents (id, user_pk, clt_address, amount_usdt, amount_clt, client_key, expires_at)
         VALUES ($1, $2, 'clt1owner', 2000000, 2000000, $3, now() + interval '30 minutes')",
    )
    .bind(id)
    .bind(user_pk)
    .bind(format!("key-{id}"))
    .execute(pool)
    .await
    .unwrap();
    id
}

/// The user asks where to send, not how much they promise to send. Two calls must give the same
/// address, or money sent to the first arrives somewhere nothing watches.
#[tokio::test]
async fn the_deposit_endpoint_returns_a_stable_address_and_needs_no_amount() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri());
    let app = router_with(pool.clone(), config);

    let post = |app: axum::Router| async move {
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/deposits")
            .header("authorization", bearer_for("0xuser-a"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"clt_address":"0xclt-a"}"#))
            .unwrap();
        body_json(app.oneshot(req).await.unwrap()).await
    };

    let first = post(app.clone()).await;
    let second = post(app).await;

    assert_eq!(first["address"], second["address"]);
    assert!(first["address"].as_str().unwrap().starts_with('T'));
    assert!(first.get("amount_usdt").is_none(), "no amount is asked for or echoed");

    let hot_until: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT hot_until FROM deposit_addresses WHERE user_pk = $1")
            .bind("0xuser-a")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        hot_until.map_or(false, |t| t > chrono::Utc::now()),
        "the route must mark the address hot"
    );
}

/// Owner check on `GET /api/v1/deposits/:id`: a valid JWT for a DIFFERENT user_pk than the
/// one that owns the intent must be rejected — not shown someone else's deposit status.
#[tokio::test]
async fn get_deposit_rejects_non_owner() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri());
    let app = router_with(pool.clone(), config.clone());

    let id = seed_intent(&pool, "0xowner").await;

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

/// Missing/invalid JWT must be rejected before touching the DB at all.
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
                .header("content-type", "application/json")
                .body(Body::from(r#"{"clt_address":"clt1nobody"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
