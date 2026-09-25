use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use tower::ServiceExt;
use treasury_service::gasfree_rail::Trace;
use treasury_service::intents::create_redemption_intent;
use treasury_service::payout::{self, HttpPayoutSigner, PayoutReply, PayoutSigner};
use treasury_service::tron_verifier::TronClient;
use treasury_service::watcher::confirm_burn;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method, path};
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
    // Belt-and-braces alongside the in-test DROP in
    // a_payout_ref_write_failure_alerts_with_the_tx_id_and_keeps_the_pass_going: a SIGKILL
    // mid-test (not just a failed assertion) can leave `tmp` behind on this shared, persistent
    // database, and every later test that writes payout_ref would then fail in a way that looks
    // nothing like the real cause. Cleared here so a previous run's hard death cannot leak in.
    sqlx::query("ALTER TABLE redemption_intents DROP CONSTRAINT IF EXISTS tmp")
        .execute(&pool).await.unwrap();
    sqlx::query("TRUNCATE treasury_events, redemption_intents, alerts RESTART IDENTITY CASCADE")
        .execute(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn matching_burn_confirms_and_ledgers_once() {
    let pool = pool().await;
    let intent = create_redemption_intent(&pool, "0xaaaa000000000000000000000000000000000001", "TTronAddr111", 2_000_000, 2_000_000).await.unwrap();

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
    let intent = create_redemption_intent(&pool, "0xaaaa000000000000000000000000000000000002", "TTronAddr222", 2_000_000, 2_000_000).await.unwrap();

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
async fn a_refused_status_is_refused_and_carries_its_reason() {
    // The signer's structured answer for a provable pre-broadcast failure (IMPORTANT 2): a
    // TronGrid balanceOf blip, a dry fee account, a bad recipient — all proven to have happened
    // before anything existed to sign, so retrying is free.
    let (_s, signer) = signer_replying(200, serde_json::json!({
        "status": "refused", "reason": "could not read the payout float's USDT balance: boom",
    })).await;
    let reply = signer.pay(Uuid::new_v4(), "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", 5).await;
    assert_eq!(reply, PayoutReply::Refused("could not read the payout float's USDT balance: boom".into()));
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
async fn a_float_dry_reply_carries_its_numbers() {
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
    /// The amount of the most recent call. The whole point of a fee is that this is NOT the
    /// intent's `amount_clt`, so a test asserting only the call count would miss it entirely.
    last_amount: AtomicI64,
}

#[async_trait::async_trait]
impl PayoutSigner for CountingSigner {
    async fn pay(&self, _intent_id: Uuid, _to: &str, amount_usdt: i64) -> PayoutReply {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.last_amount.store(amount_usdt, Ordering::SeqCst);
        self.reply.clone()
    }
}

/// A redemption sitting at payout_pending with its burn already confirmed, paying par.
async fn pending_redemption(pool: &PgPool, amount_clt: i64) -> Uuid {
    pending_redemption_paying(pool, amount_clt, amount_clt).await
}

/// The same, with the two legs deliberately different — a burn of `amount_clt` against a quoted
/// payout of `payout_amount_usdt`, which is what a configured fee produces.
async fn pending_redemption_paying(pool: &PgPool, amount_clt: i64, payout_amount_usdt: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO redemption_intents (id, redeemer_address, payout_address, amount_clt, payout_amount_usdt, status, redemption_ref, burn_tx_hash)
         VALUES ($1, '0xabc', 'TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK', $2, $4, 'payout_pending', $3, '0xburn')",
    )
    .bind(id)
    .bind(amount_clt)
    .bind(format!("{:064x}", id.as_u128()))
    .bind(payout_amount_usdt)
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
        trongrid_url: "http://unused".into(),
        trongrid_api_key: "k".into(),
        custody_tron_address: "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".into(),
        payout_float_address: "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY".into(),
        usdt_contract: "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".into(),
        deposit_confirmations: 19,
        deposit_match_window_hours: 24,
        sweep_threshold_usdt: 100_000_000,
        sweep_max_age_hours: 168,
        sweep_min_usdt: 0,
        redemption_fee_usdt: 0,
        signer_url: "http://unused".into(),
        signer_token: "s".into(),
        gasfree: None,
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
        last_amount: AtomicI64::new(0),
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
        last_amount: AtomicI64::new(0),
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
        last_amount: AtomicI64::new(0),
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
    let signer = CountingSigner { reply: PayoutReply::Paid { tx_id: "t".into() }, calls: AtomicUsize::new(0), last_amount: AtomicI64::new(0) };

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1, "the second payout crosses the cap");
}

/// Nothing caps a redemption's size at creation, so an intent larger than the cap is reachable.
/// `ORDER BY created_at` means a naive "stop the pass once the cumulative total would cross the
/// cap" check would `break` on this one forever and wedge every intent behind it in line.
#[tokio::test]
async fn an_over_cap_intent_is_skipped_and_does_not_block_smaller_ones() {
    let pool = pool().await;
    let mut cfg = config();
    cfg.daily_payout_cap_clt = 5_000_000;
    let stuck = pending_redemption(&pool, 10_000_000).await; // first in line, unpayable alone
    let payable = pending_redemption(&pool, 1_000_000).await;
    let signer = CountingSigner { reply: PayoutReply::Paid { tx_id: "t".into() }, calls: AtomicUsize::new(0), last_amount: AtomicI64::new(0) };

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    payout::drain_once(&pool, &cfg, &signer).await.unwrap(); // second pass: must not re-alert

    let (stuck_status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(stuck).fetch_one(&pool).await.unwrap();
    assert_eq!(stuck_status, "payout_pending", "unpayable under the current cap; left alone, not claimed");

    let (payable_status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(payable).fetch_one(&pool).await.unwrap();
    assert_eq!(payable_status, "payout_submitted", "must not be wedged behind the over-cap intent");

    let (alerts,): (i64,) = sqlx::query_as("SELECT count(*) FROM alerts WHERE severity = 'p1' AND source = 'payout'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(alerts, 1, "one alert for the stuck intent, not one per pass");
}

#[tokio::test]
async fn an_ambiguous_payout_counts_against_the_daily_cap() {
    // Without this, an unknown-outcome payout would consume no budget and a single pass could
    // send out more than the cap allows.
    let pool = pool().await;
    let mut cfg = config();
    cfg.daily_payout_cap_clt = 15_000_000;
    pending_redemption(&pool, 10_000_000).await;
    pending_redemption(&pool, 10_000_000).await;
    let signer = CountingSigner { reply: PayoutReply::Ambiguous("timeout".into()), calls: AtomicUsize::new(0), last_amount: AtomicI64::new(0) };

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1, "the ambiguous payout must spend budget too");
}

#[tokio::test]
async fn the_daily_cap_window_ignores_a_stale_claim_even_after_it_is_touched_again() {
    // Regression for keying the window on `updated_at`: a later write (exactly what
    // confirm_payouts_once's `paid` flip does) must not re-enter an old claim into today's budget.
    let pool = pool().await;
    let mut cfg = config();
    cfg.daily_payout_cap_clt = 10_000_000;

    let old = pending_redemption(&pool, 10_000_000).await;
    let old_signer = CountingSigner { reply: PayoutReply::Paid { tx_id: "old-tx".into() }, calls: AtomicUsize::new(0), last_amount: AtomicI64::new(0) };
    payout::drain_once(&pool, &cfg, &old_signer).await.unwrap();

    sqlx::query(
        "UPDATE redemption_intents SET payout_submitted_at = now() - interval '30 hours', updated_at = now()
         WHERE id = $1",
    )
    .bind(old)
    .execute(&pool)
    .await
    .unwrap();

    pending_redemption(&pool, 10_000_000).await;
    let new_signer = CountingSigner { reply: PayoutReply::Paid { tx_id: "new-tx".into() }, calls: AtomicUsize::new(0), last_amount: AtomicI64::new(0) };
    payout::drain_once(&pool, &cfg, &new_signer).await.unwrap();

    assert_eq!(new_signer.calls.load(Ordering::SeqCst), 1,
        "a 30h-old claim must not count against today's budget just because something touched updated_at");
}

/// Forces a REAL sqlx error on the Paid arm's payout_ref write (a temporary CHECK constraint
/// forbidding any non-null payout_ref), rather than reasoning about the failure path by
/// inspection only. Covers the one branch where money has already moved: the alert must carry
/// the tx_id verbatim, and the pass must keep going rather than abort on the first failure — the
/// "claim committed but the ack was lost" variant needs no separate test, since it is the same
/// straight-line alert-and-continue handler on the claim UPDATE just above this one.
///
/// The tight 2M cap and the third 1M intent are load-bearing, not incidental — they are what
/// pins N1 (the day_total charge sits ABOVE the fallible write, so a paid-but-unrecorded intent
/// still spends budget). `a` and `b` both always fail their write here, so if the charge ever
/// slipped back below it, day_total would stay 0 regardless of how many intents got "paid", the
/// cap would never bind, and `c` would ALSO be attempted (calls == 3 instead of 2). Do not
/// "simplify" this back down to two intents under a loose cap — that shape passes whether N1
/// holds or not, which is the exact gap that let N1 ship untested the first time.
#[tokio::test]
async fn a_payout_ref_write_failure_alerts_with_the_tx_id_and_keeps_the_pass_going() {
    let pool = pool().await;
    let mut cfg = config();
    cfg.daily_payout_cap_clt = 2_000_000;
    let a = pending_redemption(&pool, 1_000_000).await;
    let b = pending_redemption(&pool, 1_000_000).await;
    let c = pending_redemption(&pool, 1_000_000).await;
    let signer = CountingSigner {
        reply: PayoutReply::Paid { tx_id: "constrained-tx".into() },
        calls: AtomicUsize::new(0),
        last_amount: AtomicI64::new(0),
    };

    // Self-healing in case a previous run of this test panicked before reaching its own DROP
    // (pool() now also clears a leak from a harder kill — see its comment).
    sqlx::query("ALTER TABLE redemption_intents DROP CONSTRAINT IF EXISTS tmp")
        .execute(&pool).await.unwrap();
    sqlx::query("ALTER TABLE redemption_intents ADD CONSTRAINT tmp CHECK (payout_ref IS NULL)")
        .execute(&pool).await.unwrap();

    let result = payout::drain_once(&pool, &cfg, &signer).await;

    // Drop the constraint BEFORE asserting anything: a failed assertion below must not leave a
    // table-wide constraint behind for every later test in this binary to trip over.
    sqlx::query("ALTER TABLE redemption_intents DROP CONSTRAINT tmp").execute(&pool).await.unwrap();

    result.unwrap();
    assert_eq!(signer.calls.load(Ordering::SeqCst), 2,
        "a and b must charge 2M between them even though both writes fail, so c is never \
         attempted once the cap reads spent — 3 here would mean the failed writes charged nothing");

    for id in [a, b] {
        let (status, payout_ref): (String, Option<String>) =
            sqlx::query_as("SELECT status, payout_ref FROM redemption_intents WHERE id = $1")
                .bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(status, "payout_submitted", "claimed, then stuck there when the write that would advance it further failed");
        assert_eq!(payout_ref, None, "the CHECK-violating write must not have applied");
    }

    let (c_status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(c).fetch_one(&pool).await.unwrap();
    assert_eq!(c_status, "payout_pending", "never reached — the cap must already read as spent by a and b alone");

    let (alerted,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM alerts WHERE severity = 'p1' AND source = 'payout' \
         AND message LIKE '%constrained-tx%')")
        .fetch_one(&pool).await.unwrap();
    assert!(alerted, "the alert must carry the tx_id verbatim — it is the only link to money that already moved");
}

// --- confirm_payouts_once: on-chain confirmation before ledgering ---

/// A confirmed transaction that actually SUCCEEDED — the realistic shape, `ret[0].contractRet`
/// included. The original version of this helper returned `{"txID": ...}` alone: it modeled
/// presence but never execution result, so it passed against the exact bug `transfer_succeeded`
/// exists to catch (a `REVERT`/`OUT_OF_ENERGY` transaction is ALSO present with just a `txID`).
async fn mount_confirmed_tx(server: &MockServer, tx_id: &str) {
    Mock::given(method("POST"))
        .and(path("/walletsolidity/gettransactionbyid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "txID": tx_id,
            "ret": [{ "contractRet": "SUCCESS" }],
        })))
        .mount(server)
        .await;
}

/// Confirmed — present in an irreversible block — but NOT successful: `contractRet` is whatever
/// TronGrid reported for a transfer that ran out of energy or reverted.
async fn mount_confirmed_but_failed_tx(server: &MockServer, tx_id: &str, contract_ret: &str) {
    Mock::given(method("POST"))
        .and(path("/walletsolidity/gettransactionbyid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "txID": tx_id,
            "ret": [{ "contractRet": contract_ret }],
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn confirmation_writes_paid_and_the_ledger_event_once() {
    let pool = pool().await;
    let id = pending_redemption(&pool, 10_000_000).await;
    sqlx::query("UPDATE redemption_intents SET status = 'payout_submitted', payout_ref = 'abc123' WHERE id = $1")
        .bind(id).execute(&pool).await.unwrap();

    let server = MockServer::start().await;
    mount_confirmed_tx(&server, "abc123").await;
    let client = TronClient::new(server.uri(), String::new());

    payout::confirm_payouts_once(&pool, &client).await.unwrap();

    // Force the one state where ON CONFLICT is load-bearing: put the intent back at
    // payout_submitted (payout_ref and the ledger row already written stay untouched) so the
    // second confirm_payouts_once pass re-selects it and calls pay_intent again for real. Without
    // this reset the second pass's SELECT (`WHERE status = 'payout_submitted'`) would just return
    // zero rows — proving the state filter works, not that a repeat pay_intent call is safe.
    sqlx::query("UPDATE redemption_intents SET status = 'payout_submitted' WHERE id = $1")
        .bind(id).execute(&pool).await.unwrap();

    // This second call really does re-run pay_intent's INSERT for the same (intent_id, kind).
    // uq_events_intent_kind (0001_init.sql) is the unique index the ON CONFLICT clause targets;
    // delete that clause and this INSERT hits the index and errors instead of no-op'ing.
    payout::confirm_payouts_once(&pool, &client).await.unwrap();

    let (status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "paid");

    let (events,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_withdrawal'")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(events, 1, "liability must drop exactly once");
}

/// THE regression for the critical bug this rail shipped with: `transaction_confirmed` alone only
/// proves the tx is in a block, not that it moved anything. A TRC-20 transfer that ran out of
/// energy or reverted is in the block just the same — before this fix, `confirm_payouts_once`
/// would write `paid` and a `custody_withdrawal` event for USDT that never left the float.
#[tokio::test]
async fn a_reverted_payout_tx_is_never_paid_and_alerts_a_human_instead() {
    let pool = pool().await;
    let id = pending_redemption(&pool, 10_000_000).await;
    sqlx::query("UPDATE redemption_intents SET status = 'payout_submitted', payout_ref = 'abc123' WHERE id = $1")
        .bind(id).execute(&pool).await.unwrap();

    let server = MockServer::start().await;
    mount_confirmed_but_failed_tx(&server, "abc123", "REVERT").await;
    let client = TronClient::new(server.uri(), String::new());

    payout::confirm_payouts_once(&pool, &client).await.unwrap();

    let (status, payout_ref): (String, Option<String>) =
        sqlx::query_as("SELECT status, payout_ref FROM redemption_intents WHERE id = $1")
            .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "payout_submitted", "a reverted transfer must never become paid");
    assert_eq!(payout_ref.as_deref(), Some("abc123"), "left for a human, not cleared");

    let (events,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_withdrawal'")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(events, 0, "no USDT left the float, so no custody_withdrawal event");

    let (alerted,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM alerts WHERE severity = 'p1' AND source = 'payout' \
         AND message LIKE '%abc123%' AND message LIKE '%REVERT%')")
        .fetch_one(&pool).await.unwrap();
    assert!(alerted, "a confirmed-but-failed transfer must alert a human, naming the tx id and contractRet");
}

/// `outbox_poll_ms` is 2 seconds, so a dry float — the NORMAL state until the rollout funds it —
/// re-claims and re-refuses the same intent on every pass. Without dedup this inserts a fresh P1
/// row (and burns two TronGrid calls) every 2 seconds, burying the ambiguous-payout P1s the whole
/// design depends on someone actually reading.
#[tokio::test]
async fn a_repeatedly_refused_payout_alerts_once_not_every_pass() {
    let pool = pool().await;
    pending_redemption(&pool, 10_000_000).await;
    let signer = CountingSigner {
        reply: PayoutReply::FloatDry {
            float_address: "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY".into(),
            have_usdt: 0,
            need_usdt: 10_000_000,
        },
        calls: AtomicUsize::new(0),
        last_amount: AtomicI64::new(0),
    };
    let cfg = config();

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    let (alerts,): (i64,) = sqlx::query_as("SELECT count(*) FROM alerts WHERE severity = 'p1' AND source = 'payout'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(alerts, 1, "a dry float must alert once, not once per pass");
}

/// The burn sender and the recorded redeemer are the same account when they differ only in case —
/// a checksummed address is not a different person. This comparison runs AFTER the CLT is already
/// destroyed and a mismatch means the intent is failed and never paid, so treating a case
/// difference as a mismatch would burn someone's money and refuse them the payout.
#[tokio::test]
async fn a_burn_from_a_differently_cased_sender_is_still_honoured() {
    let pool = pool().await;
    let lower = "0xaaaa0000000000000000000000000000000000cd";
    let intent = create_redemption_intent(&pool, lower, "TTronAddrCase", 2_000_000, 2_000_000).await.unwrap();

    // Same account, checksummed the way a wallet might present it.
    let mixed = "0xAAAA0000000000000000000000000000000000CD";
    treasury_service::watcher::confirm_burn(&pool, &intent.redemption_ref, mixed, 2_000_000, "0xburncase")
        .await
        .unwrap();

    let (status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(intent.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(status, "failed", "a case difference must never refuse a payout on a burned redemption");
    assert_eq!(status, "payout_pending", "it is the same account, so the burn is confirmed");
}

/// The float pays energy for its own transfer, so an empty one is topped up from the fee account
/// and the NEXT pass sends the USDT. That is routine and self-resolving — the sweeper's equivalent
/// outcome is a log line — so it must not raise a P1. The first real redemption on stage did page
/// this way and then paid normally a pass later, which is how an alert stops meaning anything.
#[tokio::test]
async fn a_float_being_topped_up_with_trx_does_not_page_anyone() {
    let pool = pool().await;
    let intent = create_redemption_intent(&pool, "0xaaaa0000000000000000000000000000000000ef", "TTronAddrTrx", 2_000_000, 2_000_000).await.unwrap();
    treasury_service::watcher::confirm_burn(
        &pool, &intent.redemption_ref, "0xaaaa0000000000000000000000000000000000ef", 2_000_000, "0xburntrx",
    ).await.unwrap();

    let signer = CountingSigner { reply: PayoutReply::NeedsTrx, calls: AtomicUsize::new(0), last_amount: AtomicI64::new(0) };
    payout::drain_once(&pool, &config(), &signer).await.unwrap();

    let (status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(intent.id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "payout_pending", "it goes back for the next pass, unclaimed");

    let p1s: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM alerts WHERE severity = 'p1' AND source = 'payout'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(p1s, 0, "a routine TRX top-up must not page a human");
}

/// A fee only exists if the payout worker actually pays the quoted net. Paying `amount_clt` would
/// hand back the full burn and leave nothing in reserve — the fee would be recorded everywhere and
/// charged nowhere.
#[tokio::test]
async fn the_payout_pays_the_quoted_net_not_the_burn() {
    let pool = pool().await;
    let id = pending_redemption_paying(&pool, 10_000_000, 9_500_000).await;
    let signer = CountingSigner {
        reply: PayoutReply::Paid { tx_id: "feetx".into() },
        calls: AtomicUsize::new(0),
        last_amount: AtomicI64::new(0),
    };

    payout::drain_once(&pool, &config(), &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        signer.last_amount.load(Ordering::SeqCst),
        9_500_000,
        "the signer must be asked for the net, never the burn amount"
    );
    let _ = id;
}

/// The reconciliation half of the same fee. `custody_withdrawal` is what tells the ledger how much
/// USDT left the float; `burn_redeemed` separately drops liability by the full burn. Recording the
/// gross here would understate the reserve by the fee on every redemption, and a reserve reported
/// below liability is the one condition that halts minting.
#[tokio::test]
async fn the_ledger_records_the_usdt_that_actually_left() {
    let pool = pool().await;
    let id = pending_redemption_paying(&pool, 10_000_000, 9_500_000).await;
    sqlx::query("UPDATE redemption_intents SET status = 'payout_submitted', payout_ref = 'feetx' WHERE id = $1")
        .bind(id).execute(&pool).await.unwrap();

    let server = MockServer::start().await;
    mount_confirmed_tx(&server, "feetx").await;
    let client = TronClient::new(server.uri(), String::new());

    payout::confirm_payouts_once(&pool, &client).await.unwrap();

    let (amount_usdt,): (i64,) = sqlx::query_as(
        "SELECT amount_usdt FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_withdrawal'")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(amount_usdt, 9_500_000, "the ledger must record the net, or the reserve reads low by the fee");
}

// --- GasFree payouts (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md §4, §5) ---

/// `config().payout_float_address`: on this rail, F = gasfree(2/0).
const FLOAT: &str = "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY";
/// The plain 2/0 address that owns the float, as the signer's /internal/xpub names it.
const FLOAT_OWNER: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";
/// `pending_redemption`'s payout address.
const REDEEMER: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";
const PAYOUT_TRACE: &str = "6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60";
const USDT: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";

fn nile() -> gasfree::Settings {
    gasfree::Settings {
        chain: &gasfree::NILE,
        rail: true,
        activate_fee_max_usdt: 1_500_000,
        transfer_fee_max_usdt: 500_000,
        min_deposit_usdt: 1_000_000,
        expected_beacon_implementation: "b8eda40b467b45af107f198e94cc2fa1378adf50".into(),
        expected_controller_implementation: "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441".into(),
    }
}

fn gasfree_config(trongrid_url: String) -> treasury_service::configuration::AppConfig {
    let mut cfg = config();
    cfg.trongrid_url = trongrid_url;
    cfg.gasfree = Some(nile());
    cfg
}

fn counting(reply: PayoutReply) -> CountingSigner {
    CountingSigner { reply, calls: AtomicUsize::new(0), last_amount: AtomicI64::new(0) }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The signer's GasFree reads, faked: the relay's record names `txn_hash`, and FLOAT_OWNER owns FLOAT.
struct GasFreeReads {
    txn_hash: Option<&'static str>,
}

#[async_trait::async_trait]
impl PayoutSigner for GasFreeReads {
    async fn pay(&self, _intent_id: Uuid, _to: &str, _amount_usdt: i64) -> PayoutReply {
        panic!("settling a payout must never sign another")
    }
    async fn trace(&self, _trace_id: &str) -> Result<Trace, String> {
        Ok(Trace { state: "SUCCEED".into(), txn_hash: self.txn_hash.map(str::to_string), txn_amount: None })
    }
    async fn float_owner(&self) -> Result<(String, Option<String>), String> {
        Ok((FLOAT_OWNER.into(), Some(FLOAT.into())))
    }
}

/// A redemption whose GasFree permit is out: claimed, with the permit's trace (if any), nonce and deadline.
async fn permit_out(pool: &PgPool, amount: i64, trace: Option<&str>, nonce: i64, deadline: i64) -> Uuid {
    let id = pending_redemption(pool, amount).await;
    sqlx::query(
        "UPDATE redemption_intents
            SET status = 'payout_submitted', payout_submitted_at = now(),
                payout_trace_id = $2, payout_permit_nonce = $3, payout_permit_deadline = $4
          WHERE id = $1",
    )
    .bind(id)
    .bind(trace)
    .bind(nonce)
    .bind(deadline)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// The float's confirmed USDT history: one GasFree payout is two transfers from the float in one
/// transaction, the redeemer's and the relay's fee.
async fn mount_float_history(server: &MockServer, tx_id: &str, to: &str, value: i64) {
    let at = chrono::Utc::now().timestamp_millis();
    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{FLOAT}/transactions/trc20")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [
            {"transaction_id": tx_id, "from": FLOAT, "to": to, "value": value.to_string(), "type": "Transfer",
             "token_info": {"address": USDT}, "block_timestamp": at},
            {"transaction_id": tx_id, "from": FLOAT, "to": "TLntW9Z59LYY5KEi9cmwk3PKjQga828ird", "value": "300000",
             "type": "Transfer", "token_info": {"address": USDT}, "block_timestamp": at},
        ]})))
        .mount(server)
        .await;
}

async fn mount_float_nonce(server: &MockServer, nonce: u64) {
    Mock::given(method("POST"))
        .and(path("/wallet/triggerconstantcontract"))
        .and(body_string_contains("nonces(address)"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"constant_result": [format!("{nonce:064x}")]})),
        )
        .mount(server)
        .await;
}

/// (status, payout_ref, payout_permit_nonce)
async fn state_of(pool: &PgPool, id: Uuid) -> (String, Option<String>, Option<i64>) {
    sqlx::query_as("SELECT status, payout_ref, payout_permit_nonce FROM redemption_intents WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn payout_alerts(pool: &PgPool, severity: &str, containing: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM alerts WHERE source = 'payout' AND severity = $1 AND message LIKE '%' || $2 || '%'",
    )
    .bind(severity)
    .bind(containing)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_gasfree_payout_answers_are_read_field_by_field() {
    for (body, want) in [
        (
            serde_json::json!({"status": "submitted", "trace_id": PAYOUT_TRACE, "nonce": 4, "deadline": 1_790_000_000u64}),
            PayoutReply::Submitted { trace_id: PAYOUT_TRACE.into(), nonce: 4, deadline: 1_790_000_000 },
        ),
        (
            serde_json::json!({"status": "refused", "reason": "the relay refused the payout permit: NonceNotMatchException",
                               "nonce": 4, "deadline": 1_790_000_000u64}),
            PayoutReply::RelayRefused {
                reason: "the relay refused the payout permit: NonceNotMatchException".into(),
                nonce: 4,
                deadline: 1_790_000_000,
            },
        ),
        (
            serde_json::json!({"status": "float_not_active", "float_address": FLOAT}),
            PayoutReply::FloatNotActive { float_address: FLOAT.into() },
        ),
    ] {
        let (_s, signer) = signer_replying(200, body.clone()).await;
        assert_eq!(signer.pay(Uuid::new_v4(), REDEEMER, 5).await, want, "{body}");
    }

    // A permit this service could not follow is not a clear answer.
    for body in [
        serde_json::json!({"status": "submitted", "trace_id": PAYOUT_TRACE}),
        serde_json::json!({"status": "refused", "reason": "x", "nonce": 4}),
    ] {
        let (_s, signer) = signer_replying(200, body.clone()).await;
        let reply = signer.pay(Uuid::new_v4(), REDEEMER, 5).await;
        assert!(matches!(reply, PayoutReply::Ambiguous(_)), "{body} gave {reply:?}");
    }

    // The float's owner, from the signer's /internal/xpub.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/internal/xpub"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "account_xpub": "xpub-unused", "fee_address": "TFee", "payout_address": FLOAT_OWNER, "payout_gasfree_address": FLOAT,
        })))
        .mount(&server)
        .await;
    let signer = HttpPayoutSigner { http: reqwest::Client::new(), base_url: server.uri(), token: "t".into() };
    assert_eq!(signer.float_owner().await, Ok((FLOAT_OWNER.to_string(), Some(FLOAT.to_string()))));
}

/// One GasFree payout permit alive at a time: a second would carry the same nonce.
#[tokio::test]
async fn a_gasfree_payout_permit_holds_every_other_payout_until_it_is_settled() {
    let pool = pool().await;
    let first = pending_redemption(&pool, 10_000_000).await;
    let second = pending_redemption(&pool, 5_000_000).await;
    let signer = counting(PayoutReply::Submitted { trace_id: PAYOUT_TRACE.into(), nonce: 4, deadline: (now() + 180) as u64 });
    let cfg = gasfree_config("http://unused".into());

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
    let (trace, nonce): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT payout_trace_id, payout_permit_nonce FROM redemption_intents WHERE id = $1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((trace.as_deref(), nonce), (Some(PAYOUT_TRACE), Some(4)));
    assert_eq!(state_of(&pool, second).await.0, "payout_pending");
}

#[tokio::test]
async fn a_gasfree_payout_is_paid_once_its_transfer_from_the_float_is_confirmed() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_float_history(&server, "tx-gf-payout", REDEEMER, 10_000_000).await;
    let id = permit_out(&pool, 10_000_000, Some(PAYOUT_TRACE), 4, now() + 180).await;

    let paid = payout::confirm_gasfree_payouts_once(
        &pool,
        &gasfree_config(server.uri()),
        &nile(),
        &TronClient::new(server.uri(), "k".into()),
        &GasFreeReads { txn_hash: Some("tx-gf-payout") },
    )
    .await
    .unwrap();

    assert_eq!(paid, 1);
    assert_eq!(state_of(&pool, id).await, ("paid".into(), Some("tx-gf-payout".into()), Some(4)));
    let withdrawn: i64 = sqlx::query_scalar(
        "SELECT amount_usdt FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_withdrawal'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(withdrawn, 10_000_000);
}

#[tokio::test]
async fn a_transfer_that_does_not_pay_this_redemption_is_not_taken_as_its_payment() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_float_history(&server, "tx-gf-payout", REDEEMER, 9_999_999).await;
    let id = permit_out(&pool, 10_000_000, Some(PAYOUT_TRACE), 4, now() + 180).await;

    let paid = payout::confirm_gasfree_payouts_once(
        &pool,
        &gasfree_config(server.uri()),
        &nile(),
        &TronClient::new(server.uri(), "k".into()),
        &GasFreeReads { txn_hash: Some("tx-gf-payout") },
    )
    .await
    .unwrap();

    assert_eq!(paid, 0);
    assert_eq!(state_of(&pool, id).await, ("payout_submitted".into(), None, Some(4)));
}

/// After the deadline, a refused permit that did not run never can: the redemption is paid again.
#[tokio::test]
async fn a_refused_permit_goes_back_to_be_paid_once_its_deadline_passed_and_it_never_ran() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_float_nonce(&server, 4).await;
    let id = permit_out(&pool, 10_000_000, None, 4, now() - 400).await;

    payout::confirm_gasfree_payouts_once(
        &pool,
        &gasfree_config(server.uri()),
        &nile(),
        &TronClient::new(server.uri(), "k".into()),
        &GasFreeReads { txn_hash: None },
    )
    .await
    .unwrap();

    assert_eq!(state_of(&pool, id).await, ("payout_pending".into(), None, None));
}

/// Past its deadline but inside the grace after it, a refused permit may still show up on chain: it is
/// left alone, even though the float's nonce says it has not run.
#[tokio::test]
async fn a_refused_permit_inside_the_grace_after_its_deadline_is_left_alone() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_float_nonce(&server, 4).await;
    let id = permit_out(&pool, 10_000_000, None, 4, now() - 120).await;

    payout::confirm_gasfree_payouts_once(
        &pool,
        &gasfree_config(server.uri()),
        &nile(),
        &TronClient::new(server.uri(), "k".into()),
        &GasFreeReads { txn_hash: None },
    )
    .await
    .unwrap();

    assert_eq!(state_of(&pool, id).await, ("payout_submitted".into(), None, Some(4)));
}

/// The nonce moved and no transfer paying this redemption was found: it may be paid, so it goes to a
/// human and no longer holds the float.
#[tokio::test]
async fn a_permit_whose_nonce_moved_without_a_transfer_is_left_for_a_human() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_float_nonce(&server, 5).await;
    let id = permit_out(&pool, 10_000_000, None, 4, now() - 400).await;

    payout::confirm_gasfree_payouts_once(
        &pool,
        &gasfree_config(server.uri()),
        &nile(),
        &TronClient::new(server.uri(), "k".into()),
        &GasFreeReads { txn_hash: None },
    )
    .await
    .unwrap();

    assert_eq!(state_of(&pool, id).await, ("payout_submitted".into(), None, None));
    assert_eq!(payout_alerts(&pool, "p1", "may have been paid").await, 1);
}

#[tokio::test]
async fn a_relay_refusal_is_not_retried_before_its_deadline() {
    let pool = pool().await;
    let id = pending_redemption(&pool, 10_000_000).await;
    let signer = counting(PayoutReply::RelayRefused {
        reason: "the relay refused the payout permit: NonceNotMatchException".into(),
        nonce: 4,
        deadline: (now() + 180) as u64,
    });
    let cfg = gasfree_config("http://unused".into());

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1, "the refused permit is valid until its deadline");
    assert_eq!(state_of(&pool, id).await, ("payout_submitted".into(), None, Some(4)));
}

/// A signer on GasFree behind a treasury with GasFree off: the permit is recorded, and it pages once.
#[tokio::test]
async fn a_gasfree_payout_to_a_treasury_with_gasfree_off_pages_once() {
    let pool = pool().await;
    let id = pending_redemption(&pool, 10_000_000).await;
    let signer = counting(PayoutReply::Submitted { trace_id: "trace-1".into(), nonce: 4, deadline: (now() + 180) as u64 });
    let cfg = config();

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(payout_alerts(&pool, "p1", "GasFree off").await, 1, "paged once, not every pass");
    assert_eq!(state_of(&pool, id).await.0, "payout_submitted", "the permit is recorded, never dropped");
}

/// Spec §4: until the float's one-time activation, redemptions are "not available yet".
#[tokio::test]
async fn redemptions_wait_for_the_gasfree_floats_activation_and_it_is_said_once() {
    let pool = pool().await;
    let first = pending_redemption(&pool, 10_000_000).await;
    let second = pending_redemption(&pool, 5_000_000).await;
    let signer = counting(PayoutReply::FloatNotActive { float_address: FLOAT.into() });
    let cfg = gasfree_config("http://unused".into());

    for _ in 0..3 {
        payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    }

    assert_eq!(signer.calls.load(Ordering::SeqCst), 3, "one call per pass: every redemption would get the same answer");
    assert_eq!(state_of(&pool, first).await.0, "payout_pending");
    assert_eq!(state_of(&pool, second).await.0, "payout_pending");
    assert_eq!(payout_alerts(&pool, "warn", "not available yet").await, 1);
}

/// An answer that may have sent a permit, with its nonce unknown: nothing else is signed until that
/// permit could no longer run.
#[tokio::test]
async fn an_ambiguous_gasfree_payout_holds_the_float_for_the_longest_deadline() {
    let pool = pool().await;
    let first = pending_redemption(&pool, 10_000_000).await;
    pending_redemption(&pool, 5_000_000).await;
    let signer = counting(PayoutReply::Ambiguous("the relay gave no clear answer".into()));
    let cfg = gasfree_config("http://unused".into());

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
    let deadline: Option<i64> =
        sqlx::query_scalar("SELECT payout_permit_deadline FROM redemption_intents WHERE id = $1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    let held = deadline.expect("a deadline holds the float") - now();
    assert!((890..=910).contains(&held), "held for the longest deadline plus the grace, about 900 s, got {held}");
    assert_eq!(payout_alerts(&pool, "p1", "do not return this intent").await, 1, "the page says when the intent may be returned");
}

/// Spec §4: a redemption is refused before anything exists to burn against while the float's next
/// transfer would also pay its activation.
#[tokio::test]
async fn a_redemption_is_refused_until_the_gasfree_float_is_activated() {
    let pool = pool().await;
    let request = || {
        Request::builder()
            .method("POST")
            .uri("/internal/redemption-intents")
            .header("authorization", "Bearer i")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "redeemer_address": "0xaaaa000000000000000000000000000000000009",
                    "payout_address": REDEEMER,
                    "amount_clt": 10_000_000,
                })
                .to_string(),
            ))
            .unwrap()
    };

    let never_moved = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/wallet/getcontract"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&never_moved)
        .await;
    let app = treasury_service::api::router(pool.clone(), gasfree_config(never_moved.uri()));
    assert_eq!(app.oneshot(request()).await.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM redemption_intents").fetch_one(&pool).await.unwrap();
    assert_eq!(rows, 0, "nothing exists to burn against");

    let activated = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/wallet/getcontract"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"contract_address": "41ab", "bytecode": ""})))
        .mount(&activated)
        .await;
    let app = treasury_service::api::router(pool.clone(), gasfree_config(activated.uri()));
    assert_eq!(app.oneshot(request()).await.unwrap().status(), StatusCode::CREATED);
}

/// Two redemptions of the same amount to one address: the first one's transaction must not be taken
/// as the second one's payment, even when the relay names it — the chain says it paid the first.
#[tokio::test]
async fn a_transaction_that_already_paid_another_redemption_is_not_taken_as_this_ones() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_float_history(&server, "tx-gf-payout", REDEEMER, 10_000_000).await;
    let first = pending_redemption(&pool, 10_000_000).await;
    sqlx::query("UPDATE redemption_intents SET status = 'paid', payout_ref = 'tx-gf-payout' WHERE id = $1")
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
    let second = permit_out(&pool, 10_000_000, Some(PAYOUT_TRACE), 4, now() + 180).await;

    let paid = payout::confirm_gasfree_payouts_once(
        &pool,
        &gasfree_config(server.uri()),
        &nile(),
        &TronClient::new(server.uri(), "k".into()),
        &GasFreeReads { txn_hash: Some("tx-gf-payout") },
    )
    .await
    .unwrap();

    assert_eq!(paid, 0);
    assert_eq!(state_of(&pool, second).await, ("payout_submitted".into(), None, Some(4)));
    assert_eq!(payout_alerts(&pool, "p1", "already paid").await, 1);
}
