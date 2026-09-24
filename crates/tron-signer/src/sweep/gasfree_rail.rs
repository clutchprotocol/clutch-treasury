//! The GasFree rail: sweeping a deposit out of a GasFree account, paying a redemption out of the
//! GasFree float, and activating that float — each with a permit signed here and handed to the
//! pinned relay.
//!
//! # What does not change
//!
//! A sweep still takes an index and nothing else. Every field of a permit comes from this
//! service's config or from the chain: the token and custody from config, the receiver from the
//! float's balance, `maxFee` from the configured maxima and the chain's record of activation, the
//! nonce from the controller. A payout still spends only from the float, which on this rail is the
//! float's GasFree account, `F = gasfree(the 2/0 address)`.
//!
//! # What the relay is trusted with
//!
//! The relay submits permits and pays the network. It is never the source of a number that decides
//! how much moves: activation is read from `getcontract`, the nonce from the controller's
//! `nonces`, balances from `balanceOf`. The relay's replies are only ever used to wait — a nonce
//! ahead of the chain's, `allowSubmit` false or a `frozen` amount means a transfer is in flight.
//! And the relay's idea of a GasFree address is compared with this service's own derivation before
//! anything is signed.
//!
//! # The tripwire
//!
//! GasFree accounts are beacon proxies, and the controller that moves money out of them is an
//! upgradeable proxy too. Before every permit this service reads both `implementation()`s and signs
//! nothing when either differs from the reviewed one it was configured with. See spec §5 in
//! docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md.

use k256::ecdsa::{signature::hazmat::PrehashSigner, RecoveryId, Signature, SigningKey};

use super::{abi_address, describe_rejection, SweepClient, SweepOutcome};
use crate::keys::Signer;
use crate::relay::{Relay, RelayConfig, RelayError};

#[cfg(test)]
mod tests;

pub struct GasFreeConfig {
    /// Which GasFree deployment: `gasfree::NILE` or `gasfree::MAINNET`.
    pub chain: &'static gasfree::Chain,
    pub relay: RelayConfig,
    /// The one relay every permit names. A permit is only valid for the provider it names, so this
    /// is pinned rather than picked at runtime.
    pub service_provider: String,
    /// The most a first transfer may pay for activation, in micro-USDT. Above the live fee.
    pub activate_fee_max_usdt: i64,
    /// The most any transfer may pay the relay, in micro-USDT. Above the live fee.
    pub transfer_fee_max_usdt: i64,
    /// The beacon's `implementation()` when it was reviewed: 40 lowercase hex, no `0x`.
    pub expected_beacon_implementation: String,
    /// The controller's `implementation()` when it was reviewed: 40 lowercase hex, no `0x`.
    pub expected_controller_implementation: String,
    /// Sweeps go to the GasFree float until it holds this much, in micro-USDT.
    pub payout_float_target_usdt: i64,
    /// How long a signed permit stays valid, in seconds. The relay accepts 60 to 600.
    pub deadline_secs: u64,
    /// Redemptions are paid from the GasFree float (`APP_TRANSFER_RAIL=gasfree`), not from 2/0 in TRX.
    pub payouts: bool,
}

/// The GasFree half of a `SweepClient`: its settings and its relay.
pub(super) struct GasFree {
    pub(super) cfg: GasFreeConfig,
    pub(super) relay: Relay,
}

impl GasFree {
    pub(super) fn new(cfg: GasFreeConfig) -> Self {
        let relay = Relay::new(cfg.relay.clone());
        Self { cfg, relay }
    }
}

/// What the boot check found.
#[derive(Debug, PartialEq)]
pub enum SelfTest {
    Passed,
    /// The chain answered, and the answer is wrong for this configuration. The signer must not start.
    Failed(String),
    /// TronGrid did not answer. Not fatal: the checks before each permit still run.
    Unreachable(String),
}

/// Read the GasFree settings. `Ok(None)` means GasFree is off, which is the default.
///
/// GasFree is on when `APP_GASFREE_API_KEY` is set, and then every other setting is required: a
/// missing one stops the signer at boot, not at the first deposit.
pub fn load_gasfree_config(var: impl Fn(&str) -> Option<String>) -> Result<Option<GasFreeConfig>, String> {
    let rail = var("APP_TRANSFER_RAIL").unwrap_or_else(|| "trx".to_string());
    let payouts = match rail.trim() {
        "trx" => false,
        "gasfree" => true,
        other => return Err(format!("APP_TRANSFER_RAIL must be trx or gasfree, got {other:?}")),
    };
    let api_key = var("APP_GASFREE_API_KEY").map(|v| v.trim().to_string()).unwrap_or_default();
    if api_key.is_empty() {
        return if payouts {
            Err("APP_TRANSFER_RAIL=gasfree needs APP_GASFREE_API_KEY and the other APP_GASFREE_* settings".into())
        } else {
            Ok(None)
        };
    }

    let required = |name: &str| {
        var(name)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("{name} must be set when APP_GASFREE_API_KEY is"))
    };
    let chain: &'static gasfree::Chain = match required("APP_GASFREE_NETWORK")?.as_str() {
        "nile" => &gasfree::NILE,
        "mainnet" => &gasfree::MAINNET,
        other => return Err(format!("APP_GASFREE_NETWORK must be nile or mainnet, got {other:?}")),
    };
    let service_provider = required("APP_GASFREE_SERVICE_PROVIDER")?;
    abi_address(&service_provider).map_err(|e| format!("APP_GASFREE_SERVICE_PROVIDER: {e}"))?;
    let deadline_secs = match var("APP_GASFREE_DEADLINE_SECS") {
        None => 180,
        Some(raw) => raw
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("APP_GASFREE_DEADLINE_SECS must be whole seconds, got {raw:?}"))?,
    };
    // The relay's published limits. A permit outside them is refused at submit, every time.
    if !(60..=600).contains(&deadline_secs) {
        return Err(format!("APP_GASFREE_DEADLINE_SECS must be 60 to 600, got {deadline_secs}"));
    }

    Ok(Some(GasFreeConfig {
        chain,
        relay: RelayConfig {
            base_url: required("APP_GASFREE_API_URL")?.trim_end_matches('/').to_string(),
            api_key,
            api_secret: required("APP_GASFREE_API_SECRET")?,
        },
        service_provider,
        activate_fee_max_usdt: positive_micro_usdt(
            "APP_GASFREE_ACTIVATE_FEE_MAX_USDT",
            &required("APP_GASFREE_ACTIVATE_FEE_MAX_USDT")?,
        )?,
        transfer_fee_max_usdt: positive_micro_usdt(
            "APP_GASFREE_TRANSFER_FEE_MAX_USDT",
            &required("APP_GASFREE_TRANSFER_FEE_MAX_USDT")?,
        )?,
        expected_beacon_implementation: implementation_hex(
            "APP_GASFREE_EXPECTED_IMPLEMENTATION",
            &required("APP_GASFREE_EXPECTED_IMPLEMENTATION")?,
        )?,
        expected_controller_implementation: implementation_hex(
            "APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION",
            &required("APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION")?,
        )?,
        payout_float_target_usdt: positive_micro_usdt(
            "APP_PAYOUT_FLOAT_TARGET_USDT",
            &required("APP_PAYOUT_FLOAT_TARGET_USDT")?,
        )?,
        deadline_secs,
        payouts,
    }))
}

/// A zero maximum would sign permits the relay refuses, stopping every sweep while looking set up.
fn positive_micro_usdt(name: &str, raw: &str) -> Result<i64, String> {
    match raw.trim().parse::<i64>() {
        Ok(v) if v > 0 => Ok(v),
        _ => Err(format!("{name} must be a positive whole number of micro-USDT, got {raw:?}")),
    }
}

/// `0xA3B0…` or `a3b0…` in, 40 lowercase hex characters out.
fn implementation_hex(name: &str, raw: &str) -> Result<String, String> {
    let hex = raw.trim().trim_start_matches("0x").trim_start_matches("0X").to_ascii_lowercase();
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{name} must be a 20-byte hex address like 0xa3b0edff…, got {raw:?}"));
    }
    Ok(hex)
}

/// Sign a permit as TIP-712 wallets do: the permit hash itself, no prefix, and `v` as 27 or 28.
///
/// Not `sign_txid`'s convention, where TRON wants the bare recovery id 0 or 1. The GasFree docs'
/// own example signature ends in `1b`, which is 27.
pub(super) fn sign_permit(key: &SigningKey, chain: &gasfree::Chain, permit: &gasfree::Permit<'_>) -> Result<String, String> {
    todo!("Task 3 Step 5")
}

impl SweepClient {
    /// Whether `address` holds a deployed contract; for a GasFree account, whether it is activated.
    pub(super) async fn has_contract(&self, address: &str) -> Result<bool, String> {
        // `contract_address`, NOT `bytecode`: on 2026-09-24 the activated mainnet GasFree account
        // TBdkSW3VkKsA8RxmZFxMvNezimndUEymgg returned its contract record with an EMPTY bytecode,
        // and an address with no contract returns `{}`.
        let resp = self.post("/wallet/getcontract", serde_json::json!({"value": address, "visible": true})).await?;
        Ok(resp["contract_address"].as_str().is_some_and(|a| !a.is_empty()))
    }

    /// The first 32-byte word a view function returns, as 64 lowercase hex characters.
    async fn view_word(&self, contract: &str, selector: &str, parameter: Option<&str>) -> Result<String, String> {
        // The contract is also the caller. TronGrid wants an `owner_address`, and a view call's
        // caller changes nothing.
        let mut body = serde_json::json!({
            "owner_address": contract,
            "contract_address": contract,
            "function_selector": selector,
            "visible": true,
        });
        if let Some(p) = parameter {
            body["parameter"] = serde_json::Value::from(p);
        }
        let resp = self.post("/wallet/triggerconstantcontract", body).await?;
        let word = resp["constant_result"][0]
            .as_str()
            .ok_or_else(|| format!("{selector} on {contract} returned nothing: {}", describe_rejection(&resp)))?;
        if word.len() != 64 || !word.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{selector} on {contract} returned {word:?}, not one 32-byte word"));
        }
        Ok(word.to_ascii_lowercase())
    }

    /// Once at boot: are these GasFree constants the ones deployed on the network this TronGrid
    /// serves? A Nile setting on a mainnet signer fails here instead of handing out addresses that
    /// nobody controls.
    pub async fn gasfree_self_test(&self, signer: &Signer) -> SelfTest {
        let Some(gf) = &self.gasfree else { return SelfTest::Passed };
        let chain = gf.cfg.chain;
        match self.has_contract(chain.controller).await {
            Ok(true) => {}
            Ok(false) => {
                return SelfTest::Failed(format!(
                    "the GasFree controller {} is not a contract on this TronGrid: APP_GASFREE_NETWORK does \
                     not match APP_TRONGRID_URL",
                    chain.controller
                ))
            }
            Err(e) => return SelfTest::Unreachable(e),
        }
        let owner = match signer.address_at(0) {
            Ok(a) => a,
            Err(e) => return SelfTest::Failed(e),
        };
        let ours = match gasfree::gasfree_address(chain, &owner).and_then(|g| abi_address(&g)) {
            Ok(word) => word[24..].to_string(),
            Err(e) => return SelfTest::Failed(e),
        };
        let parameter = match abi_address(&owner) {
            Ok(p) => p,
            Err(e) => return SelfTest::Failed(e),
        };
        match self.view_word(chain.controller, "getGasFreeAddress(address)", Some(&parameter)).await {
            Ok(word) if word[24..] == ours => SelfTest::Passed,
            Ok(word) => SelfTest::Failed(format!(
                "the controller puts the GasFree account of {owner} at 0x{}, this signer derives 0x{ours}",
                &word[24..]
            )),
            Err(e) => SelfTest::Unreachable(e),
        }
    }

    /// The next nonce the controller will accept from `user`: the chain's count, not the relay's.
    pub(super) async fn chain_nonce(&self, chain: &gasfree::Chain, user: &str) -> Result<u64, String> {
        todo!("Task 3 Step 5")
    }

    /// Why permits must stop, when GasFree's code is not the reviewed code; `None` when it is.
    pub(super) async fn code_changed(&self, cfg: &GasFreeConfig) -> Result<Option<String>, String> {
        todo!("Task 3 Step 5")
    }

    /// Sweep the GasFree account `g` of the wallet `owner` at `index`, which holds `balance`.
    pub(super) async fn sweep_gasfree(
        &self,
        gf: &GasFree,
        signer: &Signer,
        index: u32,
        owner: &str,
        g: &str,
        balance: i64,
    ) -> Result<SweepOutcome, String> {
        todo!("Task 3 Step 5")
    }

    /// Where a GasFree sweep sends its value.
    async fn sweep_receiver(&self, gf: &GasFree, signer: &Signer) -> Result<String, String> {
        todo!("Task 3 Step 5")
    }
}
