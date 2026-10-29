//! Sweeping a BNB Smart Chain deposit address (docs/superpowers/specs/2026-10-29-bsc-deposit-addresses-design.md).
//!
//! The same contract as the TRON sweep: the caller names an INDEX and nothing else. The destination
//! (the pay-in address, whose key is not on this host), the token and the chain all come from this
//! service's own settings, so an owned treasury can at worst sweep a deposit a little early, to where
//! it was going anyway. Do not add a `to`, an `amount` or a `contract`.
//!
//! A fresh deposit address holds no BNB, and a USDT transfer from it costs gas. So, like the old TRX
//! rail, the gas account (`1/0` of the BSC mnemonic) first sends it exactly what the transfer will cost
//! and nothing more, then the address sends its whole USDT balance. BSC mines in about a second, so both
//! happen in one call, each waited for; a transaction not mined in time is reported and the next call
//! finds out what happened from the chain itself.

use std::time::Duration;

use async_trait::async_trait;

use crate::bsc_keys::BscKeys;
use crate::evm_tx::LegacyTx;

/// Refuse to sweep above this gas price: BSC runs at about 0.1 gwei, and a node answering far more is
/// either broken or lying, and would spend the gas account in a few sweeps.
pub const MAX_GAS_PRICE_WEI: u128 = 10_000_000_000;
/// A BEP-20 transfer uses 35,000 to 55,000 gas. A token that wants more is not the USDT we know.
pub const MAX_GAS_LIMIT: u64 = 150_000;
/// A plain BNB transfer to an address with no code.
const FUND_GAS: u64 = 21_000;

/// What the signer needs from a BSC node, so tests can script one.
#[async_trait]
pub trait BscNode: Send + Sync {
    async fn chain_id(&self) -> Result<u64, String>;
    async fn token_balance(&self, contract: &str, owner: &str) -> Result<u128, String>;
    async fn balance(&self, address: &str) -> Result<u128, String>;
    async fn gas_price(&self) -> Result<u128, String>;
    async fn nonce(&self, address: &str, pending: bool) -> Result<u64, String>;
    async fn estimate_gas(&self, from: &str, to: &str, data: &str) -> Result<u64, String>;
    async fn send_raw(&self, raw: &str) -> Result<String, String>;
    /// `None` while unmined; `Some(true)` when it succeeded.
    async fn receipt_succeeded(&self, tx_hash: &str) -> Result<Option<bool>, String>;
}

#[async_trait]
impl BscNode for bsc::Client {
    async fn chain_id(&self) -> Result<u64, String> {
        bsc::Client::chain_id(self).await
    }
    async fn token_balance(&self, contract: &str, owner: &str) -> Result<u128, String> {
        self.balance_of(contract, owner).await
    }
    async fn balance(&self, address: &str) -> Result<u128, String> {
        bsc::Client::balance(self, address).await
    }
    async fn gas_price(&self) -> Result<u128, String> {
        bsc::Client::gas_price(self).await
    }
    async fn nonce(&self, address: &str, pending: bool) -> Result<u64, String> {
        bsc::Client::nonce(self, address, pending).await
    }
    async fn estimate_gas(&self, from: &str, to: &str, data: &str) -> Result<u64, String> {
        bsc::Client::estimate_gas(self, from, to, data).await
    }
    async fn send_raw(&self, raw: &str) -> Result<String, String> {
        self.send_raw_transaction(raw).await
    }
    async fn receipt_succeeded(&self, tx_hash: &str) -> Result<Option<bool>, String> {
        Ok(self.receipt(tx_hash).await?.map(|r| r.succeeded))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BscSweepOutcome {
    /// The whole USDT balance reached the pay-in address, mined and successful.
    Swept { tx_hash: String, amount_micro: i64 },
    NothingToSweep,
    /// The address was sent its gas, which is not mined yet; the sweep follows on a later call.
    Funded { tx_hash: String, amount_wei: u128 },
    /// An operator has to send BNB to `gas_address`. Nothing was sent.
    GasAccountDry { gas_address: String, have_wei: u128, need_wei: u128 },
    /// A transaction from the address or the gas account is still pending. Nothing was sent.
    Busy,
    /// The sweep was sent and is not mined yet. The next call reads the balance again.
    NotMined { tx_hash: String },
    /// Nothing was sent, for the reason given.
    Refused(String),
    /// A transaction was mined and failed. Its gas is spent; the USDT is where it was.
    Reverted { tx_hash: String },
}

pub struct BscSweeper {
    pub keys: BscKeys,
    pub settings: bsc::Settings,
    pub node: Box<dyn BscNode>,
    /// How long to wait for each receipt, and how often to ask.
    pub wait: Duration,
    pub poll: Duration,
}

impl BscSweeper {
    pub fn new(keys: BscKeys, settings: bsc::Settings) -> Self {
        let node = Box::new(bsc::Client::new(&settings.rpc_url));
        Self { keys, settings, node, wait: Duration::from_secs(30), poll: Duration::from_secs(1) }
    }

    async fn mined(&self, tx_hash: &str) -> Result<Option<bool>, String> {
        let deadline = tokio::time::Instant::now() + self.wait;
        loop {
            if let Some(ok) = self.node.receipt_succeeded(tx_hash).await? {
                return Ok(Some(ok));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    /// `Err` only after something may have been broadcast without a clear answer, or on a read that
    /// failed; every provable "nothing was sent" is an outcome.
    pub async fn sweep(&self, index: u32) -> Result<BscSweepOutcome, String> {
        let s = &self.settings;
        let chain = self.node.chain_id().await?;
        if chain != s.chain_id {
            return Ok(BscSweepOutcome::Refused(format!("the BSC node is on chain {chain}, not {}", s.chain_id)));
        }
        let address = self.keys.address_at(index)?;
        let usdt = self.node.token_balance(&s.usdt_contract, &address).await?;
        if usdt == 0 {
            return Ok(BscSweepOutcome::NothingToSweep);
        }
        let Some(amount_micro) = bsc::to_micro(usdt, s.usdt_decimals) else {
            return Ok(BscSweepOutcome::Refused(format!("{usdt} base units at {address} do not fit a CLT amount")));
        };
        if self.node.nonce(&address, true).await? > self.node.nonce(&address, false).await? {
            return Ok(BscSweepOutcome::Busy);
        }

        let gas_price = self.node.gas_price().await?;
        if gas_price == 0 || gas_price > MAX_GAS_PRICE_WEI {
            return Ok(BscSweepOutcome::Refused(format!("gas price {gas_price} wei is outside 1..={MAX_GAS_PRICE_WEI}")));
        }
        let data = bsc::transfer_calldata(&s.pay_in_address, usdt).ok_or("the pay-in address is not a BSC address")?;
        let estimate = self.node.estimate_gas(&address, &s.usdt_contract, &data).await?;
        let gas_limit = estimate.saturating_mul(5) / 4;
        if gas_limit > MAX_GAS_LIMIT {
            return Ok(BscSweepOutcome::Refused(format!("the transfer would use {estimate} gas, above {MAX_GAS_LIMIT}")));
        }
        let need = gas_price * gas_limit as u128;
        let have = self.node.balance(&address).await?;

        if have < need {
            let gas_address = self.keys.gas_address()?;
            let gas_nonce = self.node.nonce(&gas_address, false).await?;
            if self.node.nonce(&gas_address, true).await? > gas_nonce {
                return Ok(BscSweepOutcome::Busy);
            }
            let top_up = need - have;
            let gas_need = top_up + gas_price * FUND_GAS as u128;
            let gas_have = self.node.balance(&gas_address).await?;
            if gas_have < gas_need {
                return Ok(BscSweepOutcome::GasAccountDry { gas_address, have_wei: gas_have, need_wei: gas_need });
            }
            let fund = LegacyTx { nonce: gas_nonce, gas_price, gas_limit: FUND_GAS, to: address.clone(), value: top_up, data: vec![] };
            let (raw, hash) = fund.sign(s.chain_id, &self.keys.gas_signing_key()?)?;
            let sent = self.node.send_raw(&raw).await.map_err(|e| format!("funding {address} ({hash}): {e}"))?;
            match self.mined(&sent).await? {
                Some(true) => {}
                Some(false) => return Ok(BscSweepOutcome::Reverted { tx_hash: sent }),
                None => return Ok(BscSweepOutcome::Funded { tx_hash: sent, amount_wei: top_up }),
            }
        }

        let sweep = LegacyTx {
            nonce: self.node.nonce(&address, false).await?,
            gas_price,
            gas_limit,
            to: s.usdt_contract.clone(),
            value: 0,
            data: hex::decode(&data[2..]).map_err(|e| e.to_string())?,
        };
        let (raw, hash) = sweep.sign(s.chain_id, &self.keys.signing_key_at(index)?)?;
        let sent = self.node.send_raw(&raw).await.map_err(|e| format!("sweeping {address} ({hash}): {e}"))?;
        Ok(match self.mined(&sent).await? {
            Some(true) => BscSweepOutcome::Swept { tx_hash: sent, amount_micro },
            Some(false) => BscSweepOutcome::Reverted { tx_hash: sent },
            None => BscSweepOutcome::NotMined { tx_hash: sent },
        })
    }
}

/// The JSON `/internal/bsc/sweep` answers. Wei amounts are decimal strings: they exceed 2^53.
pub fn bsc_sweep_response(outcome: &BscSweepOutcome) -> serde_json::Value {
    use serde_json::json;
    match outcome {
        BscSweepOutcome::Swept { tx_hash, amount_micro } => json!({"status": "swept", "tx_id": tx_hash, "amount_usdt": amount_micro}),
        BscSweepOutcome::NothingToSweep => json!({"status": "nothing_to_sweep"}),
        BscSweepOutcome::Funded { tx_hash, amount_wei } => json!({"status": "funded", "tx_id": tx_hash, "amount_wei": amount_wei.to_string()}),
        BscSweepOutcome::GasAccountDry { gas_address, have_wei, need_wei } => json!({
            "status": "gas_account_dry", "gas_address": gas_address, "have_wei": have_wei.to_string(), "need_wei": need_wei.to_string()
        }),
        BscSweepOutcome::Busy => json!({"status": "busy"}),
        BscSweepOutcome::NotMined { tx_hash } => json!({"status": "not_mined", "tx_id": tx_hash}),
        BscSweepOutcome::Refused(reason) => json!({"status": "refused", "reason": reason}),
        BscSweepOutcome::Reverted { tx_hash } => json!({"status": "reverted", "tx_id": tx_hash}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const USDT: &str = "0x337610d27c682e347c9cd60bd4b3b107c9d34ddd";
    const PAY_IN: &str = "0xb45e67756b2a82f83b0846ba3a7e4459f28cf6a6";
    const GWEI: u128 = 1_000_000_000;

    /// A scripted chain. Sending a transaction mines it at once (or never, with `mine: false`) and
    /// moves the balances it would.
    #[derive(Default)]
    struct Fake {
        usdt: Mutex<HashMap<String, u128>>,
        bnb: Mutex<HashMap<String, u128>>,
        pending: Mutex<HashMap<String, bool>>,
        sent: Mutex<Vec<String>>,
        mine: bool,
        gas_price: u128,
    }

    #[async_trait]
    impl BscNode for Fake {
        async fn chain_id(&self) -> Result<u64, String> {
            Ok(97)
        }
        async fn token_balance(&self, _: &str, owner: &str) -> Result<u128, String> {
            Ok(*self.usdt.lock().unwrap().get(owner).unwrap_or(&0))
        }
        async fn balance(&self, address: &str) -> Result<u128, String> {
            Ok(*self.bnb.lock().unwrap().get(address).unwrap_or(&0))
        }
        async fn gas_price(&self) -> Result<u128, String> {
            Ok(self.gas_price)
        }
        async fn nonce(&self, address: &str, pending: bool) -> Result<u64, String> {
            Ok(if pending && *self.pending.lock().unwrap().get(address).unwrap_or(&false) { 1 } else { 0 })
        }
        async fn estimate_gas(&self, _: &str, to: &str, data: &str) -> Result<u64, String> {
            assert_eq!(to, USDT, "the sweep calls the configured token");
            assert!(data.starts_with(&format!("0xa9059cbb{:0>64}", &PAY_IN[2..])), "and pays the configured pay-in address");
            Ok(40_000)
        }
        async fn send_raw(&self, raw: &str) -> Result<String, String> {
            self.sent.lock().unwrap().push(raw.to_string());
            Ok(format!("0x{:064x}", self.sent.lock().unwrap().len()))
        }
        async fn receipt_succeeded(&self, _: &str) -> Result<Option<bool>, String> {
            Ok(self.mine.then_some(true))
        }
    }

    fn sweeper(fake: Fake) -> (BscSweeper, String, String) {
        let keys = BscKeys::from_mnemonic(MNEMONIC, "").unwrap();
        let (address, gas) = (keys.address_at(3).unwrap(), keys.gas_address().unwrap());
        let settings = bsc::Settings {
            chain_id: 97,
            rpc_url: "http://unused".into(),
            usdt_contract: USDT.into(),
            usdt_decimals: 18,
            pay_in_address: PAY_IN.into(),
            confirmations: 15,
        };
        let s = BscSweeper { keys, settings, node: Box::new(fake), wait: Duration::ZERO, poll: Duration::ZERO };
        (s, address, gas)
    }

    fn fake(mine: bool) -> Fake {
        Fake { mine, gas_price: GWEI / 10, ..Default::default() }
    }

    #[tokio::test]
    async fn an_empty_address_sends_nothing() {
        let (s, _, _) = sweeper(fake(true));
        assert_eq!(s.sweep(3).await.unwrap(), BscSweepOutcome::NothingToSweep);
    }

    #[tokio::test]
    async fn funds_exactly_the_gas_then_sweeps_the_whole_balance() {
        let f = fake(true);
        let keys = BscKeys::from_mnemonic(MNEMONIC, "").unwrap();
        f.usdt.lock().unwrap().insert(keys.address_at(3).unwrap(), 25 * 10u128.pow(18) + 7);
        f.bnb.lock().unwrap().insert(keys.gas_address().unwrap(), 10u128.pow(16));
        let (s, _, _) = sweeper(f);
        match s.sweep(3).await.unwrap() {
            BscSweepOutcome::Swept { amount_micro, .. } => assert_eq!(amount_micro, 25_000_000),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_gas_account_says_where_to_send_bnb() {
        let f = fake(true);
        let keys = BscKeys::from_mnemonic(MNEMONIC, "").unwrap();
        f.usdt.lock().unwrap().insert(keys.address_at(3).unwrap(), 10u128.pow(18));
        let (s, _, gas) = sweeper(f);
        match s.sweep(3).await.unwrap() {
            BscSweepOutcome::GasAccountDry { gas_address, have_wei, need_wei } => {
                assert_eq!(gas_address, gas);
                assert_eq!(have_wei, 0);
                // 40,000 x 1.25 for the sweep, plus 21,000 for the top-up, at 0.1 gwei.
                assert_eq!(need_wei, (50_000 + 21_000) * GWEI / 10);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_pending_transaction_or_a_wild_gas_price_sends_nothing() {
        let f = fake(true);
        let keys = BscKeys::from_mnemonic(MNEMONIC, "").unwrap();
        let address = keys.address_at(3).unwrap();
        f.usdt.lock().unwrap().insert(address.clone(), 10u128.pow(18));
        f.pending.lock().unwrap().insert(address, true);
        let (s, _, _) = sweeper(f);
        assert_eq!(s.sweep(3).await.unwrap(), BscSweepOutcome::Busy);

        let mut f = fake(true);
        f.gas_price = 50 * GWEI;
        f.usdt.lock().unwrap().insert(keys.address_at(3).unwrap(), 10u128.pow(18));
        let (s, _, _) = sweeper(f);
        assert!(matches!(s.sweep(3).await.unwrap(), BscSweepOutcome::Refused(_)));
    }

    #[tokio::test]
    async fn an_unmined_top_up_is_reported_and_the_sweep_waits() {
        let f = fake(false);
        let keys = BscKeys::from_mnemonic(MNEMONIC, "").unwrap();
        f.usdt.lock().unwrap().insert(keys.address_at(3).unwrap(), 10u128.pow(18));
        f.bnb.lock().unwrap().insert(keys.gas_address().unwrap(), 10u128.pow(16));
        let (s, _, _) = sweeper(f);
        match s.sweep(3).await.unwrap() {
            BscSweepOutcome::Funded { amount_wei, .. } => assert_eq!(amount_wei, 50_000 * GWEI / 10),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn an_address_that_already_holds_its_gas_is_not_topped_up() {
        let f = fake(false);
        let keys = BscKeys::from_mnemonic(MNEMONIC, "").unwrap();
        let address = keys.address_at(3).unwrap();
        f.usdt.lock().unwrap().insert(address.clone(), 10u128.pow(18));
        f.bnb.lock().unwrap().insert(address, 10u128.pow(15));
        let (s, _, _) = sweeper(f);
        assert!(matches!(s.sweep(3).await.unwrap(), BscSweepOutcome::NotMined { .. }), "straight to the sweep");
    }
}
