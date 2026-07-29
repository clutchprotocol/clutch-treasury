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
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CUSTODY: &str = "TCustodyAddressXXXXXXXXXXXXXXXXXXX";
const USDT: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
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
        trongrid_url,
        trongrid_api_key: "test-trongrid-key".into(),
        custody_tron_address: CUSTODY.into(),
        usdt_contract: USDT.into(),
        deposit_confirmations: 19,
        deposit_match_window_hours: 24,
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
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents
            (id, beneficiary, amount_clt, credit_ref, created_by, client_ref, deposit_tx_id, expected_amount_usdt)
         VALUES ($1, 'TBeneficiary1111111111111111111111', $2, $3, 'orchestrator', $4, $5, $6)",
    )
    .bind(id)
    .bind(amount_clt)
    .bind(format!("ref-{id}"))
    .bind(client_ref)
    .bind(deposit_tx_id)
    .bind(expected_amount_usdt)
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
    json!({"transaction_id": tx_id, "to": to, "value": value,
           "token_info": {"address": contract}, "block_timestamp": block_timestamp})
}

async fn mount_trc20_list(server: &MockServer, transfers: Vec<serde_json::Value>) {
    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{CUSTODY}/transactions/trc20")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": transfers})))
        .mount(server)
        .await;
}

async fn mount_transaction_confirmed(server: &MockServer, tx_id: &str, confirmed: bool) {
    Mock::given(method("GET"))
        .and(path(format!("/v1/transactions/{tx_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"confirmed": confirmed})))
        .mount(server)
        .await;
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
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-happy", CUSTODY, USDT, "1000173")]).await;
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
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-fallback", CUSTODY, USDT, "2000042")]).await;
    let config = test_config(server.uri());

    let id = seed_deposit_intent(&pool, 2_000_000, 2_000_042, "client-ref-fallback", None).await;

    let approved = treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(approved, 1);
    assert_eq!(status_of(&pool, id).await, "approved");

    let (deposit_tx_id,): (Option<String>,) =
        sqlx::query_as("SELECT deposit_tx_id FROM mint_intents WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(deposit_tx_id.as_deref(), Some("tx-fallback"), "fallback match must backfill the tx id it found");
}

/// The fallback must match the DISCRIMINATED amount exactly, never `>=` and never `amount_clt`.
///
/// This is the defect this test exists for: on the shared static custody address the discriminated
/// amount is the only thing separating one payer from another. A `>=` match (or matching on
/// `amount_clt`, which every depositor of the same round number shares) approves this intent on
/// somebody else's larger transfer, ledgers that stranger's full amount as this deposit's custody,
/// and locks the rightful depositor out of their own transfer via uq_mint_intents_deposit_tx.
///
/// A stranger's 50 USDT transfer is on-chain; our intent expects 2.000042. Nothing may be approved.
#[tokio::test]
async fn fallback_must_not_claim_a_larger_transfer_from_another_payer() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-someone-elses", CUSTODY, USDT, "50000000")]).await;
    let config = test_config(server.uri());

    let id = seed_deposit_intent(&pool, 2_000_000, 2_000_042, "client-ref-bigger", None).await;

    let approved = treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(approved, 0, "a larger transfer from another payer must not satisfy this intent");
    assert_eq!(
        status_of(&pool, id).await,
        "created",
        "unmatched is transient (deposit not yet observed), never rejected and never approved"
    );
    let (events,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM treasury_events WHERE kind = 'custody_deposit'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(events, 0, "no custody must be ledgered off a stranger's transfer");
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
        vec![trc20_transfer_json_at("tx-stale", CUSTODY, USDT, "4000077", stale_ms)],
    )
    .await;

    let id = seed_deposit_intent(&pool, 4_000_000, 4_000_077, "client-ref-stale", None).await;

    let approved = treasury_service::tron_verifier::verify_once(&pool, &config).await.unwrap();
    assert_eq!(approved, 0, "a transfer older than the window must not be claimed");
    assert_eq!(status_of(&pool, id).await, "created");
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
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-wrong-token", CUSTODY, "TWorthlessScamTokenXXXXXXXXXXXXXX", "1000000")]).await;
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
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-short", CUSTODY, USDT, "999999")]).await;
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
        .and(path(format!("/v1/accounts/{CUSTODY}/transactions/trc20")))
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
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-pending", CUSTODY, USDT, "1000000")]).await;
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
        .and(path(format!("/v1/accounts/{CUSTODY}/transactions/trc20")))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let config = test_config(server.uri());

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents (id, beneficiary, amount_clt, credit_ref, created_by, client_ref, deposit_tx_id, expected_amount_usdt, created_at)
         VALUES ($1, 'TBeneficiary1111111111111111111111', 1000000, $2, 'orchestrator', 'client-ref-stuck', 'tx-stuck', 1000000, now() - interval '25 hours')",
    )
    .bind(id)
    .bind(format!("ref-{id}"))
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
    let err = sqlx::query(
        "INSERT INTO mint_intents (id, beneficiary, amount_clt, credit_ref, created_by, client_ref, deposit_tx_id, expected_amount_usdt)
         VALUES ($1, 'TBeneficiary2222222222222222222222', 1000000, $2, 'orchestrator', 'client-ref-b', 'tx-shared', 1000000)",
    )
    .bind(second_id)
    .bind(format!("ref-{second_id}"))
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
    mount_trc20_list(&server, vec![trc20_transfer_json("tx-claimed", CUSTODY, USDT, "3000000")]).await;
    let config = test_config(server.uri());

    // Another intent already claimed this exact transfer.
    let _already_claimed = seed_deposit_intent(&pool, 3_000_000, 3_000_000, "client-ref-first-claim", Some("tx-claimed")).await;

    // A second, different intent's fallback match finds the SAME transfer (e.g. two deposit
    // intents that happen to land on the same amount, or an operational duplicate).
    let racer = seed_deposit_intent(&pool, 3_000_000, 3_000_000, "client-ref-racer", None).await;

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
                r#"{"beneficiary":"TBeneficiary1111111111111111111111","amount_clt":1000000,"expected_amount_usdt":1000291,"client_ref":"deposit-intent-abc","deposit_tx_id":"tx-abc"}"#,
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
                    r#"{"beneficiary":"TBeneficiary1111111111111111111111","amount_clt":1000000,"expected_amount_usdt":1000456,"client_ref":"deposit-getbyid","deposit_tx_id":"tx-getbyid"}"#,
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
