//! The sweep worker, with the signer faked so no key ever exists in a test.
//!
//! The property with the most at stake is the one that is easiest to get wrong by being helpful: a
//! sweep must leave the LEDGER untouched. It moves USDT between two addresses we already control, so
//! the reserve is unchanged and the money was counted when it arrived. Appending an event here would
//! double-count every deposit.

use async_trait::async_trait;
use sqlx::PgPool;
use treasury_service::sweeper::{self, SignerReply, SweepSigner};
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ADDR: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";

/// Five real, distinct derived addresses for these tests to assign across seeded rows — nothing in
/// the schema requires per-intent uniqueness any more (a permanent address can back many deposits).
/// They must be genuinely valid because the balance read base58check-decodes them; a placeholder
/// would be rejected.
const ADDRS: [&str; 5] = [
    "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH",
    "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK",
    "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx",
    "TRhVWK5XEDkQBDevcdCWW7RW51aRncty4W",
    "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY",
];
const USDT: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_tre_sweeper");
    if !<sqlx::Postgres as sqlx::migrate::MigrateDatabase>::database_exists(&url).await.unwrap_or(false) {
        <sqlx::Postgres as sqlx::migrate::MigrateDatabase>::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE treasury_events, mint_intents, chain_outbox, alerts RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

fn config(trongrid_url: String, threshold: i64) -> treasury_service::configuration::AppConfig {
    treasury_service::configuration::AppConfig {
        http_addr: "0.0.0.0:0".into(),
        metrics_addr: "0.0.0.0:9101".into(),
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
        trongrid_url,
        trongrid_api_key: "k".into(),
        custody_tron_address: ADDR.into(),
        payout_float_address: "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY".into(),
        usdt_contract: USDT.into(),
        deposit_confirmations: 19,
        deposit_match_window_hours: 24,
        sweep_threshold_usdt: threshold,
        sweep_max_age_hours: 168,
        signer_url: "http://unused".into(),
        signer_token: "s".into(),
    }
}

/// Records which indices it was asked to sweep, so a test can assert the worker did NOT reach for
/// an address it had no business touching.
struct FakeSigner {
    reply: SignerReply,
    asked: std::sync::Mutex<Vec<i64>>,
}

impl FakeSigner {
    fn new(reply: SignerReply) -> Self {
        Self { reply, asked: std::sync::Mutex::new(Vec::new()) }
    }
    fn asked(&self) -> Vec<i64> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl SweepSigner for FakeSigner {
    async fn sweep(&self, index: i64) -> SignerReply {
        self.asked.lock().unwrap().push(index);
        match &self.reply {
            SignerReply::Swept { tx_id } => SignerReply::Swept { tx_id: tx_id.clone() },
            SignerReply::NothingToSweep => SignerReply::NothingToSweep,
            SignerReply::Funded { tx_id, amount_sun } => {
                SignerReply::Funded { tx_id: tx_id.clone(), amount_sun: *amount_sun }
            }
            SignerReply::FeeAccountDry { fee_address, have_sun, need_sun } => SignerReply::FeeAccountDry {
                fee_address: fee_address.clone(),
                have_sun: *have_sun,
                need_sun: *need_sun,
            },
            SignerReply::Failed(e) => SignerReply::Failed(e.clone()),
        }
    }
}

/// balanceOf answers the same word for any address — enough to drive the threshold.
async fn mount_balance(server: &MockServer, micro_usdt: i64) {
    Mock::given(method("POST"))
        .and(path("/wallet/triggerconstantcontract"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"constant_result": [format!("{:0>64x}", micro_usdt)]})),
        )
        .mount(server)
        .await;
}

async fn seed(pool: &PgPool, status: &str, index: i64, age_hours: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents
            (id, beneficiary, amount_clt, credit_ref, created_by, approved_by, client_ref,
             expected_amount_usdt, deposit_address, derivation_index, status, created_at)
         VALUES ($1, 'TBene', 1000000, $2, 'orchestrator',
                 -- four_eyes requires an approver that differs from the creator for anything past
                 -- `created`; NULL there is only valid while the intent is still unapproved.
                 CASE WHEN $6 = 'created' THEN NULL ELSE 'tron-verifier' END,
                 $3, 1000000, $4, $5, $6, now() - ($7 || ' hours')::interval)",
    )
    .bind(id)
    .bind(format!("ref-{id}"))
    .bind(format!("client-{id}"))
    .bind(ADDRS[(index as usize) % ADDRS.len()])
    .bind(index)
    .bind(status)
    .bind(age_hours.to_string())
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn swept_at(pool: &PgPool, id: Uuid) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar("SELECT swept_at FROM mint_intents WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn ledger_events(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM treasury_events").fetch_one(pool).await.unwrap()
}

/// THE invariant. A sweep moves money between addresses we already control, so the ledger must not
/// move. Recording an event here would double-count every deposit — inflating custody against an
/// unchanged liability, which reads as over-backing and, to anyone reconciling by hand, as money
/// appearing from nowhere.
#[tokio::test]
async fn a_sweep_records_swept_at_and_writes_no_ledger_event() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, 500_000_000).await; // well over threshold
    let id = seed(&pool, "credited", 7, 1).await;
    let before = ledger_events(&pool).await;

    let signer = FakeSigner::new(SignerReply::Swept { tx_id: "tx-swept".into() });
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;

    assert!(swept_at(&pool, id).await.is_some(), "a completed sweep must be recorded");
    assert_eq!(ledger_events(&pool).await, before, "a sweep must NOT touch the ledger");
    assert_eq!(signer.asked(), vec![7], "the signer must be asked for this intent's own index");
}

/// Below threshold and young: leave it alone. Sweeping costs TRX, and against a $1 deposit that can
/// exceed what it moves.
#[tokio::test]
async fn a_small_fresh_balance_is_not_swept() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, 1_000_000).await;
    let id = seed(&pool, "credited", 1, 1).await;

    let signer = FakeSigner::new(SignerReply::Swept { tx_id: "tx".into() });
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;

    assert!(swept_at(&pool, id).await.is_none());
    assert!(signer.asked().is_empty(), "the signer must not be called at all below threshold");
}

/// The escape valve: the same small balance, old enough, does move. Without this the reserve
/// fragments permanently across addresses nobody revisits.
#[tokio::test]
async fn a_small_but_old_balance_is_swept() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, 1_000_000).await;
    let id = seed(&pool, "credited", 2, 200).await; // older than the 168h max age

    let signer = FakeSigner::new(SignerReply::Swept { tx_id: "tx-old".into() });
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;

    assert!(swept_at(&pool, id).await.is_some(), "an aged balance must eventually move");
}

/// A fresh address holds no TRX, so the signer funds it first — the EXPECTED first answer for every
/// address, not a failure. It must not be recorded as swept: the funding transfer still has to
/// confirm, and the USDT has not moved.
#[tokio::test]
async fn a_funded_address_is_left_unswept_for_a_later_pass() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, 500_000_000).await;
    let id = seed(&pool, "credited", 3, 1).await;

    let signer = FakeSigner::new(SignerReply::Funded { tx_id: "tx-fund".into(), amount_sun: 30_000_000 });
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;

    assert!(swept_at(&pool, id).await.is_none(), "funds are still there; this must be retried");
}

/// Funding is not an incident. It happens once per address, forever, so alerting on it would train
/// whoever reads the alerts to ignore the queue that also carries the failures.
#[tokio::test]
async fn funding_an_address_does_not_raise_an_alert() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, 500_000_000).await;
    seed(&pool, "credited", 9, 1).await;

    let signer = FakeSigner::new(SignerReply::Funded { tx_id: "tx-fund".into(), amount_sun: 30_000_000 });
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;

    let alerts: i64 = sqlx::query_scalar("SELECT count(*) FROM alerts WHERE source = 'sweeper'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alerts, 0, "routine funding must not look like a problem");
}

/// An exhausted TRX float stops the whole pass, not just the address that hit it.
///
/// Every remaining address would get the identical answer, so continuing would alert once per
/// unswept address — burying the single actionable fact under a pass-sized burst of duplicates, and
/// doing it again on every tick.
#[tokio::test]
async fn a_dry_fee_account_stops_the_pass_and_alerts_once() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, 500_000_000).await;
    // Distinct ages make the walk order deterministic — the pass is ordered by created_at, so the
    // older row (index 10) is asked first.
    seed(&pool, "credited", 10, 5).await;
    seed(&pool, "credited", 11, 1).await;

    let signer = FakeSigner::new(SignerReply::FeeAccountDry {
        fee_address: "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".into(),
        have_sun: 0,
        need_sun: 31_000_000,
    });
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;

    assert_eq!(signer.asked(), vec![10], "the pass must stop, not ask every remaining address");
    let alerts: i64 = sqlx::query_scalar("SELECT count(*) FROM alerts WHERE source = 'sweeper'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alerts, 1, "one alert per pass, naming the account to top up");
}

/// A signer failure must not mark the address swept, or real funds are abandoned at an address
/// nothing looks at again.
#[tokio::test]
async fn a_failed_sweep_is_alerted_and_left_unswept() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, 500_000_000).await;
    let id = seed(&pool, "credited", 4, 1).await;

    let signer = FakeSigner::new(SignerReply::Failed("broadcast rejected".into()));
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;

    assert!(swept_at(&pool, id).await.is_none());
    let alerts: i64 = sqlx::query_scalar("SELECT count(*) FROM alerts WHERE source = 'sweeper'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(alerts >= 1, "a failed sweep must be surfaced");
}

/// An intent whose deposit is not yet credited must never be swept: that would move the evidence
/// out from under the verifier before it has finished with it.
#[tokio::test]
async fn an_uncredited_intent_is_never_swept() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, 500_000_000).await;
    let created = seed(&pool, "created", 5, 1).await;
    let approved = seed(&pool, "approved", 6, 1).await;

    let signer = FakeSigner::new(SignerReply::Swept { tx_id: "tx".into() });
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;

    assert!(swept_at(&pool, created).await.is_none(), "a created intent has no verified deposit yet");
    assert!(swept_at(&pool, approved).await.is_none(), "approved is not yet credited");
    assert!(signer.asked().is_empty(), "neither address may be touched");
}

/// Re-running must be a no-op rather than a second transaction: the signer answers NothingToSweep
/// for an already-empty address, and the worker records that as done.
#[tokio::test]
async fn re_running_over_an_empty_address_settles_it_without_a_second_transfer() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, 500_000_000).await;
    let id = seed(&pool, "credited", 8, 1).await;

    let signer = FakeSigner::new(SignerReply::NothingToSweep);
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;
    assert!(swept_at(&pool, id).await.is_some(), "an empty address is complete, not pending");

    let first = swept_at(&pool, id).await;
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;
    assert_eq!(swept_at(&pool, id).await, first, "swept_at must not be rewritten by a later pass");
}

/// The bug the Task 7 review caught before it shipped (R17): the query above only ever selects rows
/// WITH a `derivation_index`, so a credited deposit whose row lacks one is skipped forever, and the
/// pass logs the same "unswept address(es)" line a healthy, idle pass would show. `sweep_once` must
/// surface it instead of staying silent.
#[tokio::test]
async fn a_credited_row_missing_derivation_index_is_reported_and_left_unswept() {
    let pool = pool().await;
    let server = MockServer::start().await; // no balance mock — this row must never reach the sweep loop

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents
            (id, beneficiary, amount_clt, credit_ref, created_by, approved_by, client_ref,
             expected_amount_usdt, deposit_address, derivation_index, status, created_at)
         VALUES ($1, 'TBene', 1000000, $2, 'orchestrator', 'tron-verifier', $3, 1000000, $4, NULL, 'credited', now())",
    )
    .bind(id)
    .bind(format!("ref-{id}"))
    .bind(format!("client-{id}"))
    .bind(ADDRS[0])
    .execute(&pool)
    .await
    .unwrap();

    let signer = FakeSigner::new(SignerReply::Swept { tx_id: "tx-unreachable".into() });
    let missing = sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;

    assert_eq!(missing, 1, "the pass must count the credited row with no derivation_index");
    assert!(signer.asked().is_empty(), "a row with no index must be reported, not swept");
    assert!(swept_at(&pool, id).await.is_none(), "unchanged behaviour: the row is still left unswept");

    let alerts: i64 = sqlx::query_scalar("SELECT count(*) FROM alerts WHERE source = 'sweeper'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alerts, 1, "a credited row with no derivation_index must reach the alerts pipeline, not just the log");
}

fn tron(server: &MockServer) -> treasury_service::tron_verifier::TronClient {
    treasury_service::tron_verifier::TronClient::new(server.uri(), "k".into())
}
