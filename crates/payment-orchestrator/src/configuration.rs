use config::{Config, ConfigError, Environment, File};
use dotenv::dotenv;
use serde::Deserialize;

fn default_allowed_origins() -> String {
    "*".to_string()
}

fn default_metrics_addr() -> String {
    "0.0.0.0:9102".to_string()
}

fn default_rate_limit_per_minute() -> u32 {
    10
}

#[derive(Debug, Deserialize, Clone)]
pub struct OrchConfig {
    pub http_addr: String,
    /// Where the Prometheus listener binds. A separate port from `http_addr` on purpose: this
    /// service's API must never grow a public metrics route. Defaulted so an existing
    /// deployment boots unchanged - a missing field here would panic at startup.
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: String,
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
    /// Gates `POST /api/v1/deposits` (`create_deposit_handler`) — same shape as
    /// `redemptions_enabled` below: while this is `false` (the default) the route 503s before
    /// authentication even runs, rather than deriving and handing out a new address.
    ///
    /// This flag protects the rollout, not the decision: once a user has been handed an address
    /// and sent USDT to it, that address must be watched and swept forever, regardless of what
    /// this is switched to later. Turning it off only stops NEW addresses from being issued — it
    /// does not un-issue, stop watching, or stop sweeping the ones already out.
    pub permanent_deposit_addresses_enabled: bool,
    pub poll_interval_secs: u64,
    /// How long a user's address stays on the fast poll tier after they open the deposit panel.
    ///
    /// Long enough that someone who opens the panel, goes to fetch USDT and comes back the next day
    /// is still on the fast path; short enough that the hot set stays a small fraction of all
    /// addresses, which is what makes the per-pass budget mean anything. Setting this very large
    /// collapses tiering back into polling everything — that degrades cost, not correctness.
    pub deposit_hot_window_hours: i64,
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
    /// The smallest redemption this service will accept, in CLT base units.
    ///
    /// It MUST stay above the treasury's `redemption_fee_usdt`. The two live in different
    /// services and are not derived from each other, so nothing enforces the ordering at boot:
    /// the treasury refuses an intent whose net payout would be zero or negative with a 400,
    /// and this service maps that to `TreasuryRejected`, a 502 reading "treasury refused the
    /// redemption request". Every amount between the fee and this minimum would fail that way,
    /// with no message naming the real reason. Move the two together.
    pub min_redemption_clt: i64,
    pub max_redemption_clt: i64,
    /// Requests per minute per authenticated identity, on the two POST routes.
    ///
    /// Keypairs are free, so the JWT proves who is calling and bounds nothing about how often.
    /// Both POST routes leave something durable behind — a permanently polled deposit address,
    /// or a redemption intent against the payout float — so they need a bound that is not
    /// "however fast you can sign challenges". Ten is far above what the demo app's one POST per
    /// panel open needs, and far below what a flood needs to matter. See `ratelimit`.
    #[serde(default = "default_rate_limit_per_minute")]
    pub rate_limit_per_minute: u32,
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
