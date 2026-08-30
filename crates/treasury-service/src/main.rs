use treasury_service::api;
use treasury_service::configuration::AppConfig;
use treasury_service::payout;
use treasury_service::tron_verifier::TronClient;

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
    let peers = treasury_service::chain_sync::peer_clients(&config.node_peer_ws_urls);
    {
        let pool = pool.clone();
        let node = node.clone();
        let cfg = config.clone();
        // Threshold sweep: move credited deposits off their derived addresses into the treasury.
        // Decides WHEN here; tron-signer knows HOW and owns the keys.
        //
        // The interval must stay comfortably above Tron's block time. Funding an address and
        // sweeping it are two separate passes -- the TRX has to confirm before it can be spent --
        // and a pass that came round before the funding landed would read a zero balance and fund
        // the same address again. An hour is not a latency requirement (the deposit is already
        // credited; this is only consolidation), so there is no reason to shorten it.
        tokio::spawn(treasury_service::sweeper::run(
            pool.clone(),
            config.clone(),
            config.reconciliation_interval_secs.min(3600),
        ));

        tokio::spawn(async move {
            loop {
                match treasury_service::reconciliation::run_once(
                    &pool, &node, cfg.genesis_allocation as u64, &cfg,
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

    // Does the chain agree that we are the mint authority?
    //
    // The authority is fixed at genesis. Hold a different key and every Mint is signed correctly,
    // accepted by the RPC, and then dropped by consensus -- the outbox records `submitted` with
    // zero attempts and no error, total supply never moves, and the intent sits in `submitted`
    // forever. It looks exactly like a mint that is merely slow, which is how it went unnoticed on
    // stage until someone asked why a swept deposit had minted nothing.
    //
    // Checked once, in the background: the node may not be up yet at boot, so this retries rather
    // than crash-looping the whole service over a dependency that is still starting.
    {
        use clutch_chain::signer::ChainSigner;
        let node = node.clone();
        let pool = pool.clone();
        let ours = signer.address();
        tokio::spawn(async move {
            loop {
                match node.get_chain_info().await {
                    Ok(info) => {
                        if info.mint_authority.eq_ignore_ascii_case(&ours) {
                            tracing::info!("mint authority confirmed by the chain: {ours}");
                        } else {
                            let msg = format!(
                                "MINT AUTHORITY MISMATCH: this service signs as {ours} but the chain's \
                                 authority is {}. Every mint will be accepted by the RPC and then \
                                 silently dropped by consensus. Minting halted.",
                                info.mint_authority
                            );
                            tracing::error!("{msg}");
                            // Halt rather than let mints keep vanishing: a mint that is submitted
                            // and dropped leaves a depositor credited in the ledger and holding
                            // nothing on chain.
                            let _ = treasury_service::breakers::manual_halt(&pool, &msg, "startup-check").await;
                            treasury_service::ledger::alert(&pool, "p1", "startup", &msg).await;
                        }
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("mint authority check: node not ready yet ({e}); retrying in 10s");
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    }
                }
            }
        });
    }
    {
        let pool = pool.clone();
        let node = node.clone();
        let cfg = config.clone();
        tokio::spawn(async move {
            loop {
                match treasury_service::outbox::drain_once(&pool, &node, &peers, &signer, &cfg).await {
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
        // Real TRC-20 payouts via tron-signer's /internal/payout, then an on-chain confirmation
        // pass right after. Same poll cadence as the mint outbox/watcher; no dedicated interval
        // justified here either.
        //
        // Confirmation runs immediately after drain, in the same tick, rather than waiting for the
        // next interval: a payout submitted this pass gets its first confirmation check now instead
        // of a full interval later. Nothing claims money moved until that leg confirms it on chain —
        // a paid intent parks at `payout_submitted` with its `payout_ref` set until then.
        let pool = pool.clone();
        let payout_signer = payout::HttpPayoutSigner {
            // 30s: tron-signer's own handler makes several sequential TronGrid round trips
            // before it can answer at all (balance reads, building the transfer, sometimes a TRX
            // top-up first), so this has to be generous enough that a slow-but-real chain of
            // those doesn't masquerade as a timeout. A timeout here is indistinguishable from a
            // broadcast that landed (see HttpPayoutSigner::pay's Ambiguous arm) and is treated
            // exactly that conservatively, so it must also not be unbounded: confirm_payouts_once
            // runs right after drain_once in this same loop tick, so a hung signer call would
            // otherwise stall on-chain confirmation checks too, forever.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client builder"),
            base_url: config.signer_url.clone(),
            token: config.signer_token.clone(),
        };
        let tron_client = TronClient::new(config.trongrid_url.clone(), config.trongrid_api_key.clone());
        let cfg = config.clone();
        tokio::spawn(async move {
            loop {
                match payout::drain_once(&pool, &cfg, &payout_signer).await {
                    Ok(n) if n > 0 => tracing::info!("payout: paid {} redemption(s)", n),
                    Ok(_) => {}
                    Err(e) => tracing::error!("payout drain failed: {}", e),
                }
                if let Err(e) = payout::confirm_payouts_once(&pool, &tron_client).await {
                    tracing::error!("payout confirmation failed: {e}");
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
