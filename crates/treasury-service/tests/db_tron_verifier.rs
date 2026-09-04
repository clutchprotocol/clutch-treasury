//! Plan C T5 treasury-side tests: wiremock stands in for TronGrid. Covers every case the
//! brief names as required: happy path approves and ledgers exactly once (proven via an
//! actual rerun, not assumed); wrong recipient / wrong token / insufficient amount each
//! reject; a transient TronGrid failure never rejects; a duplicate `client_ref` create
//! replays instead of duplicating; and the new `uq_mint_intents_deposit_tx` index refuses a
//! second intent claiming the same on-chain transfer.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// THIS intent's own derived deposit address. Deposits no longer share one custody address, so the
/// verifier gathers evidence here — at the address the intent names — rather than at a global from
/// its own config.
const DEPOSIT_ADDR: &str = "TCustodyAddressXXXXXXXXXXXXXXXXXXX";
/// A DIFFERENT intent's address, for proving evidence never crosses between them.
const OTHER_ADDR: &str = "TOtherIntentAddressYYYYYYYYYYYYYYY";
const USDT: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";

/// The balanceOf path base58check-DECODES the address it is given, so a mistyped one cannot read a
/// stranger's balance. The DEPOSIT_ADDR/OTHER_ADDR placeholders above are opaque strings the trc20
/// tests only ever compare, never decode — the reserve-sum tests need genuinely valid addresses.
const REAL_MAIN: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";
const REAL_UNSWEPT_A: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";
const REAL_UNSWEPT_B: &str = "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx";
/// The payout float's address — also genuinely valid base58check, since it goes through the same
/// balanceOf/abi_encode_address path as the addresses above.
const FLOAT: &str = "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY";

async fn pool() -> PgPool {
    // Each test BINARY gets its own database. --test-threads=1 only serialises tests WITHIN a
    // binary; cargo runs binaries in PARALLEL, and every pool() here TRUNCATEs shared tables —
    // so binaries were wiping each other mid-test. That produced a ~1-in-6 flake that moved
    // between tests run to run (see progress.md).
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_tre_tron_verifier");
    if !<sqlx::Postgres as sqlx::migrate::MigrateDatabase>::database_exists(&url).await.unwrap_or(false) {
        <sqlx::Postgres as sqlx::migrate::MigrateDatabase>::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query(
        "TRUNCATE treasury_events, mint_intents, chain_outbox, reconciliation_runs, alerts RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE breaker_state SET minting_halted = FALSE, halt_reason = NULL")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

fn test_config(trongrid_url: String) -> treasury_service::configuration::AppConfig {
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
        trongrid_api_key: "test-trongrid-key".into(),
        custody_tron_address: DEPOSIT_ADDR.into(),
        payout_float_address: "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY".into(),
        usdt_contract: USDT.into(),
        deposit_confirmations: 19,
        deposit_match_window_hours: 24,
        sweep_threshold_usdt: 100_000_000,
        sweep_max_age_hours: 168,
        signer_url: "http://unused".into(),
        signer_token: "s".into(),
    }
}

/// Inserts a deposit-backed `created` mint intent directly (bypassing the API, since these
/// tests drive `verify_once` at the module level) with a given `deposit_tx_id` (or NULL for
/// the fallback-match path).
///
/// `expected_amount_usdt` is the DISCRIMINATED pay amount and is what the verifier matches
/// against; `amount_clt` is the intended (undiscriminated) figure and is deliberately never used
/// for matching. Keeping them distinct in every fixture is what makes these tests able to catch a
/// regression back to matching on `amount_clt`.
async fn seed_deposit_intent(
    pool: &PgPool,
    amount_clt: i64,
    expected_amount_usdt: i64,
    client_ref: &str,
    deposit_tx_id: Option<&str>,
) -> Uuid {
    seed_deposit_intent_at(pool, amount_clt, expected_amount_usdt, client_ref, deposit_tx_id, DEPOSIT_ADDR).await
}

/// As above, but naming the deposit address explicitly — used to prove evidence never crosses
/// between two intents' addresses.
async fn seed_deposit_intent_at(
    pool: &PgPool,
    amount_clt: i64,
    expected_amount_usdt: i64,
    client_ref: &str,
    deposit_tx_id: Option<&str>,
    deposit_address: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents
            (id, beneficiary, amount_clt, credit_ref, created_by, client_ref, deposit_tx_id, expected_amount_usdt,
             deposit_address)
         VALUES ($1, 'TBeneficiary1111111111111111111111', $2, $3, 'orchestrator', $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(amount_clt)
    .bind(format!("ref-{id}"))
    .bind(client_ref)
    .bind(deposit_tx_id)
    .bind(expected_amount_usdt)
    .bind(deposit_address)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn status_of(pool: &PgPool, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM mint_intents WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn trc20_transfer_json(tx_id: &str, to: &str, contract: &str, value: &str) -> serde_json::Value {
    trc20_transfer_json_at(tx_id, to, contract, value, chrono::Utc::now().timestamp_millis())
}

/// `block_timestamp` matters because the fallback (no-tx-hash) match is time-bounded: discriminator
/// amounts get recycled once an invoice goes terminal, so an old unclaimed transfer at the same
/// amount must not be swept up to back a stranger's later intent.
fn trc20_transfer_json_at(
    tx_id: &str,
    to: &str,
    contract: &str,
    value: &str,
    block_timestamp: i64,
) -> serde_json::Value {
    json!({"transaction_id": tx_id, "to": to, "value": value, "type": "Transfer",
           "token_info": {"address": contract}, "block_timestamp": block_timestamp})
}

/// A non-Transfer TRC-20 event (e.g. `Approval`) carries a `value` and a `to` but moves no
/// tokens — the verifier must not accept one as evidence.
fn trc20_event_json(tx_id: &str, to: &str, contract: &str, value: &str, event_type: &str) -> serde_json::Value {
    json!({"transaction_id": tx_id, "to": to, "value": value, "type": event_type,
           "token_info": {"address": contract},
           "block_timestamp": chrono::Utc::now().timestamp_millis()})
}

/// Mocks `POST /wallet/triggerconstantcontract` (balanceOf) with one canned 32-byte result word,
/// answering for ANY address — enough to prove the sum walks every address it is given.
async fn mount_balance_of(server: &MockServer, hex_word: &str) {
    Mock::given(method("POST"))
        .and(path("/wallet/triggerconstantcontract"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"constant_result": [hex_word]})))
        .mount(server)
        .await;
}

/// Mocks `POST /wallet/triggerconstantcontract` (balanceOf) so that only a request naming THIS
/// `address` sees `amount` (micro-USDT) — unlike `mount_balance_of`'s blanket any-address stub,
/// needed wherever two addresses must report two different balances within the same test.
async fn mount_balance(server: &MockServer, address: &str, amount: i64) {
    Mock::given(method("POST"))
        .and(path("/wallet/triggerconstantcontract"))
        .and(body_string_contains(address))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"constant_result": [format!("{amount:064x}")]})))
        .mount(server)
        .await;
}

async fn mount_trc20_list(server: &MockServer, transfers: Vec<serde_json::Value>) {
    mount_trc20_list_for(server, DEPOSIT_ADDR, transfers).await
}

/// Mocks the transfer list for ONE address — the verifier now queries the intent's own address, so
/// a mock mounted at a different one must not be found.
async fn mount_trc20_list_for(server: &MockServer, address: &str, transfers: Vec<serde_json::Value>) {
    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{address}/transactions/trc20")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": transfers})))
        .mount(server)
        .await;
}

/// Mocks `POST /walletsolidity/gettransactionbyid`, which is how confirmed depth is established:
/// the solidity node only serves irreversible blocks, so the transaction being ECHOED BACK is the
/// proof, and `{}` means "not final yet".
///
/// This previously mocked `GET /v1/transactions/{tx_id}` returning `{"confirmed": bool}`. Neither
/// the endpoint nor the field exists — TronGrid answers 404 — so these tests passed against a
/// fiction while the real has-tx_id path could never confirm anything. A mock is only evidence if
/// the shape it returns is the shape the service really receives.
async fn mount_transaction_confirmed(server: &MockServer, tx_id: &str, confirmed: bool) {
    let body = if confirmed { json!({"txID": tx_id}) } else { json!({}) };
    Mock::given(method("POST"))
        .and(path("/walletsolidity/gettransactionbyid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// A `"ret": null` response must read as "no execution result yet", NOT as a transport failure.
///
/// `#[serde(default)]` only covers the field being ABSENT. An explicit null fails to deserialize
/// into a bare `Vec`, and `SolidityTransaction` is shared with the DEPOSIT path — where every
/// `Err` becomes `Evidence::Transient` and the deposit retries until the 24h stuck-sweep hands it
/// to a human. So a null must be `Ok(false)`, never `Err`.
///
/// This fails against a bare `Vec<ContractResult>`: serde rejects null for a sequence, `.json()`
/// returns an error, and `transfer_succeeded` yields `Err` instead of `Ok(false)`.
#[tokio::test]
async fn a_null_ret_is_not_yet_successful_rather_than_a_read_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/walletsolidity/gettransactionbyid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"txID": "tx-null-ret", "ret": null})))
        .mount(&server)
        .await;
    let client = treasury_service::tron_verifier::TronClient::new(server.uri(), String::new());

    let succeeded = client
        .transfer_succeeded("tx-null-ret")
        .await
        .expect("a null ret must not surface as a transport error");
    assert!(!succeeded, "no execution result yet means not-successful, not paid");
}

/// Happy path, deposit_tx_id already known (Bitcart returned a hash): all four evidence
/// conditions hold, so the intent is approved and the custody event lands. Rerunning
/// `verify_once` (simulating a crash-then-restart of the poll loop) must NOT append a second
/// custody event — proven by actually calling it twice and counting rows, not assumed from
/// the ON CONFLICT clause alone.
#[tokio::test]
async fn happy_path_approves_and_ledgers_custody_exactly_once_across_a_rerun() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-happy", DEPOSIT_ADDR, USDT, "1000173")]).await;
    mount_transaction_confirmed(&server, "tx-happy", true).await;
    let config = test_config(server.uri());

    let id = seed_deposit_intent(&pool, 1_000_000, 1_000_173, "client-ref-happy", Some("tx-happy")).await;

    let approved_first = treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(approved_first, 1);
    assert_eq!(status_of(&pool, id).await, "approved");

    // Rerun — the exact scenario a crash-then-restart produces.
    let approved_second = treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(approved_second, 0, "already-approved intent must not be picked up again (status != created)");

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_deposit'")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "rerun must not double-count recorded reserve");

    // The OBSERVED amount (1,000,173 — includes the discriminator), not amount_clt (1,000,000).
    let (amount_usdt,): (i64,) = sqlx::query_as(
        "SELECT amount_usdt FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_deposit'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(amount_usdt, 1_000_173, "ledger must record the observed on-chain amount, not the intended amount_clt");

    let (outbox_n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM chain_outbox WHERE intent_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(outbox_n, 1, "exactly one outbox row, not duplicated by the rerun");

    let (verified_at,): (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT verified_at FROM mint_intents WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
    assert!(verified_at.is_some());
}

/// Happy path via the FALLBACK match (deposit_tx_id NULL — Bitcart's response lacked a hash):
/// the verifier must find the transfer by amount+recipient+contract alone and backfill
/// deposit_tx_id, then approve exactly as the has-hash path does.
#[tokio::test]
async fn fallback_match_backfills_deposit_tx_id_and_approves() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-fallback", DEPOSIT_ADDR, USDT, "2000042")]).await;
    let config = test_config(server.uri());

    let id = seed_deposit_intent(&pool, 2_000_000, 2_000_042, "client-ref-fallback", None).await;

    let approved = treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(approved, 1);
    assert_eq!(status_of(&pool, id).await, "approved");

    let (deposit_tx_id,): (Option<String>,) =
        sqlx::query_as("SELECT deposit_tx_id FROM mint_intents WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(deposit_tx_id.as_deref(), Some("tx-fallback"), "fallback match must backfill the tx id it found");
}

/// R24: the exact bug Task 12 fixed on the orchestrator's `custody.rs`, left standing on the side
/// that decides mints. A user's own deposit history, or a dust flood, can fill TronGrid's page 1
/// with nothing this intent cares about — under permanent addresses this is the normal case, not an
/// edge case, so the intent's real evidence sitting on page 2 must still be found and approved.
#[tokio::test]
async fn a_deposit_on_the_second_page_still_approves() {
    let pool = pool().await;
    let server = MockServer::start().await;

    // Page 1: a full page of filler rows to a DIFFERENT address, plus a cursor to page 2. The
    // fallback match's own `t.to == deposit_address` check would drop these anyway — the point is
    // they fill the page, so the only way the assertion below can pass is if page 2 was actually
    // requested.
    let filler: Vec<serde_json::Value> = (0..200)
        .map(|i| trc20_transfer_json(&format!("filler-{i}"), OTHER_ADDR, USDT, "1000000"))
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{DEPOSIT_ADDR}/transactions/trc20")))
        .and(query_param_is_missing("fingerprint"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": filler,
            "meta": { "fingerprint": "abc" },
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Page 2: the intent's real transfer.
    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{DEPOSIT_ADDR}/transactions/trc20")))
        .and(query_param("fingerprint", "abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [trc20_transfer_json("tx-page-two", DEPOSIT_ADDR, USDT, "7000099")],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let config = test_config(server.uri());
    let id = seed_deposit_intent(&pool, 7_000_000, 7_000_099, "client-ref-page-two", None).await;

    let approved = treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(approved, 1, "the intent's evidence sitting on page 2 must still approve it");
    assert_eq!(status_of(&pool, id).await, "approved");

    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_deposit'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1, "page 2's transfer must be ledgered exactly once");
    // Both `.expect(1)`s above are verified when `server` drops: page 2 never being requested (the
    // pre-fix behaviour) panics there too.
}

/// Evidence must never cross between two intents' addresses.
///
/// This test used to assert the opposite-shaped rule: that a LARGER transfer could not satisfy the
/// intent. That was correct only while every deposit shared one custody address, where the amount
/// was the sole thing separating payers and `>=` would have let one user's bigger deposit back
/// another's intent. With one address per intent that reasoning inverts — a larger transfer to THIS
/// address IS this payer overpaying (covered below) — and the real hazard moves to the address
/// itself: a transfer that landed somewhere else must never be claimed here.
#[tokio::test]
async fn evidence_at_another_intents_address_is_never_claimed() {
    let pool = pool().await;
    let server = MockServer::start().await;
    // A generous transfer, but at a DIFFERENT intent's address. Nothing at this intent's own.
    mount_trc20_list_for(&server, OTHER_ADDR, vec![trc20_transfer_json("tx-someone-elses", OTHER_ADDR, USDT, "50000000")]).await;
    mount_trc20_list(&server, vec![]).await;
    let config = test_config(server.uri());

    let id = seed_deposit_intent(&pool, 2_000_000, 2_000_042, "client-ref-bigger", None).await;

    let approved = treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(approved, 0, "a transfer to another intent's address must not satisfy this one");
    assert_eq!(
        status_of(&pool, id).await,
        "created",
        "unmatched is transient (deposit not yet observed), never rejected and never approved"
    );
    let (events,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM treasury_events WHERE kind = 'custody_deposit'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(events, 0, "no custody event may be ledgered from someone else's address");
}

/// The other half of the inversion: an overpayment at the intent's OWN address now verifies, where
/// the shared-address design had to send it to manual review. The ledger must record what ARRIVED,
/// not what was expected — recording the expectation would build a permanent gap into the
/// reconciliation cross-check against custody.
#[tokio::test]
async fn an_overpayment_at_the_intents_own_address_verifies_and_ledgers_what_arrived() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-generous", DEPOSIT_ADDR, USDT, "9000000")]).await;
    let config = test_config(server.uri());

    let id = seed_deposit_intent(&pool, 2_000_000, 2_000_000, "client-ref-generous", None).await;

    let approved = treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(approved, 1, "an overpayment at this intent's own address is this payer's money");
    assert_eq!(status_of(&pool, id).await, "approved");

    let (observed,): (i64,) = sqlx::query_as(
        "SELECT amount_usdt FROM treasury_events WHERE kind = 'custody_deposit' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(observed, 9_000_000, "the ledger must record what arrived, not the expected figure");
}

/// Same amount, but the transfer predates the match window: discriminator slots are recycled once
/// an invoice goes terminal, so an old unclaimed transfer at a reused amount must not back a later
/// intent. Only the fallback path is time-bounded — a known tx hash is identity enough.
#[tokio::test]
async fn fallback_must_not_claim_a_transfer_older_than_the_match_window() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let config = test_config(server.uri());
    let stale_ms = (chrono::Utc::now() - chrono::Duration::hours(config.deposit_match_window_hours + 1))
        .timestamp_millis();
    mount_trc20_list(
        &server,
        vec![trc20_transfer_json_at("tx-stale", DEPOSIT_ADDR, USDT, "4000077", stale_ms)],
    )
    .await;

    let id = seed_deposit_intent(&pool, 4_000_000, 4_000_077, "client-ref-stale", None).await;

    let approved = treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(approved, 0, "a transfer older than the window must not be claimed");
    assert_eq!(status_of(&pool, id).await, "created");
}

/// An `Approval` event to the custody address for exactly the expected amount must not back a
/// mint. It carries a `to` and a `value` like a Transfer does, but no tokens moved — approving on
/// one would mint CLT against a deposit that never arrived. Verified against a live TronGrid
/// response that `type` is present and carries the event kind.
#[tokio::test]
async fn approval_event_never_backs_a_mint() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_trc20_list(
        &server,
        vec![trc20_event_json("tx-approval", DEPOSIT_ADDR, USDT, "6000088", "Approval")],
    )
    .await;
    mount_transaction_confirmed(&server, "tx-approval", true).await;
    let config = test_config(server.uri());

    // Both paths: the known-hash path rejects it outright as hard evidence...
    let known = seed_deposit_intent(&pool, 6_000_000, 6_000_088, "client-ref-approval", Some("tx-approval")).await;
    // ...and the fallback path must not select it at all.
    // The fallback intent sits at OTHER_ADDR so its own query never sees this event at all —
    // address-scoped isolation — while the known/hash assertion above is what proves an Approval
    // is rejected by type.
    let fallback =
        seed_deposit_intent_at(&pool, 6_000_000, 6_000_088, "client-ref-approval-fb", None, OTHER_ADDR).await;

    let approved = treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(approved, 0, "an Approval event must never approve a mint");
    assert_eq!(status_of(&pool, known).await, "rejected", "a named Approval tx is hard evidence, not transient");
    assert_eq!(status_of(&pool, fallback).await, "created", "the fallback must not select a non-Transfer event");

    let (events,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM treasury_events WHERE kind = 'custody_deposit'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(events, 0, "no custody may be ledgered from an event that moved no tokens");
}

/// Wrong recipient: a confirmed transfer of the right amount and token, but to some other
/// address, is real evidence this is not our deposit — must reject, not retry.
#[tokio::test]
async fn wrong_recipient_rejects() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-wrong-to", "TSomeoneElseEntirelyXXXXXXXXXXXXX", USDT, "1000000")]).await;
    mount_transaction_confirmed(&server, "tx-wrong-to", true).await;
    let config = test_config(server.uri());

    let id = seed_deposit_intent(&pool, 1_000_000, 1_000_000, "client-ref-wrong-to", Some("tx-wrong-to")).await;
    treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "rejected");
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alerts WHERE source = 'tron_verifier'").fetch_one(&pool).await.unwrap();
    assert!(n >= 1, "a hard mismatch must leave an audit trail");
}

/// Wrong token contract: a worthless token sent in the right amount to the right address must
/// not pass — the plan's explicit example of evidence that looks superficially right.
#[tokio::test]
async fn wrong_token_contract_rejects() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-wrong-token", DEPOSIT_ADDR, "TWorthlessScamTokenXXXXXXXXXXXXXX", "1000000")]).await;
    mount_transaction_confirmed(&server, "tx-wrong-token", true).await;
    let config = test_config(server.uri());

    let id = seed_deposit_intent(&pool, 1_000_000, 1_000_000, "client-ref-wrong-token", Some("tx-wrong-token")).await;
    treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "rejected");
}

/// Insufficient amount: a confirmed transfer to the right address in the right token, but
/// below the expected amount, must reject rather than approve a partial deposit.
#[tokio::test]
async fn insufficient_amount_rejects() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-short", DEPOSIT_ADDR, USDT, "999999")]).await;
    mount_transaction_confirmed(&server, "tx-short", true).await;
    let config = test_config(server.uri());

    let id = seed_deposit_intent(&pool, 1_000_000, 1_000_000, "client-ref-short", Some("tx-short")).await;
    treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "rejected");
}

/// The central distinction under test: TronGrid returning a hard error (simulating an outage)
/// must NEVER reject the intent. It must stay `created` for the next poll tick — rejecting a
/// real user's deposit because OUR infrastructure failed is exactly the failure mode the brief
/// calls the worst possible outcome.
#[tokio::test]
async fn transient_trongrid_failure_never_rejects() {
    let pool = pool().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{DEPOSIT_ADDR}/transactions/trc20")))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let config = test_config(server.uri());

    let id = seed_deposit_intent(&pool, 1_000_000, 1_000_000, "client-ref-outage", Some("tx-during-outage")).await;
    let approved = treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();

    assert_eq!(approved, 0);
    assert_eq!(status_of(&pool, id).await, "created", "a TronGrid outage must leave the intent untouched, never rejected");
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM treasury_events").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0, "no custody event on a transient failure");
}

/// A confirmed transfer that TronGrid has not yet marked `confirmed` (still below the
/// solidity depth) is ALSO transient, not a mismatch — the money is real, it just hasn't
/// settled deep enough yet.
#[tokio::test]
async fn not_yet_confirmed_is_transient_not_a_rejection() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-pending", DEPOSIT_ADDR, USDT, "1000000")]).await;
    mount_transaction_confirmed(&server, "tx-pending", false).await;
    let config = test_config(server.uri());

    let id = seed_deposit_intent(&pool, 1_000_000, 1_000_000, "client-ref-pending", Some("tx-pending")).await;
    treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "created", "not-yet-confirmed must retry, never reject");
}

/// A `created` intent's own age past 24h gets a p1 alert (the stuck-intent sweep) even though
/// its status is untouched — the sweep is a separate signal from the pass/reject verdict.
#[tokio::test]
async fn stuck_intent_past_24h_pages_p1() {
    let pool = pool().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{DEPOSIT_ADDR}/transactions/trc20")))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let config = test_config(server.uri());

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents (id, beneficiary, amount_clt, credit_ref, created_by, client_ref, deposit_tx_id, expected_amount_usdt, deposit_address, created_at)
         VALUES ($1, 'TBeneficiary1111111111111111111111', 1000000, $2, 'orchestrator', 'client-ref-stuck', 'tx-stuck', 1000000, $3, now() - interval '25 hours')",
    )
    .bind(id)
    .bind(format!("ref-{id}"))
    .bind(DEPOSIT_ADDR)
    .execute(&pool)
    .await
    .unwrap();

    treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alerts WHERE severity = 'p1' AND source = 'tron_verifier'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(n >= 1, "an intent unresolved for over 24h must page p1");
}

/// The gap the brief requires closing: the SAME on-chain transfer cannot back two different
/// mint intents. Direct DB-level proof of the constraint, named explicitly so this can't pass
/// on an incidental error.
#[tokio::test]
async fn unique_index_refuses_a_second_intent_claiming_the_same_deposit_tx() {
    let pool = pool().await;
    let first = seed_deposit_intent(&pool, 1_000_000, 1_000_000, "client-ref-a", Some("tx-shared")).await;
    assert_eq!(status_of(&pool, first).await, "created");

    let second_id = Uuid::new_v4();
    // A DISTINCT address on purpose: without one, deposit_backed_needs_address fires first and this
    // test would pass on the wrong constraint. The point is the tx-id index specifically.
    let err = sqlx::query(
        "INSERT INTO mint_intents (id, beneficiary, amount_clt, credit_ref, created_by, client_ref, deposit_tx_id, expected_amount_usdt, deposit_address)
         VALUES ($1, 'TBeneficiary2222222222222222222222', 1000000, $2, 'orchestrator', 'client-ref-b', 'tx-shared', 1000000, $3)",
    )
    .bind(second_id)
    .bind(format!("ref-{second_id}"))
    .bind(OTHER_ADDR)
    .execute(&pool)
    .await
    .unwrap_err();

    assert_eq!(
        err.as_database_error().and_then(|e| e.constraint()),
        Some("uq_mint_intents_deposit_tx"),
        "rejection must come from the deposit_tx_id index by name, not some incidental constraint"
    );
}

/// End-to-end proof that the backfill path (fallback match, no pre-known deposit_tx_id) also
/// respects the unique index rather than approving on a transfer another intent already
/// claimed: seed a SECOND intent expecting the SAME transfer's amount as a first intent that
/// already holds that deposit_tx_id, and confirm the second is rejected, not approved.
#[tokio::test]
async fn fallback_backfill_losing_the_race_rejects_not_approves() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-claimed", DEPOSIT_ADDR, USDT, "3000000")]).await;
    // The SAME transaction id presented at a second address. Per-address deposits make this
    // unreachable in normal operation — one transfer lands at exactly one address — so this is
    // deliberately simulating the impossible to keep the uq_mint_intents_deposit_tx guard exercised.
    // Defence in depth is only defence while something still proves it fires.
    mount_trc20_list_for(&server, OTHER_ADDR, vec![trc20_transfer_json("tx-claimed", OTHER_ADDR, USDT, "3000000")])
        .await;
    let config = test_config(server.uri());

    // Another intent already claimed this exact transfer.
    let _already_claimed = seed_deposit_intent(&pool, 3_000_000, 3_000_000, "client-ref-first-claim", Some("tx-claimed")).await;

    // A second, different intent's fallback match finds the SAME transfer id at its own address.
    let racer =
        seed_deposit_intent_at(&pool, 3_000_000, 3_000_000, "client-ref-racer", None, OTHER_ADDR).await;

    treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();

    assert_eq!(
        status_of(&pool, racer).await,
        "rejected",
        "losing the deposit_tx_id backfill race must reject THIS intent, never approve a second mint against a claimed transfer"
    );
    let (custody_events,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM treasury_events WHERE kind = 'custody_deposit'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(custody_events, 0, "the racer must not have ledgered a custody event for a transfer it lost the claim on");
}

/// A deposit-backed intent with no `expected_amount_usdt` is unverifiable — the verifier would
/// have nothing to match on-chain transfers against except `amount_clt`, which every depositor of
/// the same round number shares. Refuse at the door rather than create a row that can only ever
/// age into manual review.
#[tokio::test]
async fn deposit_backed_create_without_expected_amount_is_refused() {
    let pool = pool().await;
    let mut config = test_config("http://unused".to_string());
    config.initiator_token = "test-initiator".to_string();
    let app = treasury_service::api::router(pool.clone(), config);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/internal/mint-intents")
                .header("authorization", "Bearer test-initiator")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"beneficiary":"TBeneficiary1111111111111111111111","amount_clt":1000000,"client_ref":"deposit-no-expected"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mint_intents WHERE client_ref = 'deposit-no-expected'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "no intent row may be created for an unverifiable deposit-backed request");
}

/// The API-level requirement: a duplicate `client_ref` on POST /internal/mint-intents replays
/// the existing intent (200, same id) rather than creating a second row.
#[tokio::test]
async fn duplicate_client_ref_create_replays_instead_of_duplicating() {
    let pool = pool().await;
    let mut config = test_config("http://unused".to_string());
    config.initiator_token = "test-initiator".to_string();
    let app = treasury_service::api::router(pool.clone(), config);

    let make_request = || {
        Request::builder()
            .method("POST")
            .uri("/internal/mint-intents")
            .header("authorization", "Bearer test-initiator")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"beneficiary":"TBeneficiary1111111111111111111111","amount_clt":1000000,"expected_amount_usdt":1000291,"deposit_address":"TCustodyAddressXXXXXXXXXXXXXXXXXXX","client_ref":"deposit-intent-abc","deposit_tx_id":"tx-abc"}"#,
            ))
            .unwrap()
    };

    let first = app.clone().oneshot(make_request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_body = axum::body::to_bytes(first.into_body(), usize::MAX).await.unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    let first_id = first_json["id"].as_str().unwrap().to_string();

    let second = app.oneshot(make_request()).await.unwrap();
    assert_eq!(second.status(), StatusCode::OK, "replay must be 200, not a fresh 201");
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX).await.unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(second_json["id"].as_str().unwrap(), first_id, "replay must return the SAME intent id");

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mint_intents WHERE client_ref = 'deposit-intent-abc'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "duplicate client_ref must not create a second row");
}

/// Plan C 5b: `GET /internal/mint-intents/:id` is new in this task — the bridge worker has to
/// poll a deposit-backed intent's status and there was no route to read one by id before. Any
/// role may call it (readonly is what the bridge actually uses), and it returns the same
/// `intent_json` shape the create/approve routes already do, plus 404 for an id no row holds.
#[tokio::test]
async fn get_mint_intent_by_id_returns_intent_json_with_readonly_token() {
    let pool = pool().await;
    let mut config = test_config("http://unused".to_string());
    config.initiator_token = "test-initiator".to_string();
    config.readonly_token = "test-readonly".to_string();
    let app = treasury_service::api::router(pool.clone(), config);

    let create_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/internal/mint-intents")
                .header("authorization", "Bearer test-initiator")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"beneficiary":"TBeneficiary1111111111111111111111","amount_clt":1000000,"expected_amount_usdt":1000456,"deposit_address":"TCustodyAddressXXXXXXXXXXXXXXXXXXX","client_ref":"deposit-getbyid","deposit_tx_id":"tx-getbyid"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::CREATED);
    let created_body = axum::body::to_bytes(create_res.into_body(), usize::MAX).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&created_body).unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let get_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/internal/mint-intents/{id}"))
                .header("authorization", "Bearer test-readonly")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_res.status(), StatusCode::OK, "the bridge's readonly token must be able to read a mint intent by id");
    let get_body = axum::body::to_bytes(get_res.into_body(), usize::MAX).await.unwrap();
    let get_json: serde_json::Value = serde_json::from_slice(&get_body).unwrap();
    assert_eq!(get_json["id"].as_str().unwrap(), id);
    assert_eq!(get_json["status"].as_str().unwrap(), "created");
    assert_eq!(get_json["client_ref"].as_str().unwrap(), "deposit-getbyid");

    let missing = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/internal/mint-intents/{}", Uuid::new_v4()))
                .header("authorization", "Bearer test-readonly")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND, "an id no row holds must 404, not 500 or an empty 200");
}

/// `/internal/reserve-status` must expose `daily_headroom_clt` (5b's blocked dependency) and
/// it must actually shrink as approved-or-later mint intents accumulate within the 24h window
/// — proving it reuses the real daily-cap sum, not a hardcoded/static number.
#[tokio::test]
async fn reserve_status_reports_daily_headroom_that_shrinks_with_approved_mints() {
    let pool = pool().await;
    let mut config = test_config("http://unused".to_string());
    config.readonly_token = "test-readonly".to_string();
    config.daily_mint_cap_clt = 10_000_000;
    let app = treasury_service::api::router(pool.clone(), config);

    let get_headroom = |app: axum::Router| async move {
        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/internal/reserve-status")
                    .header("authorization", "Bearer test-readonly")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        json["daily_headroom_clt"].as_i64().unwrap()
    };

    let before = get_headroom(app.clone()).await;
    assert_eq!(before, 10_000_000, "no mints yet — full cap is headroom");

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents (id, beneficiary, amount_clt, status, credit_ref, created_by, approved_by)
         VALUES ($1, 'bob', 4000000, 'approved', $2, 'alice', 'carol')",
    )
    .bind(id)
    .bind(format!("ref-{id}"))
    .execute(&pool)
    .await
    .unwrap();

    let after = get_headroom(app).await;
    assert_eq!(after, 6_000_000, "headroom must shrink by exactly the approved mint's amount");
}

/// The reserve is a SUM across three buckets now: the main custody address, every unswept
/// per-intent deposit address, and the payout float.
///
/// Reading only the main treasury address reports a reserve near zero while unswept deposits sit
/// elsewhere, and leaving the float out makes topping it up from custody look like the reserve
/// shrinking. Neither is a halt risk — `judge` keys on the LEDGER's `custody_reported`, and
/// `trongrid_balance` is a cross-check column that plays no part in any branch — but a fourth source
/// that is permanently wrong is worse than one that is absent: people stop reading it, and then
/// disbelieve it on the day it is right.
#[tokio::test]
async fn reserve_balance_sums_the_main_address_and_every_unswept_deposit_address() {
    let server = MockServer::start().await;
    // balanceOf returns a 32-byte hex word; 1_000_000 = 0xF4240, 2_500_000 = 0x2625A0.
    mount_balance_of(&server, "00000000000000000000000000000000000000000000000000000000000f4240").await;
    let client = treasury_service::tron_verifier::TronClient::new(server.uri(), "k".into());

    let only_main = client.get_reserve_balance(REAL_MAIN, &[], FLOAT, USDT).await.unwrap();
    assert_eq!(only_main, 2_000_000, "with nothing unswept the reserve is main + float");

    // Four addresses now, each answering the same mocked balance.
    let with_unswept = client
        .get_reserve_balance(REAL_MAIN, &[REAL_UNSWEPT_A.to_string(), REAL_UNSWEPT_B.to_string()], FLOAT, USDT)
        .await
        .unwrap();
    assert_eq!(with_unswept, 4_000_000, "main + two unswept addresses + float must all be counted");
}

/// A failure on ANY address must fail the whole sum. A partial total understates the reserve, which
/// looks exactly like a shortfall — the one direction a reserve figure must never silently err in.
#[tokio::test]
async fn a_single_unreadable_address_fails_the_whole_reserve_sum() {
    let server = MockServer::start().await;
    // No balanceOf mock mounted at all, so the very first read fails.
    let client = treasury_service::tron_verifier::TronClient::new(server.uri(), "k".into());
    let err = client
        .get_reserve_balance(REAL_MAIN, &[REAL_UNSWEPT_A.to_string()], FLOAT, USDT)
        .await
        .expect_err("an unreadable address must not yield a partial total");
    assert!(!err.is_empty(), "the failure must be reported, not swallowed into a smaller number");
}

/// The trap this test guards: the float is funded FROM custody, so if it is not counted, the first
/// top-up looks like custody shrinking and reconciliation reports a shortfall that halts minting.
/// That is exactly the failure this task exists to prevent.
#[tokio::test]
async fn the_reserve_includes_the_payout_float() {
    let server = MockServer::start().await;
    mount_balance(&server, REAL_MAIN, 700).await;
    mount_balance(&server, FLOAT, 300).await;

    let client = treasury_service::tron_verifier::TronClient::new(server.uri(), String::new());
    let total = client.get_reserve_balance(REAL_MAIN, &[], FLOAT, USDT).await.unwrap();

    assert_eq!(total, 1000, "float USDT is reserve backing CLT, not spare money");
}
