//! The sweep worker, with the signer faked so no key ever exists in a test.
//!
//! The property with the most at stake is the one that is easiest to get wrong by being helpful: a
//! sweep must leave the LEDGER untouched. It moves USDT between two addresses we already control, so
//! the reserve is unchanged and the money was counted when it arrived. Appending an event here would
//! double-count every deposit.

use async_trait::async_trait;
use sqlx::PgPool;
use treasury_service::gasfree_rail::Trace;
use treasury_service::sweeper::{self, SignerReply, SweepSigner};
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method, path};
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
    sqlx::query("TRUNCATE treasury_events, mint_intents, chain_outbox, alerts, gasfree_accounts RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    // The tripwire sets the breaker; no test may inherit it from another.
    sqlx::query("UPDATE breaker_state SET minting_halted = FALSE, halt_reason = NULL")
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
        trongrid_api_key: "k".into(),
        custody_tron_address: ADDR.into(),
        payout_float_address: "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY".into(),
        usdt_contract: USDT.into(),
        deposit_confirmations: 19,
        deposit_match_window_hours: 24,
        sweep_threshold_usdt: threshold,
        sweep_max_age_hours: 168,
        sweep_min_usdt: 0,
        redemption_fee_usdt: 0,
        signer_url: "http://unused".into(),
        signer_token: "s".into(),
        gasfree: None,
    }
}

/// Records which indices it was asked to sweep, so a test can assert the worker did NOT reach for
/// an address it had no business touching.
struct FakeSigner {
    reply: SignerReply,
    asked: std::sync::Mutex<Vec<i64>>,
    /// What the relay's record of any permit says reached the receiver. `None`: not known yet.
    trace_amount: Option<i64>,
}

impl FakeSigner {
    fn new(reply: SignerReply) -> Self {
        Self { reply, asked: std::sync::Mutex::new(Vec::new()), trace_amount: None }
    }
    fn asked(&self) -> Vec<i64> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl SweepSigner for FakeSigner {
    async fn sweep(&self, index: i64) -> SignerReply {
        self.asked.lock().unwrap().push(index);
        self.reply.clone()
    }

    async fn trace(&self, _trace_id: &str) -> Result<Trace, String> {
        Ok(Trace { state: "SUCCEED".into(), txn_hash: Some("tx-permit".into()), txn_amount: self.trace_amount })
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

/// Several deposits at one address are one sweep of its whole balance: the signer is asked once,
/// and every row is marked. Asking per row swept the address, then read it empty for the next row,
/// which then stayed unswept for good.
#[tokio::test]
async fn every_deposit_at_one_index_is_asked_for_once_and_marked_together() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, 500_000_000).await;
    let first = seed(&pool, "credited", 7, 2).await;
    let second = seed(&pool, "credited", 7, 1).await;

    let signer = FakeSigner::new(SignerReply::Swept { tx_id: "tx-both".into() });
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;

    assert_eq!(signer.asked(), vec![7], "one request for the index, not one per deposit");
    assert!(swept_at(&pool, first).await.is_some());
    assert!(swept_at(&pool, second).await.is_some());
}

// --- GasFree (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md §3, §5) ---

/// Plain addresses of indexes 7 and 8: the permits' owners, as the verifier recorded them.
const OWNER_7: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";
const OWNER_8: &str = "TRhVWK5XEDkQBDevcdCWW7RW51aRncty4W";
/// Nile's reviewed implementations (Plan 2, facts 3 and 4).
const BEACON_OK: &str = "b8eda40b467b45af107f198e94cc2fa1378adf50";
const CONTROLLER_OK: &str = "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441";
const TRACE: &str = "6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60";

fn account_of(owner: &str) -> String {
    gasfree::gasfree_address(&gasfree::NILE, owner).unwrap()
}

fn nile() -> gasfree::Settings {
    gasfree::Settings {
        chain: &gasfree::NILE,
        rail: true,
        activate_fee_max_usdt: 1_500_000,
        transfer_fee_max_usdt: 500_000,
        min_deposit_usdt: 1_000_000,
        expected_beacon_implementation: BEACON_OK.into(),
        expected_controller_implementation: CONTROLLER_OK.into(),
    }
}

/// GasFree on, with the TRX threshold left at $100: a GasFree account ignores it.
fn gasfree_config(server: &MockServer) -> treasury_service::configuration::AppConfig {
    let mut config = config(server.uri(), 100_000_000);
    config.gasfree = Some(nile());
    config
}

fn in_three_minutes() -> u64 {
    (chrono::Utc::now().timestamp() + 180) as u64
}

fn pending(value_usdt: i64, max_fee_usdt: i64, nonce: u64) -> SignerReply {
    SignerReply::Pending {
        trace_id: TRACE.into(),
        gasfree_address: account_of(OWNER_7),
        value_usdt,
        max_fee_usdt,
        nonce,
        deadline: in_three_minutes(),
    }
}

/// `balanceOf` for one address only.
async fn mount_usdt_balance(server: &MockServer, address: &str, micro_usdt: i64) {
    Mock::given(method("POST"))
        .and(path("/wallet/triggerconstantcontract"))
        .and(body_string_contains("balanceOf(address)"))
        .and(body_string_contains(address))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"constant_result": [format!("{micro_usdt:064x}")]})),
        )
        .mount(server)
        .await;
}

/// Both GasFree proxies' `implementation()`: the beacon as given, the controller as reviewed.
async fn mount_implementations(server: &MockServer, beacon: &str) {
    for (proxy, implementation) in [(gasfree::NILE.beacon, beacon), (gasfree::NILE.controller, CONTROLLER_OK)] {
        Mock::given(method("POST"))
            .and(path("/wallet/triggerconstantcontract"))
            .and(body_string_contains("implementation()"))
            .and(body_string_contains(proxy))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"constant_result": [format!("{implementation:0>64}")]})),
            )
            .mount(server)
            .await;
    }
}

/// TronGrid as a GasFree pass reads it: one account's USDT balance, the owner's nonce, and both
/// proxies. Each mock answers its own call only.
async fn mount_gasfree_chain(server: &MockServer, account: &str, balance: i64, nonce: u64, beacon: &str) {
    mount_usdt_balance(server, account, balance).await;
    Mock::given(method("POST"))
        .and(path("/wallet/triggerconstantcontract"))
        .and(body_string_contains("nonces(address)"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"constant_result": [format!("{nonce:064x}")]})),
        )
        .mount(server)
        .await;
    mount_implementations(server, beacon).await;
}

/// A credited deposit at a GasFree account, as the verifier leaves one: its fee held back, verified a minute ago.
async fn seed_gasfree_deposit(pool: &PgPool, index: i64, account: &str, fee_held_usdt: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents
            (id, beneficiary, amount_clt, credit_ref, created_by, approved_by, client_ref,
             expected_amount_usdt, deposit_address, derivation_index, status, verified_at, fee_held_usdt)
         VALUES ($1, 'TBene', $2, $3, 'orchestrator', 'tron-verifier', $4, 10000000, $5, $6,
                 'credited', now() - interval '1 minute', $7)",
    )
    .bind(id)
    .bind(10_000_000 - fee_held_usdt)
    .bind(format!("ref-{id}"))
    .bind(format!("client-{id}"))
    .bind(account)
    .bind(index)
    .bind(fee_held_usdt)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn seed_account(pool: &PgPool, index: i64, owner: &str, seen_first_transfer: bool) {
    sqlx::query(
        "INSERT INTO gasfree_accounts (derivation_index, gasfree_address, owner_address, first_transfer_at)
         VALUES ($1, $2, $3, CASE WHEN $4 THEN now() END)",
    )
    .bind(index)
    .bind(account_of(owner))
    .bind(owner)
    .bind(seen_first_transfer)
    .execute(pool)
    .await
    .unwrap();
}

/// A permit already in flight for index 7.
async fn seed_pending_7(pool: &PgPool, trace_id: &str, nonce: i64, deadline: i64, value_usdt: i64) {
    sqlx::query(
        "UPDATE gasfree_accounts
            SET pending_trace_id = $1, pending_nonce = $2, pending_deadline = $3,
                pending_value_usdt = $4, pending_requested_at = now()
          WHERE derivation_index = 7",
    )
    .bind(trace_id)
    .bind(nonce)
    .bind(deadline)
    .bind(value_usdt)
    .execute(pool)
    .await
    .unwrap();
}

/// (trace id in flight, its nonce, first transfer seen) for an index.
async fn account_state(pool: &PgPool, index: i64) -> (Option<String>, Option<i64>, bool) {
    sqlx::query_as(
        "SELECT pending_trace_id, pending_nonce, first_transfer_at IS NOT NULL FROM gasfree_accounts WHERE derivation_index = $1",
    )
    .bind(index)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn sweeper_alerts(pool: &PgPool, severity: &str, containing: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM alerts WHERE source = 'sweeper' AND severity = $1 AND message LIKE '%' || $2 || '%'",
    )
    .bind(severity)
    .bind(containing)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Spec §3 and §5: a sweep by permit is not a sweep until the chain says so. The account keeps the
/// relay's unused margin, so its balance never says it; the owner's nonce moving past the permit's does.
#[tokio::test]
async fn a_gasfree_deposit_is_swept_by_permit_and_counted_swept_only_once_the_nonce_moves() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 0, BEACON_OK).await;
    seed_account(&pool, 7, OWNER_7, false).await;
    let id = seed_gasfree_deposit(&pool, 7, &account, 2_000_000).await;
    let signer = FakeSigner::new(pending(8_000_000, 2_000_000, 0));
    let cfg = gasfree_config(&server);

    sweeper::sweep_once(&pool, &cfg, &tron(&server), &signer).await;
    assert_eq!(signer.asked(), vec![7], "a credited deposit above the fee is swept at once, whatever the threshold");
    assert!(swept_at(&pool, id).await.is_none(), "a permit with the relay is not a sweep");
    assert_eq!(account_state(&pool, 7).await, (Some(TRACE.into()), Some(0), false));

    sweeper::sweep_once(&pool, &cfg, &tron(&server), &signer).await;
    assert_eq!(signer.asked(), vec![7], "one permit at a time: none while one may still run");

    server.reset().await;
    mount_gasfree_chain(&server, &account, 700_000, 1, BEACON_OK).await;
    sweeper::sweep_once(&pool, &cfg, &tron(&server), &signer).await;
    assert!(swept_at(&pool, id).await.is_some(), "the nonce moved past the permit's, so it ran");
    assert_eq!(account_state(&pool, 7).await, (None, None, true), "the treasury saw its own first sweep run");
    assert_eq!(ledger_events(&pool).await, 0, "a sweep must NOT touch the ledger");
    assert_eq!(signer.asked(), vec![7], "nothing is left to ask for");
}

/// Past its deadline and a grace minute, a permit that did not run never can: a fresh one replaces it.
#[tokio::test]
async fn an_expired_permit_that_did_not_run_is_replaced() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 3, BEACON_OK).await;
    seed_account(&pool, 7, OWNER_7, true).await;
    seed_pending_7(&pool, "11111111-1111-4111-8111-111111111111", 3, chrono::Utc::now().timestamp() - 120, 9_500_000).await;
    let id = seed_gasfree_deposit(&pool, 7, &account, 500_000).await;
    let signer = FakeSigner::new(pending(9_500_000, 500_000, 3));

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert_eq!(signer.asked(), vec![7], "the dead permit is dropped and a fresh one asked for in the same pass");
    assert_eq!(account_state(&pool, 7).await, (Some(TRACE.into()), Some(3), true));
    assert!(swept_at(&pool, id).await.is_none());
}

/// Spec §5: after GasFree's code changes, no more money goes in or moves by permit until a human looks.
#[tokio::test]
async fn a_changed_gasfree_implementation_stops_gasfree_sweeps_and_pages_once() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 0, "00000000000000000000000000000000000000ff").await;
    seed_account(&pool, 7, OWNER_7, false).await;
    let id = seed_gasfree_deposit(&pool, 7, &account, 2_000_000).await;
    let signer = FakeSigner::new(pending(8_000_000, 2_000_000, 0));
    let cfg = gasfree_config(&server);

    sweeper::sweep_once(&pool, &cfg, &tron(&server), &signer).await;
    sweeper::sweep_once(&pool, &cfg, &tron(&server), &signer).await;

    assert!(signer.asked().is_empty(), "no permit is asked for while GasFree's code is not the reviewed code");
    assert_eq!(sweeper_alerts(&pool, "p1", "not the reviewed").await, 1, "paged once, not every pass");
    assert!(swept_at(&pool, id).await.is_none());
}

/// The GasFree design's §2 rule: CLT is minted only against USDT that will reach custody. Once
/// GasFree's code changes that is no longer known, so minting halts until a human looks.
#[tokio::test]
async fn a_changed_gasfree_implementation_halts_minting() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 0, "00000000000000000000000000000000000000ff").await;
    seed_account(&pool, 7, OWNER_7, false).await;
    seed_gasfree_deposit(&pool, 7, &account, 2_000_000).await;
    let signer = FakeSigner::new(pending(8_000_000, 2_000_000, 0));

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    let (halted, reason): (bool, Option<String>) =
        sqlx::query_as("SELECT minting_halted, halt_reason FROM breaker_state").fetch_one(&pool).await.unwrap();
    assert!(halted, "minting halts while GasFree's code is not the reviewed code");
    assert!(reason.unwrap_or_default().starts_with("GasFree tripwire: "), "the reason names the tripwire");
}

/// A breaker already set, by a person or by a mismatch, keeps its own reason: the tripwire does not
/// overwrite what a human is already reading.
#[tokio::test]
async fn the_tripwire_keeps_an_earlier_halt_reason() {
    let pool = pool().await;
    sqlx::query("UPDATE breaker_state SET minting_halted = TRUE, halt_reason = 'halted by hand'")
        .execute(&pool)
        .await
        .unwrap();
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 0, "00000000000000000000000000000000000000ff").await;
    seed_account(&pool, 7, OWNER_7, false).await;
    seed_gasfree_deposit(&pool, 7, &account, 2_000_000).await;

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &FakeSigner::new(SignerReply::Busy)).await;

    let reason: Option<String> = sqlx::query_scalar("SELECT halt_reason FROM breaker_state").fetch_one(&pool).await.unwrap();
    assert_eq!(reason.as_deref(), Some("halted by hand"));
}

#[tokio::test]
async fn a_relay_refusal_pages_and_leaves_the_deposit_where_it_is() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 0, BEACON_OK).await;
    seed_account(&pool, 7, OWNER_7, false).await;
    let id = seed_gasfree_deposit(&pool, 7, &account, 2_000_000).await;
    let signer = FakeSigner::new(SignerReply::Rejected { reason: "MaxFeeExceededException".into(), message: "max fee exceeded".into() });

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert_eq!(signer.asked(), vec![7]);
    assert!(swept_at(&pool, id).await.is_none());
    assert_eq!(account_state(&pool, 7).await, (None, None, false), "nothing is in flight");
    assert_eq!(sweeper_alerts(&pool, "p1", "refused").await, 1);
}

/// Spec §2: "Never sign a higher `maxFee` than was held back". The permit is signed by then, so paging is what is left.
#[tokio::test]
async fn a_permit_whose_max_fee_is_above_what_was_held_pages() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 4, BEACON_OK).await;
    seed_account(&pool, 7, OWNER_7, true).await;
    seed_gasfree_deposit(&pool, 7, &account, 500_000).await;
    let signer = FakeSigner::new(pending(8_000_000, 2_000_000, 4));

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert_eq!(sweeper_alerts(&pool, "p1", "held back").await, 1);
    assert_eq!(account_state(&pool, 7).await, (Some(TRACE.into()), Some(4), true), "the permit exists, so it is followed");
}

/// Spec §2: "Never sign a higher `maxFee` than was held back". Deposits that held less than the fee a
/// permit may now take are not signed for, and it pages once.
#[tokio::test]
async fn no_permit_is_signed_for_deposits_that_held_less_than_the_fee() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 0, BEACON_OK).await;
    seed_account(&pool, 7, OWNER_7, false).await;
    let id = seed_gasfree_deposit(&pool, 7, &account, 500_000).await;
    let signer = FakeSigner::new(pending(8_000_000, 2_000_000, 0));

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;
    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert!(signer.asked().is_empty(), "nothing is signed for deposits that held less than the fee");
    assert!(swept_at(&pool, id).await.is_none());
    assert_eq!(sweeper_alerts(&pool, "p1", "nothing was signed").await, 1, "paged once, not every pass");
}

/// A signer on GasFree behind a treasury with GasFree off: nothing here follows what it did, so it pages once.
#[tokio::test]
async fn a_gasfree_answer_to_a_treasury_with_gasfree_off_pages_once() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, 500_000_000).await;
    let id = seed(&pool, "credited", 7, 1).await;
    let signer = FakeSigner::new(pending(9_500_000, 500_000, 0));

    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;

    assert!(swept_at(&pool, id).await.is_none());
    assert_eq!(sweeper_alerts(&pool, "p1", "GasFree off").await, 1, "paged once, not every pass");
}

/// A balance no larger than what the relay may take would move nothing (spec §3). The fee is sized on
/// the treasury's own record, so activation counts until its own first sweep of the account ran.
#[tokio::test]
async fn a_gasfree_account_is_swept_only_when_it_holds_more_than_the_fee() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_gasfree_chain(&server, &account_of(OWNER_7), 2_000_000, 0, BEACON_OK).await;
    mount_usdt_balance(&server, &account_of(OWNER_8), 2_000_001).await;
    seed_account(&pool, 7, OWNER_7, false).await;
    seed_account(&pool, 8, OWNER_8, false).await;
    seed_gasfree_deposit(&pool, 7, &account_of(OWNER_7), 2_000_000).await;
    seed_gasfree_deposit(&pool, 8, &account_of(OWNER_8), 2_000_000).await;
    let signer = FakeSigner::new(SignerReply::Busy);

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert_eq!(signer.asked(), vec![8], "exactly the fee is not enough; one micro-USDT more is");
}

/// Open question 1, checked on every sweep: the receiver must get exactly the permit's `value`, with
/// the relay's fee on top of it.
#[tokio::test]
async fn the_relays_record_of_what_reached_the_receiver_is_checked() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 700_000, 1, BEACON_OK).await;
    seed_account(&pool, 7, OWNER_7, false).await;
    let id = seed_gasfree_deposit(&pool, 7, &account, 2_000_000).await;
    seed_pending_7(&pool, TRACE, 0, chrono::Utc::now().timestamp() + 180, 8_000_000).await;
    let mut signer = FakeSigner::new(SignerReply::Busy);
    signer.trace_amount = Some(7_700_000);

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert!(swept_at(&pool, id).await.is_some());
    assert_eq!(sweeper_alerts(&pool, "p1", "reached the receiver").await, 1);
}

/// The fee account is empty by design on this rail. Plain addresses still waiting on it must not stop
/// the GasFree accounts, which need no TRX.
#[tokio::test]
async fn with_gasfree_on_a_dry_fee_account_does_not_stop_the_pass() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_implementations(&server, BEACON_OK).await;
    mount_usdt_balance(&server, ADDRS[0], 500_000_000).await; // index 10
    mount_usdt_balance(&server, ADDRS[1], 500_000_000).await; // index 11
    seed(&pool, "credited", 10, 5).await;
    seed(&pool, "credited", 11, 1).await;
    let signer = FakeSigner::new(SignerReply::FeeAccountDry {
        fee_address: "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".into(),
        have_sun: 0,
        need_sun: 31_000_000,
    });

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert_eq!(signer.asked(), vec![10, 11], "the pass goes on");
    let alerts: i64 = sqlx::query_scalar("SELECT count(*) FROM alerts WHERE source = 'sweeper'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alerts, 1, "still one alert per pass, naming the account to top up");
}

#[tokio::test]
async fn the_gasfree_sweep_answers_are_read_field_by_field() {
    let cases = [
        (
            serde_json::json!({"status": "pending", "trace_id": TRACE, "gasfree_address": "TGasFree", "receiver": "TFloat",
                               "value_usdt": 8_000_000, "max_fee_usdt": 2_000_000, "nonce": 4, "deadline": 1_790_000_000u64}),
            SignerReply::Pending {
                trace_id: TRACE.into(),
                gasfree_address: "TGasFree".into(),
                value_usdt: 8_000_000,
                max_fee_usdt: 2_000_000,
                nonce: 4,
                deadline: 1_790_000_000,
            },
        ),
        (serde_json::json!({"status": "busy", "gasfree_address": "TGasFree"}), SignerReply::Busy),
        (
            serde_json::json!({"status": "rejected", "reason": "MaxFeeExceededException", "message": "max fee exceeded"}),
            SignerReply::Rejected { reason: "MaxFeeExceededException".into(), message: "max fee exceeded".into() },
        ),
        (
            serde_json::json!({"status": "halted", "reason": "GasFree's code changed"}),
            SignerReply::Halted { reason: "GasFree's code changed".into() },
        ),
        (
            serde_json::json!({"status": "below_fee", "gasfree_address": "TGasFree", "balance_usdt": 1, "max_fee_usdt": 2_000_000}),
            SignerReply::BelowFee,
        ),
    ];
    for (body, want) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/sweep"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .mount(&server)
            .await;
        let signer = sweeper::HttpSigner { http: reqwest::Client::new(), base_url: server.uri(), token: "t".into() };
        assert_eq!(signer.sweep(7).await, want, "{body}");
    }

    // A pending answer without its permit's fields cannot be followed, so it is not taken as one.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/internal/sweep"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"status": "pending", "trace_id": TRACE})))
        .mount(&server)
        .await;
    let signer = sweeper::HttpSigner { http: reqwest::Client::new(), base_url: server.uri(), token: "t".into() };
    assert!(matches!(signer.sweep(7).await, SignerReply::Failed(_)));
}

#[tokio::test]
async fn a_trace_is_read_from_the_signer() {
    for (body, want) in [
        (
            serde_json::json!({"state": "SUCCEED", "txn_hash": "abc", "txn_state": "ON_CHAIN", "txn_amount": 8_000_000, "txn_total_fee": 1_300_000}),
            Trace { state: "SUCCEED".into(), txn_hash: Some("abc".into()), txn_amount: Some(8_000_000) },
        ),
        (
            serde_json::json!({"state": "WAITING", "txn_hash": null, "txn_state": null, "txn_amount": null, "txn_total_fee": null}),
            Trace { state: "WAITING".into(), txn_hash: None, txn_amount: None },
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/internal/gasfree/trace/{TRACE}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .mount(&server)
            .await;
        let signer = sweeper::HttpSigner { http: reqwest::Client::new(), base_url: server.uri(), token: "t".into() };
        assert_eq!(signer.trace(TRACE).await, Ok(want), "{body}");
    }
}
