use payment_orchestrator::configuration::OrchConfig;
use payment_orchestrator::deposits::{self, CreateOutcome};
use serde_json::json;
use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};
use uuid::Uuid;

/// Account xpub for the canonical public BIP39 all-"abandon" test mnemonic (m/44'/195'/0').
/// Public test material; never holds funds.
const TEST_XPUB: &str = "xpub6D1AabNHCupeiLM65ZR9UStMhJ1vCpyV4XbZdyhMZBiJXALQtmn9p42VTQckoHVn8WNqS7dqnJokZHAHcHGoaQgmv8D45oNUKx6DZMNZBCd";

/// `&'static` so call sites can pass it without creating a temporary that is dropped while
/// borrowed, and so the xpub is parsed once per test binary rather than per call.
fn test_deriver() -> &'static payment_orchestrator::derive::AddressDeriver {
    static D: std::sync::OnceLock<payment_orchestrator::derive::AddressDeriver> = std::sync::OnceLock::new();
    D.get_or_init(|| payment_orchestrator::derive::AddressDeriver::from_account_xpub(TEST_XPUB).unwrap())
}

/// docker-compose.test.yml points every crate's DATABASE_URL at ONE shared database
/// (`treasury_test`) for simplicity — real dev/prod already gives each service its own
/// database (.env.example: treasury on 5433/treasury, orchestrator on 5434/orchestrator).
/// sqlx's migrator hardcodes a single unqualified `_sqlx_migrations` table with no
/// configurable name (sqlx-postgres 0.8.6 migrate.rs) — two crates' independent
/// `sqlx::migrate!` calls against the SAME database therefore corrupt each other's
/// migration history (VersionMismatch/VersionMissing) regardless of using separate
/// migrations directories. Deriving a sibling database name here restores the real
/// per-service isolation without touching the shared compose file or treasury-service.
async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    // Swap the last path segment (the database name) for a sibling name — the URL shape
    // is fixed (postgres://user:pass@host:port/dbname), so a plain string split is enough
    // and avoids pulling in a URL-parsing crate for one rename.
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_orch_deposits");

    if !Postgres::database_exists(&url).await.unwrap_or(false) {
        Postgres::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    // Each test file starts clean — order-independent tests. This crate's own database
    // now holds only this crate's own tables.
    sqlx::query("TRUNCATE deposit_intents RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

fn test_config() -> OrchConfig {
    OrchConfig {
        http_addr: "0.0.0.0:0".into(),
        database_url: std::env::var("DATABASE_URL").unwrap(),
        jwt_secret: "test-jwt-secret".into(),
        allowed_origins: "*".into(),
        treasury_url: "http://unused".into(),
        treasury_initiator_token: "i".into(),
        treasury_readonly_token: "r".into(),
        custody_tron_address: "Tunused".into(),
        deposit_account_xpub: TEST_XPUB.into(),
        trongrid_url: "http://localhost:0".to_string(),
        trongrid_api_key: "test-key".to_string(),
        usdt_contract: "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf".to_string(),
        deposit_ttl_minutes: 30,
        min_deposit_usdt: 1_000_000,
        max_deposit_usdt: 50_000_000,
        poll_interval_secs: 30,
        // This file never exercises the redemption routes (Plan C T6) — off by default,
        // same as production, and unused otherwise.
        redemptions_enabled: false,
        min_redemption_clt: 1_000_000,
        max_redemption_clt: 50_000_000,
    }
}

fn user_pk() -> String {
    "0xalice0000000000000000000000000000000001".to_string()
}

/// Simulates a completed create-flow (what T2b will do after this module hands back
/// `Created`): stamp an invoice id + response onto the row via the layer-4 CAS mechanism.
async fn complete_invoice(pool: &PgPool, id: Uuid, status: i16, body: &serde_json::Value) {
    let stored = deposits::store_invoice(pool, id, &format!("inv-{id}"), status, body).await.unwrap();
    assert!(stored, "test setup: store_invoice should win uncontested");
}

#[tokio::test]
async fn replay_same_body_returns_stored_response_and_status() {
    let pool = pool().await;
    let cfg = test_config();
    let clt_address = "clt1address";
    let amount = 2_000_000i64;
    let key = "idem-key-1";

    let CreateOutcome::Created(intent) =
        deposits::create(&pool, &cfg, test_deriver(), &user_pk(), clt_address, amount, key).await.unwrap()
    else {
        panic!("expected Created on first call");
    };
    let body = json!({"id": intent.id, "pay_amount_usdt": intent.amount_usdt, "status": "invoiced"});
    complete_invoice(&pool, intent.id, 201, &body).await;

    // Replay: identical (user_pk, client_key, clt_address, amount_usdt).
    let outcome = deposits::create(&pool, &cfg, test_deriver(), &user_pk(), clt_address, amount, key).await.unwrap();
    match outcome {
        CreateOutcome::Replay { status, body: replayed } => {
            assert_eq!(status, 201, "replay must return the ORIGINAL stored status, not a fresh 200/201");
            assert_eq!(replayed, body, "replay must return the ORIGINAL stored body");
        }
        other => panic!("expected Replay, got {other:?}"),
    }
}

#[tokio::test]
async fn same_key_different_body_is_conflict() {
    let pool = pool().await;
    let cfg = test_config();
    let key = "idem-key-2";

    let CreateOutcome::Created(intent) =
        deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt1address", 2_000_000, key).await.unwrap()
    else {
        panic!("expected Created on first call");
    };
    complete_invoice(&pool, intent.id, 201, &json!({"ok": true})).await;

    // Same idempotency key, different amount_usdt this time — must not silently replay
    // the first request's outcome under a second user's intended amount.
    let outcome = deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt1address", 3_000_000, key).await.unwrap();
    assert!(matches!(outcome, CreateOutcome::Conflict), "different body under same key must be Conflict");

    // Also different clt_address, same amount — same rule.
    let outcome2 = deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt-different-address", 2_000_000, key).await.unwrap();
    assert!(matches!(outcome2, CreateOutcome::Conflict));
}

/// Row with response_body IS NULL simulates dying between INSERT and store_invoice — a
/// crash, or an adapter timeout where Bitcart may still hold a LIVE invoice at this
/// amount that we never recorded the id of.
///
/// Resume must keep the SAME deposit address. Moving to a fresh one would
/// release the old amount while that orphan invoice is still live, and the amount is the
/// only thing Bitcart can match a payment by on the shared static custody address — so a
/// later, DIFFERENT user allocated that amount would collide with a stranger's invoice.
/// The property with the money in it, now address-shaped: a resumed row must keep the address its
/// user may already be paying to. Re-deriving would be the per-address form of a32a101.
///
/// The "another user cannot claim it" half is gone from here because it is no longer an amount
/// slot that could be reissued — derivation indices are never reused (migration 0007), and the
/// uniqueness of index and address is proven directly in db_derivation_index.rs.
#[tokio::test]
async fn crash_resume_keeps_the_same_deposit_address() {
    let pool = pool().await;
    let cfg = test_config();
    let key = "idem-key-3";

    let CreateOutcome::Created(first) =
        deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt1address", 2_000_000, key).await.unwrap()
    else {
        panic!("expected Created on first call");
    };
    let orphan_address = first.deposit_address.clone().expect("a fresh intent must carry an address");
    // No store_invoice call — response_body stays NULL, simulating the crash.

    let outcome = deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt1address", 2_000_000, key).await.unwrap();
    let CreateOutcome::Created(resumed) = outcome else {
        panic!("expected Created (resume) on NULL response_body, got {outcome:?}");
    };
    assert_eq!(resumed.id, first.id, "resume continues the SAME intent row, not a new one");
    assert_eq!(
        resumed.deposit_address.as_deref(), Some(orphan_address.as_str()),
        "resume must keep the orphan's amount reserved, not move to a fresh discriminator"
    );
}

/// Idempotency layer 4: two writers race to store an invoice_id on the SAME intent row.
/// Bitcart has no order_id dedup server-side, so this compare-and-set is what makes the
/// store exactly-once. Second writer must lose (0 rows affected) and defer to the winner.
#[tokio::test]
async fn store_invoice_compare_and_set_second_writer_loses() {
    let pool = pool().await;
    let cfg = test_config();
    let CreateOutcome::Created(intent) =
        deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt1address", 2_000_000, "idem-key-4").await.unwrap()
    else {
        panic!("expected Created");
    };

    let first_body = json!({"invoice": "first"});
    let second_body = json!({"invoice": "second"});

    let first_won = deposits::store_invoice(&pool, intent.id, "invoice-A", 201, &first_body).await.unwrap();
    assert!(first_won, "first writer must win an uncontested CAS");

    let second_won = deposits::store_invoice(&pool, intent.id, "invoice-B", 201, &second_body).await.unwrap();
    assert!(!second_won, "second writer must lose — invoice_id was no longer NULL");

    // The canonical stored state is the FIRST writer's, not the second's.
    let row = deposits::find_by_id(&pool, intent.id).await.unwrap().unwrap();
    assert_eq!(row.invoice_id.as_deref(), Some("invoice-A"));
    assert_eq!(row.response_body, Some(first_body.clone()));

    // The losing writer's caller replays the canonical response rather than treating this
    // as an error — proving that response is actually retrievable is the point of the test.
    let CreateOutcome::Replay { status, body } =
        deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt1address", 2_000_000, "idem-key-4").await.unwrap()
    else {
        panic!("expected Replay of the canonical (first writer's) response");
    };
    assert_eq!(status, 201);
    assert_eq!(body, first_body);
}

/// The core money-safety property: two DIFFERENT users depositing the exact same
/// amount_usdt concurrently must never collide on deposit_address — that's the whole
/// reason the discriminator exists (Tron/Bitcart watch-only matches by amount on one
/// static address, not by per-invoice derived address).
#[tokio::test]
async fn concurrent_same_amount_intents_get_different_pay_amounts() {
    let pool = pool().await;
    let cfg = test_config();
    let amount = 5_000_000i64;

    let (a, b) = tokio::join!(
        deposits::create(&pool, &cfg, test_deriver(), "user-a", "clt-addr-a", amount, "key-a"),
        deposits::create(&pool, &cfg, test_deriver(), "user-b", "clt-addr-b", amount, "key-b"),
    );
    let CreateOutcome::Created(a) = a.unwrap() else { panic!("expected Created for user-a") };
    let CreateOutcome::Created(b) = b.unwrap() else { panic!("expected Created for user-b") };

    assert_eq!(a.amount_usdt, amount);
    assert_eq!(b.amount_usdt, amount);
    assert_ne!(
        a.deposit_address, b.deposit_address,
        "two concurrent active intents for the same amount_usdt must never share a deposit address"
    );
}


#[tokio::test]
async fn transition_allows_expired_to_confirmed_late_honour() {
    let pool = pool().await;
    let cfg = test_config();
    let CreateOutcome::Created(intent) =
        deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt1address", 2_000_000, "idem-key-5").await.unwrap()
    else {
        panic!("expected Created")
    };
    assert!(deposits::transition(&pool, intent.id, &["created", "invoiced", "paying"], "expired").await.unwrap());

    // Late-but-genuine payment: expired -> confirmed is legal (no FX risk at par).
    let applied = deposits::transition(&pool, intent.id, &["paying", "invoiced", "expired"], "confirmed")
        .await
        .unwrap();
    assert!(applied, "expired -> confirmed must be a legal late-honour transition");

    let row = deposits::find_by_id(&pool, intent.id).await.unwrap().unwrap();
    assert_eq!(row.status, "confirmed");
}

#[tokio::test]
async fn transition_refuses_confirmed_to_failed() {
    let pool = pool().await;
    let cfg = test_config();
    let CreateOutcome::Created(intent) =
        deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt1address", 2_000_000, "idem-key-6").await.unwrap()
    else {
        panic!("expected Created")
    };
    assert!(deposits::transition(&pool, intent.id, &["created", "invoiced", "paying"], "confirmed").await.unwrap());

    // Out-of-order webhook: a 'failed' arriving after 'confirmed' must be absorbed, not applied.
    let applied = deposits::transition(&pool, intent.id, &["created", "invoiced", "paying"], "failed")
        .await
        .unwrap();
    assert!(!applied, "confirmed -> failed must be refused (out-of-order webhook, spec §6)");

    let row = deposits::find_by_id(&pool, intent.id).await.unwrap().unwrap();
    assert_eq!(row.status, "confirmed", "status must remain confirmed, not regress to failed");
}

#[tokio::test]
async fn bounds_are_enforced() {
    let pool = pool().await;
    let cfg = test_config();

    let too_small = deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt1address", cfg.min_deposit_usdt - 1, "key-small").await;
    assert!(
        matches!(too_small, Err(deposits::ApiError::OutOfBounds { .. })),
        "below min_deposit_usdt must be rejected"
    );

    let too_large = deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt1address", cfg.max_deposit_usdt + 1, "key-large").await;
    assert!(
        matches!(too_large, Err(deposits::ApiError::OutOfBounds { .. })),
        "above max_deposit_usdt must be rejected"
    );

    // Boundary values themselves are legal.
    let at_min = deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt1address", cfg.min_deposit_usdt, "key-min").await;
    assert!(matches!(at_min, Ok(CreateOutcome::Created(_))), "exactly min_deposit_usdt must be accepted");

    let at_max = deposits::create(&pool, &cfg, test_deriver(), "user-max", "clt1address", cfg.max_deposit_usdt, "key-max").await;
    assert!(matches!(at_max, Ok(CreateOutcome::Created(_))), "exactly max_deposit_usdt must be accepted");
}

/// All money amounts are BIGINT end to end — no float, no NUMERIC/DECIMAL anywhere. At
/// par (1 micro-USDT = 1 CLT) amount_clt must equal amount_usdt exactly, a plain integer
/// identity, not a computed ratio.
#[tokio::test]
async fn amount_clt_equals_amount_usdt_at_par() {
    let pool = pool().await;
    let cfg = test_config();
    let amount = 12_345_678i64;
    let CreateOutcome::Created(intent) =
        deposits::create(&pool, &cfg, test_deriver(), &user_pk(), "clt1address", amount, "idem-key-par").await.unwrap()
    else {
        panic!("expected Created")
    };
    assert_eq!(intent.amount_clt, amount, "at par, amount_clt must equal amount_usdt exactly (integer identity)");
    // The discriminator-range assertion that used to sit here is gone with the discriminator: the
    // amount a user pays is now simply the amount they asked for.
}
