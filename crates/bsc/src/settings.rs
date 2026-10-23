//! The BSC settings, read from the environment by both services with one function, so they cannot
//! name different contracts or pay-in addresses.

use crate::normalize_address;

pub const MAINNET_CHAIN_ID: u64 = 56;
pub const TESTNET_CHAIN_ID: u64 = 97;
/// Binance-Peg BSC-USD, the USDT on BSC mainnet (18 decimals).
pub const MAINNET_USDT: &str = "0x55d398326f99059ff775485246999027b3197955";

const DEFAULT_CONFIRMATIONS: u64 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// 56 (mainnet) or 97 (testnet).
    pub chain_id: u64,
    pub rpc_url: String,
    /// `0x` + 40 lowercase hex.
    pub usdt_contract: String,
    pub usdt_decimals: u8,
    /// Where users pay. Receiving needs only the address: its key is not on the host in phase 1.
    pub pay_in_address: String,
    /// How deep a transfer must be before it is credited.
    pub confirmations: u64,
}

/// `Ok(None)` while `APP_BSC_TOPUPS_ENABLED` is unset or false. When it is true every other setting
/// is required, and a wrong one stops the service at boot rather than at the first deposit.
pub fn load_settings(var: impl Fn(&str) -> Option<String>) -> Result<Option<Settings>, String> {
    let optional = |name: &str| var(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    match optional("APP_BSC_TOPUPS_ENABLED").as_deref() {
        None | Some("false") => return Ok(None),
        Some("true") => {}
        Some(other) => return Err(format!("APP_BSC_TOPUPS_ENABLED must be true or false, got {other:?}")),
    }
    let required = |name: &str| optional(name).ok_or_else(|| format!("{name} must be set when APP_BSC_TOPUPS_ENABLED is true"));
    let address = |name: &str| {
        required(name).and_then(|raw| normalize_address(&raw).ok_or_else(|| format!("{name} must be 0x and 40 hex characters, got {raw:?}")))
    };

    let raw_chain = required("APP_BSC_CHAIN_ID")?;
    let chain_id = match raw_chain.parse::<u64>() {
        Ok(id @ (MAINNET_CHAIN_ID | TESTNET_CHAIN_ID)) => id,
        _ => return Err(format!("APP_BSC_CHAIN_ID must be 56 (mainnet) or 97 (testnet), got {raw_chain:?}")),
    };
    let rpc_url = required("APP_BSC_RPC_URL")?;
    if !rpc_url.starts_with("https://") && !rpc_url.starts_with("http://") {
        return Err(format!("APP_BSC_RPC_URL must be an http(s) URL, got {rpc_url:?}"));
    }
    let usdt_contract = address("APP_BSC_USDT_CONTRACT")?;
    // On mainnet only the one real USDT counts. A look-alike token anyone can mint would otherwise
    // be a typo away from minting CLT against nothing.
    if chain_id == MAINNET_CHAIN_ID && usdt_contract != MAINNET_USDT {
        return Err(format!("APP_BSC_USDT_CONTRACT on BSC mainnet must be {MAINNET_USDT}, got {usdt_contract}"));
    }
    let raw_decimals = required("APP_BSC_USDT_DECIMALS")?;
    let usdt_decimals = match raw_decimals.parse::<u8>() {
        Ok(d @ 6..=30) => d,
        _ => return Err(format!("APP_BSC_USDT_DECIMALS must be between 6 and 30, got {raw_decimals:?}")),
    };
    if chain_id == MAINNET_CHAIN_ID && usdt_decimals != 18 {
        return Err(format!("APP_BSC_USDT_DECIMALS on BSC mainnet must be 18, got {usdt_decimals}"));
    }
    let pay_in_address = address("APP_BSC_PAY_IN_ADDRESS")?;
    if pay_in_address == format!("0x{}", "0".repeat(40)) {
        return Err("APP_BSC_PAY_IN_ADDRESS must not be the zero address".into());
    }
    let confirmations = match optional("APP_BSC_CONFIRMATIONS") {
        None => DEFAULT_CONFIRMATIONS,
        Some(raw) => match raw.parse::<u64>() {
            Ok(n) if (1..=1000).contains(&n) => n,
            _ => return Err(format!("APP_BSC_CONFIRMATIONS must be between 1 and 1000, got {raw:?}")),
        },
    };
    Ok(Some(Settings { chain_id, rpc_url, usdt_contract, usdt_decimals, pay_in_address, confirmations }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const PAY_IN: &str = "0x2222222222222222222222222222222222222222";
    const TEST_USDT: &str = "0x337610d27c682E347C9cD60BD4b3b107C9d34dDd";

    fn load(pairs: &[(&str, &str)]) -> Result<Option<Settings>, String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        load_settings(|name| map.get(name).cloned())
    }

    fn testnet() -> Vec<(&'static str, &'static str)> {
        vec![
            ("APP_BSC_TOPUPS_ENABLED", "true"),
            ("APP_BSC_CHAIN_ID", "97"),
            ("APP_BSC_RPC_URL", "https://bsc-testnet.example"),
            ("APP_BSC_USDT_CONTRACT", TEST_USDT),
            ("APP_BSC_USDT_DECIMALS", "18"),
            ("APP_BSC_PAY_IN_ADDRESS", PAY_IN),
        ]
    }

    fn with(mut pairs: Vec<(&'static str, &'static str)>, name: &'static str, value: &'static str) -> Vec<(&'static str, &'static str)> {
        pairs.retain(|(k, _)| *k != name);
        pairs.push((name, value));
        pairs
    }

    #[test]
    fn off_unless_enabled() {
        assert_eq!(load(&[]).unwrap(), None);
        assert_eq!(load(&with(testnet(), "APP_BSC_TOPUPS_ENABLED", "false")).unwrap(), None);
        assert!(load(&with(testnet(), "APP_BSC_TOPUPS_ENABLED", "yes")).is_err());
    }

    #[test]
    fn testnet_loads_lowercased_with_default_confirmations() {
        let s = load(&testnet()).unwrap().unwrap();
        assert_eq!(s.chain_id, 97);
        assert_eq!(s.usdt_contract, TEST_USDT.to_ascii_lowercase());
        assert_eq!((s.usdt_decimals, s.confirmations), (18, 15));
    }

    #[test]
    fn half_set_refuses() {
        for name in ["APP_BSC_CHAIN_ID", "APP_BSC_RPC_URL", "APP_BSC_USDT_CONTRACT", "APP_BSC_USDT_DECIMALS", "APP_BSC_PAY_IN_ADDRESS"] {
            let mut pairs = testnet();
            pairs.retain(|(k, _)| *k != name);
            assert!(load(&pairs).is_err(), "{name} missing");
        }
    }

    #[test]
    fn wrong_values_refuse() {
        assert!(load(&with(testnet(), "APP_BSC_CHAIN_ID", "1")).is_err(), "Ethereum is not BSC");
        assert!(load(&with(testnet(), "APP_BSC_PAY_IN_ADDRESS", "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH")).is_err());
        assert!(load(&with(testnet(), "APP_BSC_PAY_IN_ADDRESS", "0x0000000000000000000000000000000000000000")).is_err());
        assert!(load(&with(testnet(), "APP_BSC_USDT_DECIMALS", "5")).is_err());
        assert!(load(&with(testnet(), "APP_BSC_CONFIRMATIONS", "0")).is_err());
        assert!(load(&with(testnet(), "APP_BSC_RPC_URL", "wss://x")).is_err());
    }

    #[test]
    fn mainnet_takes_only_the_real_usdt() {
        let main = with(testnet(), "APP_BSC_CHAIN_ID", "56");
        assert!(load(&main).is_err(), "a testnet token on mainnet");
        let main = with(main, "APP_BSC_USDT_CONTRACT", "0x55d398326f99059fF775485246999027B3197955");
        assert_eq!(load(&main).unwrap().unwrap().usdt_contract, MAINNET_USDT);
        assert!(load(&with(main, "APP_BSC_USDT_DECIMALS", "6")).is_err());
    }
}
