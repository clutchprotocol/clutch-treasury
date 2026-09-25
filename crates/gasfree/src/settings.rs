//! The GasFree settings the treasury and the orchestrator read: the same variables, from the same
//! env file, as the signer.
//!
//! Parsing only, with no network, like the rest of this crate. The signer reads these variables
//! too, plus its relay credentials, in its own loader (`tron-signer`'s `load_gasfree_config`).

use crate::{Chain, MAINNET, NILE};

/// GasFree as the treasury and the orchestrator see it.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Which GasFree deployment: `NILE` or `MAINNET`.
    pub chain: &'static Chain,
    /// `APP_TRANSFER_RAIL=gasfree`: new users are given GasFree addresses, and redemptions are paid
    /// from the GasFree float. False is the TRX rail. The settings still apply then, to every user
    /// who already has a GasFree address, because deposit addresses are permanent (spec §5).
    pub rail: bool,
    /// The most a first transfer may pay for activation, in micro-USDT. Above the live fee.
    pub activate_fee_max_usdt: i64,
    /// The most any transfer may pay the relay, in micro-USDT. Above the live fee.
    pub transfer_fee_max_usdt: i64,
    /// Below this after the fee, a deposit mints nothing and waits for a human (spec §2), in micro-USDT.
    pub min_deposit_usdt: i64,
    /// The beacon's `implementation()` when it was reviewed: 40 lowercase hex, no `0x`.
    pub expected_beacon_implementation: String,
    /// The controller's `implementation()` when it was reviewed: 40 lowercase hex, no `0x`.
    pub expected_controller_implementation: String,
}

/// Read the settings. `Ok(None)` means GasFree is off, which is the default.
///
/// GasFree is on when `APP_GASFREE_NETWORK` is set, and then every other setting is required: a
/// missing one stops the service at boot, not at the first deposit. A blank value counts as unset,
/// because the deploy repo passes an unset optional value as an empty string (`${X:-}`).
pub fn load_settings(var: impl Fn(&str) -> Option<String>) -> Result<Option<Settings>, String> {
    todo!("Task 1 Step 5")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn nile_vars() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("APP_TRANSFER_RAIL", "gasfree"),
            ("APP_GASFREE_NETWORK", "nile"),
            ("APP_GASFREE_ACTIVATE_FEE_MAX_USDT", "1500000"),
            ("APP_GASFREE_TRANSFER_FEE_MAX_USDT", "500000"),
            ("APP_MIN_DEPOSIT_USDT", "1000000"),
            ("APP_GASFREE_EXPECTED_IMPLEMENTATION", "0xB8EDA40B467B45AF107F198E94CC2FA1378ADF50"),
            ("APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION", "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441"),
        ])
    }

    fn load(vars: &HashMap<&'static str, &'static str>) -> Result<Option<Settings>, String> {
        load_settings(|name| vars.get(name).map(|v| v.to_string()))
    }

    #[test]
    fn nothing_set_is_gasfree_off() {
        assert!(load(&HashMap::new()).unwrap().is_none());
    }

    #[test]
    fn the_full_nile_set_is_read_field_by_field() {
        let s = load(&nile_vars()).unwrap().expect("GasFree is on");
        assert_eq!(s.chain.chain_id, NILE.chain_id);
        assert!(s.rail);
        assert_eq!(
            (s.activate_fee_max_usdt, s.transfer_fee_max_usdt, s.min_deposit_usdt),
            (1_500_000, 500_000, 1_000_000)
        );
        assert_eq!(
            s.expected_beacon_implementation, "b8eda40b467b45af107f198e94cc2fa1378adf50",
            "0x and upper case are accepted and normalised"
        );
        assert_eq!(s.expected_controller_implementation, "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441");
    }

    /// Switching back to trx keeps the settings: users who were given a GasFree address keep it.
    #[test]
    fn the_trx_rail_with_gasfree_settings_keeps_them() {
        let mut vars = nile_vars();
        vars.insert("APP_TRANSFER_RAIL", "trx");
        let s = load(&vars).unwrap().expect("the settings stay on");
        assert!(!s.rail);
    }

    #[test]
    fn the_gasfree_rail_without_a_network_is_refused() {
        let vars = HashMap::from([("APP_TRANSFER_RAIL", "gasfree")]);
        assert!(load(&vars).unwrap_err().contains("APP_GASFREE_NETWORK"));
    }

    #[test]
    fn a_missing_zero_or_malformed_setting_is_refused_by_name() {
        for (name, bad) in [
            ("APP_GASFREE_TRANSFER_FEE_MAX_USDT", "0"),
            ("APP_MIN_DEPOSIT_USDT", "1.5"),
            ("APP_GASFREE_ACTIVATE_FEE_MAX_USDT", ""),
            ("APP_GASFREE_EXPECTED_IMPLEMENTATION", "0xb8eda40b"),
            ("APP_GASFREE_NETWORK", "shasta"),
            ("APP_TRANSFER_RAIL", "TRX"),
        ] {
            let mut vars = nile_vars();
            vars.insert(name, bad);
            let err = load(&vars).expect_err(name);
            assert!(err.contains(name), "{name}={bad:?} gave {err:?}");
        }
    }
}
