use rlp::RlpStream;
use sha3::{Digest, Keccak256};

use crate::signer::ChainSigner;

pub fn keccak_hex(bytes: &[u8]) -> String {
    let mut hasher = Keccak256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// credit_ref / redemption_ref = keccak256 of the intent UUID string (node expects
/// 64 lowercase hex). Deterministic — a retried intent produces the same ref, and the
/// node's processed_ref set makes the mint exactly-once.
pub fn intent_ref(intent_id: &str) -> String {
    keccak_hex(intent_id.as_bytes())
}

fn strip_0x(s: &str) -> &str {
    s.trim_start_matches("0x").trim_start_matches("0X")
}

fn normalize_address(addr: &str) -> String {
    format!("0x{}", strip_0x(addr).to_lowercase())
}

pub enum FunctionData {
    Transfer { to: String, value: u64 },
    Mint { to: String, amount: u64, credit_ref: String },
    Burn { amount: u64, redemption_ref: Option<String> },
}

/// RLP tags must byte-match clutch-node rlp_encoding.rs: Transfer=0, Mint=6, Burn=7.
fn encode_function_call(data: &FunctionData) -> Vec<u8> {
    let (tag, args_out): (u8, Vec<u8>) = match data {
        FunctionData::Transfer { to, value } => {
            let mut args = RlpStream::new_list(2);
            args.append(&normalize_address(to));
            args.append(value);
            (0u8, args.out().to_vec())
        }
        FunctionData::Mint { to, amount, credit_ref } => {
            let mut args = RlpStream::new_list(3);
            args.append(&normalize_address(to));
            args.append(amount);
            args.append(credit_ref);
            (6u8, args.out().to_vec())
        }
        FunctionData::Burn { amount, redemption_ref } => {
            let mut args = RlpStream::new_list(2);
            args.append(amount);
            let ref_str = redemption_ref.clone().unwrap_or_default();
            args.append(&ref_str);
            (7u8, args.out().to_vec())
        }
    };
    let mut fc = RlpStream::new_list(2);
    fc.append(&tag);
    fc.append_raw(&args_out, 1);
    fc.out().to_vec()
}

pub struct SignedTx {
    pub raw_hex: String,
    pub tx_hash: String,
}

/// Node wire format (Plan A): preimage [from(no 0x), nonce, chain_id, data] → Keccak;
/// signed tx [from, nonce, chain_id, r, s, v, hash, data], hash WITHOUT 0x in-slot.
pub fn build_raw_transaction(
    signer: &dyn ChainSigner,
    nonce: u64,
    chain_id: u64,
    data: &FunctionData,
) -> Result<SignedTx, String> {
    let data_rlp = encode_function_call(data);
    let from_clean = strip_0x(&signer.address()).to_string();

    let mut unsigned = RlpStream::new_list(4);
    unsigned.append(&from_clean);
    unsigned.append(&nonce);
    unsigned.append(&chain_id);
    unsigned.append_raw(&data_rlp, 1);
    let hash_hex = keccak_hex(unsigned.out().as_ref());

    let (r, s, v) = signer.sign_hash_hex(&hash_hex)?;

    let mut full = RlpStream::new_list(8);
    full.append(&from_clean);
    full.append(&nonce);
    full.append(&chain_id);
    full.append(&strip_0x(&r).to_string());
    full.append(&strip_0x(&s).to_string());
    full.append(&v);
    full.append(&hash_hex);
    full.append_raw(&data_rlp, 1);

    Ok(SignedTx {
        raw_hex: format!("0x{}", hex::encode(full.out().as_ref())),
        tx_hash: format!("0x{}", hash_hex),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::EnvKeySigner;

    const DEV_SK: &str = "0883ddd3d07303b87c954b0c9383f7b78f45e002520fc03a8adc80595dbf6509";
    const DEV_ADDR: &str = "0x9b6e8afff8329743cac73dbef83ca3cbf9a74c20";

    #[test]
    fn env_signer_derives_known_dev_address() {
        let s = EnvKeySigner::from_secret_hex(DEV_SK).unwrap();
        assert_eq!(s.address(), DEV_ADDR);
    }

    #[test]
    fn intent_ref_is_64_lowercase_hex() {
        let r = intent_ref("018f2c7a-0000-7000-8000-000000000001");
        assert_eq!(r.len(), 64);
        assert!(r.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Deterministic: same intent id, same ref (exactly-once anchor).
        assert_eq!(r, intent_ref("018f2c7a-0000-7000-8000-000000000001"));
    }

    /// Structural pin of the wire format against the node's decoder rules
    /// (Plan A Task 4): 8-item signed list, 4-item hash preimage, Mint tag 6.
    #[test]
    fn mint_raw_tx_has_node_wire_shape() {
        let signer = EnvKeySigner::from_secret_hex(DEV_SK).unwrap();
        let data = FunctionData::Mint {
            to: "0x4444444444444444444444444444444444444444".to_string(),
            amount: 5_000_000,
            credit_ref: "a".repeat(64),
        };
        let signed = build_raw_transaction(&signer, 1, 2077, &data).unwrap();
        assert!(signed.raw_hex.starts_with("0x"));
        assert!(signed.tx_hash.starts_with("0x"));

        let bytes = hex::decode(signed.raw_hex.trim_start_matches("0x")).unwrap();
        let rlp = rlp::Rlp::new(&bytes);
        assert!(rlp.is_list());
        assert_eq!(rlp.item_count().unwrap(), 8, "signed tx must be the 8-item format");
        let nonce: u64 = rlp.val_at(1).unwrap();
        let chain_id: u64 = rlp.val_at(2).unwrap();
        assert_eq!((nonce, chain_id), (1, 2077));
        let hash_in_tx: String = rlp.val_at(6).unwrap();
        assert_eq!(format!("0x{}", hash_in_tx), signed.tx_hash);

        // data = [tag, args]; Mint tag 6, args [to, amount, credit_ref]
        let data_rlp = rlp.at(7).unwrap();
        assert_eq!(data_rlp.item_count().unwrap(), 2);
        let tag: u8 = data_rlp.val_at(0).unwrap();
        assert_eq!(tag, 6);
        let args = data_rlp.at(1).unwrap();
        assert_eq!(args.item_count().unwrap(), 3);
        let amount: u64 = args.val_at(1).unwrap();
        assert_eq!(amount, 5_000_000);
    }

    #[test]
    fn burn_encodes_tag7_with_optional_ref() {
        let signer = EnvKeySigner::from_secret_hex(DEV_SK).unwrap();
        let with_ref = FunctionData::Burn { amount: 9, redemption_ref: Some("b".repeat(64)) };
        let signed = build_raw_transaction(&signer, 2, 2077, &with_ref).unwrap();
        let bytes = hex::decode(signed.raw_hex.trim_start_matches("0x")).unwrap();
        let rlp = rlp::Rlp::new(&bytes);
        let data_rlp = rlp.at(7).unwrap();
        let tag: u8 = data_rlp.val_at(0).unwrap();
        assert_eq!(tag, 7);
        let args = data_rlp.at(1).unwrap();
        let ref_str: String = args.val_at(1).unwrap();
        assert_eq!(ref_str, "b".repeat(64));

        // None → empty string on the wire (node convention).
        let no_ref = FunctionData::Burn { amount: 9, redemption_ref: None };
        let signed = build_raw_transaction(&signer, 3, 2077, &no_ref).unwrap();
        let bytes = hex::decode(signed.raw_hex.trim_start_matches("0x")).unwrap();
        let args = rlp::Rlp::new(&bytes);
        let ref_str: String = args.at(7).unwrap().at(1).unwrap().val_at(1).unwrap();
        assert_eq!(ref_str, "");
    }
}
