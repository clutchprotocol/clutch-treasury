//! The only place in the system that turns a mnemonic into spending keys.
//!
//! # What this service exists to make impossible
//!
//! Per-intent deposit addresses mean funds land on N addresses that someone must be able to sweep.
//! That "someone" holds spending authority over user money, so the design question is not "where do
//! we keep the key" but "what can a compromise of everything else make it do".
//!
//! The answer here: nothing but move funds to the configured treasury address.
//!
//! The signer takes `(index, amount)` and CONSTRUCTS the transfer itself, with the destination read
//! from its own config. The caller cannot name a recipient. So an orchestrator that is entirely
//! owned by an attacker can, at worst, sweep deposits into the treasury slightly early — which is
//! where they were going anyway.
//!
//! A signer that signs caller-supplied transaction bytes would have none of this property: it would
//! be a key with extra steps and an audit log. Any future change that adds a `to` parameter to the
//! sweep API removes the entire security argument for this service existing.
//!
//! # Derivation must agree with the orchestrator, exactly
//!
//! The orchestrator derives receive addresses from the ACCOUNT xpub at `m/44'/195'/0'/0/i`. This
//! service derives the matching private keys from the mnemonic at the same path. If the two ever
//! disagree the funds are not lost, but they are unspendable until someone works out which path was
//! actually used — so `address_at` here exists specifically so the agreement can be asserted rather
//! than assumed, and the tests check both against the same published fixture.

use bip32::{ChildNumber, DerivationPath, XPrv};
use bip39::Mnemonic;
use k256::ecdsa::SigningKey;
use sha3::{Digest, Keccak256};

/// Tron's BIP44 account path. The orchestrator is given the xpub OF THIS PATH and derives `0/i`
/// children from it; deriving anything else here silently produces addresses whose keys this
/// service cannot reconstruct.
const ACCOUNT_PATH: &str = "m/44'/195'/0'";
const CHANGE_LEVEL: u32 = 0;
const TRON_ADDRESS_VERSION: u8 = 0x41;

pub struct Signer {
    account: XPrv,
}

impl Signer {
    /// BIP39 mnemonic, optional passphrase. Rejects an invalid mnemonic rather than deriving from
    /// nonsense — a typo would otherwise yield a perfectly valid but completely different wallet,
    /// and the first symptom would be deposits arriving at addresses nobody can sweep.
    pub fn from_mnemonic(phrase: &str, passphrase: &str) -> Result<Self, String> {
        let mnemonic = Mnemonic::parse_normalized(phrase.trim())
            .map_err(|e| format!("invalid BIP39 mnemonic: {e}"))?;
        let seed = mnemonic.to_seed(passphrase);
        let path: DerivationPath = ACCOUNT_PATH.parse().map_err(|e| format!("bad account path: {e}"))?;
        let account = XPrv::derive_from_path(&seed, &path).map_err(|e| format!("account derivation failed: {e}"))?;
        Ok(Self { account })
    }

    /// The ACCOUNT-level xpub, for configuring the orchestrator.
    ///
    /// Exposed so the public half can be read off the service that owns the private half, rather
    /// than transcribed by hand from wherever the mnemonic was generated. A mistyped xpub in the
    /// orchestrator means every deposit address is one this signer cannot sweep.
    pub fn account_xpub(&self) -> String {
        self.account.public_key().to_string(bip32::Prefix::XPUB)
    }

    fn child(&self, index: u32) -> Result<XPrv, String> {
        if index >= 0x8000_0000 {
            return Err(format!("index {index} is in the hardened range; the orchestrator cannot derive its address"));
        }
        let change = ChildNumber::new(CHANGE_LEVEL, false).map_err(|e| e.to_string())?;
        let idx = ChildNumber::new(index, false).map_err(|e| e.to_string())?;
        self.account
            .derive_child(change)
            .map_err(|e| format!("change-level derivation failed: {e}"))?
            .derive_child(idx)
            .map_err(|e| format!("index {index} derivation failed: {e}"))
    }

    /// The signing key for a deposit address.
    pub fn signing_key_at(&self, index: u32) -> Result<SigningKey, String> {
        Ok(self.child(index)?.private_key().clone().into())
    }

    /// The address for `index` — MUST equal what the orchestrator derived from the xpub.
    ///
    /// Exists so that agreement is checkable rather than assumed. A sweep signed for an address the
    /// depositor was never given is a transaction that moves nothing and costs fees.
    pub fn address_at(&self, index: u32) -> Result<String, String> {
        let child = self.child(index)?;
        let pubkey = child.public_key().public_key().to_encoded_point(false);
        Ok(tron_address_from_uncompressed(pubkey.as_bytes()))
    }
}

/// `base58check(0x41 || keccak256(pubkey[1..])[12..32])` — identical to the orchestrator's, and
/// deliberately duplicated rather than shared: a crate boundary between the service that holds
/// spending keys and everything else is worth more than the twenty lines it costs, and the tests
/// pin both to the same external fixture so they cannot drift apart unnoticed.
fn tron_address_from_uncompressed(pubkey: &[u8]) -> String {
    let hash = Keccak256::digest(&pubkey[1..]);
    let mut payload = Vec::with_capacity(21);
    payload.push(TRON_ADDRESS_VERSION);
    payload.extend_from_slice(&hash[12..32]);
    bs58::encode(payload).with_check().into_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical public BIP39 test mnemonic. Public material, never used to hold funds — and
    /// the SAME wallet the orchestrator's derive.rs tests pin, which is the point: these two
    /// services must agree on every address or sweeps sign for the wrong account.
    const MNEMONIC: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    /// Ground truth from @scure/bip32, generated independently of both implementations. Index 0 is
    /// additionally a real Tron mainnet account created 2018-07-12.
    const EXPECTED: [(u32, &str); 5] = [
        (0, "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH"),
        (1, "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK"),
        (2, "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx"),
        (3, "TRhVWK5XEDkQBDevcdCWW7RW51aRncty4W"),
        (4, "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY"),
    ];

    /// THE agreement property. The private keys this service derives must correspond to exactly the
    /// addresses the orchestrator hands depositors. A mismatch is money that arrives somewhere
    /// nobody can spend from.
    #[test]
    fn derives_the_same_addresses_the_orchestrator_gives_depositors() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        for (index, want) in EXPECTED {
            assert_eq!(s.address_at(index).unwrap(), want, "address at index {index}");
        }
    }

    /// The xpub this service publishes must be the one the orchestrator is configured with —
    /// otherwise every address it generates belongs to a different wallet.
    #[test]
    fn publishes_the_account_xpub_the_orchestrator_expects() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        assert_eq!(
            s.account_xpub(),
            "xpub6D1AabNHCupeiLM65ZR9UStMhJ1vCpyV4XbZdyhMZBiJXALQtmn9p42VTQckoHVn8WNqS7dqnJokZHAHcHGoaQgmv8D45oNUKx6DZMNZBCd"
        );
    }

    /// The signing key must actually correspond to the address — proven by deriving the address
    /// back OUT of the key, not by trusting that two code paths agree.
    #[test]
    fn the_signing_key_corresponds_to_the_address() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        for (index, want) in EXPECTED {
            let key = s.signing_key_at(index).unwrap();
            let pubkey = key.verifying_key().to_encoded_point(false);
            assert_eq!(
                tron_address_from_uncompressed(pubkey.as_bytes()),
                want,
                "the key at index {index} must control {want}"
            );
        }
    }

    /// A mistyped mnemonic must be refused, not silently used. It would otherwise be a valid but
    /// DIFFERENT wallet, and the first symptom would be unsweepable deposits.
    #[test]
    fn an_invalid_mnemonic_is_refused() {
        for bad in [
            "",
            "not a mnemonic at all",
            // Valid words, wrong checksum.
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon",
        ] {
            assert!(Signer::from_mnemonic(bad, "").is_err(), "must reject {bad:?}");
        }
    }

    /// A passphrase selects a different wallet entirely. Asserted so nobody "fixes" a config
    /// mismatch by adding one and quietly moves every address.
    #[test]
    fn a_passphrase_yields_a_completely_different_wallet() {
        let plain = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let with_pass = Signer::from_mnemonic(MNEMONIC, "extra").unwrap();
        assert_ne!(plain.account_xpub(), with_pass.account_xpub());
        assert_ne!(plain.address_at(0).unwrap(), with_pass.address_at(0).unwrap());
    }

    #[test]
    fn hardened_indices_are_refused() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        assert!(s.address_at(0x8000_0000).is_err());
        assert!(s.signing_key_at(0x8000_0000).is_err());
    }
}
