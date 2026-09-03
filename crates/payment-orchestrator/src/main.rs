use std::sync::Arc;

use payment_orchestrator::custody::{CustodyWatcher, DepositWatcher, TronGridWatcher};
use payment_orchestrator::derive::AddressDeriver;
use payment_orchestrator::api;
use payment_orchestrator::configuration::OrchConfig;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let env = std::env::args().nth(2).unwrap_or_else(|| "default".to_string()); // `-- --env X` later; default fine
    let config = OrchConfig::load(&env).expect("load config");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await
        .expect("connect postgres");
    sqlx::migrate!("./migrations").run(&pool).await.expect("migrations");

    // Fail at boot, not per request: a malformed xpub means no deposit can be addressed at all,
    // and discovering that on a user's first request is strictly worse than not starting.
    let deriver = Arc::new(
        AddressDeriver::from_account_xpub(&config.deposit_account_xpub)
            .expect("APP_DEPOSIT_ACCOUNT_XPUB must be a valid account-level xpub (m/44'/195'/0')"),
    );

    let watcher: Arc<dyn CustodyWatcher> = Arc::new(TronGridWatcher::new(
        config.trongrid_url.clone(),
        config.trongrid_api_key.clone(),
        config.usdt_contract.clone(),
    ));
    // The per-user watcher: polls a bounded, hot-first slice of permanent addresses per pass (see
    // poller::due_addresses) rather than every address that has ever existed.
    let tiered: Arc<dyn DepositWatcher> = Arc::new(payment_orchestrator::poller::TieredPoller {
        pool: pool.clone(),
        inner: watcher.clone(),
        budget: payment_orchestrator::poller::MAX_ADDRESSES_PER_PASS,
    });

    // The ONLY detection path. Bitcart is gone: its TRX daemon attributes payments by the sender's
    // address, which we cannot know in advance (see custody.rs). This polls each user's permanent
    // deposit address (tiered) and any still-open legacy per-intent address (watcher), matching by
    // destination.
    tokio::spawn(payment_orchestrator::poller::run(pool.clone(), tiered, watcher, config.poll_interval_secs));

    // The deposit->mint bridge (Plan C 5b) — the only thing in this crate that crosses into the
    // treasury's private zone. Same poll-interval convention as the poller above.
    tokio::spawn(payment_orchestrator::treasury_bridge::run(pool.clone(), config.clone(), config.poll_interval_secs));

    let app = api::router(pool, config.clone(), deriver);
    let listener = tokio::net::TcpListener::bind(&config.http_addr).await.expect("bind");
    tracing::info!("payment-orchestrator listening on {}", config.http_addr);
    axum::serve(listener, app).await.expect("serve");
}
