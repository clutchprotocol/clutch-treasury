use config::{Config, ConfigError, Environment, File};
use dotenv::dotenv;
use serde::Deserialize;

/// 50 blocks. Comfortably above normal propagation, far below the 115,000-block lag that went
/// unnoticed on stage for a day.
fn default_max_node_lag_blocks() -> u64 {
    50
}

/// $5, in micro-USDT. Measured on 2026-09-10 rather than guessed: a TRC-20 USDT transfer into
/// an address that already holds USDT burns 64,285 energy, and the chain parameter
/// `getEnergyFee` was 100 sun per unit of energy. That is 6.43 TRX, about $2.19 at a TRX price
/// of $0.34. A sweep is always that shape, because the treasury address holds USDT from its
/// first sweep onward.
///
/// $5 is roughly twice the measured cost, and the doubling is the point: TRX price moves eat a
/// thinner margin, and the energy price is a governance parameter that has already halved once
/// (210 sun per energy to 100), so the cost can change without TRX moving at all. Re-measure
/// this number rather than scaling it.
fn default_sweep_min_usdt() -> i64 {
    5_000_000
}

/// No fee. Adding the mechanism must not start charging anyone by itself — the number is a
/// business decision, and a deployment that upgrades without choosing one keeps paying par.
fn default_redemption_fee_usdt() -> i64 {
    0
}

fn default_metrics_addr() -> String {
    "0.0.0.0:9101".to_string()
}

/// "env" -- `EnvKeySigner` off `mint_authority_secret`, exactly as before. This is what every CI
/// run uses, since CI has no live KMS to call and never will. "azure_kms" switches to
/// `AzureKmsSigner` off the `azure_*` fields below and refuses to start if a plaintext secret is
/// still present -- readiness A1. Nothing sets this in `test.yml`, so CI is unaffected by its
/// existence.
fn default_signer_kind() -> String {
    "env".to_string()
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
    /// Which `ChainSigner` to construct — see `default_signer_kind` above.
    #[serde(default = "default_signer_kind")]
    pub signer_kind: String,
    /// Required only when `signer_kind = "env"` (the default). Must be EMPTY when
    /// `signer_kind = "azure_kms"` — `load()` panics otherwise, deliberately: a plaintext mint
    /// key must not sit in `.env` once KMS custody is live.
    #[serde(default)]
    pub mint_authority_secret: String,
    /// The six below are required only when `signer_kind = "azure_kms"`, and ignored otherwise.
    /// See `crates/clutch-chain/src/azure_kms_signer.rs` for what each becomes.
    #[serde(default)]
    pub azure_tenant_id: String,
    #[serde(default)]
    pub azure_client_id: String,
    #[serde(default)]
    pub azure_client_secret: String,
    /// e.g. `https://clutch-mint-vault-1.vault.azure.net` — no trailing slash.
    #[serde(default)]
    pub azure_vault_url: String,
    #[serde(default)]
    pub azure_key_name: String,
    /// Pinned, never "current" — see azure_kms_signer.rs on why floating to latest is refused.
    #[serde(default)]
    pub azure_key_version: String,
    /// Addresses whose approval signatures this service will accept and relay, comma-separated.
    ///
    /// Verification convenience only: the node is authoritative and re-checks every signature
    /// against the genesis-committed set. Its job here is to reject a useless signature at the
    /// approve call rather than at submission time, where it would burn a nonce.
    #[serde(default)]
    pub mint_authorities: String,
    /// Signatures the chain requires for a Mint. 0 and 1 both mean single-signer.
    ///
    /// Must match the chain's genesis `mint_threshold`. Used to refuse submitting a mint that
    /// cannot possibly be accepted, rather than discovering it from the node.
    #[serde(default)]
    pub mint_threshold: u8,
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
    /// Taken off the USDT leg of a redemption, in micro-USDT. The user burns the full CLT and
    /// receives this much less.
    ///
    /// It is charged here rather than on the chain because `Burn` destroys exactly what it is
    /// handed and knows nothing about the payout — and because the reserve is where the fee needs
    /// to end up. Burning 10 and paying 9.5 leaves 0.5 of reserve behind with no liability against
    /// it, which reconciliation already treats as fine: it fails on reserve BELOW liability, never
    /// above.
    ///
    /// Keep `payment-orchestrator`'s `min_redemption_clt` above this. The treasury refuses an
    /// amount that does not cover the fee, but that refusal reaches the user as a bare gateway
    /// error; the orchestrator's own bound is what turns it into a sensible message.
    #[serde(default = "default_redemption_fee_usdt")]
    pub redemption_fee_usdt: i64,
    pub signer_url: String,
    pub signer_token: String,
    /// GasFree (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md), read by `load`
    /// from the environment with `gasfree::load_settings`, never from TOML. `None`, the default, is
    /// the TRX rail exactly as before.
    #[serde(skip)]
    pub gasfree: Option<gasfree::Settings>,
}

impl AppConfig {
    /// The configured approval authorities, lowercased and 0x-prefixed for comparison.
    pub fn mint_authority_set(&self) -> Vec<String> {
        self.mint_authorities
            .split(',')
            .map(|a| a.trim().trim_start_matches("0x").trim_start_matches("0X").to_lowercase())
            .filter(|a| !a.is_empty())
            .map(|a| format!("0x{a}"))
            .collect()
    }

    /// Signatures a Mint needs. 0 and 1 both mean one, matching the node.
    pub fn effective_mint_threshold(&self) -> usize {
        self.mint_threshold.max(1) as usize
    }

    pub fn load(env: &str) -> Result<Self, ConfigError> {
        dotenv().ok();
        let mut cfg: Self = Config::builder()
            .add_source(File::with_name(&format!("config/{}.toml", env)))
            .add_source(Environment::with_prefix("APP"))
            .build()?
            .try_deserialize()?;
        // Secrets are env-only; fail loudly, never run half-configured (spec §5).
        for (name, v) in [
            ("APP_INITIATOR_TOKEN", &cfg.initiator_token),
            ("APP_APPROVER_TOKEN", &cfg.approver_token),
            ("APP_READONLY_TOKEN", &cfg.readonly_token),
        ] {
            if v.trim().is_empty() {
                panic!("{name} is empty — set it in the environment (.env), never in TOML");
            }
        }
        // Which signer gets built, and the one check that makes readiness A1 a real property
        // rather than a suggestion: once a deployment declares azure_kms, it is refused outright
        // if the old plaintext secret is still sitting in .env, rather than silently ignoring it.
        match cfg.signer_kind.as_str() {
            "env" => {
                if cfg.mint_authority_secret.trim().is_empty() {
                    panic!(
                        "APP_MINT_AUTHORITY_SECRET is empty — set it in the environment (.env), \
                         never in TOML. (signer_kind=env, the default — set APP_SIGNER_KIND=azure_kms \
                         to use KMS custody instead.)"
                    );
                }
            }
            "azure_kms" => {
                for (name, v) in [
                    ("APP_AZURE_TENANT_ID", &cfg.azure_tenant_id),
                    ("APP_AZURE_CLIENT_ID", &cfg.azure_client_id),
                    ("APP_AZURE_CLIENT_SECRET", &cfg.azure_client_secret),
                    ("APP_AZURE_VAULT_URL", &cfg.azure_vault_url),
                    ("APP_AZURE_KEY_NAME", &cfg.azure_key_name),
                    ("APP_AZURE_KEY_VERSION", &cfg.azure_key_version),
                ] {
                    if v.trim().is_empty() {
                        panic!("{name} is empty — required when APP_SIGNER_KIND=azure_kms");
                    }
                }
                if !cfg.mint_authority_secret.trim().is_empty() {
                    panic!(
                        "APP_MINT_AUTHORITY_SECRET is set but APP_SIGNER_KIND=azure_kms — remove \
                         it from .env. A plaintext mint key must not remain on the host once KMS \
                         custody is live (readiness A1); this refusal is what makes that true \
                         rather than merely intended."
                    );
                }
            }
            other => panic!(
                "APP_SIGNER_KIND must be \"env\" or \"azure_kms\", got \"{other}\""
            ),
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
        // From the environment only, like the secrets above: the three services read the same
        // variables from one env file (spec §6), and a half-set rail stops the service here.
        cfg.gasfree = gasfree::load_settings(|name| std::env::var(name).ok()).unwrap_or_else(|e| panic!("{e}"));
        Ok(cfg)
    }
}
