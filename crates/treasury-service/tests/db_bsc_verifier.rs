//! BEP-20 top-ups (bsc_verifier.rs): wiremock stands in for the treasury's own BSC node. The same
//! rules as the TronLink direct top-up, on the other chain: the sender must be the beneficiary, the
//! USDT must reach the configured pay-in address, and only what arrived mints.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::matchers::{body_partial_json, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const USER: &str = "0xc8599111f29c1e1e061265b4af93ea1f274ad78a";
const STRANGER: &str = "0x1111111111111111111111111111111111111111";
const PAY_IN: &str = "0x2222222222222222222222222222222222222222";
const USDT: &str = "0x337610d27c682e347c9cd60bd4b3b107c9d34ddd";
const TRANSFER: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const E18: u128 = 1_000_000_000_000_000_000;

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_tre_bsc_verifier");
    if !<sqlx::Postgres as sqlx::migrate::MigrateDatabase>::database_exists(&url).await.unwrap_or(false) {
        <sqlx::Postgres as sqlx::migrate::MigrateDatabase>::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE treasury_events, mint_intents, chain_outbox, reconciliation_runs, alerts, gasfree_accounts RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE breaker_state SET minting_halted = FALSE, halt_reason = NULL").execute(&pool).await.unwrap();
    pool
}

fn test_config(trongrid_url: String) -> treasury_service::configuration::AppConfig {
    treasury_service::configuration::AppConfig {
        http_addr: "0.0.0.0:0".into(),
        metrics_addr: "0.0.0.0:9101".into(),
        database_url: std::env::var("DATABASE_URL").unwrap(),
        node_ws_url: "ws://unused".into(),
        node_peer_ws_urls: String::new(),
        max_node_lag_blocks: 50,
        chain_id: 2077,
        signer_kind: "env".into(),
        mint_authority_secret: "0883ddd3d07303b87c954b0c9383f7b78f45e002520fc03a8adc80595dbf6509".into(),
        azure_tenant_id: String::new(),
        azure_client_id: String::new(),
        azure_client_secret: String::new(),
        azure_vault_url: String::new(),
        azure_key_name: String::new(),
        azure_key_version: String::new(),
        mint_authorities: String::new(),
        mint_threshold: 0,
        initiator_token: "i".into(),
        approver_token: "a".into(),
        readonly_token: "r".into(),
        daily_mint_cap_clt: 500_000_000,
        daily_payout_cap_clt: 500_000_000,
        per_tx_mint_cap_clt: 50_000_000,
        backing_target_bps: 10_050,
        backing_halt_bps: 10_000,
        genesis_allocation: 1_000_000_000_000_000,
        confirmations: 2,
        outbox_poll_ms: 2000,
        reconciliation_interval_secs: 86400,
        trongrid_url,
        trongrid_api_key: "test-trongrid-key".into(),
        custody_tron_address: "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".into(),
        payout_float_address: "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY".into(),
        usdt_contract: "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".into(),
        deposit_confirmations: 19,
        deposit_match_window_hours: 24,
        sweep_threshold_usdt: 100_000_000,
        sweep_max_age_hours: 168,
        sweep_min_usdt: 0,
        redemption_fee_usdt: 0,
        signer_url: "http://unused".into(),
        signer_token: "s".into(),
        gasfree: None,
        service_fee: None,
        direct_topups_enabled: false,
        bsc: None,
    }
}

fn bsc_config(server: &MockServer) -> treasury_service::configuration::AppConfig {
    let mut config = test_config("http://unused".into());
    config.bsc = Some(bsc::Settings {
        chain_id: 97,
        rpc_url: server.uri(),
        usdt_contract: USDT.into(),
        usdt_decimals: 18,
        pay_in_address: PAY_IN.into(),
        confirmations: 15,
    });
    config
}

fn tx(n: u8) -> String {
    format!("0x{}", format!("{n:02x}").repeat(32))
}

fn word(address: &str) -> String {
    format!("0x{:0>64}", &address[2..])
}

fn transfer_log(tx_hash: &str, contract: &str, from: &str, to: &str, value: u128) -> Value {
    json!({"address": contract, "topics": [TRANSFER, word(from), word(to)], "data": format!("0x{value:064x}"),
           "transactionHash": tx_hash, "logIndex": "0x0", "blockNumber": "0x64", "removed": false})
}

async fn rpc(server: &MockServer, method_name: &str, result: Value) {
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": method_name})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1, "result": result})))
        .mount(server)
        .await;
}

/// A node on `chain`, at `head`, holding one receipt mined in block 100.
async fn node(server: &MockServer, chain: u64, head: u64, status: &str, logs: Vec<Value>) {
    rpc(server, "eth_chainId", json!(format!("0x{chain:x}"))).await;
    rpc(server, "eth_blockNumber", json!(format!("0x{head:x}"))).await;
    rpc(server, "eth_getTransactionReceipt", json!({"status": status, "blockNumber": "0x64", "logs": logs})).await;
}

async fn seed(pool: &PgPool, from: &str, amount: i64, tx_hash: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents
            (id, beneficiary, amount_clt, credit_ref, created_by, client_ref, deposit_tx_id, expected_amount_usdt, direct_from, network)
         VALUES ($1, $2, $3, $4, 'orchestrator', $5, $6, $3, $7, 'bsc')",
    )
    .bind(id)
    .bind(USER)
    .bind(amount)
    .bind(format!("ref-{id}"))
    .bind(format!("client-{id}"))
    .bind(tx_hash)
    .bind(from)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn status_of(pool: &PgPool, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM mint_intents WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}

async fn amount_of(pool: &PgPool, id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT amount_clt FROM mint_intents WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn a_top_up_from_the_users_own_wallet_mints_what_arrived() {
    let pool = pool().await;
    let server = MockServer::start().await;
    // 25.0000009 USDT: the part below a micro-USDT stays in the reserve.
    node(&server, 97, 115, "0x1", vec![transfer_log(&tx(1), USDT, USER, PAY_IN, 25 * E18 + 900_000_000_000)]).await;
    let id = seed(&pool, USER, 25_000_000, &tx(1)).await;

    treasury_service::tron_verifier::verify_once(&pool, &bsc_config(&server)).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "approved");
    assert_eq!(amount_of(&pool, id).await, 25_000_000);
    let (usdt, description): (i64, String) =
        sqlx::query_as("SELECT amount_usdt, description FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_deposit'")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((usdt, description.as_str()), (25_000_000, "BSC-verified deposit"));
    let outbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chain_outbox WHERE intent_id = $1").bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(outbox, 1);
}

#[tokio::test]
async fn never_more_than_arrived_and_only_the_users_own_usdt_to_the_pay_in_address_counts() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let fake_token = "0x9999999999999999999999999999999999999999";
    node(
        &server,
        97,
        200,
        "0x1",
        vec![
            transfer_log(&tx(2), USDT, USER, PAY_IN, 10 * E18),
            transfer_log(&tx(2), fake_token, USER, PAY_IN, 1000 * E18),
            transfer_log(&tx(2), USDT, STRANGER, PAY_IN, 1000 * E18),
            transfer_log(&tx(2), USDT, USER, STRANGER, 1000 * E18),
        ],
    )
    .await;
    let id = seed(&pool, USER, 30_000_000, &tx(2)).await;

    treasury_service::tron_verifier::verify_once(&pool, &bsc_config(&server)).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "rejected", "10 arrived against 30 proposed");
}

#[tokio::test]
async fn usdt_sent_by_someone_else_is_rejected() {
    let pool = pool().await;
    let server = MockServer::start().await;
    node(&server, 97, 200, "0x1", vec![transfer_log(&tx(3), USDT, STRANGER, PAY_IN, 25 * E18)]).await;
    let id = seed(&pool, USER, 25_000_000, &tx(3)).await;

    treasury_service::tron_verifier::verify_once(&pool, &bsc_config(&server)).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "rejected");
}

#[tokio::test]
async fn a_sender_that_is_not_the_beneficiary_is_rejected_before_any_read() {
    let pool = pool().await;
    let server = MockServer::start().await;
    node(&server, 97, 200, "0x1", vec![transfer_log(&tx(4), USDT, STRANGER, PAY_IN, 25 * E18)]).await;
    // The CHECK refuses this shape, so it is written past it to prove the verifier holds on its own.
    sqlx::query("ALTER TABLE mint_intents DROP CONSTRAINT bsc_shape").execute(&pool).await.unwrap();
    let id = seed(&pool, STRANGER, 25_000_000, &tx(4)).await;

    treasury_service::tron_verifier::verify_once(&pool, &bsc_config(&server)).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "rejected");
    sqlx::query("DELETE FROM mint_intents WHERE id = $1").bind(id).execute(&pool).await.unwrap();
    sqlx::query(
        "ALTER TABLE mint_intents ADD CONSTRAINT bsc_shape
         CHECK (network <> 'bsc'
                OR (direct_from IS NOT NULL AND lower(direct_from) = lower(beneficiary) AND deposit_address IS NULL)
                OR (direct_from IS NULL AND deposit_address IS NOT NULL AND derivation_index IS NOT NULL))",
    )
    .execute(&pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_reverted_transaction_is_rejected() {
    let pool = pool().await;
    let server = MockServer::start().await;
    node(&server, 97, 200, "0x0", vec![]).await;
    let id = seed(&pool, USER, 25_000_000, &tx(5)).await;

    treasury_service::tron_verifier::verify_once(&pool, &bsc_config(&server)).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "rejected");
}

#[tokio::test]
async fn shallow_unknown_wrong_chain_or_off_decides_nothing() {
    let pool = pool().await;

    // 14 blocks deep, 15 wanted.
    let shallow = MockServer::start().await;
    node(&shallow, 97, 113, "0x1", vec![transfer_log(&tx(6), USDT, USER, PAY_IN, 25 * E18)]).await;
    let id = seed(&pool, USER, 25_000_000, &tx(6)).await;
    treasury_service::tron_verifier::verify_once(&pool, &bsc_config(&shallow)).await.unwrap();
    assert_eq!(status_of(&pool, id).await, "created", "not yet confirmed");

    // A node of another chain is never the witness, whatever it says.
    let other_chain = MockServer::start().await;
    node(&other_chain, 56, 500, "0x1", vec![transfer_log(&tx(6), USDT, USER, PAY_IN, 25 * E18)]).await;
    treasury_service::tron_verifier::verify_once(&pool, &bsc_config(&other_chain)).await.unwrap();
    assert_eq!(status_of(&pool, id).await, "created", "wrong chain");

    // Not mined yet.
    let unknown = MockServer::start().await;
    rpc(&unknown, "eth_chainId", json!("0x61")).await;
    rpc(&unknown, "eth_getTransactionReceipt", Value::Null).await;
    treasury_service::tron_verifier::verify_once(&pool, &bsc_config(&unknown)).await.unwrap();
    assert_eq!(status_of(&pool, id).await, "created", "no receipt");

    // BSC off here.
    let mut off = bsc_config(&shallow);
    off.bsc = None;
    treasury_service::tron_verifier::verify_once(&pool, &off).await.unwrap();
    assert_eq!(status_of(&pool, id).await, "created", "off");
}

async fn post(config: treasury_service::configuration::AppConfig, pool: &PgPool, body: Value) -> StatusCode {
    let mut config = config;
    config.initiator_token = "test-initiator".to_string();
    treasury_service::api::router(pool.clone(), config)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/internal/mint-intents")
                .header("authorization", "Bearer test-initiator")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn the_create_route_takes_a_bsc_top_up_only_in_its_one_shape() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let upper = format!("0x{}", tx(7)[2..].to_ascii_uppercase());
    let good = json!({
        "beneficiary": USER, "amount_clt": 25_000_000, "expected_amount_usdt": 25_000_000, "network": "bsc",
        "client_ref": "bsc-1", "deposit_tx_id": upper, "direct_from": USER.to_ascii_uppercase().replace("0X", "0x"),
    });
    let with = |k: &str, v: Value| {
        let mut b = good.clone();
        b[k] = v;
        b
    };
    let mut off = bsc_config(&server);
    off.bsc = None;
    assert_eq!(post(off, &pool, good.clone()).await, StatusCode::BAD_REQUEST, "off");
    assert_eq!(post(bsc_config(&server), &pool, with("direct_from", json!(STRANGER))).await, StatusCode::BAD_REQUEST);
    assert_eq!(post(bsc_config(&server), &pool, with("direct_from", Value::Null)).await, StatusCode::BAD_REQUEST);
    assert_eq!(post(bsc_config(&server), &pool, with("deposit_tx_id", json!("ab".repeat(32)))).await, StatusCode::BAD_REQUEST, "a TRON id");
    assert_eq!(post(bsc_config(&server), &pool, with("deposit_address", json!(PAY_IN))).await, StatusCode::BAD_REQUEST);
    assert_eq!(post(bsc_config(&server), &pool, with("network", json!("eth"))).await, StatusCode::BAD_REQUEST);
    assert_eq!(post(bsc_config(&server), &pool, good).await, StatusCode::CREATED);
    let (network, tx_id, from): (String, String, String) =
        sqlx::query_as("SELECT network, deposit_tx_id, direct_from FROM mint_intents WHERE client_ref = 'bsc-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((network.as_str(), tx_id, from.as_str()), ("bsc", tx(7), USER), "stored lowercase");
}

#[tokio::test]
async fn the_reserve_counts_the_pay_in_balance_floored_and_refuses_another_chain() {
    let server = MockServer::start().await;
    rpc(&server, "eth_chainId", json!("0x61")).await;
    rpc(&server, "eth_call", json!(format!("0x{:064x}", 40 * E18 + 123))).await;
    let config = bsc_config(&server);
    let settings = config.bsc.as_ref().unwrap();
    assert_eq!(treasury_service::bsc_verifier::reserve_micro(settings, &[]).await.unwrap(), 40_000_000);

    let wrong = MockServer::start().await;
    rpc(&wrong, "eth_chainId", json!("0x38")).await;
    rpc(&wrong, "eth_call", json!(format!("0x{:064x}", 40 * E18))).await;
    let config = bsc_config(&wrong);
    assert!(treasury_service::bsc_verifier::reserve_micro(config.bsc.as_ref().unwrap(), &[]).await.is_err());
}

// ---- Deposits at the user's own BSC address (spec 2026-10-29).

/// A user's BSC deposit address, at index 3.
const DEPOSIT: &str = "0x8e9f2f0d4b7cd8aa9ba8b7d5e2f1f9b6f1c0e2a3";

async fn signer_answering(address: &str) -> MockServer {
    let signer = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path("/internal/bsc/addresses/3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"index": 3, "address": address})))
        .mount(&signer)
        .await;
    signer
}

async fn seed_at_address(pool: &PgPool, address: &str, amount: i64, tx_hash: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents
            (id, beneficiary, amount_clt, credit_ref, created_by, client_ref, deposit_tx_id, expected_amount_usdt,
             deposit_address, derivation_index, network)
         VALUES ($1, $2, $3, $4, 'orchestrator', $5, $6, $3, $7, 3, 'bsc')",
    )
    .bind(id)
    .bind(USER)
    .bind(amount)
    .bind(format!("ref-{id}"))
    .bind(format!("client-{id}"))
    .bind(tx_hash)
    .bind(address)
    .execute(pool)
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn a_deposit_at_the_users_own_address_mints_what_arrived_from_anyone() {
    let pool = pool().await;
    let server = MockServer::start().await;
    // An exchange's withdrawal, in two Transfers, plus a Transfer to someone else that must not count.
    node(
        &server,
        97,
        200,
        "0x1",
        vec![
            transfer_log(&tx(20), USDT, STRANGER, DEPOSIT, 7 * E18),
            transfer_log(&tx(20), USDT, STRANGER, DEPOSIT, 3 * E18),
            transfer_log(&tx(20), USDT, STRANGER, PAY_IN, 100 * E18),
        ],
    )
    .await;
    let signer = signer_answering(DEPOSIT).await;
    let mut config = bsc_config(&server);
    config.signer_url = signer.uri();
    let id = seed_at_address(&pool, DEPOSIT, 10_000_000, &tx(20)).await;

    treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "approved");
    assert_eq!(amount_of(&pool, id).await, 10_000_000);
}

#[tokio::test]
async fn an_address_the_signer_does_not_derive_at_that_index_is_rejected_and_an_unreachable_signer_decides_nothing() {
    let pool = pool().await;
    let server = MockServer::start().await;
    node(&server, 97, 200, "0x1", vec![transfer_log(&tx(21), USDT, STRANGER, DEPOSIT, 10 * E18)]).await;

    let mut config = bsc_config(&server);
    config.signer_url = "http://127.0.0.1:9".into();
    let id = seed_at_address(&pool, DEPOSIT, 10_000_000, &tx(21)).await;
    treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(status_of(&pool, id).await, "created", "signer unreachable");

    let signer = signer_answering(STRANGER).await;
    config.signer_url = signer.uri();
    treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(status_of(&pool, id).await, "rejected");
}

#[tokio::test]
async fn the_create_route_takes_a_deposit_at_an_address_with_its_index() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let good = json!({
        "beneficiary": USER, "amount_clt": 10_000_000, "expected_amount_usdt": 10_000_000, "network": "bsc",
        "client_ref": "bsc-addr-1", "deposit_tx_id": tx(22), "deposit_address": DEPOSIT.to_ascii_uppercase().replace("0X", "0x"),
        "derivation_index": 3,
    });
    let with = |k: &str, v: Value| {
        let mut b = good.clone();
        b[k] = v;
        b
    };
    assert_eq!(post(bsc_config(&server), &pool, with("derivation_index", Value::Null)).await, StatusCode::BAD_REQUEST);
    assert_eq!(post(bsc_config(&server), &pool, with("deposit_address", json!(PAY_IN))).await, StatusCode::BAD_REQUEST);
    assert_eq!(post(bsc_config(&server), &pool, with("direct_from", json!(USER))).await, StatusCode::BAD_REQUEST, "one shape at a time");
    assert_eq!(post(bsc_config(&server), &pool, good).await, StatusCode::CREATED);
    let address: String = sqlx::query_scalar("SELECT deposit_address FROM mint_intents WHERE client_ref = 'bsc-addr-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(address, DEPOSIT, "stored lowercase");
}

#[tokio::test]
async fn the_reserve_adds_each_unswept_deposit_address_once() {
    let server = MockServer::start().await;
    rpc(&server, "eth_chainId", json!("0x61")).await;
    rpc(&server, "eth_call", json!(format!("0x{:064x}", 4 * E18))).await;
    let config = bsc_config(&server);
    let unswept = vec![DEPOSIT.to_string(), STRANGER.to_string()];
    assert_eq!(treasury_service::bsc_verifier::reserve_micro(config.bsc.as_ref().unwrap(), &unswept).await.unwrap(), 12_000_000);
}

#[tokio::test]
async fn the_sweeper_sweeps_credited_bsc_addresses_and_records_it() {
    let pool = pool().await;
    let id = seed_at_address(&pool, DEPOSIT, 10_000_000, &tx(23)).await;
    sqlx::query("UPDATE mint_intents SET status = 'credited' WHERE id = $1").bind(id).execute(&pool).await.unwrap();
    let signer = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/internal/bsc/sweep"))
        .and(body_partial_json(json!({"index": 3})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "swept", "tx_id": tx(24), "amount_usdt": 10_000_000})))
        .expect(1)
        .mount(&signer)
        .await;
    let mut config = test_config("http://unused".into());
    config.signer_url = signer.uri();
    let http = treasury_service::sweeper::HttpSigner { http: reqwest::Client::new(), base_url: signer.uri(), token: "s".into() };
    let client = treasury_service::tron_verifier::TronClient::new("http://unused".into(), String::new());
    treasury_service::sweeper::sweep_once(&pool, &config, &client, &http).await;
    let swept: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT swept_at FROM mint_intents WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
    assert!(swept.is_some());
}
