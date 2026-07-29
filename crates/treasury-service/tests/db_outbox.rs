use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use tower::ServiceExt;
use treasury_service::intents::{approve_mint_intent, create_mint_intent};

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
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
    let intent = create_mint_intent(&pool, "0x4444444444444444444444444444444444444444", 1_000_000, "alice").await.unwrap();
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
        chain_id: 2077,
        mint_authority_secret: "0883ddd3d07303b87c954b0c9383f7b78f45e002520fc03a8adc80595dbf6509".into(),
        initiator_token: "i".into(),
        approver_token: "a".into(),
        readonly_token: "r".into(),
        daily_mint_cap_clt: 500_000_000,
        per_tx_mint_cap_clt: 50_000_000,
        backing_target_bps: 10_050,
        backing_halt_bps: 10_000,
        custody_stub_balance_usdt: 1_000_000_000,
        genesis_allocation: 1_000_000_000_000_000,
        confirmations: 2,
        outbox_poll_ms: 2000,
        reconciliation_interval_secs: 86400,
    }
}
