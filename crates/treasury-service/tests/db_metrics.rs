//! The metrics scrape reads Postgres, so the thing worth testing is that it reports what the
//! database actually holds — not that it renders. A metric that silently stops tracking its
//! source is worse than no metric, because someone writes an alert against it.

use sqlx::PgPool;
use treasury_service::intents::{approve_mint_intent, create_mint_intent};

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_tre_metrics");
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

/// Zero must be reported as zero, not as a missing series. `GROUP BY` returns nothing for a status
/// with no rows, so an alert written against `needs_manual > 0` would never evaluate on an empty
/// database and would look healthy for the wrong reason.
#[tokio::test]
async fn a_status_with_no_rows_still_reports_zero() {
    let pool = pool().await;
    let body = treasury_service::metrics::render(&pool).await;

    assert!(body.contains("clutch_treasury_up 1"));
    assert!(
        body.contains(r#"clutch_treasury_mint_intents{status="needs_manual"} 0"#),
        "an empty status must read 0, not vanish:\n{body}"
    );
    assert!(body.contains(r#"clutch_treasury_alerts_total{severity="p1"} 0"#));
    assert!(body.contains("clutch_treasury_minting_halted 0"));
}

/// The numbers track the database. Seeds one intent of each interesting shape and checks the
/// scrape moves with them — including the unswept-address count, which is the one an operator
/// watches to see that sweeping has not quietly stopped.
#[tokio::test]
async fn the_scrape_reports_what_the_database_holds() {
    let pool = pool().await;

    let credited = create_mint_intent(
        &pool, "0x4444444444444444444444444444444444444444", 1_000_000, "orchestrator",
        Some("metrics-a"), Some(&"aa".repeat(32)), Some(1_000_000),
        Some("TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".into()), Some(3),
    ).await.unwrap();
    approve_mint_intent(&pool, credited.id, "bob").await.unwrap();

    sqlx::query("UPDATE mint_intents SET status = 'needs_manual' WHERE id = $1")
        .bind(credited.id).execute(&pool).await.unwrap();
    treasury_service::ledger::alert(&pool, "p1", "test", "something needs a human").await;
    sqlx::query(
        "INSERT INTO reconciliation_runs (onchain_supply, genesis_allocation, ledger_liability, custody_reported, status)
         VALUES (0, 0, 0, 0, 'mismatch')",
    ).execute(&pool).await.unwrap();

    let body = treasury_service::metrics::render(&pool).await;

    assert!(body.contains(r#"clutch_treasury_mint_intents{status="needs_manual"} 1"#), "{body}");
    assert!(body.contains(r#"clutch_treasury_alerts_total{severity="p1"} 1"#), "{body}");
    assert!(body.contains(r#"clutch_treasury_reconciliation_status{status="mismatch"} 1"#), "{body}");
    assert!(body.contains(r#"clutch_treasury_reconciliation_status{status="ok"} 0"#), "{body}");
    assert!(body.contains("clutch_treasury_reconciliation_age_seconds "), "{body}");

    // A needs_manual deposit is still unswept money at a real address, and the metric must say so
    // for the same reason reconciliation counts it.
    assert!(body.contains("clutch_treasury_unswept_deposit_addresses 1"), "{body}");

    sqlx::query("UPDATE breaker_state SET minting_halted = TRUE, halt_reason = 'test'")
        .execute(&pool).await.unwrap();
    let body = treasury_service::metrics::render(&pool).await;
    assert!(body.contains("clutch_treasury_minting_halted 1"), "{body}");
}
