use payment_orchestrator::api;
use payment_orchestrator::configuration::OrchConfig;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let env = std::env::args().nth(2).unwrap_or_else(|| "default".to_string()); // `-- --env X` later; default fine
    let config = OrchConfig::load(&env).expect("load config");

    let app = api::router(config.clone());
    let listener = tokio::net::TcpListener::bind(&config.http_addr).await.expect("bind");
    tracing::info!("payment-orchestrator listening on {}", config.http_addr);
    axum::serve(listener, app).await.expect("serve");
}
