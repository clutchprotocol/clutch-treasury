use config::{Config, ConfigError, Environment, File};
use dotenv::dotenv;
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub http_addr: String,
    pub database_url: String,
    pub node_ws_url: String,
    pub chain_id: u64,
    pub mint_authority_secret: String,
    pub initiator_token: String,
    pub approver_token: String,
    pub readonly_token: String,
    pub daily_mint_cap_clt: i64,
    pub per_tx_mint_cap_clt: i64,
    pub backing_target_bps: i64,
    pub backing_halt_bps: i64,
    pub custody_stub_balance_usdt: i64,
    pub confirmations: u64,
    pub outbox_poll_ms: u64,
    pub reconciliation_interval_secs: u64,
    pub genesis_allocation: i64,
    pub trongrid_url: String,
    pub trongrid_api_key: String,
    pub custody_tron_address: String,
    pub usdt_contract: String,
    pub deposit_confirmations: u32,
    /// How far back the verifier's fallback (no-tx-hash) match may reach for a transfer, relative
    /// to the intent's creation. Bounded because discriminator slots are recycled after an invoice
    /// goes terminal, so without a limit an old unclaimed transfer at the same amount could be
    /// swept up to back a stranger's later deposit.
    pub deposit_match_window_hours: i64,
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
