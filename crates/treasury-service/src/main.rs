mod api;
mod configuration;

use configuration::AppConfig;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let env = std::env::args().nth(2).unwrap_or_else(|| "default".to_string()); // `-- --env X` later; default fine
    let config = AppConfig::load(&env).expect("load config");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await
        .expect("connect postgres");
    sqlx::migrate!("./migrations").run(&pool).await.expect("migrations");

    let app = api::router(pool.clone(), config.clone());
    let listener = tokio::net::TcpListener::bind(&config.http_addr).await.expect("bind");
    tracing::info!("treasury-service listening on {}", config.http_addr);
    axum::serve(listener, app).await.expect("serve");
}
