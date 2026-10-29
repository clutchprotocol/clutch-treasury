//! The keys of the BNB Smart Chain deposit addresses (docs/superpowers/specs/2026-10-29-bsc-deposit-addresses-design.md).
//!
//! A SECOND mnemonic, `APP_BSC_DEPOSIT_MNEMONIC`, never a path on `APP_DEPOSIT_MNEMONIC`: that one
//! already holds the whole TRON reserve, and one leak must not take both chains.
//!
//! The path is MetaMask's own, `m/44'/60'/0'/0/i`, so a human can open any deposit address by
//! importing the mnemonic into MetaMask. The orchestrator holds the xpub of `m/44'/60'/0'` and derives
//! the same `0/i` addresses without any key; `address_at` here exists so that agreement is tested, not
//! assumed (the same reasoning as `keys.rs`). Index `i` is the user's TRON derivation index: one index
//! per user, two chains.
//!
//! The gas account is `1/0`, the internal chain: nothing at `0/i` can ever be it, so the BNB that pays
//! for sweeps is never handed to a depositor.

use bip32::{ChildNumber, DerivationPath, XPrv};
use bip39::Mnemonic;
use k256::ecdsa::SigningKey;
use sha3::{Digest, Keccak256};

const ACCOUNT_PATH: &str = "m/44'/60'/0'";
const DEPOSIT_CHANGE: u32 = 0;
const GAS_CHANGE: u32 = 1;
const GAS_INDEX: u32 = 0;

pub struct BscKeys {
    account: XPrv,
}

impl BscKeys {
    /// Refuses an invalid mnemonic: a typo is a valid but DIFFERENT wallet, and the first symptom would
    /// be deposits at addresses this service cannot sweep.
    pub fn from_mnemonic(phrase: &str, passphrase: &str) -> Result<Self, String> {
        let mnemonic = Mnemonic::parse_normalized(phrase.trim()).map_err(|e| format!("invalid BIP39 mnemonic: {e}"))?;
        let path: DerivationPath = ACCOUNT_PATH.parse().map_err(|e| format!("bad account path: {e}"))?;
        let account = XPrv::derive_from_path(mnemonic.to_seed(passphrase), &path).map_err(|e| format!("account derivation failed: {e}"))?;
        Ok(Self { account })
    }

    /// The ACCOUNT-level xpub (`m/44'/60'/0'`), for the orchestrator's `APP_BSC_DEPOSIT_ACCOUNT_XPUB`.
    pub fn account_xpub(&self) -> String {
        self.account.public_key().to_string(bip32::Prefix::XPUB)
    }

    fn child(&self, change: u32, index: u32) -> Result<XPrv, String> {
        if index >= 0x8000_0000 {
            return Err(format!("index {index} is in the hardened range; the orchestrator cannot derive its address"));
        }
        self.account
            .derive_child(ChildNumber::new(change, false).map_err(|e| e.to_string())?)
            .map_err(|e| format!("change-level derivation failed: {e}"))?
            .derive_child(ChildNumber::new(index, false).map_err(|e| e.to_string())?)
            .map_err(|e| format!("index {index} derivation failed: {e}"))
    }

    pub fn address_at(&self, index: u32) -> Result<String, String> {
        Ok(evm_address(&self.child(DEPOSIT_CHANGE, index)?))
    }

    pub fn signing_key_at(&self, index: u32) -> Result<SigningKey, String> {
        Ok(self.child(DEPOSIT_CHANGE, index)?.private_key().clone())
    }

    /// Where an operator sends the BNB that pays for sweeps.
    pub fn gas_address(&self) -> Result<String, String> {
        Ok(evm_address(&self.child(GAS_CHANGE, GAS_INDEX)?))
    }

    pub fn gas_signing_key(&self) -> Result<SigningKey, String> {
        Ok(self.child(GAS_CHANGE, GAS_INDEX)?.private_key().clone())
    }
}

/// `0x` + the low 20 bytes of `keccak256(uncompressed pubkey without its 0x04)`, lowercase.
fn evm_address(key: &XPrv) -> String {
    let point = key.public_key().public_key().to_encoded_point(false);
    format!("0x{}", hex::encode(&Keccak256::digest(&point.as_bytes()[1..])[12..]))
}

/// The address of a signing key, the same way.
pub fn address_of(key: &SigningKey) -> String {
    let point = key.verifying_key().to_encoded_point(false);
    format!("0x{}", hex::encode(&Keccak256::digest(&point.as_bytes()[1..])[12..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical public BIP39 test mnemonic; never holds funds.
    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    /// MetaMask's first two accounts for that mnemonic, as every Ethereum wallet shows them. If these
    /// match, the path is MetaMask's and a human can recover any deposit address by importing it.
    const METAMASK: [(u32, &str); 2] =
        [(0, "0x9858effd232b4033e47d90003d41ec34ecaeda94"), (1, "0x6fac4d18c912343bf86fa7049364dd4e424ab9c0")];

    #[test]
    fn deposit_addresses_are_metamasks_accounts() {
        let k = BscKeys::from_mnemonic(MNEMONIC, "").unwrap();
        for (index, want) in METAMASK {
            assert_eq!(k.address_at(index).unwrap(), want);
            assert_eq!(address_of(&k.signing_key_at(index).unwrap()), want, "the key spends the address it names");
        }
    }

    #[test]
    fn the_gas_account_is_none_of_the_deposit_addresses() {
        let k = BscKeys::from_mnemonic(MNEMONIC, "").unwrap();
        let gas = k.gas_address().unwrap();
        assert_eq!(address_of(&k.gas_signing_key().unwrap()), gas);
        assert!((0..50).all(|i| k.address_at(i).unwrap() != gas));
    }

    #[test]
    fn bad_input_is_refused() {
        assert!(BscKeys::from_mnemonic("abandon abandon", "").is_err());
        let k = BscKeys::from_mnemonic(MNEMONIC, "").unwrap();
        assert!(k.address_at(0x8000_0000).is_err());
        assert!(k.account_xpub().starts_with("xpub"));
    }
}
