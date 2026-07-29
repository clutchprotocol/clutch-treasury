use secp256k1::{ecdsa::RecoverableSignature, Message, PublicKey, Secp256k1, SecretKey};
use sha3::{Digest, Keccak256};

/// Signing boundary. Env-var key today; `KmsSigner` (AWS KMS) implements this same
/// trait for mainnet — see docs/keys.md. Callers never see key material.
pub trait ChainSigner: Send + Sync {
    fn address(&self) -> String;
    /// Input: the tx hash as 64-char lowercase hex WITHOUT 0x.
    /// Output: (r_hex, s_hex, v) with v in {27, 28} — the stack's convention.
    fn sign_hash_hex(&self, hash_hex: &str) -> Result<(String, String, u64), String>;
}

pub struct EnvKeySigner {
    secret: SecretKey,
    address: String,
}

impl EnvKeySigner {
    pub fn from_secret_hex(secret_hex: &str) -> Result<Self, String> {
        let clean = secret_hex.trim_start_matches("0x");
        let bytes = hex::decode(clean).map_err(|e| e.to_string())?;
        let secret = SecretKey::from_slice(&bytes).map_err(|e| e.to_string())?;
        let secp = Secp256k1::new();
        let pk = PublicKey::from_secret_key(&secp, &secret);
        let ser = pk.serialize_uncompressed();
        let mut hasher = Keccak256::new();
        hasher.update(&ser[1..]);
        let h = hasher.finalize();
        Ok(Self {
            secret,
            address: format!("0x{}", hex::encode(&h[12..32])),
        })
    }
}

impl ChainSigner for EnvKeySigner {
    fn address(&self) -> String {
        self.address.clone()
    }

    fn sign_hash_hex(&self, hash_hex: &str) -> Result<(String, String, u64), String> {
        // Stack convention (node signature_keys.rs / SDK signHashHex): the message
        // digest is Keccak-256 of the hex STRING's UTF-8 bytes, not of the hash bytes.
        let mut hasher = Keccak256::new();
        hasher.update(hash_hex.as_bytes());
        let digest = hasher.finalize();
        let msg = Message::from_digest_slice(&digest).map_err(|e| e.to_string())?;
        let secp = Secp256k1::new();
        let sig: RecoverableSignature = secp.sign_ecdsa_recoverable(&msg, &self.secret);
        let (rec_id, compact) = sig.serialize_compact();
        let r = hex::encode(&compact[..32]);
        let s = hex::encode(&compact[32..]);
        let v = rec_id.to_i32() as u64 + 27;
        Ok((r, s, v))
    }
}
