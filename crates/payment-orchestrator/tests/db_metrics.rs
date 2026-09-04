//! The scrape reads Postgres, so what is worth testing is that it reports what the database
//! holds. A metric that quietly stops tracking its source is worse than none, because an alert
//! gets written against it.

use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_orch_metrics");
    if !Postgres::database_exists(&url).await.unwrap_or(false) {
        Postgres::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE deposit_intents, deposit_addresses, alerts RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

/// Zero reads as zero rather than as a missing series: an alert on `needs_manual > 0` must
/// evaluate on an empty database instead of looking healthy because nothing was emitted.
#[tokio::test]
async fn a_status_with_no_rows_still_reports_zero() {
    let pool = pool().await;
    let body = payment_orchestrator::metrics::render(&pool).await;

    assert!(body.contains("clutch_orchestrator_up 1"));
    assert!(
        body.contains(r#"clutch_orchestrator_deposit_intents{status="needs_manual"} 0"#),
        "an empty status must read 0, not vanish:\n{body}"
    );
    assert!(body.contains(r#"clutch_orchestrator_alerts_total{severity="p1"} 0"#));
    assert!(body.contains("clutch_orchestrator_deposit_addresses 0"));
    assert!(body.contains("clutch_orchestrator_addresses_never_polled 0"));
}

/// The poller-health numbers are the point of this module: an address that has never been read
/// successfully, and the age of the oldest successful read, are what a stuck or throttled watcher
/// looks like from outside. `last_polled_at` advances only on a successful TronGrid read.
#[tokio::test]
async fn poller_health_tracks_the_read_watermark() {
    let pool = pool().await;

    // One address never read, one read an hour ago.
    sqlx::query(
        "INSERT INTO deposit_addresses (user_pk, derivation_index, address, clt_address, last_polled_at)
         VALUES ('0xaaaa', 1, 'TNeverPolled11111111111111111111111', 'clt', NULL),
                ('0xbbbb', 2, 'TPolledAnHourAgo1111111111111111111', 'clt', now() - interval '1 hour')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let body = payment_orchestrator::metrics::render(&pool).await;

    assert!(body.contains("clutch_orchestrator_deposit_addresses 2"), "{body}");
    assert!(body.contains("clutch_orchestrator_addresses_never_polled 1"), "{body}");

    let age: f64 = body
        .lines()
        .find_map(|l| l.strip_prefix("clutch_orchestrator_oldest_poll_age_seconds "))
        .expect("the oldest-read age must be reported when any address has been read")
        .parse()
        .unwrap();
    assert!(
        (3400.0..3800.0).contains(&age),
        "the oldest successful read was an hour ago, got {age}s"
    );
}
