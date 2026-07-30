use std::sync::Arc;

use payment_orchestrator::adapter::BitcartAdapter;
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

    let adapter: Arc<dyn payment_orchestrator::adapter::PaymentAdapter> = Arc::new(BitcartAdapter {
        http: reqwest::Client::new(),
        base_url: config.bitcart_url.clone(),
        token: config.bitcart_token.clone(),
        store_id: config.bitcart_store_id.clone(),
        deposit_ttl_minutes: config.deposit_ttl_minutes,
        invoice_currency: config.bitcart_invoice_currency.clone(),
    });

    // The poller is the reliability path (Bitcart's IPN is unsigned and never retried) — every
    // state the webhook can reach must also be reachable here alone, on a plain timer.
    tokio::spawn(payment_orchestrator::poller::run(pool.clone(), adapter.clone(), config.poll_interval_secs));

    // The deposit->mint bridge (Plan C 5b) — the only thing in this crate that crosses into the
    // treasury's private zone. Same poll-interval convention as the Bitcart poller above.
    tokio::spawn(payment_orchestrator::treasury_bridge::run(pool.clone(), config.clone(), config.poll_interval_secs));

    let app = api::router(pool, config.clone(), adapter);
    let listener = tokio::net::TcpListener::bind(&config.http_addr).await.expect("bind");
    tracing::info!("payment-orchestrator listening on {}", config.http_addr);
    axum::serve(listener, app).await.expect("serve");
}
