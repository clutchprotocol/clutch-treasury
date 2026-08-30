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
    /// TronGrid, for watching the custody address. MUST name the same network as
    /// `usdt_contract` lives on, or every deposit goes unmatched forever.
    pub trongrid_url: String,
    /// Optional but strongly advised: unkeyed TronGrid throttles hard, and a throttled watcher
    /// looks exactly like "nobody is paying".
    #[serde(default)]
    pub trongrid_api_key: String,
    /// The TRC-20 contract deposits arrive in. Checked against every observed transfer, so a
    /// wrong value here silently matches nothing rather than crediting the wrong token.
    pub usdt_contract: String,
    pub treasury_url: String,
    pub treasury_initiator_token: String,
    pub treasury_readonly_token: String,
    /// Where swept deposits land. NOT the address users pay into — each intent gets its own
    /// derived address now (see `derive.rs`); this is the sweep destination.
    pub custody_tron_address: String,
    /// ACCOUNT-level xpub, `m/44'/195'/0'`. Public material: it can derive receive addresses and
    /// nothing else, which is why this service may hold it while the signer holds the mnemonic.
    /// Parsed once at startup so a malformed value fails at boot rather than per request.
    pub deposit_account_xpub: String,
    pub deposit_ttl_minutes: i64,
    pub min_deposit_usdt: i64,
    pub max_deposit_usdt: i64,
    pub poll_interval_secs: u64,
    /// Plan C T6 gate, default `false`. The treasury's payout rail is real now — a TRC-20
    /// transfer from a derived float via `tron-signer`'s `/internal/payout` — not the old stub
    /// that fabricated `payout_ref = "stub:<uuid>"` and sent nothing. This flag no longer waits
    /// on the rail existing; it waits on the rollout in
    /// `clutch-treasury/docs/superpowers/specs/2026-08-30-redemption-payout-rail-design.md` §5
    /// (float funded, reconciliation confirmed `ok` with it counted). While this is `false` both
    /// redemption routes 503 rather than accept a request, because a burn is irreversible: a user
    /// who is allowed to redeem destroys their CLT claim on the reserve and gets a
    /// `redemption_ref` for a payout that cannot happen while the float behind it is unfunded.
    /// Flip only once that rollout checklist is done.
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
