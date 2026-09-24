//! GasFree on TRON: the address where a wallet's GasFree account lives, and the hash its owner
//! signs to move a token out of it.
//!
//! Pure functions over public constants, with no key and no network. The key-free orchestrator
//! and the signer both compute from this one copy, because two copies that drifted apart would
//! show a user one address while the signer swept another.
//!
//! Ported from the official SDK, `gasfreeio/gasfree-sdk-js` at commit
//! `70f4d5aa12da785735903eda8efd568406ba6ffc`, and tested against that SDK's own vectors.
//! Design: `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md`.

mod address;
mod permit;

pub use address::gasfree_address;
pub use permit::{permit_hash, Permit};

use sha3::{Digest, Keccak256};

/// One GasFree deployment: the public constants that an address and a permit are computed from.
///
/// Only `NILE` and `MAINNET` exist. The creation code field is private, so a caller cannot build a
/// third `Chain` that mixes one network's controller with another network's creation code. They
/// are `static`, not `const`, and `Chain` is neither `Copy` nor `Clone`, so a caller cannot take an
/// owned copy of one and change its public fields either. Keep both true: the two creation codes
/// differ in only 32 bytes, so a mixed `Chain` gives a valid-looking address that nobody controls.
pub struct Chain {
    /// The TIP-712 domain's `chainId`.
    pub chain_id: u64,
    /// The GasFreeController. It deploys every GasFree address with CREATE2, and it is the permit's
    /// `verifyingContract`.
    pub controller: &'static str,
    /// The beacon that every GasFree address proxies to. Whoever controls it can change the code
    /// at every GasFree address at once. The design's tripwire watches it.
    pub beacon: &'static str,
    creation_code_hex: &'static str,
}

/// TRON's Nile testnet, from the SDK's `DefaultChainInfoMap`.
pub static NILE: Chain = Chain {
    chain_id: 3_448_148_188, // 0xcd8690dc
    controller: "THQGuFzL87ZqhxkgqYEryRAd7gqFqL5rdc",
    beacon: "TLtCGmaxH3PbuaF6kbybwteZcHptEdgQGC",
    creation_code_hex: include_str!("creation_code_nile.hex"),
};

/// TRON mainnet, from the SDK's `DefaultChainInfoMap`.
pub static MAINNET: Chain = Chain {
    chain_id: 728_126_428, // 0x2b6653dc
    controller: "TFFAMQLZybALaLb4uxHA9RBE7pxhUAjF3U",
    beacon: "TSP9UW6FQhT76XD2jWA6ipGMx3yGbjDffP",
    creation_code_hex: include_str!("creation_code_mainnet.hex"),
};

impl Chain {
    /// The beacon proxy's creation code, 997 bytes, copied from the SDK's
    /// `src/constant/common.ts`. The address tests fail if a single byte of it is wrong.
    fn creation_code(&self) -> Vec<u8> {
        hex::decode(self.creation_code_hex.trim())
            .expect("the creation code is hex; the address tests fail if it is not")
    }
}

const TRON_ADDRESS_VERSION: u8 = 0x41;

/// A TRON address's 20-byte body, after its base58check checksum and version byte are checked.
fn decode(address: &str) -> Result<[u8; 20], String> {
    let bytes = bs58::decode(address)
        .with_check(Some(TRON_ADDRESS_VERSION))
        .into_vec()
        .map_err(|e| format!("address {address} failed base58check: {e}"))?;
    if bytes.len() != 21 {
        return Err(format!("address {address} decoded to {} bytes, want 21", bytes.len()));
    }
    let mut body = [0u8; 20];
    body.copy_from_slice(&bytes[1..]);
    Ok(body)
}

/// The base58check TRON address of a 20-byte body.
fn encode(body: &[u8; 20]) -> String {
    let mut payload = Vec::with_capacity(21);
    payload.push(TRON_ADDRESS_VERSION);
    payload.extend_from_slice(body);
    // bs58's `with_check` appends the 4-byte double-SHA256 checksum, which is exactly TRON's.
    bs58::encode(payload).with_check().into_string()
}

/// An address as one 32-byte ABI word: twelve zero bytes, then the 20-byte body.
fn address_word(body: &[u8; 20]) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(body);
    word
}

/// A `uint256` as one 32-byte ABI word, big-endian.
fn uint_word(n: u64) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[24..].copy_from_slice(&n.to_be_bytes());
    word
}

fn keccak(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}
