use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use tower::ServiceExt;
use treasury_service::intents::{approve_mint_intent, create_mint_intent};

async fn pool() -> PgPool {
    // Each test BINARY gets its own database. --test-threads=1 only serialises tests WITHIN a
    // binary; cargo runs binaries in PARALLEL, and every pool() here TRUNCATEs shared tables —
    // so binaries were wiping each other mid-test. That produced a ~1-in-6 flake that moved
    // between tests run to run (see progress.md).
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_tre_outbox");
    if !<sqlx::Postgres as sqlx::migrate::MigrateDatabase>::database_exists(&url).await.unwrap_or(false) {
        <sqlx::Postgres as sqlx::migrate::MigrateDatabase>::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE treasury_events, mint_intents, chain_outbox, reconciliation_runs, alerts RESTART IDENTITY CASCADE")
        .execute(&pool).await.unwrap();
    sqlx::query("UPDATE breaker_state SET minting_halted = FALSE, halt_reason = NULL")
        .execute(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn watcher_credit_is_idempotent() {
    let pool = pool().await;
    let intent = create_mint_intent(&pool, "0x4444444444444444444444444444444444444444", 1_000_000, "alice", None, None, None, None, None).await.unwrap();
    approve_mint_intent(&pool, intent.id, "bob").await.unwrap();

    // Simulate the watcher seeing the tx twice (reorg replay / crash-restart).
    for _ in 0..2 {
        treasury_service::watcher::credit_mint(&pool, intent.id, 1_000_000, "0xdeadbeef").await.unwrap();
    }
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM treasury_events WHERE intent_id = $1 AND kind = 'mint_executed'",
    ).bind(intent.id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1, "double-processing must not double-count liability");

    let (status,): (String,) = sqlx::query_as("SELECT status FROM mint_intents WHERE id = $1")
        .bind(intent.id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "credited");
}

/// The API-level half of four-eyes (the DB CHECK is tested at the module level in
/// db_ledger.rs::four_eyes_enforced_in_db). This proves the route itself — bearer auth,
/// role check, handler → module fn wiring — rejects the initiator approving their own intent,
/// not just that the underlying function does.
#[tokio::test]
async fn approve_route_rejects_initiators_own_token() {
    let pool = pool().await;
    let mut config = test_config();
    config.initiator_token = "alice-initiator-token".to_string();
    config.approver_token = "bob-approver-token".to_string();
    let app = treasury_service::api::router(pool.clone(), config);

    // Create as the initiator.
    let create_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/internal/mint-intents")
                .header("authorization", "Bearer alice-initiator-token")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"beneficiary":"0x4444444444444444444444444444444444444444","amount_clt":1000000}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(create_res.into_body(), usize::MAX).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let id = created["id"].as_str().unwrap();

    // Same bearer token (the initiator's) tries to approve its own intent — must be rejected,
    // never falling through to "only a different token can approve" being merely a UI nicety.
    let approve_res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/internal/mint-intents/{id}/approve"))
                .header("authorization", "Bearer alice-initiator-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        approve_res.status(),
        StatusCode::FORBIDDEN,
        "initiator-role token must never satisfy the approver check, regardless of who created the intent"
    );
}

fn test_config() -> treasury_service::configuration::AppConfig {
    treasury_service::configuration::AppConfig {
        http_addr: "0.0.0.0:0".into(),
        database_url: std::env::var("DATABASE_URL").unwrap(),
        node_ws_url: "ws://unused".into(),
        node_peer_ws_urls: String::new(),
        max_node_lag_blocks: 50,
        chain_id: 2077,
        mint_authority_secret: "0883ddd3d07303b87c954b0c9383f7b78f45e002520fc03a8adc80595dbf6509".into(),
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
        trongrid_url: "http://unused".into(),
        trongrid_api_key: "test-trongrid-key".into(),
        custody_tron_address: "TCustodyAddressXXXXXXXXXXXXXXXXXXX".into(),
        payout_float_address: "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY".into(),
        usdt_contract: "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".into(),
        deposit_confirmations: 19,
        deposit_match_window_hours: 24,
        sweep_threshold_usdt: 100_000_000,
        sweep_max_age_hours: 168,
        signer_url: "http://unused".into(),
        signer_token: "s".into(),
    }
}

/// A deposit-backed intent (`client_ref` set — only the orchestrator sets it, and only for a
/// verified on-chain deposit) that the breaker denies at submission time must PARK, not burn an
/// attempt. The USDT is real and already sitting at a derived address, so failing the intent after
/// ten tight-cap passes strands it: a `failed` row leaves the reserve sum and the sweeper skips it.
/// A hand-created intent has no such backing and takes the ordinary fail-or-backoff path. Both are
/// denied by the same cap in the same pass, so the only difference between them is `client_ref`.
#[tokio::test]
async fn a_deposit_backed_intent_denied_by_the_breaker_parks_instead_of_failing() {
    let pool = pool().await;
    let deposit_tx = "aa".repeat(32);
    let backed = create_mint_intent(
        &pool, "0x4444444444444444444444444444444444444444", 1_000_000, "orchestrator",
        Some("deposit-ref-1"), Some(deposit_tx.as_str()), Some(1_000_000),
        Some("TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".into()), Some(7),
    ).await.unwrap();
    let manual = create_mint_intent(
        &pool, "0x5555555555555555555555555555555555555555", 1_000_000, "alice", None, None, None, None, None,
    ).await.unwrap();
    approve_mint_intent(&pool, backed.id, "bob").await.unwrap();
    approve_mint_intent(&pool, manual.id, "bob").await.unwrap();

    // The cap tightens AFTER approval — the real shape of the problem: approval passed, then the
    // window closed before the outbox reached the row. No peers, so the sync check is skipped and
    // the node is never contacted; the denial happens before any submission.
    let mut cfg = test_config();
    cfg.per_tx_mint_cap_clt = 1_000;
    let node = clutch_chain::node_client::NodeClient::new("ws://unused".into());
    let signer = clutch_chain::signer::EnvKeySigner::from_secret_hex(&cfg.mint_authority_secret).unwrap();
    let processed = treasury_service::outbox::drain_once(&pool, &node, &[], &signer, &cfg).await.unwrap();
    assert_eq!(processed, 0, "nothing may be submitted while denied");

    let (status, attempts, parked_for): (String, i32, f64) = sqlx::query_as(
        "SELECT status, attempts, EXTRACT(EPOCH FROM (next_attempt_at - now()))::float8
         FROM chain_outbox WHERE intent_id = $1",
    ).bind(backed.id).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), attempts), ("pending", 0), "deposit-backed: parked, no attempt consumed");
    assert!(parked_for > 3000.0, "parked about an hour out, got {parked_for}s");

    let (status, attempts): (String, i32) = sqlx::query_as(
        "SELECT status, attempts FROM chain_outbox WHERE intent_id = $1",
    ).bind(manual.id).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), attempts), ("pending", 1), "hand-created: ordinary backoff, one attempt burned");

    let (intent_status,): (String,) = sqlx::query_as("SELECT status FROM mint_intents WHERE id = $1")
        .bind(backed.id).fetch_one(&pool).await.unwrap();
    assert_eq!(intent_status, "approved", "a parked deposit stays approved, and inside the reserve sum");
}

/// Over the per-transaction cap, no retry can ever pass: the cap is a property of the amount, not
/// of the moment. Such an intent must go straight to `needs_manual` rather than burn ten attempts
/// and end `failed` — a `failed` deposit-backed row leaves the reserve sum and the sweeper skips
/// it, which is how a real 1,000 USDT deposit was stranded on 2026-09-03.
#[tokio::test]
async fn an_over_cap_intent_goes_to_needs_manual_instead_of_retrying() {
    let pool = pool().await;
    let mut cfg = test_config();
    cfg.per_tx_mint_cap_clt = 1_000;

    let intent = create_mint_intent(
        &pool,
        "0x4444444444444444444444444444444444444444",
        1_000_000,
        "orchestrator",
        Some("over-cap-ref"),
        Some(&"bb".repeat(32)),
        Some(1_000_000),
        Some("TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".into()),
        Some(9),
    )
    .await
    .unwrap();
    approve_mint_intent(&pool, intent.id, "bob").await.unwrap();

    let node = clutch_chain::node_client::NodeClient::new("ws://unused".into());
    let signer =
        clutch_chain::signer::EnvKeySigner::from_secret_hex(&cfg.mint_authority_secret).unwrap();
    let processed = treasury_service::outbox::drain_once(&pool, &node, &[], &signer, &cfg)
        .await
        .unwrap();
    assert_eq!(processed, 0);

    let (status,): (String,) = sqlx::query_as("SELECT status FROM mint_intents WHERE id = $1")
        .bind(intent.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "needs_manual", "over the per-tx cap is not a retryable condition");

    let (ob_status, attempts): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM chain_outbox WHERE intent_id = $1")
            .bind(intent.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((ob_status.as_str(), attempts), ("failed", 0), "closed, not retried");

    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM alerts WHERE severity = 'p1' AND source = 'outbox'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1, "a human has to be told");

    // Still inside the reserve sum: the USDT is at the address and nothing has swept it.
    let addrs = treasury_service::reconciliation::unswept_addresses(&pool).await.unwrap();
    assert!(
        addrs.contains(&"TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".to_string()),
        "a needs_manual deposit's money is still there and must still be counted"
    );

    // And it no longer hogs the daily-cap budget it was consuming while `approved`.
    let (day_total,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(amount_clt), 0)::BIGINT FROM mint_intents
         WHERE status IN ('approved','submitted','credited')
           AND created_at > now() - interval '24 hours'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(day_total, 0, "a parked intent must not spend other intents' daily budget");
}

/// The way out: a human raises the cap and approves again. The intent must be approvable from
/// `needs_manual`, and its closed outbox row must reopen with a clean slate rather than collide
/// with the UNIQUE(intent_id) insert or resume at nine spent attempts.
#[tokio::test]
async fn raising_the_cap_and_re_approving_releases_a_needs_manual_intent() {
    let pool = pool().await;
    let mut cfg = test_config();
    cfg.per_tx_mint_cap_clt = 1_000;

    let intent = create_mint_intent(
        &pool,
        "0x4444444444444444444444444444444444444444",
        1_000_000,
        "orchestrator",
        Some("released-ref"),
        Some(&"cc".repeat(32)),
        Some(1_000_000),
        Some("TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".into()),
        Some(11),
    )
    .await
    .unwrap();
    approve_mint_intent(&pool, intent.id, "bob").await.unwrap();

    let node = clutch_chain::node_client::NodeClient::new("ws://unused".into());
    let signer =
        clutch_chain::signer::EnvKeySigner::from_secret_hex(&cfg.mint_authority_secret).unwrap();
    treasury_service::outbox::drain_once(&pool, &node, &[], &signer, &cfg).await.unwrap();

    // The operator raises the cap and approves the same intent again.
    let released = approve_mint_intent(&pool, intent.id, "bob").await.unwrap();
    assert_eq!(released.status, "approved");

    let (ob_status, attempts, err): (String, i32, Option<String>) = sqlx::query_as(
        "SELECT status, attempts, last_error FROM chain_outbox WHERE intent_id = $1",
    )
    .bind(intent.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((ob_status.as_str(), attempts), ("pending", 0), "reopened with a clean slate");
    assert!(err.is_none(), "the old cap denial must not linger as this row's error");
}
