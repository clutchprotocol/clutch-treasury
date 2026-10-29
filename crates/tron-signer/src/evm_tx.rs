//! A legacy (type 0) EVM transaction, signed EIP-155 for one chain: what a BNB Smart Chain sweep sends.
//!
//! Legacy rather than EIP-1559 because BSC prices gas with one `gasPrice`, and the format is the
//! smallest one every BSC node accepts. The RLP is written here (lists of byte strings and integers,
//! nothing else is needed) and pinned to EIP-155's own example below.

use k256::ecdsa::{signature::hazmat::PrehashSigner, RecoveryId, Signature, SigningKey};
use sha3::{Digest, Keccak256};

#[derive(Debug, Clone)]
pub struct LegacyTx {
    pub nonce: u64,
    /// wei per gas.
    pub gas_price: u128,
    pub gas_limit: u64,
    /// `0x` + 40 hex.
    pub to: String,
    /// wei.
    pub value: u128,
    pub data: Vec<u8>,
}

/// One RLP item.
enum Item {
    Bytes(Vec<u8>),
    List(Vec<Item>),
}

fn uint(n: u128) -> Item {
    let bytes = n.to_be_bytes();
    let first = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len());
    Item::Bytes(bytes[first..].to_vec())
}

fn length_prefix(len: usize, short: u8, long: u8) -> Vec<u8> {
    if len <= 55 {
        return vec![short + len as u8];
    }
    let be = (len as u64).to_be_bytes();
    let first = be.iter().position(|b| *b != 0).unwrap_or(be.len() - 1);
    let mut out = vec![long + (be.len() - first) as u8];
    out.extend_from_slice(&be[first..]);
    out
}

fn encode(item: &Item) -> Vec<u8> {
    match item {
        Item::Bytes(b) if b.len() == 1 && b[0] < 0x80 => b.clone(),
        Item::Bytes(b) => [length_prefix(b.len(), 0x80, 0xb7), b.clone()].concat(),
        Item::List(items) => {
            let payload: Vec<u8> = items.iter().flat_map(encode).collect();
            [length_prefix(payload.len(), 0xc0, 0xf7), payload].concat()
        }
    }
}

impl LegacyTx {
    fn fields(&self) -> Result<Vec<Item>, String> {
        let to = hex::decode(self.to.strip_prefix("0x").unwrap_or(&self.to)).map_err(|e| format!("`to` is not hex: {e}"))?;
        if to.len() != 20 {
            return Err(format!("`to` is {} bytes, want 20", to.len()));
        }
        Ok(vec![
            uint(self.nonce as u128),
            uint(self.gas_price),
            uint(self.gas_limit as u128),
            Item::Bytes(to),
            uint(self.value),
            Item::Bytes(self.data.clone()),
        ])
    }

    /// The signed transaction (`0x` + hex, for `eth_sendRawTransaction`) and its hash (`0x` + 64 hex).
    pub fn sign(&self, chain_id: u64, key: &SigningKey) -> Result<(String, String), String> {
        let mut unsigned = self.fields()?;
        unsigned.extend([uint(chain_id as u128), uint(0), uint(0)]);
        let digest = Keccak256::digest(encode(&Item::List(unsigned)));
        let (sig, recid): (Signature, RecoveryId) = key.sign_prehash(&digest).map_err(|e| format!("signing failed: {e}"))?;
        // Low s only: nodes refuse the other half (EIP-2), and flipping s flips the recovery parity.
        let (sig, recid) = match sig.normalize_s() {
            Some(low) => (low, RecoveryId::new(!recid.is_y_odd(), recid.is_x_reduced())),
            None => (sig, recid),
        };
        let bytes = sig.to_bytes();
        let v = chain_id as u128 * 2 + 35 + recid.to_byte() as u128;
        let mut signed = self.fields()?;
        signed.extend([uint(v), Item::Bytes(strip(&bytes[..32])), Item::Bytes(strip(&bytes[32..]))]);
        let raw = encode(&Item::List(signed));
        Ok((format!("0x{}", hex::encode(&raw)), format!("0x{}", hex::encode(Keccak256::digest(&raw)))))
    }
}

/// A 32-byte big-endian integer, minimal: RLP integers carry no leading zeros.
fn strip(word: &[u8]) -> Vec<u8> {
    let first = word.iter().position(|b| *b != 0).unwrap_or(word.len());
    word[first..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EIP-155's own example: key 0x4646…46, nonce 9, 20 gwei, 21000 gas, 1 ether to 0x3535…35,
    /// chain 1. The signed bytes are the EIP's, byte for byte.
    #[test]
    fn signs_the_eip155_example() {
        let key = SigningKey::from_slice(&[0x46; 32]).unwrap();
        let tx = LegacyTx {
            nonce: 9,
            gas_price: 20_000_000_000,
            gas_limit: 21_000,
            to: format!("0x{}", "35".repeat(20)),
            value: 1_000_000_000_000_000_000,
            data: vec![],
        };
        let (raw, hash) = tx.sign(1, &key).unwrap();
        assert_eq!(
            raw,
            "0xf86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83"
        );
        assert_eq!(hash.len(), 66);
    }

    #[test]
    fn rlp_lengths_past_55_bytes_take_a_length_of_length() {
        assert_eq!(encode(&Item::Bytes(vec![0x7f])), vec![0x7f]);
        assert_eq!(encode(&Item::Bytes(vec![0x80])), vec![0x81, 0x80]);
        assert_eq!(encode(&uint(0)), vec![0x80]);
        let long = encode(&Item::Bytes(vec![1; 56]));
        assert_eq!(&long[..2], &[0xb8, 56]);
        assert_eq!(long.len(), 58);
    }

    #[test]
    fn a_short_to_is_refused() {
        let key = SigningKey::from_slice(&[0x46; 32]).unwrap();
        let tx = LegacyTx { nonce: 0, gas_price: 1, gas_limit: 1, to: "0x35".into(), value: 0, data: vec![] };
        assert!(tx.sign(97, &key).is_err());
    }
}
