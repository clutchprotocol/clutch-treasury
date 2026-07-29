use config::{Config, ConfigError, Environment, File};
use dotenv::dotenv;
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct OrchConfig {
    pub http_addr: String,
    pub database_url: String,
    pub jwt_secret: String,
    pub bitcart_url: String,
    pub bitcart_token: String,
    pub bitcart_store_id: String,
    pub treasury_url: String,
    pub treasury_initiator_token: String,
    pub treasury_readonly_token: String,
    pub custody_tron_address: String,
    pub deposit_ttl_minutes: i64,
    pub min_deposit_usdt: i64,
    pub max_deposit_usdt: i64,
    pub poll_interval_secs: u64,
}

impl OrchConfig {
    pub fn load(env: &str) -> Result<Self, ConfigError> {
        dotenv().ok();
        let cfg: Self = Config::builder()
            .add_source(File::with_name(&format!("config/{}.toml", env)))
            .add_source(Environment::with_prefix("APP"))
            .build()?
            .try_deserialize()?;
        // Secrets are env-only; fail loudly, never run half-configured (same rule as
        // treasury-service — the orchestrator holds no chain keys or approver token, but
        // its own four secrets get the identical treatment).
        for (name, v) in [
            ("APP_JWT_SECRET", &cfg.jwt_secret),
            ("APP_BITCART_TOKEN", &cfg.bitcart_token),
            ("APP_TREASURY_INITIATOR_TOKEN", &cfg.treasury_initiator_token),
            ("APP_TREASURY_READONLY_TOKEN", &cfg.treasury_readonly_token),
        ] {
            if v.trim().is_empty() {
                panic!("{name} is empty — set it in the environment (.env), never in TOML");
            }
        }
        Ok(cfg)
    }
}
