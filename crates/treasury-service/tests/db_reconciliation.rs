use sqlx::PgPool;
use treasury_service::reconciliation::{judge, record, Sources};

/// A real, base58check-valid derived address — the same fixture value db_sweeper.rs's ADDRS uses.
const SHARED_ADDR: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";

/// Own database per test binary: --test-threads=1 only serialises tests WITHIN a binary, and cargo
/// runs binaries in parallel while every pool() here TRUNCATEs shared tables.
async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_tre_recon");
    if !<sqlx::Postgres as sqlx::migrate::MigrateDatabase>::database_exists(&url).await.unwrap_or(false) {
        <sqlx::Postgres as sqlx::migrate::MigrateDatabase>::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE treasury_events, mint_intents, reconciliation_runs, alerts RESTART IDENTITY CASCADE")
        .execute(&pool).await.unwrap();
    sqlx::query("UPDATE breaker_state SET minting_halted = FALSE, halt_reason = NULL")
        .execute(&pool).await.unwrap();
    pool
}

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
    // Each test BINARY gets its own database. --test-threads=1 only serialises tests WITHIN a
    // binary; cargo runs binaries in PARALLEL, and every pool() here TRUNCATEs shared tables —
    // so binaries were wiping each other mid-test. That produced a ~1-in-6 flake that moved
    // between tests run to run (see progress.md).
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_tre_reconciliation");
    if !<sqlx::Postgres as sqlx::migrate::MigrateDatabase>::database_exists(&url).await.unwrap_or(false) {
        <sqlx::Postgres as sqlx::migrate::MigrateDatabase>::create_database(&url).await.unwrap();
    }
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

/// The regression that halted stage.
///
/// The real numbers from 2026-08-27: a $10 liability against a $1,004.12 reserve — over a hundred
/// times over-collateralized — judged a `mismatch` and tripped the breaker, because
/// `custody_reported` was a config stub that defaulted to 0 while the figure proving the system was
/// healthy sat in the same JSON, deliberately excluded from the decision.
#[test]
fn an_over_collateralized_reserve_is_not_a_mismatch() {
    let s = treasury_service::reconciliation::Sources {
        onchain_supply: 1_000_000_000_000_000,
        genesis_allocation: 1_000_000_000_000_000,
        ledger_liability: 10_000_000,      // $10
        custody_reported: 1_004_124_930,   // $1,004.12 actually held on chain
    };
    let (status, _) = treasury_service::reconciliation::judge(&s);
    assert_ne!(status, "mismatch", "a reserve 100x the liability must never halt minting");
}

/// The peg makes the two figures directly comparable, and the whole comparison is meaningless if
/// that ever stops being true: USDT carries 6 decimals and 1 USD is 1,000,000 CLT, so one
/// micro-USDT is exactly one CLT. A units slip here reads as a 10^6 under- or over-backing.
#[test]
fn one_micro_usdt_is_one_clt() {
    let exactly_backed = treasury_service::reconciliation::Sources {
        onchain_supply: 1_000_000_000_000_000,
        genesis_allocation: 1_000_000_000_000_000,
        ledger_liability: 5_000_000,     // $5 of CLT
        custody_reported: 5_000_000,     // $5 of USDT, same integer
    };
    let (status, _) = treasury_service::reconciliation::judge(&exactly_backed);
    assert_ne!(status, "mismatch", "exact 1:1 backing is the healthy case, not a halt");

    let one_short = treasury_service::reconciliation::Sources {
        ledger_liability: 5_000_001,
        ..exactly_backed
    };
    let (status, _) = treasury_service::reconciliation::judge(&one_short);
    assert_eq!(status, "mismatch", "one micro-unit of under-backing must still halt");
}

/// Under-issuance is not "over-backed drift" in the benign sense once it persists.
///
/// A mint moves chain supply and ledger liability together, and so does a burn, so the two figures
/// are equal in a settled system. A gap means one side recorded something the other has not — fine
/// for a few seconds. The same gap on consecutive runs means minted CLT the ledger counted never
/// reached the chain, or was destroyed after it did.
///
/// Stage lost a $10 mint to a chain reset and this reported it as benign for a day.
#[tokio::test]
async fn a_persistent_under_issuance_escalates_to_p1() {
    let pool = pool().await;

    // Ledger says $1,000 issued; the chain holds $990. The missing $10 is a lost mint.
    let s = Sources {
        onchain_supply: 1_000_000_990_000_000,
        genesis_allocation: 1_000_000_000_000_000,
        ledger_liability: 1_000_000_000,
        custody_reported: 1_004_124_930,
    };

    // First run: nothing to compare against, so treated as a possible timing race.
    let status = record(&pool, &s).await.unwrap();
    assert_eq!(status, "over_backed_drift");
    let p1s: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM alerts WHERE source = 'reconciliation' AND severity = 'p1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(p1s, 0, "a first-seen gap must not page anyone");

    // Second run, same gap: no longer explicable as a race.
    let status = record(&pool, &s).await.unwrap();
    assert_eq!(status, "over_backed_drift", "still drift, but no longer benign");
    let p1s: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM alerts WHERE source = 'reconciliation' AND severity = 'p1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(p1s, 1, "a gap that survives a run is a lost mint, not a race");

    let msg: String = sqlx::query_scalar(
        "SELECT message FROM alerts WHERE severity = 'p1' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(msg.contains("10000000"), "the alert must name the shortfall: {msg}");
}

/// A reversed mint leaves liability matching what the chain actually holds.
///
/// treasury_events is append-only, so the original mint_executed row stays — it is a true record of
/// something that did happen. mint_reversed records the chain losing it afterwards, which is what
/// the ledger previously had no way to express: correcting it with burn_redeemed would assert a
/// redemption, and a redemption owes a payout.
#[tokio::test]
async fn a_reversed_mint_subtracts_from_liability() {
    let pool = pool().await;
    let intent = uuid::Uuid::new_v4();

    sqlx::query(
        "INSERT INTO treasury_events (kind, amount_clt, intent_id, description)
         VALUES ('mint_executed', 10000000, $1, 'original')",
    )
    .bind(intent)
    .execute(&pool)
    .await
    .unwrap();

    let before: i64 = sqlx::query_scalar("SELECT clt_liability FROM ledger_balances")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, 10_000_000);

    sqlx::query(
        "INSERT INTO treasury_events (kind, amount_clt, intent_id, description)
         VALUES ('mint_reversed', 10000000, $1, 'chain reset destroyed it')",
    )
    .bind(intent)
    .execute(&pool)
    .await
    .unwrap();

    let after: i64 = sqlx::query_scalar("SELECT clt_liability FROM ledger_balances")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, 0, "the view must subtract mint_reversed");

    // The original survives: reversal records history, it does not erase it.
    let originals: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM treasury_events WHERE intent_id = $1 AND kind = 'mint_executed'",
    )
    .bind(intent)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(originals, 1, "append-only: the original mint must remain on record");

    // uq_events_intent_kind must stop a second reversal halving liability again.
    let twice = sqlx::query(
        "INSERT INTO treasury_events (kind, amount_clt, intent_id, description)
         VALUES ('mint_reversed', 10000000, $1, 'again')",
    )
    .bind(intent)
    .execute(&pool)
    .await;
    assert!(twice.is_err(), "an intent must not be reversible twice");
}

/// Inserts a `credited`, unswept mint intent at `address` evidenced by `tx_id`. Names every NOT NULL
/// column mint_intents has with no default (`beneficiary`, `amount_clt`, `credit_ref`, `created_by`)
/// plus `approved_by`, since four_eyes requires a distinct approver past `created`. `swept_at` is
/// omitted so it takes its NULL default — the "still unswept" state this test needs.
///
/// `derivation_index` is nullable and unconstrained (treasury-service migration 0006) — `None`
/// leaves it NULL, `Some` sets it, so this one helper can also seed the per-intent-era shape
/// `legacy_unswept_addresses_are_still_counted` needs.
async fn seed_unswept_mint(pool: &PgPool, address: &str, tx_id: &str, derivation_index: Option<i64>) {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents (id, beneficiary, amount_clt, status, credit_ref, created_by, approved_by,
                                    deposit_tx_id, deposit_address, derivation_index)
         VALUES ($1, 'TBeneficiary1111111111111111111111', 1000000, 'credited', $2, 'orchestrator', 'tron-verifier',
                 $3, $4, $5)",
    )
    .bind(id)
    .bind(format!("ref-{id}"))
    .bind(tx_id)
    .bind(address)
    .bind(derivation_index)
    .execute(pool)
    .await
    .unwrap();
}

/// Per-user deposit addresses make this the NORMAL case: one user, several deposits, one address.
/// Counting per row inflates the reserve, and an over-backed reading licenses mints that nothing
/// backs — strictly worse than under-counting, which only halts minting.
#[tokio::test]
async fn one_address_on_two_unswept_rows_is_counted_once() {
    let pool = pool().await;
    seed_unswept_mint(&pool, SHARED_ADDR, "tx-a", None).await;
    seed_unswept_mint(&pool, SHARED_ADDR, "tx-b", None).await;

    let addrs = treasury_service::reconciliation::unswept_addresses(&pool).await.unwrap();

    assert_eq!(addrs, vec![SHARED_ADDR.to_string()], "one address, counted once");
}

/// The design's §5 promised this test and nothing wrote it: `unswept_addresses`' predicate is
/// independent of `derivation_index` and of how the address was issued. A non-NULL
/// `derivation_index` is NOT the legacy shape — permanent per-user rows carry one too (since R17) —
/// so it is not what distinguishes this row from the ones above; it is simply a per-intent-era row,
/// seeded to pin that any unswept row's address is counted the same way, legacy or permanent, because
/// the function's WHERE clause never mentions the column at all.
#[tokio::test]
async fn legacy_unswept_addresses_are_still_counted() {
    let pool = pool().await;
    seed_unswept_mint(&pool, SHARED_ADDR, "tx-legacy", Some(42)).await;

    let addrs = treasury_service::reconciliation::unswept_addresses(&pool).await.unwrap();

    assert_eq!(addrs, vec![SHARED_ADDR.to_string()], "a per-intent-era row must still be counted");
}
