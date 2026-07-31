use treasury_service::api;
use treasury_service::configuration::AppConfig;

/// How long to wait before retrying a FAILED reconciliation run. Deliberately far shorter than
/// `reconciliation_interval_secs`: minting is blocked until a run succeeds, so a transient failure
/// (node not up yet, a brief RPC outage) must not cost a full interval of downtime.
const RECONCILIATION_RETRY_SECS: u64 = 30;

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
        // Threshold sweep: move credited deposits off their derived addresses into the treasury.
        // Decides WHEN here; tron-signer knows HOW and owns the keys.
        tokio::spawn(treasury_service::sweeper::run(
            pool.clone(),
            config.clone(),
            config.reconciliation_interval_secs.min(3600),
        ));

        tokio::spawn(async move {
            loop {
                match treasury_service::reconciliation::run_once(
                    &pool, &node, cfg.custody_stub_balance_usdt, cfg.genesis_allocation as u64, &cfg,
                ).await {
                    Ok(status) => {
                        tracing::info!("reconciliation run: {}", status);
                        tokio::time::sleep(std::time::Duration::from_secs(cfg.reconciliation_interval_secs)).await;
                    }
                    // A FAILED run must not wait the full interval before trying again.
                    //
                    // Found on a real deployment: this loop's first run fires at startup and lost
                    // a race with the node WebSocket ("connection not established"). Sleeping
                    // `reconciliation_interval_secs` (86400) then meant no reconciliation existed
                    // for 24 hours — and the backing breaker correctly refuses to "mint blind"
                    // without a recent one, so the treasury could not mint at all for a day after
                    // every fresh deploy. The breaker was right; the retry cadence was wrong.
                    Err(e) => {
                        tracing::error!(
                            "reconciliation failed to run: {e} — retrying in {}s rather than \
                             waiting the full {}s interval (minting stays blocked until one \
                             succeeds)",
                            RECONCILIATION_RETRY_SECS,
                            cfg.reconciliation_interval_secs
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(RECONCILIATION_RETRY_SECS)).await;
                    }
                }
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
