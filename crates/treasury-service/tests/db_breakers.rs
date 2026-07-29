use sqlx::PgPool;
use treasury_service::breakers;
use treasury_service::configuration::AppConfig;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE treasury_events, mint_intents, reconciliation_runs, alerts RESTART IDENTITY CASCADE")
        .execute(&pool).await.unwrap();
    sqlx::query("UPDATE breaker_state SET minting_halted = FALSE, halt_reason = NULL")
        .execute(&pool).await.unwrap();
    pool
}

fn test_config() -> AppConfig {
    AppConfig {
        http_addr: "0.0.0.0:0".into(),
        database_url: std::env::var("DATABASE_URL").unwrap(),
        node_ws_url: "ws://unused".into(),
        chain_id: 2077,
        mint_authority_secret: "x".into(),
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
        trongrid_url: "http://unused".into(),
        trongrid_api_key: "test-trongrid-key".into(),
        custody_tron_address: "TCustodyAddressXXXXXXXXXXXXXXXXXXX".into(),
        usdt_contract: "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".into(),
        deposit_confirmations: 19,
    }
}

async fn seed_ok_reconciliation(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO reconciliation_runs
         (onchain_supply, genesis_allocation, ledger_liability, custody_reported, status)
         VALUES (0, 0, 0, 0, 'ok')",
    ).execute(pool).await.unwrap();
}

/// The backing check (gate 4) reads the LEDGER custody balance, which starts at zero —
/// an empty ledger is legitimately under-backed and gate 4 denies first (see
/// `backing_below_halt_denies_and_trips`). Tests isolating a *different* gate must clear
/// gate 4 first, or they'd pass/fail for gate 4's reason instead of their own.
async fn seed_well_backed_ledger(pool: &PgPool) {
    // Custody comfortably ABOVE liability, not merely equal: the ratio divides by
    // (liability + this mint's amount), so equal balances still land under 10_000 bps.
    treasury_service::ledger::append_event(pool, "mint_executed", 1_000, 0, None, None, "seed").await.unwrap();
    treasury_service::ledger::append_event(pool, "custody_deposit", 0, 2_000, None, None, "seed").await.unwrap();
}

#[tokio::test]
async fn per_tx_cap_denies() {
    let pool = pool().await;
    seed_ok_reconciliation(&pool).await;
    let err = breakers::check_mint(&pool, &test_config(), 50_000_001).await.unwrap_err();
    assert!(err.reason.contains("per-transaction cap"));
}

#[tokio::test]
async fn halted_state_denies_everything() {
    let pool = pool().await;
    seed_ok_reconciliation(&pool).await;
    breakers::manual_halt(&pool, "drill", "alice").await.unwrap();
    let err = breakers::check_mint(&pool, &test_config(), 1).await.unwrap_err();
    assert!(err.reason.contains("halted"));
    // The halt and the resume both leave an audit trail.
    breakers::manual_resume(&pool, "bob").await.unwrap();
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alerts").fetch_one(&pool).await.unwrap();
    assert!(n >= 2, "halt + resume must both write alerts");
}

#[tokio::test]
async fn missing_reconciliation_denies() {
    let pool = pool().await; // no runs seeded
    seed_well_backed_ledger(&pool).await; // isolate gate 5: gate 4 must not intercept first
    let err = breakers::check_mint(&pool, &test_config(), 1).await.unwrap_err();
    assert!(err.reason.contains("reconciliation"));
}

#[tokio::test]
async fn daily_cap_denies() {
    let pool = pool().await;
    seed_ok_reconciliation(&pool).await;
    seed_well_backed_ledger(&pool).await; // isolate gate 3: gate 4 must not intercept first
    let mut cfg = test_config();
    cfg.daily_mint_cap_clt = 100;
    cfg.per_tx_mint_cap_clt = 100; // keep the per-tx gate out of the way
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents (id, beneficiary, amount_clt, status, credit_ref, created_by, approved_by)
         VALUES ($1, 'bob', 60, 'approved', 'ref-daily-cap-1', 'alice', 'carol')",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    // 60 already counted + 50 more exceeds the 100 daily cap, and per-tx cap doesn't fire (50 <= 100).
    let err = breakers::check_mint(&pool, &cfg, 50).await.unwrap_err();
    assert!(err.reason.contains("daily cap"));
}

#[tokio::test]
async fn backing_below_halt_denies_and_trips() {
    let pool = pool().await;
    seed_ok_reconciliation(&pool).await;
    // Liability 100, custody 99 → 9900 bps < 10000.
    treasury_service::ledger::append_event(&pool, "mint_executed", 100, 0, None, None, "seed").await.unwrap();
    treasury_service::ledger::append_event(&pool, "custody_deposit", 0, 99, None, None, "seed").await.unwrap();
    let mut cfg = test_config();
    cfg.custody_stub_balance_usdt = 0; // force ledger custody to be the number used
    let err = breakers::check_mint(&pool, &cfg, 1).await.unwrap_err();
    assert!(err.reason.contains("backing"));
    let (halted,): (bool,) = sqlx::query_as("SELECT minting_halted FROM breaker_state")
        .fetch_one(&pool).await.unwrap();
    assert!(halted, "backing breach must auto-trip the halt");
}

#[tokio::test]
async fn stale_reconciliation_denies() {
    let pool = pool().await;
    seed_well_backed_ledger(&pool).await; // isolate gate 5: gate 4 must not intercept first
    // A run exists, but it's outside the 48h freshness window.
    sqlx::query(
        "INSERT INTO reconciliation_runs
         (run_at, onchain_supply, genesis_allocation, ledger_liability, custody_reported, status)
         VALUES (now() - interval '49 hours', 0, 0, 0, 0, 'ok')",
    ).execute(&pool).await.unwrap();
    let err = breakers::check_mint(&pool, &test_config(), 1).await.unwrap_err();
    assert!(err.reason.contains("reconciliation"));
}

#[tokio::test]
async fn mismatch_reconciliation_denies() {
    let pool = pool().await;
    seed_well_backed_ledger(&pool).await; // isolate gate 5: gate 4 must not intercept first
    sqlx::query(
        "INSERT INTO reconciliation_runs
         (onchain_supply, genesis_allocation, ledger_liability, custody_reported, status)
         VALUES (0, 0, 0, 0, 'mismatch')",
    ).execute(&pool).await.unwrap();
    let err = breakers::check_mint(&pool, &test_config(), 1).await.unwrap_err();
    assert!(err.reason.contains("mismatch"));
}

/// The outbox path re-checks an intent that is already 'approved' — its own amount is
/// already inside the daily-cap sum. Without exclusion this would deny work already
/// authorised; `check_mint_excluding` is the entry point that must not double-count it.
#[tokio::test]
async fn daily_cap_excludes_the_intent_being_processed() {
    let pool = pool().await;
    seed_ok_reconciliation(&pool).await;
    seed_well_backed_ledger(&pool).await; // isolate gate 3: gate 4 must not intercept first
    let mut cfg = test_config();
    cfg.daily_mint_cap_clt = 100;
    cfg.per_tx_mint_cap_clt = 100;
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents (id, beneficiary, amount_clt, status, credit_ref, created_by, approved_by)
         VALUES ($1, 'bob', 100, 'approved', 'ref-daily-cap-2', 'alice', 'carol')",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    // Naive check_mint would see day_total=100 (this intent already counted) + 100 = 200 > 100 and deny.
    assert!(breakers::check_mint(&pool, &cfg, 100).await.is_err());

    // check_mint_excluding removes this intent's own 100 from day_total first: 0 + 100 = 100, not > 100.
    breakers::check_mint_excluding(&pool, &cfg, 100, id).await.unwrap();
}
