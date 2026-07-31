//! Plan C T6: the two redemption proxy routes, exercised through the real router (same
//! convention as `db_deposit_api.rs`) — auth header parsing, JSON extraction, the
//! `redemptions_enabled` gate, address validation, bounds, and the owner check, all against a
//! wiremock standing in for the treasury (same convention `db_bridge.rs` uses).
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
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const JWT_SECRET: &str = "test-jwt-secret";

/// A genuinely base58check-valid Tron mainnet address (the widely-published USDT-TRC20
/// contract address) — independently verified (checksum + 0x41 version byte) rather than
/// invented, so the corruption test below starts from a fixture that's actually valid.
const VALID_TRON_ADDRESS: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_orch_redemptions");

    if !Postgres::database_exists(&url).await.unwrap_or(false) {
        Postgres::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE deposit_intents, alerts, redemption_map RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

fn test_config(treasury_url: String, redemptions_enabled: bool) -> OrchConfig {
    OrchConfig {
        http_addr: "0.0.0.0:0".into(),
        database_url: std::env::var("DATABASE_URL").unwrap(),
        jwt_secret: JWT_SECRET.into(),
        allowed_origins: "*".into(),
        treasury_url,
        treasury_initiator_token: "test-treasury-initiator".into(),
        treasury_readonly_token: "test-treasury-readonly".into(),
        custody_tron_address: "Tunused".into(),
        trongrid_url: "http://localhost:0".to_string(),
        trongrid_api_key: "test-key".to_string(),
        usdt_contract: "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf".to_string(),
        deposit_ttl_minutes: 30,
        min_deposit_usdt: 1_000_000,
        max_deposit_usdt: 50_000_000,
        poll_interval_secs: 30,
        redemptions_enabled,
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

/// `api::router` no longer takes a payment adapter — there is no gateway to inject. The stub that
/// used to be passed here (panicking if invoked, to catch a redemption routed through the deposit
/// path) is gone with it: that wiring mistake is now impossible to express.
fn router_with(pool: PgPool, config: OrchConfig) -> axum::Router {
    payment_orchestrator::api::router(pool, config)
}

async fn body_json_of(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn redemption_intent_response(id: uuid::Uuid, redemption_ref: &str, amount_clt: i64) -> Value {
    json!({
        "id": id,
        "redeemer_address": "whatever-the-request-carried",
        "payout_address": VALID_TRON_ADDRESS,
        "amount_clt": amount_clt,
        "status": "created",
        "redemption_ref": redemption_ref,
        "burn_tx_hash": serde_json::Value::Null,
    })
}

fn post_redemption_request(auth: &str, payout_tron_address: &str, amount_clt: i64) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/redemptions")
        .header("authorization", auth)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"payout_tron_address": payout_tron_address, "amount_clt": amount_clt}).to_string(),
        ))
        .unwrap()
}

/// THE money-safety property (brief requirement #1): `redeemer_address` must come from the JWT,
/// never the request body. Proven on the wire, same discipline `db_bridge.rs` uses for
/// `expected_amount_usdt` — the wiremock matcher only accepts a POST whose `redeemer_address`
/// equals the AUTHENTICATED caller's pk ("0xalice"), even though the request body below smuggles
/// in a DIFFERENT address ("0xattacker-victim-address") under that exact field name. If the
/// handler ever read it from the body, wiremock would 404 the request (no matching mock), the
/// call would come back as `TreasuryRejected`/`TreasuryUnavailable`, and this test would fail —
/// not silently pass.
#[tokio::test]
async fn redeemer_address_comes_from_jwt_never_from_request_body() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = uuid::Uuid::new_v4();

    Mock::given(method("POST"))
        .and(path("/internal/redemption-intents"))
        .and(body_json(json!({
            "redeemer_address": "0xalice",
            "payout_address": VALID_TRON_ADDRESS,
            "amount_clt": 2_000_000,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(redemption_intent_response(treasury_id, "ref-alice", 2_000_000)))
        .expect(1)
        .mount(&server)
        .await;

    let config = test_config(server.uri(), true);
    let app = router_with(pool.clone(), config);

    // The request body has NO redeemer_address field in CreateRedemptionBody at all, but even
    // if a client sends one anyway (as raw JSON, bypassing the struct), it must be ignored:
    // serde drops unknown fields by default, so this proves the field never reaches the
    // handler's logic, let alone gets forwarded to the treasury.
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/redemptions")
        .header("authorization", bearer_for("0xalice"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "payout_tron_address": VALID_TRON_ADDRESS,
                "amount_clt": 2_000_000,
                "redeemer_address": "0xattacker-victim-address",
            })
            .to_string(),
        ))
        .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED, "the request must still succeed — the smuggled field is silently dropped, not rejected");
    let body = body_json_of(res).await;
    assert_eq!(body["redemption_ref"], "ref-alice");

    // wiremock's .expect(1) with the exact redeemer_address="0xalice" match already proves the
    // property; this confirms the mapping row also recorded the JWT's pk as the owner.
    let (user_pk,): (String,) = sqlx::query_as("SELECT user_pk FROM redemption_map WHERE id = $1")
        .bind(uuid::Uuid::parse_str(body["id"].as_str().unwrap()).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(user_pk, "0xalice", "the stored owner must be the JWT's pk, never anything from the body");
}

/// A second caller cannot redeem AS someone else either — same property, different angle: even
/// authenticating as a DIFFERENT pk, the treasury call still carries THAT caller's pk, never one
/// they merely typed into the body.
#[tokio::test]
async fn different_caller_cannot_name_a_different_redeemer() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = uuid::Uuid::new_v4();

    Mock::given(method("POST"))
        .and(path("/internal/redemption-intents"))
        .and(body_json(json!({
            "redeemer_address": "0xbob",
            "payout_address": VALID_TRON_ADDRESS,
            "amount_clt": 3_000_000,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(redemption_intent_response(treasury_id, "ref-bob", 3_000_000)))
        .expect(1)
        .mount(&server)
        .await;

    let config = test_config(server.uri(), true);
    let app = router_with(pool.clone(), config);

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/redemptions")
        .header("authorization", bearer_for("0xbob"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "payout_tron_address": VALID_TRON_ADDRESS,
                "amount_clt": 3_000_000,
                "redeemer_address": "0xalice",
            })
            .to_string(),
        ))
        .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
}

/// Brief requirement #2, THE required case: a shape check ("T…", base58, 34 chars) passes a
/// one-character corruption of a valid address — same length, same alphabet, same leading
/// character. Only a real base58check verification catches it. Proven through the real HTTP
/// route, not just the unit-level `is_valid_tron_address` (that's covered in `redemptions.rs`).
#[tokio::test]
async fn one_character_corrupted_address_is_rejected_despite_passing_shape() {
    let pool = pool().await;
    let server = MockServer::start().await; // no mock mounted — must never be called
    let config = test_config(server.uri(), true);
    let app = router_with(pool, config);

    let mut chars: Vec<char> = VALID_TRON_ADDRESS.chars().collect();
    let idx = 10;
    chars[idx] = if chars[idx] == 'a' { 'b' } else { 'a' };
    let corrupted: String = chars.into_iter().collect();
    assert_eq!(corrupted.len(), VALID_TRON_ADDRESS.len(), "test bug: corruption changed the length");
    assert!(corrupted.starts_with('T'), "test bug: corruption changed the leading character");
    assert_ne!(corrupted, VALID_TRON_ADDRESS, "test bug: corruption didn't change anything");

    let res = app.oneshot(post_redemption_request(&bearer_for("0xcarol"), &corrupted, 2_000_000)).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "checksum-failing address must be rejected even though it passes a shape check");
}

/// A structurally implausible address (wrong length/charset) must also be rejected — the floor
/// below the checksum case.
#[tokio::test]
async fn garbage_address_is_rejected() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let config = test_config(server.uri(), true);
    let app = router_with(pool, config);

    let res = app.oneshot(post_redemption_request(&bearer_for("0xcarol"), "not-a-tron-address", 2_000_000)).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

/// Brief requirement #3: bounds are enforced, consistent with the deposit path's shape (400,
/// not a silent pass-through or a 500).
#[tokio::test]
async fn out_of_bounds_amount_is_rejected() {
    let pool = pool().await;
    let server = MockServer::start().await; // must never be called — bounds fail before any treasury call
    let config = test_config(server.uri(), true);
    let app = router_with(pool, config);

    let too_small = app
        .clone()
        .oneshot(post_redemption_request(&bearer_for("0xdave"), VALID_TRON_ADDRESS, 1))
        .await
        .unwrap();
    assert_eq!(too_small.status(), StatusCode::BAD_REQUEST, "below min_redemption_clt must be rejected");

    let too_large = app
        .oneshot(post_redemption_request(&bearer_for("0xdave"), VALID_TRON_ADDRESS, 999_000_000))
        .await
        .unwrap();
    assert_eq!(too_large.status(), StatusCode::BAD_REQUEST, "above max_redemption_clt must be rejected");
}

/// Brief requirement #4: GET by a non-owner is 404, matching `GET /api/v1/deposits/:id`'s
/// convention — never confirm a resource exists to a caller who isn't allowed to see it.
#[tokio::test]
async fn get_redemption_rejects_non_owner() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = uuid::Uuid::new_v4();

    Mock::given(method("POST"))
        .and(path("/internal/redemption-intents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(redemption_intent_response(treasury_id, "ref-owner", 2_000_000)))
        .mount(&server)
        .await;

    let config = test_config(server.uri(), true);
    let app = router_with(pool.clone(), config);

    let create_res = app
        .clone()
        .oneshot(post_redemption_request(&bearer_for("0xowner"), VALID_TRON_ADDRESS, 2_000_000))
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let created = body_json_of(create_res).await;
    let id = created["id"].as_str().unwrap();

    // The rightful owner CAN read it.
    let owner_get = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/redemptions/{id}"))
                .header("authorization", bearer_for("0xowner"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(owner_get.status(), StatusCode::OK, "the owner must be able to read their own redemption intent");
    let owner_body = body_json_of(owner_get).await;
    assert_eq!(owner_body["redemption_ref"], "ref-owner");

    // A different authenticated user must NOT be able to read it — 404, not 403.
    let intruder_get = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/redemptions/{id}"))
                .header("authorization", bearer_for("0xintruder"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(intruder_get.status(), StatusCode::NOT_FOUND, "a caller whose pk does not own the intent must be rejected with 404, not 403");
}

/// GET must report the treasury's CURRENT status, not the one captured at creation.
///
/// `watcher::confirm_burn` is what advances a redemption, so serving the stored snapshot would show
/// `created` forever — including after the user's CLT was burned and the payout made. That is worse
/// than unhelpful: someone checking on their own money would be told nothing had happened.
#[tokio::test]
async fn get_redemption_reports_live_treasury_status_not_the_creation_snapshot() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = uuid::Uuid::new_v4();

    Mock::given(method("POST"))
        .and(path("/internal/redemption-intents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(redemption_intent_response(treasury_id, "ref-live", 2_000_000)))
        .mount(&server)
        .await;
    // The treasury has since moved on: burn confirmed, payout made.
    Mock::given(method("GET"))
        .and(path(format!("/internal/redemption-intents/{treasury_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": treasury_id,
            "redeemer_address": "0xlive",
            "payout_address": VALID_TRON_ADDRESS,
            "amount_clt": 2_000_000,
            "status": "paid",
            "redemption_ref": "ref-live",
            "burn_tx_hash": "0xburned",
        })))
        .mount(&server)
        .await;

    let config = test_config(server.uri(), true);
    let app = router_with(pool.clone(), config);

    let created = body_json_of(
        app.clone()
            .oneshot(post_redemption_request(&bearer_for("0xlive"), VALID_TRON_ADDRESS, 2_000_000))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(created["status"], "created", "creation returns the status the treasury reported then");
    let id = created["id"].as_str().unwrap();

    // The stored snapshot still says `created`; the response must not.
    let (stored,): (String,) = sqlx::query_as("SELECT status FROM redemption_map WHERE id = $1::uuid")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, "created", "test premise: the stored snapshot is stale");

    let body = body_json_of(
        app.oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/redemptions/{id}"))
                .header("authorization", bearer_for("0xlive"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(body["status"], "paid", "GET must serve the treasury's live status, not the stale snapshot");
    assert_eq!(body["status_live"], true, "and must say the status was read live");
}

/// When the treasury cannot be reached, GET falls back to the stored status and SAYS SO. Reading a
/// status moves no money, so the create path's fail-closed 503 would be the wrong trade here — but
/// a client must be able to tell "nothing has happened yet" from "we could not ask".
#[tokio::test]
async fn get_redemption_falls_back_to_stored_status_and_flags_it_when_treasury_is_down() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let treasury_id = uuid::Uuid::new_v4();

    // Only the POST is mounted — the status GET 404s, standing in for an unreachable treasury.
    Mock::given(method("POST"))
        .and(path("/internal/redemption-intents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(redemption_intent_response(treasury_id, "ref-down", 2_000_000)))
        .mount(&server)
        .await;

    let config = test_config(server.uri(), true);
    let app = router_with(pool.clone(), config);

    let created = body_json_of(
        app.clone()
            .oneshot(post_redemption_request(&bearer_for("0xdown"), VALID_TRON_ADDRESS, 2_000_000))
            .await
            .unwrap(),
    )
    .await;
    let id = created["id"].as_str().unwrap();

    let get_res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/redemptions/{id}"))
                .header("authorization", bearer_for("0xdown"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_res.status(), StatusCode::OK, "an unreachable treasury must not fail the read");
    let body = body_json_of(get_res).await;
    assert_eq!(body["status"], "created", "falls back to the stored snapshot");
    assert_eq!(body["status_live"], false, "and flags that it is NOT live");
}

/// A GET for an id that was never created must also be 404 (not found, distinct from the
/// non-owner case above but must resolve to the same status code).
#[tokio::test]
async fn get_redemption_for_unknown_id_is_404() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let config = test_config(server.uri(), true);
    let app = router_with(pool, config);

    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/redemptions/{}", uuid::Uuid::new_v4()))
                .header("authorization", bearer_for("0xnobody"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

/// Brief requirement #5: both routes 503 while `redemptions_enabled` is false (the default) —
/// the treasury payout rail is still a stub, so this must refuse before ever validating the
/// address or bounds, and before ever calling the treasury (the mock below has no route mounted;
/// a call to it would be a connection to a server with nothing mounted, i.e. NOT this gate).
#[tokio::test]
async fn both_routes_503_while_redemptions_disabled() {
    let pool = pool().await;
    let server = MockServer::start().await; // no mock mounted — must never be called
    let config = test_config(server.uri(), false);
    let app = router_with(pool.clone(), config);

    // POST: even a well-formed, in-bounds, validly-addressed request must 503, not proceed to
    // validation or the treasury call.
    let post_res = app
        .clone()
        .oneshot(post_redemption_request(&bearer_for("0xeve"), VALID_TRON_ADDRESS, 2_000_000))
        .await
        .unwrap();
    assert_eq!(post_res.status(), StatusCode::SERVICE_UNAVAILABLE, "POST must 503 while redemptions_enabled is false");

    // GET: seed a mapping row directly (bypassing the disabled POST) to prove the gate applies
    // independently to GET too, not merely as a side effect of nothing ever having been created.
    let seeded_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO redemption_map (id, user_pk, treasury_intent_id, payout_tron_address, amount_clt, redemption_ref, status)
         VALUES ($1, '0xeve', $2, $3, 2000000, 'ref-seeded', 'created')",
    )
    .bind(seeded_id)
    .bind(uuid::Uuid::new_v4())
    .bind(VALID_TRON_ADDRESS)
    .execute(&pool)
    .await
    .unwrap();

    let get_res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/redemptions/{seeded_id}"))
                .header("authorization", bearer_for("0xeve"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_res.status(), StatusCode::SERVICE_UNAVAILABLE, "GET must ALSO 503 while redemptions_enabled is false, even for an existing, owned row");
}

/// Missing/invalid JWT must still be rejected on the redemption routes — proves auth wasn't
/// accidentally dropped when this task added the disabled-gate ahead of it.
#[tokio::test]
async fn missing_auth_returns_401() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let config = test_config(server.uri(), true);
    let app = router_with(pool, config);

    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/redemptions")
                .header("content-type", "application/json")
                .body(Body::from(json!({"payout_tron_address": VALID_TRON_ADDRESS, "amount_clt": 2_000_000}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

/// A non-2xx treasury response (the treasury answered but refused, e.g. a config/role
/// mismatch) must surface distinctly rather than as a generic 500 — same "Rejected vs
/// Unreachable" distinction `treasury_bridge.rs` makes, applied at the route layer.
#[tokio::test]
async fn treasury_rejection_surfaces_as_bad_gateway_not_500() {
    let pool = pool().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/internal/redemption-intents"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": "forbidden"})))
        .mount(&server)
        .await;

    let config = test_config(server.uri(), true);
    let app = router_with(pool, config);

    let res = app.oneshot(post_redemption_request(&bearer_for("0xfrank"), VALID_TRON_ADDRESS, 2_000_000)).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
}
