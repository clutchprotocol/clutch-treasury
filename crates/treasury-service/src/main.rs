use treasury_service::api;
use treasury_service::configuration::AppConfig;

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

    let node = clutch_chain::node_client::NodeClient::new(config.node_ws_url.clone());
    {
        let pool = pool.clone();
        let node = node.clone();
        let cfg = config.clone();
        tokio::spawn(async move {
            loop {
                match treasury_service::reconciliation::run_once(
                    &pool, &node, cfg.custody_stub_balance_usdt, cfg.genesis_allocation as u64, &cfg,
                ).await {
                    Ok(status) => tracing::info!("reconciliation run: {}", status),
                    Err(e) => tracing::error!("reconciliation failed to run: {}", e),
                }
                tokio::time::sleep(std::time::Duration::from_secs(cfg.reconciliation_interval_secs)).await;
            }
        });
    }
    // Plan C T5: the independent TronGrid verifier — scans `created` deposit-backed intents
    // (client_ref IS NOT NULL) and auto-approves on confirmed on-chain evidence alone. Same
    // poll-loop shape as the outbox/watcher below; "reschedule with backoff" for a transient
    // TronGrid failure is exactly this loop leaving the intent untouched for the next tick.
    {
        let pool = pool.clone();
        let cfg = config.clone();
        tokio::spawn(async move {
            loop {
                match treasury_service::tron_verifier::verify_once(&pool, &cfg).await {
                    Ok(n) if n > 0 => tracing::info!("tron_verifier: approved {} deposit(s)", n),
                    Ok(_) => {}
                    Err(e) => tracing::error!("tron_verifier pass failed: {}", e),
                }
                tokio::time::sleep(std::time::Duration::from_millis(cfg.outbox_poll_ms)).await;
            }
        });
    }

    // Serial single worker: node enforces one tx per sender per block, so one mint per
    // block cadence is the ceiling anyway — fine at pilot volume.
    // ponytail: batch Mint tx submission if volume ever demands more than one per block.
    let signer = clutch_chain::signer::EnvKeySigner::from_secret_hex(&config.mint_authority_secret)
        .expect("mint authority secret must decode to a valid secp256k1 key");
    {
        let pool = pool.clone();
        let node = node.clone();
        let cfg = config.clone();
        tokio::spawn(async move {
            loop {
                match treasury_service::outbox::drain_once(&pool, &node, &signer, &cfg).await {
                    Ok(n) if n > 0 => tracing::info!("outbox: submitted {} mint(s)", n),
                    Ok(_) => {}
                    Err(e) => tracing::error!("outbox drain failed: {}", e),
                }
                tokio::time::sleep(std::time::Duration::from_millis(cfg.outbox_poll_ms)).await;
            }
        });
    }
    {
        let pool = pool.clone();
        let node = node.clone();
        let cfg = config.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = treasury_service::watcher::poll_once(&pool, &node, cfg.confirmations).await {
                    tracing::error!("watcher poll failed: {}", e);
                }
                tokio::time::sleep(std::time::Duration::from_millis(cfg.outbox_poll_ms)).await;
            }
        });
    }
    {
        // StubRail only — the real Tron rail is Plan C follow-on (docs/keys.md: the payout
        // key doesn't exist yet). Same poll cadence as the mint outbox/watcher; no dedicated
        // interval justified for a stub.
        let pool = pool.clone();
        let rail = treasury_service::payout::StubRail;
        let cfg = config.clone();
        tokio::spawn(async move {
            loop {
                match treasury_service::payout::drain_once(&pool, &rail).await {
                    Ok(n) if n > 0 => tracing::info!("payout: paid {} redemption(s)", n),
                    Ok(_) => {}
                    Err(e) => tracing::error!("payout drain failed: {}", e),
                }
                tokio::time::sleep(std::time::Duration::from_millis(cfg.outbox_poll_ms)).await;
            }
        });
    }

    let app = api::router(pool.clone(), config.clone());
    let listener = tokio::net::TcpListener::bind(&config.http_addr).await.expect("bind");
    tracing::info!("treasury-service listening on {}", config.http_addr);
    axum::serve(listener, app).await.expect("serve");
}
