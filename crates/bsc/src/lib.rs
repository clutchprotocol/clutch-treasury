//! BEP-20 USDT on BNB Smart Chain, phase 1: deposits from the user's own wallet
//! (docs/superpowers/specs/2026-10-23-bsc-usdt-design.md).
//!
//! A user's BSC address is their CLT address: the same key, the same 20 bytes, the same `0x`
//! spelling. So a USDT Transfer FROM `0xX` can only have been signed by whoever holds CLT address
//! `0xX`'s key, and minting its value to `0xX` pays the person who paid. Both services use this
//! crate, so they cannot disagree about the settings, the units or what counts as a transfer.

mod rpc;
mod settings;

pub use rpc::{Client, Receipt};
pub use settings::{load_settings, Settings, MAINNET_CHAIN_ID, MAINNET_USDT, TESTNET_CHAIN_ID};

/// `keccak256("Transfer(address,address,uint256)")`, the first topic of every ERC-20 Transfer log.
pub const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// One ERC-20 Transfer log, decoded. Addresses are `0x` + 40 lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    /// `0x` + 64 lowercase hex.
    pub tx_hash: String,
    pub log_index: u64,
    pub block_number: u64,
    /// The token contract that emitted the log.
    pub contract: String,
    pub from: String,
    pub to: String,
    /// In the token's base units (18 decimals for BSC USDT).
    pub value: u128,
}

/// `0x` + 40 hex in any case, out as `0x` + 40 lowercase hex. `None` for anything else.
pub fn normalize_address(address: &str) -> Option<String> {
    let hex = address.strip_prefix("0x").or_else(|| address.strip_prefix("0X"))?;
    (hex.len() == 40 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| format!("0x{}", hex.to_ascii_lowercase()))
}

/// `0x` + 64 hex in any case, out lowercase. `None` for anything else, a TRON id included.
pub fn normalize_tx_hash(hash: &str) -> Option<String> {
    let hex = hash.strip_prefix("0x").or_else(|| hash.strip_prefix("0X"))?;
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| format!("0x{}", hex.to_ascii_lowercase()))
}

/// Base units of a token with `decimals` decimals, in micro-USDT (which is CLT at par), rounded DOWN.
///
/// Down, because the remainder (below a millionth of a dollar) then stays in the reserve as surplus:
/// flooring can only under-credit. `None` when the result does not fit an `i64`, or the token has
/// fewer than 6 decimals, which no USDT has and which would need a different rule.
pub fn to_micro(value: u128, decimals: u8) -> Option<i64> {
    let shift = decimals.checked_sub(6)?;
    let micro = value / 10u128.checked_pow(shift as u32)?;
    i64::try_from(micro).ok()
}

/// What a set of logs moved of `contract` from `from` to `to`: the sum of the matching Transfers.
/// `None` on overflow, which no real balance reaches.
pub fn sum_transfers(transfers: &[Transfer], contract: &str, from: &str, to: &str) -> Option<u128> {
    transfers
        .iter()
        .filter(|t| t.contract == contract && t.from == from && t.to == to)
        .try_fold(0u128, |sum, t| sum.checked_add(t.value))
}

/// The calldata of an ERC-20 `transfer(to, value)`: its selector `a9059cbb`, then the two words.
pub fn transfer_calldata(to: &str, value: u128) -> Option<String> {
    let word = address_word(to)?;
    Some(format!("0xa9059cbb{}{value:064x}", &word[2..]))
}

/// An address as a 32-byte log topic or ABI word: 12 zero bytes, then the 20.
pub fn address_word(address: &str) -> Option<String> {
    let a = normalize_address(address)?;
    Some(format!("0x{:0>64}", &a[2..]))
}

/// The address in a 32-byte topic, `None` unless the first 12 bytes are zero.
fn word_address(word: &str) -> Option<String> {
    let hex = word.strip_prefix("0x")?;
    if hex.len() != 64 || !hex[..24].bytes().all(|b| b == b'0') {
        return None;
    }
    normalize_address(&format!("0x{}", &hex[24..]))
}

/// A hex quantity or 32-byte word as a `u128`; `None` above that, or for anything not hex.
pub(crate) fn hex_u128(word: &str) -> Option<u128> {
    let hex = word.strip_prefix("0x")?;
    let trimmed = hex.trim_start_matches('0');
    if trimmed.is_empty() {
        return hex.chars().all(|c| c == '0').then_some(0);
    }
    if trimmed.len() > 32 {
        return None;
    }
    u128::from_str_radix(trimmed, 16).ok()
}

/// A log as `eth_getLogs` and `eth_getTransactionReceipt` both return it, decoded if it is an
/// ERC-20 Transfer. Anything else, a removed (reorged) log included, is `None`.
pub(crate) fn decode_transfer(log: &serde_json::Value) -> Option<Transfer> {
    if log.get("removed").and_then(|r| r.as_bool()) == Some(true) {
        return None;
    }
    let topics: Vec<&str> = log.get("topics")?.as_array()?.iter().filter_map(|t| t.as_str()).collect();
    // Exactly three topics: an ERC-721 Transfer has the same first topic and a fourth (the token id),
    // and its `data` is empty, so it must never read as an amount.
    if topics.len() != 3 || !topics[0].eq_ignore_ascii_case(TRANSFER_TOPIC) {
        return None;
    }
    let data = log.get("data")?.as_str()?;
    if data.len() != 66 {
        return None;
    }
    Some(Transfer {
        tx_hash: normalize_tx_hash(log.get("transactionHash")?.as_str()?)?,
        log_index: hex_u128(log.get("logIndex")?.as_str()?)? as u64,
        block_number: hex_u128(log.get("blockNumber")?.as_str()?)? as u64,
        contract: normalize_address(log.get("address")?.as_str()?)?,
        from: word_address(&topics[1].to_ascii_lowercase())?,
        to: word_address(&topics[2].to_ascii_lowercase())?,
        value: hex_u128(data)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const USDT: &str = "0x55d398326f99059ff775485246999027b3197955";
    const ALICE: &str = "0x1111111111111111111111111111111111111111";
    const PAY_IN: &str = "0x2222222222222222222222222222222222222222";

    fn log(from: &str, to: &str, value: u128) -> serde_json::Value {
        json!({
            "address": USDT,
            "topics": [TRANSFER_TOPIC, address_word(from).unwrap(), address_word(to).unwrap()],
            "data": format!("0x{value:064x}"),
            "transactionHash": format!("0x{}", "ab".repeat(32)),
            "logIndex": "0x3",
            "blockNumber": "0x10",
            "removed": false,
        })
    }

    #[test]
    fn eighteen_decimals_floor_to_micro_usdt() {
        assert_eq!(to_micro(10 * 10u128.pow(18), 18), Some(10_000_000));
        // 1.0000009 USDT: the 0.0000009 below a micro is left in the reserve.
        assert_eq!(to_micro(1_000_000_900_000_000_000, 18), Some(1_000_000));
        assert_eq!(to_micro(999_999_999_999, 18), Some(0));
        assert_eq!(to_micro(5_000_000, 6), Some(5_000_000));
        assert_eq!(to_micro(1, 5), None, "fewer than 6 decimals needs another rule");
        assert_eq!(to_micro(u128::MAX, 6), None, "does not fit i64");
    }

    #[test]
    fn a_transfer_log_decodes() {
        let t = decode_transfer(&log(ALICE, PAY_IN, 25 * 10u128.pow(18))).unwrap();
        assert_eq!((t.from.as_str(), t.to.as_str(), t.contract.as_str()), (ALICE, PAY_IN, USDT));
        assert_eq!((t.value, t.log_index, t.block_number), (25 * 10u128.pow(18), 3, 16));
    }

    #[test]
    fn removed_and_nft_and_other_logs_do_not_decode() {
        let mut removed = log(ALICE, PAY_IN, 1);
        removed["removed"] = json!(true);
        assert_eq!(decode_transfer(&removed), None);

        let mut nft = log(ALICE, PAY_IN, 1);
        nft["topics"].as_array_mut().unwrap().push(json!(format!("0x{:064x}", 7)));
        nft["data"] = json!("0x");
        assert_eq!(decode_transfer(&nft), None);

        let mut approval = log(ALICE, PAY_IN, 1);
        approval["topics"][0] = json!("0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925");
        assert_eq!(decode_transfer(&approval), None);

        let mut dirty = log(ALICE, PAY_IN, 1);
        dirty["topics"][1] = json!(format!("0x{}{}", "ff".repeat(12), &ALICE[2..]));
        assert_eq!(decode_transfer(&dirty), None, "a topic with high bytes set is not an address");
    }

    #[test]
    fn sums_only_the_named_route() {
        let ts: Vec<Transfer> = [log(ALICE, PAY_IN, 5), log(ALICE, PAY_IN, 7), log(PAY_IN, ALICE, 100), log(ALICE, ALICE, 1000)]
            .iter()
            .map(|l| decode_transfer(l).unwrap())
            .collect();
        assert_eq!(sum_transfers(&ts, USDT, ALICE, PAY_IN), Some(12));
        assert_eq!(sum_transfers(&ts, "0x0000000000000000000000000000000000000001", ALICE, PAY_IN), Some(0));
    }

    #[test]
    fn transfer_calldata_is_the_erc20_call() {
        // The same bytes ethers' `interface.encodeFunctionData("transfer", [PAY_IN, 25e18])` writes.
        assert_eq!(
            transfer_calldata(PAY_IN, 25 * 10u128.pow(18)).unwrap(),
            "0xa9059cbb00000000000000000000000022222222222222222222222222222222222222220000000000000000000000000000000000000000000000015af1d78b58c40000"
        );
        assert_eq!(transfer_calldata("not an address", 1), None);
    }

    #[test]
    fn addresses_and_hashes_normalize() {
        assert_eq!(normalize_address("0xABCDEF0123456789abcdef0123456789ABCDEF01").unwrap(), "0xabcdef0123456789abcdef0123456789abcdef01");
        assert_eq!(normalize_address("abcdef0123456789abcdef0123456789abcdef01"), None, "the 0x is required");
        assert_eq!(normalize_address("TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH"), None);
        assert!(normalize_tx_hash(&format!("0x{}", "AB".repeat(32))).is_some());
        assert_eq!(normalize_tx_hash(&"ab".repeat(32)), None, "a TRON id is not a BSC hash");
    }
}
