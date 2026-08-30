use std::sync::atomic::{AtomicUsize, Ordering};

use sqlx::PgPool;
use treasury_service::intents::create_redemption_intent;
use treasury_service::payout::{self, HttpPayoutSigner, PayoutReply, PayoutSigner};
use treasury_service::watcher::confirm_burn;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn pool() -> PgPool {
    // Each test BINARY gets its own database. --test-threads=1 only serialises tests WITHIN a
    // binary; cargo runs binaries in PARALLEL, and every pool() here TRUNCATEs shared tables —
    // so binaries were wiping each other mid-test. That produced a ~1-in-6 flake that moved
    // between tests run to run (see progress.md).
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_tre_redemption");
    if !<sqlx::Postgres as sqlx::migrate::MigrateDatabase>::database_exists(&url).await.unwrap_or(false) {
        <sqlx::Postgres as sqlx::migrate::MigrateDatabase>::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE treasury_events, redemption_intents, alerts RESTART IDENTITY CASCADE")
        .execute(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn matching_burn_confirms_and_ledgers_once() {
    let pool = pool().await;
    let intent = create_redemption_intent(&pool, "0xaaaa000000000000000000000000000000000001", "TTronAddr111", 2_000_000).await.unwrap();

    for _ in 0..2 {
        confirm_burn(&pool, &intent.redemption_ref, "0xaaaa000000000000000000000000000000000001", 2_000_000, "0xburn1").await.unwrap();
    }
    let (status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(intent.id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "payout_pending");
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM treasury_events WHERE intent_id = $1 AND kind = 'burn_redeemed'")
        .bind(intent.id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn mismatched_burn_fails_intent_never_pays() {
    let pool = pool().await;
    let intent = create_redemption_intent(&pool, "0xaaaa000000000000000000000000000000000002", "TTronAddr222", 2_000_000).await.unwrap();

    // Right ref, wrong amount — someone burned the wrong sum against our ref.
    confirm_burn(&pool, &intent.redemption_ref, "0xaaaa000000000000000000000000000000000002", 1_999_999, "0xburn2").await.unwrap();
    let (status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(intent.id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "failed");
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM alerts WHERE severity = 'p1'")
        .fetch_one(&pool).await.unwrap();
    assert!(n >= 1);
}

async fn signer_replying(status: u16, body: serde_json::Value) -> (MockServer, HttpPayoutSigner) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/internal/payout"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    let signer = HttpPayoutSigner {
        http: reqwest::Client::new(),
        base_url: server.uri(),
        token: "t".into(),
    };
    (server, signer)
}

#[tokio::test]
async fn a_paid_reply_with_a_tx_id_is_paid() {
    let (_s, signer) = signer_replying(200, serde_json::json!({"status": "paid", "tx_id": "abc"})).await;
    assert_eq!(
        signer.pay(Uuid::new_v4(), "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", 5).await,
        PayoutReply::Paid { tx_id: "abc".into() }
    );
}

#[tokio::test]
async fn a_paid_reply_without_a_tx_id_is_ambiguous_not_paid() {
    // It claimed success but cannot name the transaction. It may well have broadcast, so this must
    // NOT be retried — treating it as a plain failure is how a burn gets paid twice.
    let (_s, signer) = signer_replying(200, serde_json::json!({"status": "paid"})).await;
    let reply = signer.pay(Uuid::new_v4(), "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", 5).await;
    assert!(matches!(reply, PayoutReply::Ambiguous(_)), "got {reply:?}");
}

#[tokio::test]
async fn a_400_is_refused_because_the_signer_rejected_the_shape() {
    // The one status that proves nothing was broadcast: the signer refused the request itself.
    let (_s, signer) = signer_replying(400, serde_json::json!({"error": "bad"})).await;
    let reply = signer.pay(Uuid::new_v4(), "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", 5).await;
    assert!(matches!(reply, PayoutReply::Refused(_)), "got {reply:?}");
}

#[tokio::test]
async fn a_500_is_ambiguous_not_refused() {
    // A 500 can follow a broadcast that then failed to report. Classifying it Refused would make
    // it retryable, and the retry would pay the same burn again.
    let (_s, signer) = signer_replying(500, serde_json::json!({"error": "boom"})).await;
    let reply = signer.pay(Uuid::new_v4(), "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", 5).await;
    assert!(matches!(reply, PayoutReply::Ambiguous(_)), "got {reply:?}");
}

#[tokio::test]
async fn an_unrecognised_status_is_ambiguous() {
    // A newer signer may describe a broadcast this version does not understand. Never Refused.
    let (_s, signer) = signer_replying(200, serde_json::json!({"status": "teleported"})).await;
    let reply = signer.pay(Uuid::new_v4(), "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", 5).await;
    assert!(matches!(reply, PayoutReply::Ambiguous(_)), "got {reply:?}");
}

#[tokio::test]
async fn a_float_dry_reply_is_refused_and_carries_its_numbers() {
    let (_s, signer) = signer_replying(200, serde_json::json!({
        "status": "float_dry", "float_address": "TKTuTvBn4qZpeYFuXz1SuL1B94NgtK5EnT",
        "have_usdt": 0, "need_usdt": 5,
    })).await;
    assert_eq!(
        signer.pay(Uuid::new_v4(), "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", 5).await,
        PayoutReply::FloatDry {
            float_address: "TKTuTvBn4qZpeYFuXz1SuL1B94NgtK5EnT".into(),
            have_usdt: 0,
            need_usdt: 5,
        }
    );
}

// --- drain_once: claim-before-call and the double-pay guard ---
//
// The property under test throughout this section: an intent is claimed (`payout_submitted`,
// committed) before the signer is ever called, so the only question a reply has to answer is
// whether it PROVED no broadcast happened. Only that proof earns a trip back to `payout_pending`.
// Anything else — most of all `Ambiguous` — stays claimed forever and alerts. A retry after an
// unproven reply is not a retry, it is a second payout for a burn that was already paid once.

/// Counts calls so a test can assert the signer was — or, just as important, was NOT — called
/// again on a second pass.
struct CountingSigner {
    reply: PayoutReply,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl PayoutSigner for CountingSigner {
    async fn pay(&self, _intent_id: Uuid, _to: &str, _amount_usdt: i64) -> PayoutReply {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.reply {
            PayoutReply::Paid { tx_id } => PayoutReply::Paid { tx_id: tx_id.clone() },
            PayoutReply::Ambiguous(m) => PayoutReply::Ambiguous(m.clone()),
            PayoutReply::CapExceeded { limit_usdt } => PayoutReply::CapExceeded { limit_usdt: *limit_usdt },
            PayoutReply::FloatDry { float_address, have_usdt, need_usdt } => PayoutReply::FloatDry {
                float_address: float_address.clone(),
                have_usdt: *have_usdt,
                need_usdt: *need_usdt,
            },
            PayoutReply::NeedsTrx => PayoutReply::NeedsTrx,
            PayoutReply::Refused(m) => PayoutReply::Refused(m.clone()),
        }
    }
}

/// A redemption sitting at payout_pending with its burn already confirmed.
async fn pending_redemption(pool: &PgPool, amount_clt: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO redemption_intents (id, redeemer_address, payout_address, amount_clt, status, redemption_ref, burn_tx_hash)
         VALUES ($1, '0xabc', 'TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK', $2, 'payout_pending', $3, '0xburn')",
    )
    .bind(id)
    .bind(amount_clt)
    .bind(format!("{:064x}", id.as_u128()))
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Same builder as `db_sweeper.rs`'s `config()`, minus its two knobs this file never varies
/// (`trongrid_url`, `sweep_threshold_usdt`) — every `drain_once` test either uses these
/// defaults outright or overrides a field on the returned value (`daily_payout_cap_clt`).
fn config() -> treasury_service::configuration::AppConfig {
    treasury_service::configuration::AppConfig {
        http_addr: "0.0.0.0:0".into(),
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
        trongrid_url: "http://unused".into(),
        trongrid_api_key: "k".into(),
        custody_tron_address: "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".into(),
        payout_float_address: "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY".into(),
        usdt_contract: "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".into(),
        deposit_confirmations: 19,
        deposit_match_window_hours: 24,
        sweep_threshold_usdt: 100_000_000,
        sweep_max_age_hours: 168,
        signer_url: "http://unused".into(),
        signer_token: "s".into(),
    }
}

#[tokio::test]
async fn an_ambiguous_payout_is_never_retried() {
    // THE regression test for this rail. A lost response after a possible broadcast must leave the
    // intent parked and alerting, not queued for another attempt that could pay the same burn twice.
    let pool = pool().await;
    let id = pending_redemption(&pool, 10_000_000).await;
    let signer = CountingSigner {
        reply: PayoutReply::Ambiguous("timeout".into()),
        calls: AtomicUsize::new(0),
    };
    let cfg = config();

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1, "second pass must not re-call the signer");

    let (status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "payout_submitted");

    let (alerts,): (i64,) = sqlx::query_as("SELECT count(*) FROM alerts WHERE severity = 'p1' AND source = 'payout'")
        .fetch_one(&pool).await.unwrap();
    assert!(alerts >= 1, "an ambiguous payout must alert — it needs a human");
}

#[tokio::test]
async fn a_refusal_returns_the_intent_for_retry() {
    let pool = pool().await;
    let id = pending_redemption(&pool, 10_000_000).await;
    let signer = CountingSigner {
        reply: PayoutReply::FloatDry {
            float_address: "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY".into(),
            have_usdt: 0,
            need_usdt: 10_000_000,
        },
        calls: AtomicUsize::new(0),
    };
    let cfg = config();

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    let (status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "payout_pending", "a proven non-broadcast is retryable");
}

#[tokio::test]
async fn a_paid_payout_records_the_tx_and_waits_for_confirmation() {
    let pool = pool().await;
    let id = pending_redemption(&pool, 10_000_000).await;
    let signer = CountingSigner {
        reply: PayoutReply::Paid { tx_id: "abc123".into() },
        calls: AtomicUsize::new(0),
    };

    payout::drain_once(&pool, &config(), &signer).await.unwrap();

    let (status, payout_ref): (String, Option<String>) =
        sqlx::query_as("SELECT status, payout_ref FROM redemption_intents WHERE id = $1")
            .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "payout_submitted", "not paid until confirmed on chain");
    assert_eq!(payout_ref.as_deref(), Some("abc123"));

    // The ledger event belongs to confirmation, not submission.
    let (events,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_withdrawal'")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(events, 0);
}

#[tokio::test]
async fn payouts_stop_at_the_daily_cap() {
    let pool = pool().await;
    let mut cfg = config();
    cfg.daily_payout_cap_clt = 15_000_000;
    pending_redemption(&pool, 10_000_000).await;
    pending_redemption(&pool, 10_000_000).await;
    let signer = CountingSigner { reply: PayoutReply::Paid { tx_id: "t".into() }, calls: AtomicUsize::new(0) };

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1, "the second payout crosses the cap");
}
