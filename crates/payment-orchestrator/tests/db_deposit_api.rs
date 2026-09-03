//! The public deposit routes, exercised through the real router (auth header parsing, JSON
//! extraction, handler → `addresses::address_for_user`/`mark_hot`). Task 6 retired the
//! amount-bearing, idempotency-keyed create flow this file used to cover — and `db_deposits.rs`,
//! which covered its module functions directly, along with it. What remains: a POST that hands
//! back a stable address, an owner-checked GET on an existing intent, and a GET that lists the
//! caller's own recent deposits.
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

/// Address-shaped (`0x` + 40 hex) token for tests that go through the route — the beneficiary is
/// now the JWT's own `pk`, so a successful POST requires an address-shaped one. Mixed-case so
/// `the_deposit_endpoint_returns_a_stable_address_and_needs_no_amount` also proves the stored
/// value was normalised to lowercase, not just accepted.
const USER_A: &str = "0x00000000000000000000000000000000000000A1";

/// A second address-shaped user, used only by the list endpoint's isolation test. Must differ
/// from `USER_A` so seeding both and checking which rows come back actually exercises the `WHERE
/// user_pk` clause, not just that a query ran.
const USER_B: &str = "0x00000000000000000000000000000000000000B2";

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
    sqlx::query("TRUNCATE deposit_intents, deposit_addresses RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

fn test_config(treasury_url: String, permanent_deposit_addresses_enabled: bool) -> OrchConfig {
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
        permanent_deposit_addresses_enabled,
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

/// Seeds a `deposit_intents` row directly, like `seed_intent` above, but with explicit control
/// over the columns the list endpoint's own tests turn on: `amount_usdt`, `received_usdt`,
/// `tron_tx_id` and `created_at` (so ordering and the twenty-row cap can be exercised with rows of
/// a known age), plus `status` (so the expired-legacy-intent exclusion can be exercised) rather
/// than relying on the table's own `DEFAULT 'created'`. Every NOT-NULL column without a default is
/// still covered; `client_key` is still derived from the fresh id so `(user_pk, client_key)` stays
/// unique across repeated calls for the same user.
async fn seed_deposit(
    pool: &PgPool,
    user_pk: &str,
    amount_usdt: i64,
    received_usdt: Option<i64>,
    tron_tx_id: &str,
    created_at: chrono::DateTime<chrono::Utc>,
    status: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deposit_intents
            (id, user_pk, clt_address, amount_usdt, amount_clt, client_key, expires_at,
             received_usdt, tron_tx_id, created_at, status)
         VALUES ($1, $2, 'clt1owner', $3, $4, $5, now() + interval '30 minutes', $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(user_pk)
    .bind(amount_usdt)
    .bind(amount_usdt)
    .bind(format!("key-{id}"))
    .bind(received_usdt)
    .bind(tron_tx_id)
    .bind(created_at)
    .bind(status)
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
    let config = test_config(treasury.uri(), true);
    let app = router_with(pool.clone(), config);

    let post = |app: axum::Router| async move {
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/deposits")
            .header("authorization", bearer_for(USER_A))
            .body(Body::empty())
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
            .bind(USER_A)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        hot_until.map_or(false, |t| t > chrono::Utc::now()),
        "the route must mark the address hot"
    );

    let stored_clt_address: String =
        sqlx::query_scalar("SELECT clt_address FROM deposit_addresses WHERE user_pk = $1")
            .bind(USER_A)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored_clt_address, "0x00000000000000000000000000000000000000a1",
        "the beneficiary must be the authenticated identity, normalised to lowercase"
    );
}

/// Regression test for the foot-gun the review flagged: a body's `clt_address` must be silently
/// ignored, not honoured — the beneficiary is the caller's own authenticated identity. Must fail
/// if a body extractor is ever reintroduced and wired up to `address_for_user`.
#[tokio::test]
async fn the_beneficiary_is_the_authenticated_identity_not_the_body() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri(), true);
    let app = router_with(pool.clone(), config);

    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .header("authorization", bearer_for(USER_A))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"clt_address":"0x00000000000000000000000000000000000000ff"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let stored_clt_address: String =
        sqlx::query_scalar("SELECT clt_address FROM deposit_addresses WHERE user_pk = $1")
            .bind(USER_A)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored_clt_address, "0x00000000000000000000000000000000000000a1",
        "the stored beneficiary must be the authenticated identity, not whatever the body sent"
    );
}

/// A public-key-shaped token (130 hex chars — what the JWT `pk` claim held before the demo app
/// was fixed to send an address) must be refused, not silently normalized into an account no key
/// can spend.
#[tokio::test]
async fn a_public_key_token_is_refused() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri(), true);
    let app = router_with(pool.clone(), config);

    let pubkey = format!("04{}", "ab".repeat(64));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .header("authorization", bearer_for(&pubkey))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM deposit_addresses WHERE user_pk = $1")
        .bind(&pubkey)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "a refused token must not create a deposit_addresses row");
}

/// Owner check on `GET /api/v1/deposits/:id`: a valid JWT for a DIFFERENT user_pk than the
/// one that owns the intent must be rejected — not shown someone else's deposit status.
#[tokio::test]
async fn get_deposit_rejects_non_owner() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri(), true);
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
    let config = test_config(treasury.uri(), true);
    let app = router_with(pool.clone(), config.clone());

    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

/// Task 8: `permanent_deposit_addresses_enabled` gates this route the same way
/// `redemptions_enabled` gates the redemption routes (see
/// `db_redemptions.rs::both_routes_503_while_redemptions_disabled`) — 503 before authentication
/// even runs. Proven with TWO requests: a valid bearer token 503s (necessary, but both
/// gate-then-auth and auth-then-gate would also produce this), and a request with NO
/// `Authorization` header ALSO 503s rather than 401ing — the only case where the two orderings
/// diverge, which is what actually proves the gate runs first.
///
/// Uses its own pk rather than `USER_A`, to stay independent of whatever other tests do with that
/// identity, and the gate fires before `canonical_clt_address` ever runs, so an address-shaped
/// token isn't needed either.
#[tokio::test]
async fn deposit_route_503s_while_disabled_even_with_valid_auth() {
    const USER_DISABLED: &str = "0xflag-disabled-test-user";
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri(), false);
    let app = router_with(pool.clone(), config);

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .header("authorization", bearer_for(USER_DISABLED))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "must 503 while permanent_deposit_addresses_enabled is false, even with a valid bearer token"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM deposit_addresses WHERE user_pk = $1")
        .bind(USER_DISABLED)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "a disabled route must not create a deposit_addresses row");

    // THE case that actually distinguishes gate-then-auth from auth-then-gate: no Authorization
    // header at all. If auth ran first this would be 401; the gate must still answer 503.
    let no_auth_res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/deposits")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        no_auth_res.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "must 503 even with NO Authorization header — proves the gate runs before auth, not just before a valid auth check"
    );
}

/// The load-bearing test: seeds deposits for two different users and checks that USER_A's list
/// contains only USER_A's own rows — and that USER_B's `tron_tx_id` appears nowhere at all in the
/// response body, not merely absent from a field we happen to inspect. Fails if
/// `recent_for_user`'s `WHERE user_pk = $1` clause is ever dropped.
#[tokio::test]
async fn the_list_returns_only_the_callers_own_deposits() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri(), true);
    let app = router_with(pool.clone(), config);

    let now = chrono::Utc::now();
    seed_deposit(&pool, USER_A, 1_000_000, None, "tx-user-a-first", now, "created").await;
    seed_deposit(&pool, USER_A, 2_000_000, None, "tx-user-a-second", now, "created").await;
    seed_deposit(&pool, USER_B, 9_000_000, None, "tx-user-b-secret", now, "created").await;

    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/deposits")
                .header("authorization", bearer_for(USER_A))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body = body_json(res).await;
    assert_eq!(status, StatusCode::OK);

    assert!(
        !body.to_string().contains("tx-user-b-secret"),
        "the other user's tron_tx_id must appear nowhere in the response body"
    );

    let rows = body["deposits"].as_array().expect("deposits must be an array");
    assert_eq!(rows.len(), 2, "only USER_A's own two deposits, none of USER_B's");
    let tx_ids: Vec<&str> = rows.iter().map(|r| r["tron_tx_id"].as_str().unwrap()).collect();
    assert!(tx_ids.contains(&"tx-user-a-first"));
    assert!(tx_ids.contains(&"tx-user-a-second"));
}

/// Seeds twenty-one rows with distinct `created_at` values and checks both ends of the ordering:
/// the newest row must come first, and the oldest of the twenty-one must be dropped entirely, not
/// merely pushed past a slice that still contains it.
#[tokio::test]
async fn the_list_is_newest_first_and_capped_at_twenty() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri(), true);
    let app = router_with(pool.clone(), config);

    let base = chrono::Utc::now();
    for i in 0i64..21 {
        // i = 0 is the oldest row, i = 20 the newest.
        let tx_id = format!("tx-{i:02}");
        seed_deposit(&pool, USER_A, 1_000_000, None, &tx_id, base + chrono::Duration::seconds(i), "created").await;
    }

    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/deposits")
                .header("authorization", bearer_for(USER_A))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body = body_json(res).await;
    assert_eq!(status, StatusCode::OK);

    let rows = body["deposits"].as_array().expect("deposits must be an array");
    assert_eq!(rows.len(), 20, "must cap at twenty even though twenty-one rows exist");
    assert_eq!(rows[0]["tron_tx_id"].as_str().unwrap(), "tx-20", "the first row must be the newest");
    assert!(
        rows.iter().all(|r| r["tron_tx_id"].as_str().unwrap() != "tx-00"),
        "the oldest row must be excluded by the LIMIT, not merely sorted last"
    );
}

/// `received_usdt` is what actually arrived; `amount_usdt` alone is only what was once asked for.
/// The response's `amount_usdt` must report the former whenever it is set, AND must still fall
/// back to `amount_usdt` when `received_usdt` is NULL — a live state, not a hypothetical one: the
/// legacy per-intent loop leaves rows with `received_usdt = NULL` until `set_received_usdt` runs,
/// and `recent_for_user` has no status filter, so such rows come back from this endpoint too. Pin
/// both directions, or a fallback mis-edited to `.unwrap_or(0)`/`.unwrap_or_default()` would
/// silently zero every not-yet-settled deposit and nothing here would fail. Rows are looked up by
/// `tron_tx_id` rather than by position, since ordering is not what this test is about.
#[tokio::test]
async fn the_list_reports_what_arrived_not_what_was_asked_for() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri(), true);
    let app = router_with(pool.clone(), config);

    seed_deposit(&pool, USER_A, 20_000_000, Some(25_000_000), "tx-overpaid", chrono::Utc::now(), "created").await;
    seed_deposit(&pool, USER_A, 30_000_000, None, "tx-unsettled", chrono::Utc::now(), "created").await;

    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/deposits")
                .header("authorization", bearer_for(USER_A))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body = body_json(res).await;
    assert_eq!(status, StatusCode::OK);

    let rows = body["deposits"].as_array().expect("deposits must be an array");
    assert_eq!(rows.len(), 2);
    let find = |tx_id: &str| {
        rows.iter()
            .find(|r| r["tron_tx_id"].as_str() == Some(tx_id))
            .unwrap_or_else(|| panic!("no row with tron_tx_id {tx_id}"))
    };
    assert_eq!(
        find("tx-overpaid")["amount_usdt"].as_i64().unwrap(),
        25_000_000,
        "amount_usdt must report what arrived (received_usdt) when it is set"
    );
    assert_eq!(
        find("tx-unsettled")["amount_usdt"].as_i64().unwrap(),
        30_000_000,
        "amount_usdt must fall back to amount_usdt when received_usdt is NULL, not silently zero it"
    );
}

/// A user with no deposits gets a normal 200 and an empty list — never a 404, which would read as
/// "we don't know who you are" rather than "you have no history yet".
#[tokio::test]
async fn an_empty_list_is_two_hundred_not_a_miss() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri(), true);
    let app = router_with(pool.clone(), config);

    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/deposits")
                .header("authorization", bearer_for(USER_A))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body = body_json(res).await;
    assert_eq!(status, StatusCode::OK, "no deposits is not an error");
    assert_eq!(body, json!({"deposits": []}), "an empty list, not a missing field or null");
}

/// `permanent_deposit_addresses_enabled` gates this route exactly like it gates the POST (see
/// `deposit_route_503s_while_disabled_even_with_valid_auth`): 503 before authentication even runs.
/// Proven with two requests — a valid bearer token 503s, and a request with NO `Authorization`
/// header ALSO 503s rather than 401ing, which is the only case that actually distinguishes the
/// gate running first from auth running first.
#[tokio::test]
async fn the_list_is_gated_by_the_rollout_flag() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri(), false);
    let app = router_with(pool.clone(), config);

    let with_valid_token = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/deposits")
                .header("authorization", bearer_for(USER_A))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        with_valid_token.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "must 503 while permanent_deposit_addresses_enabled is false, even with a valid bearer token"
    );

    let with_no_auth_header = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/deposits")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        with_no_auth_header.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "must 503 even with NO Authorization header — proves the gate runs before auth, not just before a valid auth check"
    );
}

/// `expired` rows are pre-permanent-address legacy intents — invoices nobody ever paid, not
/// deposits that happened — and must never come back here. Fails if `recent_for_user`'s `status <>
/// 'expired'` clause is ever dropped, whether by an edit to the query or by moving the filter into
/// the UI (which the design deliberately rejects: with `LIMIT 20`, a client-side filter would
/// still spend the cap on rows the user is never shown).
#[tokio::test]
async fn expired_legacy_intents_are_not_listed() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri(), true);
    let app = router_with(pool.clone(), config);

    seed_deposit(&pool, USER_A, 5_000_000, None, "tx-expired-legacy", chrono::Utc::now(), "expired").await;
    seed_deposit(&pool, USER_A, 6_000_000, None, "tx-real-deposit", chrono::Utc::now(), "created").await;

    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/deposits")
                .header("authorization", bearer_for(USER_A))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body = body_json(res).await;
    assert_eq!(status, StatusCode::OK);

    assert!(
        !body.to_string().contains("tx-expired-legacy"),
        "an expired legacy intent's tron_tx_id must appear nowhere in the response body"
    );

    let rows = body["deposits"].as_array().expect("deposits must be an array");
    assert_eq!(rows.len(), 1, "only the real deposit — the expired legacy intent must be excluded");
    assert_eq!(rows[0]["tron_tx_id"].as_str().unwrap(), "tx-real-deposit");
}
