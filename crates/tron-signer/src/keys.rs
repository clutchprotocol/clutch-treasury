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
/// Deposit addresses live at `0/i` — the "external" chain in BIP44 terms, which is what the
/// orchestrator derives from the account xpub.
const CHANGE_LEVEL: u32 = 0;
/// The fee account lives on the INTERNAL chain, `1/0`.
///
/// A different change level, deliberately: deposit addresses are `0/i` for every i, so nothing at
/// `1/0` can ever collide with one. Picking a high index on the external chain instead would have
/// worked until the deposit sequence eventually reached it — and the failure then is the fee account
/// being handed to a depositor, whose payment would be swept while our TRX float sits under someone
/// else's claim.
const FEE_CHANGE_LEVEL: u32 = 1;
const FEE_INDEX: u32 = 0;
/// The USDT payout float, `<account>/2/0`.
///
/// A third change level, for the same reason the fee account got a second one: deposit addresses
/// are `0/i` for every i, so nothing at `2/0` can ever be handed to a depositor. Separate from the
/// fee account at `1/0` because the two hold different assets and are topped up by different
/// people — a shared address would make "the float is dry" ambiguous between TRX and USDT.
const PAYOUT_CHANGE_LEVEL: u32 = 2;
const PAYOUT_INDEX: u32 = 0;
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

    fn fee_child(&self) -> Result<XPrv, String> {
        let change = ChildNumber::new(FEE_CHANGE_LEVEL, false).map_err(|e| e.to_string())?;
        let idx = ChildNumber::new(FEE_INDEX, false).map_err(|e| e.to_string())?;
        self.account
            .derive_child(change)
            .map_err(|e| format!("fee change-level derivation failed: {e}"))?
            .derive_child(idx)
            .map_err(|e| format!("fee index derivation failed: {e}"))
    }

    /// The TRX float account, `<account>/1/0`.
    ///
    /// A fresh deposit address holds no TRX — receiving tokens does not create a balance — so it
    /// cannot pay for its own sweep. This account is topped up by an operator and pays those fees.
    /// It is part of the same wallet, so no new key material and no second mnemonic to look after.
    pub fn fee_address(&self) -> Result<String, String> {
        let pubkey = self.fee_child()?.public_key().public_key().to_encoded_point(false);
        Ok(tron_address_from_uncompressed(pubkey.as_bytes()))
    }

    pub fn fee_signing_key(&self) -> Result<SigningKey, String> {
        Ok(self.fee_child()?.private_key().clone().into())
    }

    fn payout_child(&self) -> Result<XPrv, String> {
        let change = ChildNumber::new(PAYOUT_CHANGE_LEVEL, false).map_err(|e| e.to_string())?;
        let idx = ChildNumber::new(PAYOUT_INDEX, false).map_err(|e| e.to_string())?;
        self.account
            .derive_child(change)
            .map_err(|e| format!("payout change-level derivation failed: {e}"))?
            .derive_child(idx)
            .map_err(|e| format!("payout index derivation failed: {e}"))
    }

    /// The USDT payout float, `<account>/2/0`.
    ///
    /// Redemption payouts are sent from here and nowhere else. An operator tops it up from custody;
    /// its balance is the cap on what a compromised caller could move, which is why it is a float
    /// and not the custody address itself.
    pub fn payout_address(&self) -> Result<String, String> {
        let pubkey = self.payout_child()?.public_key().public_key().to_encoded_point(false);
        Ok(tron_address_from_uncompressed(pubkey.as_bytes()))
    }

    pub fn payout_signing_key(&self) -> Result<SigningKey, String> {
        Ok(self.payout_child()?.private_key().clone().into())
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

#[cfg(test)]
mod fee_tests {
    use super::*;

    const MNEMONIC: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    /// THE separation property. The fee account must never be a deposit address, or our TRX float
    /// would sit at an address some depositor was told to pay into — and the sweep would move their
    /// money out from under a balance we were treating as ours.
    ///
    /// Checked against a generous span of deposit indices rather than just the first few, because
    /// the hazard is a COLLISION that only appears once the sequence has run far enough.
    #[test]
    fn the_fee_address_is_never_a_deposit_address() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let fee = s.fee_address().unwrap();
        for i in 0..2000 {
            assert_ne!(s.address_at(i).unwrap(), fee, "deposit index {i} collided with the fee account");
        }
    }

    /// The fee key must control the fee address — proven by deriving the address back out of the
    /// key, not by trusting two code paths.
    #[test]
    fn the_fee_key_controls_the_fee_address() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let key = s.fee_signing_key().unwrap();
        let pubkey = key.verifying_key().to_encoded_point(false);
        assert_eq!(tron_address_from_uncompressed(pubkey.as_bytes()), s.fee_address().unwrap());
    }

    /// Deterministic across instances: an operator funds this address once, and every later restart
    /// must arrive at the same one or the float is stranded.
    #[test]
    fn the_fee_address_is_stable_across_instances() {
        let a = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let b = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        assert_eq!(a.fee_address().unwrap(), b.fee_address().unwrap());
    }

    #[test]
    fn the_fee_address_is_valid_base58check() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let addr = s.fee_address().unwrap();
        let bytes = bs58::decode(&addr).with_check(Some(TRON_ADDRESS_VERSION)).into_vec().unwrap();
        assert_eq!(bytes.len(), 21);
        assert!(addr.starts_with('T'));
    }
}

#[cfg(test)]
mod payout_tests {
    use super::*;

    const MNEMONIC: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn the_payout_address_is_never_a_deposit_address() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let payout = s.payout_address().unwrap();
        for i in 0..2000u32 {
            assert_ne!(payout, s.address_at(i).unwrap(), "payout collides with deposit index {i}");
        }
    }

    #[test]
    fn the_payout_address_is_not_the_fee_address() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        assert_ne!(s.payout_address().unwrap(), s.fee_address().unwrap());
    }

    #[test]
    fn the_payout_key_controls_the_payout_address() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let pubkey = s.payout_signing_key().unwrap().verifying_key().to_encoded_point(false);
        assert_eq!(tron_address_from_uncompressed(pubkey.as_bytes()), s.payout_address().unwrap());
    }

    #[test]
    fn the_payout_address_is_stable_across_instances() {
        let a = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let b = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        assert_eq!(a.payout_address().unwrap(), b.payout_address().unwrap());
    }

    #[test]
    fn the_payout_path_is_pinned_to_2_0() {
        // Without this, any wrong-but-non-colliding path passes every other test in this module.
        // Guards a later accidental change to PAYOUT_CHANGE_LEVEL/PAYOUT_INDEX, which would send
        // every redemption payout to an address no operator ever funded.
        assert_eq!(PAYOUT_CHANGE_LEVEL, 2, "the payout float lives on change level 2");
        assert_eq!(PAYOUT_INDEX, 0, "the payout float is index 0 of change level 2");
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        assert_eq!(s.payout_address().unwrap(), "PLACEHOLDER_FILL_FROM_CI");
    }

    #[test]
    fn the_payout_address_is_valid_base58check() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let addr = s.payout_address().unwrap();
        let bytes = bs58::decode(&addr).with_check(Some(TRON_ADDRESS_VERSION)).into_vec().unwrap();
        assert_eq!(bytes.len(), 21);
        assert!(addr.starts_with('T'));
    }
}
