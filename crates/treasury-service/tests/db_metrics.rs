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
    sqlx::query("TRUNCATE treasury_events, mint_intents, chain_outbox, reconciliation_runs, alerts, redemption_intents, gasfree_accounts RESTART IDENTITY CASCADE")
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

/// Spec §5: a stuck relay "pages after a threshold". A count cannot see one deposit or one
/// redemption stuck for a day; an age can. Both read 0 when nothing waits, and a plain-address
/// deposit, which waits for the sweep threshold by design, does not count toward the GasFree age.
#[tokio::test]
async fn the_stall_ages_follow_the_oldest_waiting_row() {
    let pool = pool().await;
    let body = treasury_service::metrics::render(&pool).await;
    assert!(body.contains("clutch_treasury_oldest_unswept_gasfree_seconds 0\n"), "{body}");
    assert!(body.contains("clutch_treasury_oldest_unpaid_redemption_seconds 0\n"), "{body}");

    sqlx::query(
        "INSERT INTO gasfree_accounts (derivation_index, gasfree_address, owner_address)
         VALUES (3, 'TGasFreeAccountOf3', 'TOwnerOf3')",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (address, index, hours) in [("TGasFreeAccountOf3", 3_i64, 2_i32), ("TPlainAddressOf4", 4, 30)] {
        sqlx::query(
            "INSERT INTO mint_intents
                (id, beneficiary, amount_clt, credit_ref, created_by, approved_by, client_ref,
                 expected_amount_usdt, deposit_address, derivation_index, status, verified_at)
             VALUES ($1, 'TBene', 1000000, $2, 'orchestrator', 'tron-verifier', $3, 1000000, $4, $5,
                     'credited', now() - make_interval(hours => $6))",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(format!("ref-{address}"))
        .bind(format!("client-{address}"))
        .bind(address)
        .bind(index)
        .bind(hours)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO redemption_intents (id, redeemer_address, payout_address, amount_clt, payout_amount_usdt, status, redemption_ref, created_at)
         VALUES ($1, '0xaaaa000000000000000000000000000000000009', 'TRedeemer', 5000000, 5000000, 'payout_pending', $2,
                 now() - interval '3 hours')",
    )
    .bind(uuid::Uuid::new_v4())
    .bind("a".repeat(64))
    .execute(&pool)
    .await
    .unwrap();

    let body = treasury_service::metrics::render(&pool).await;
    let gauge = |name: &str| -> i64 {
        body.lines()
            .find_map(|l| l.strip_prefix(name).and_then(|rest| rest.strip_prefix(' ')))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("{name} missing:\n{body}"))
    };
    let gasfree = gauge("clutch_treasury_oldest_unswept_gasfree_seconds");
    assert!((7_100..=7_300).contains(&gasfree), "the GasFree deposit, two hours old, not the older plain one: {gasfree}");
    let unpaid = gauge("clutch_treasury_oldest_unpaid_redemption_seconds");
    assert!((10_700..=10_900).contains(&unpaid), "three hours: {unpaid}");
}
