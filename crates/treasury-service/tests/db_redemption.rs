use sqlx::PgPool;
use treasury_service::intents::create_redemption_intent;
use treasury_service::watcher::confirm_burn;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
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
