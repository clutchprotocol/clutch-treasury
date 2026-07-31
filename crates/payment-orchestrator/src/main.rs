use std::sync::Arc;

use payment_orchestrator::custody::{CustodyWatcher, TronGridWatcher};
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

    let watcher: Arc<dyn CustodyWatcher> = Arc::new(TronGridWatcher::new(
        config.trongrid_url.clone(),
        config.trongrid_api_key.clone(),
        config.custody_tron_address.clone(),
        config.usdt_contract.clone(),
    ));

    // The ONLY detection path. Bitcart is gone: its TRX daemon attributes payments by the sender's
    // address, which cannot work for a shared custody address with payers unknown until they pay
    // (see custody.rs). This watches the custody address directly and matches on the exact
    // discriminated amount.
    tokio::spawn(payment_orchestrator::poller::run(pool.clone(), watcher, config.poll_interval_secs));

    // The deposit->mint bridge (Plan C 5b) — the only thing in this crate that crosses into the
    // treasury's private zone. Same poll-interval convention as the poller above.
    tokio::spawn(payment_orchestrator::treasury_bridge::run(pool.clone(), config.clone(), config.poll_interval_secs));

    let app = api::router(pool, config.clone());
    let listener = tokio::net::TcpListener::bind(&config.http_addr).await.expect("bind");
    tracing::info!("payment-orchestrator listening on {}", config.http_addr);
    axum::serve(listener, app).await.expect("serve");
}
