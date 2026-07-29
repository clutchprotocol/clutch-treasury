use sqlx::PgPool;
use uuid::Uuid;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    // Each test file starts clean — order-independent tests.
    sqlx::query("TRUNCATE treasury_events, mint_intents, redemption_intents, chain_outbox, reconciliation_runs, alerts RESTART IDENTITY CASCADE")
        .execute(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn ledger_is_append_only() {
    let pool = pool().await;
    let id = treasury_service::ledger::append_event(
        &pool, "mint_executed", 5_000_000, 5_000_000, None, Some("0xabc"), "test mint",
    )
    .await
    .unwrap();

    let upd = sqlx::query("UPDATE treasury_events SET amount_clt = 1 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await;
    assert!(upd.is_err(), "UPDATE must be rejected by trigger");
    let del = sqlx::query("DELETE FROM treasury_events WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await;
    assert!(del.is_err(), "DELETE must be rejected by trigger");
}

#[tokio::test]
async fn balances_derive_from_events() {
    let pool = pool().await;
    use treasury_service::ledger::{append_event, balances};
    append_event(&pool, "custody_deposit", 0, 10_000_000, None, None, "usdt in").await.unwrap();
    append_event(&pool, "mint_executed", 5_000_000, 0, None, None, "mint").await.unwrap();
    append_event(&pool, "burn_redeemed", 2_000_000, 0, None, None, "burn").await.unwrap();
    append_event(&pool, "custody_withdrawal", 0, 2_000_000, None, None, "payout").await.unwrap();
    let b = balances(&pool).await.unwrap();
    assert_eq!(b.clt_liability, 3_000_000);
    assert_eq!(b.custody_usdt, 8_000_000);
}

#[tokio::test]
async fn four_eyes_enforced_in_db() {
    let pool = pool().await;
    use treasury_service::intents::{approve_mint_intent, create_mint_intent};
    let intent = create_mint_intent(&pool, "0x4444444444444444444444444444444444444444", 1_000_000, "alice")
        .await
        .unwrap();
    assert_eq!(intent.status, "created");
    assert_eq!(intent.credit_ref.len(), 64);

    // Same person cannot approve — DB CHECK, not just app logic (spec §5).
    let err = approve_mint_intent(&pool, intent.id, "alice").await;
    assert!(err.is_err(), "initiator == approver must be rejected");

    let approved = approve_mint_intent(&pool, intent.id, "bob").await.unwrap();
    assert_eq!(approved.status, "approved");

    // Approval created exactly one outbox row.
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM chain_outbox WHERE intent_id = $1")
            .bind(intent.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 1);

    // Idempotent: approving again neither errors into a second outbox row nor regresses status.
    let again = approve_mint_intent(&pool, intent.id, "bob").await;
    assert!(again.is_err() || again.unwrap().status == "approved");
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM chain_outbox WHERE intent_id = $1")
            .bind(intent.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 1);
}
