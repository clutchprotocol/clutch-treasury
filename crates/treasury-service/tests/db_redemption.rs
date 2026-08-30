use sqlx::PgPool;
use treasury_service::intents::create_redemption_intent;
use treasury_service::payout::{HttpPayoutSigner, PayoutReply, PayoutSigner};
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
