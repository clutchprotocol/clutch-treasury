use treasury_service::reconciliation::{judge, Sources};

fn s(onchain: u64, genesis: u64, ledger: i64, custody: i64) -> Sources {
    Sources {
        onchain_supply: onchain,
        genesis_allocation: genesis,
        ledger_liability: ledger,
        custody_reported: custody,
    }
}

#[test]
fn all_four_agree_is_ok() {
    let (status, _) = judge(&s(1_000_000_000_000_000 + 5_000_000, 1_000_000_000_000_000, 5_000_000, 5_000_000));
    assert_eq!(status, "ok");
}

#[test]
fn chain_above_ledger_is_p1_mismatch() {
    // More CLT exists on-chain than the ledger backs — unbacked supply.
    let (status, _) = judge(&s(1_000_000_000_000_000 + 6_000_000, 1_000_000_000_000_000, 5_000_000, 5_000_000));
    assert_eq!(status, "mismatch");
}

#[test]
fn custody_below_liability_is_p1_mismatch() {
    let (status, _) = judge(&s(1_000_000_000_000_000 + 5_000_000, 1_000_000_000_000_000, 5_000_000, 4_999_999));
    assert_eq!(status, "mismatch");
}

#[test]
fn plain_burns_are_benign_drift() {
    // Users burned CLT without redemption: chain < ledger, custody untouched.
    let (status, _) = judge(&s(1_000_000_000_000_000 + 3_000_000, 1_000_000_000_000_000, 5_000_000, 5_000_000));
    assert_eq!(status, "over_backed_drift");
}

#[tokio::test]
async fn mismatch_halts_minting() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("UPDATE breaker_state SET minting_halted = FALSE, halt_reason = NULL")
        .execute(&pool).await.unwrap();
    sqlx::query("TRUNCATE treasury_events, reconciliation_runs, alerts RESTART IDENTITY CASCADE")
        .execute(&pool).await.unwrap();

    // Ledger says 0 liability, chain reports 1 CLT above genesis → mismatch.
    treasury_service::reconciliation::record(
        &pool,
        &Sources {
            onchain_supply: 1_000_000_000_000_001,
            genesis_allocation: 1_000_000_000_000_000,
            ledger_liability: 0,
            custody_reported: 0,
        },
    )
    .await
    .unwrap();

    let (halted, reason): (bool, Option<String>) =
        sqlx::query_as("SELECT minting_halted, halt_reason FROM breaker_state")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(halted);
    assert!(reason.unwrap().contains("reconciliation"));
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alerts WHERE severity = 'p1'")
        .fetch_one(&pool).await.unwrap();
    assert!(n >= 1, "P1 alert row required — a mismatch is an incident, not a metric");
}
