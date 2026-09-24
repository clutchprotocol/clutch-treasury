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

use super::{abi_address, describe_rejection, SweepClient};
use crate::keys::Signer;
use crate::relay::RelayConfig;

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

/// The GasFree half of a `SweepClient`.
pub(super) struct GasFree {
    pub(super) cfg: GasFreeConfig,
}

impl GasFree {
    pub(super) fn new(cfg: GasFreeConfig) -> Self {
        Self { cfg }
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
    todo!("Task 2 Step 5")
}

impl SweepClient {
    /// Whether `address` holds a deployed contract; for a GasFree account, whether it is activated.
    pub(super) async fn has_contract(&self, address: &str) -> Result<bool, String> {
        todo!("Task 2 Step 5")
    }

    /// The first 32-byte word a view function returns, as 64 lowercase hex characters.
    async fn view_word(&self, contract: &str, selector: &str, parameter: Option<&str>) -> Result<String, String> {
        todo!("Task 2 Step 5")
    }

    /// Once at boot: are these GasFree constants the ones deployed on the network this TronGrid
    /// serves? A Nile setting on a mainnet signer fails here instead of handing out addresses that
    /// nobody controls.
    pub async fn gasfree_self_test(&self, signer: &Signer) -> SelfTest {
        todo!("Task 2 Step 5")
    }
}
