use config::{Config, ConfigError, Environment, File};
use dotenv::dotenv;
use serde::Deserialize;

fn default_allowed_origins() -> String {
    "*".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct OrchConfig {
    pub http_addr: String,
    pub database_url: String,
    pub jwt_secret: String,
    /// CORS: `"*"` or a comma-separated list of allowed origins (e.g.
    /// `https://app.example.com,https://app-stage.example.com`). Same config style as
    /// clutch-hub-api's `AppConfig::allowed_origins` — this service has no browser routes of its
    /// own before this change, so there was nothing to mirror until now.
    #[serde(default = "default_allowed_origins")]
    pub allowed_origins: String,
    pub bitcart_url: String,
    pub bitcart_token: String,
    pub bitcart_store_id: String,
    /// Must be the payment TOKEN (e.g. "USDT"), never a fiat code — see
    /// `BitcartAdapter::invoice_currency` for what a fiat value silently destroys.
    pub bitcart_invoice_currency: String,
    /// This service's own externally-reachable base URL — used only to build the
    /// `notification_url` passed to `adapter.create_invoice` (Bitcart's IPN webhook
    /// target, T4's handler). Not a secret.
    pub public_base_url: String,
    pub treasury_url: String,
    pub treasury_initiator_token: String,
    pub treasury_readonly_token: String,
    pub custody_tron_address: String,
    pub deposit_ttl_minutes: i64,
    pub min_deposit_usdt: i64,
    pub max_deposit_usdt: i64,
    pub poll_interval_secs: u64,
    /// Plan C T6 gate, default `false`: the treasury's payout rail is still `payout::StubRail`
    /// (fabricates `payout_ref = "stub:<uuid>"`, sends nothing — spec §7.6 wants a working
    /// off-ramp before real deposits). While this is `false` both redemption routes 503 rather
    /// than accept a request, because a burn is irreversible: a user who is allowed to redeem
    /// destroys their CLT claim on the reserve and gets a `redemption_ref` for a payout that
    /// cannot happen. Flip only after a real TRC-20 payout rail replaces the stub.
    pub redemptions_enabled: bool,
    pub min_redemption_clt: i64,
    pub max_redemption_clt: i64,
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
