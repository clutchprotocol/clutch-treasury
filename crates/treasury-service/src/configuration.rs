use config::{Config, ConfigError, Environment, File};
use dotenv::dotenv;
use serde::Deserialize;

/// 50 blocks. Comfortably above normal propagation, far below the 115,000-block lag that went
/// unnoticed on stage for a day.
fn default_max_node_lag_blocks() -> u64 {
    50
}

/// $1, in micro-USDT. A starting point, NOT a measurement: the real number is what one sweep
/// costs in TRX at your energy prices, and it moves. Raise it once you have observed the cost.
fn default_sweep_min_usdt() -> i64 {
    1_000_000
}

fn default_metrics_addr() -> String {
    "0.0.0.0:9101".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub http_addr: String,
    /// Where the Prometheus listener binds. A separate port from `http_addr` on purpose: this
    /// service's API must never grow a public metrics route. Defaulted so an existing
    /// deployment boots unchanged - a missing field here would panic at startup.
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: String,
    pub database_url: String,
    pub node_ws_url: String,
    /// Comma-separated peer node WebSocket URLs, used ONLY to ask "is `node_ws_url` at the tip".
    /// Empty means the check cannot run, which is reported as unknown rather than as healthy.
    #[serde(default)]
    pub node_peer_ws_urls: String,
    /// How far the primary may trail the best peer before this service stops acting on what it
    /// says. Some lag is normal — blocks propagate — so this is a tolerance, not zero.
    #[serde(default = "default_max_node_lag_blocks")]
    pub max_node_lag_blocks: u64,
    pub chain_id: u64,
    pub mint_authority_secret: String,
    pub initiator_token: String,
    pub approver_token: String,
    pub readonly_token: String,
    pub daily_mint_cap_clt: i64,
    /// Rolling 24h payout ceiling in CLT base units.
    ///
    /// Separate from the signer's `per_tx_payout_cap_usdt`, which is micro-USDT and per-transaction.
    /// The two are equal at 1:1 par and must still be configured independently — collapsing them
    /// would silently couple a unit change on one side to the other.
    pub daily_payout_cap_clt: i64,
    pub per_tx_mint_cap_clt: i64,
    pub backing_target_bps: i64,
    pub backing_halt_bps: i64,
    pub confirmations: u64,
    pub outbox_poll_ms: u64,
    pub reconciliation_interval_secs: u64,
    pub genesis_allocation: i64,
    pub trongrid_url: String,
    pub trongrid_api_key: String,
    pub custody_tron_address: String,
    /// The payout float address, read off tron-signer's /internal/xpub.
    ///
    /// Configured rather than derived: this service holds no key material and must not be able to
    /// derive spending addresses. It only needs to know where to LOOK, so it is given the address.
    pub payout_float_address: String,
    pub usdt_contract: String,
    pub deposit_confirmations: u32,
    /// How far back the verifier's fallback (no-tx-hash) match may reach for a transfer, relative
    /// to the intent's creation. Bounded because discriminator slots are recycled after an invoice
    /// goes terminal, so without a limit an old unclaimed transfer at the same amount could be
    /// swept up to back a stranger's later deposit.
    pub deposit_match_window_hours: i64,
    /// Sweep a deposit address once it holds at least this much (micro-USDT). A sweep costs TRX for
    /// energy, so per-deposit sweeping can cost more than it moves at the $1 minimum.
    pub sweep_threshold_usdt: i64,
    /// ...or once it is this old, whatever the balance. Without this a sub-threshold balance sits
    /// forever and the reserve fragments across addresses nobody revisits.
    pub sweep_max_age_hours: i64,
    /// The floor under that age valve: never sweep less than this (micro-USDT), however old.
    ///
    /// Without a floor the age rule also sweeps dust, and a TRC-20 transfer costs TRX for energy —
    /// so moving $0.10 can burn several dollars of it. That is a real loss. Leaving the dust alone
    /// is not: an unswept deposit address is still counted in the reserve, and since addresses are
    /// permanent per user, the balance sweeps by itself once that user's next deposit lifts it over
    /// this line.
    #[serde(default = "default_sweep_min_usdt")]
    pub sweep_min_usdt: i64,
    pub signer_url: String,
    pub signer_token: String,
}

impl AppConfig {
    pub fn load(env: &str) -> Result<Self, ConfigError> {
        dotenv().ok();
        let cfg: Self = Config::builder()
            .add_source(File::with_name(&format!("config/{}.toml", env)))
            .add_source(Environment::with_prefix("APP"))
            .build()?
            .try_deserialize()?;
        // Secrets are env-only; fail loudly, never run half-configured (spec §5).
        for (name, v) in [
            ("APP_MINT_AUTHORITY_SECRET", &cfg.mint_authority_secret),
            ("APP_INITIATOR_TOKEN", &cfg.initiator_token),
            ("APP_APPROVER_TOKEN", &cfg.approver_token),
            ("APP_READONLY_TOKEN", &cfg.readonly_token),
        ] {
            if v.trim().is_empty() {
                panic!("{name} is empty — set it in the environment (.env), never in TOML");
            }
        }
        // The TronGrid key is deliberately NOT in the list above. TronGrid serves the endpoints
        // this service reads without any key, just at a lower rate limit — so demanding one makes
        // a keyless testnet run impossible and pushes operators into inventing a placeholder. That
        // is strictly worse than an empty value: a fake key still gets sent as a header, still
        // lands on the rate-limited tier, and makes the config assert something untrue.
        //
        // Warn instead, so a production deployment running unkeyed is visible rather than silent.
        // The env-only rule still applies whenever a key IS set.
        if cfg.trongrid_api_key.trim().is_empty() {
            tracing::warn!(
                "APP_TRONGRID_API_KEY is not set — deposit verification will use TronGrid's \
                 rate-limited public tier. Acceptable for local/testnet; set a key for production."
            );
        }
        assert!(cfg.backing_halt_bps <= cfg.backing_target_bps, "halt bps above target bps");
        Ok(cfg)
    }
}
