//! Moving a deposit off its derived address and into the treasury.
//!
//! This is the only code in the system that spends user money, so the shape matters more than the
//! mechanics.
//!
//! # The caller cannot say where funds go
//!
//! `sweep` takes an INDEX and nothing else about the destination. The recipient comes from this
//! service's own config, the token contract comes from its own config, and the amount is whatever
//! that address actually holds. There is no parameter an attacker who owns the orchestrator could
//! set to redirect a single micro-USDT.
//!
//! Everything else follows from that: the request needs no authentication beyond reaching the
//! service, because the worst a hostile request achieves is sweeping a real deposit into the real
//! treasury slightly early. (It is still behind a token and an internal-only network — defence in
//! depth — but the design does not depend on those holding.)
//!
//! Do not add a `to`, a `contract`, or an `amount` parameter. Each one individually converts this
//! from "can only do the right thing" into "does whatever it is told by whoever got in".
//!
//! # Sweeping the whole balance, not the deposited amount
//!
//! A derived address exists for exactly one deposit, so anything sitting there is that deposit —
//! including an overpayment, and including a second transfer that arrived after crediting. Sweeping
//! the full balance means no dust is left stranded at an address nothing will ever look at again,
//! and it removes a whole class of "how much exactly" arithmetic from the spending path.

use k256::ecdsa::{signature::hazmat::PrehashSigner, RecoveryId, Signature, SigningKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::keys::Signer;

/// Enough TRX at the derived address to pay for one TRC-20 transfer.
///
/// A fresh address holds none: it has only ever received tokens, and receiving does not create a
/// TRX balance. So every sweep needs the address funded first, and a sweep attempted without it
/// fails at broadcast with a message about bandwidth that reads like a bug rather than an
/// operational gap. Checked up front so the error names the real problem.
///
/// 30 TRX covers a TRC-20 transfer to an already-created account at unstaked energy prices, with
/// room for the fee market moving. It is a floor for a preflight check, not a spend.
const MIN_TRX_SUN_FOR_TRANSFER: i64 = 30_000_000;

pub struct SweepConfig {
    pub trongrid_url: String,
    pub trongrid_api_key: String,
    /// Where every sweep goes. NOT a parameter, deliberately — see the module docs.
    pub treasury_address: String,
    pub usdt_contract: String,
    pub fee_limit: i64,
}

#[derive(Debug, PartialEq)]
pub enum SweepOutcome {
    /// Broadcast accepted; `tx_id` is the on-chain transfer.
    Swept { tx_id: String, amount_usdt: i64 },
    /// The address holds no USDT. Not an error: a sweep worker re-running over an
    /// already-swept address must be a no-op, not a failure.
    NothingToSweep,
    /// The address cannot pay for its own transfer yet. Distinct from an error so a worker can
    /// fund it and retry rather than treating the deposit as broken.
    NeedsTrx { have_sun: i64, need_sun: i64 },
}

/// ABI-encode a Tron address into the 32-byte word `transfer(address,uint256)` expects.
///
/// Decodes base58check with the version byte enforced, so a corrupted destination fails here rather
/// than sending funds to whatever the malformed string happened to encode.
pub fn abi_address(address: &str) -> Result<String, String> {
    let bytes = bs58::decode(address)
        .with_check(Some(0x41))
        .into_vec()
        .map_err(|e| format!("address {address} failed base58check: {e}"))?;
    if bytes.len() != 21 {
        return Err(format!("address {address} decoded to {} bytes, want 21", bytes.len()));
    }
    Ok(format!("{:0>64}", hex::encode(&bytes[1..])))
}

/// `transfer(address,uint256)` parameters: recipient then amount, each right-aligned in 32 bytes.
pub fn transfer_parameter(to: &str, amount: i64) -> Result<String, String> {
    if amount <= 0 {
        return Err(format!("refusing to build a transfer of {amount}"));
    }
    Ok(format!("{}{:0>64x}", abi_address(to)?, amount))
}

/// Sign a Tron transaction id.
///
/// Two details differ from the Clutch chain's signing and are easy to get wrong: the digest is the
/// raw txID bytes (sha256 of raw_data, NOT keccak, and NOT re-hashed), and `v` is the recovery id
/// itself, 0 or 1 — Ethereum adds 27, Tron does not.
pub fn sign_txid(key: &SigningKey, txid_hex: &str) -> Result<String, String> {
    let digest = hex::decode(txid_hex).map_err(|e| format!("txID is not hex: {e}"))?;
    if digest.len() != 32 {
        return Err(format!("txID is {} bytes, want 32", digest.len()));
    }
    let (sig, recid): (Signature, RecoveryId) =
        key.sign_prehash(&digest).map_err(|e| format!("signing failed: {e}"))?;
    Ok(format!("{}{:02x}", hex::encode(sig.to_bytes()), recid.to_byte()))
}

#[derive(Deserialize)]
struct BuiltTx {
    #[serde(rename = "txID")]
    tx_id: String,
    raw_data_hex: String,
}

#[derive(Deserialize)]
struct BuildResponse {
    transaction: Option<BuiltTx>,
}

#[derive(Deserialize)]
struct AccountsResponse {
    #[serde(default)]
    data: Vec<AccountRow>,
}

#[derive(Deserialize)]
struct AccountRow {
    #[serde(default)]
    balance: i64,
}

pub struct SweepClient {
    http: reqwest::Client,
    cfg: SweepConfig,
}

impl SweepClient {
    pub fn new(cfg: SweepConfig) -> Self {
        Self { http: reqwest::Client::new(), cfg }
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
        self.http
            .post(format!("{}{path}", self.cfg.trongrid_url))
            .header("TRON-PRO-API-KEY", &self.cfg.trongrid_api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())
    }

    /// USDT held at `address`, in base units.
    async fn usdt_balance(&self, address: &str) -> Result<i64, String> {
        let resp = self
            .post(
                "/wallet/triggerconstantcontract",
                serde_json::json!({
                    "owner_address": address,
                    "contract_address": self.cfg.usdt_contract,
                    "function_selector": "balanceOf(address)",
                    "parameter": abi_address(address)?,
                    "visible": true,
                }),
            )
            .await?;
        let word = resp["constant_result"][0].as_str().ok_or("balanceOf returned no result")?;
        let trimmed = word.trim_start_matches('0');
        if trimmed.is_empty() {
            return Ok(0);
        }
        i64::from_str_radix(trimmed, 16).map_err(|_| format!("balanceOf returned an unrepresentable value: 0x{word}"))
    }

    /// TRX held at `address`, in sun. An address with no account record holds none.
    async fn trx_balance_sun(&self, address: &str) -> Result<i64, String> {
        let resp: AccountsResponse = self
            .http
            .get(format!("{}/v1/accounts/{address}", self.cfg.trongrid_url))
            .header("TRON-PRO-API-KEY", &self.cfg.trongrid_api_key)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        Ok(resp.data.first().map(|a| a.balance).unwrap_or(0))
    }

    /// Move everything at `index` to the configured treasury address.
    ///
    /// The destination and the token are this service's config; only the index comes from the
    /// caller. See the module docs for why that is the whole point.
    pub async fn sweep(&self, signer: &Signer, index: u32) -> Result<SweepOutcome, String> {
        let from = signer.address_at(index)?;

        let amount = self.usdt_balance(&from).await?;
        if amount == 0 {
            return Ok(SweepOutcome::NothingToSweep);
        }

        // Preflight, so "this address cannot pay its own fee" is reported as that rather than as a
        // broadcast failure about bandwidth.
        let trx = self.trx_balance_sun(&from).await?;
        if trx < MIN_TRX_SUN_FOR_TRANSFER {
            return Ok(SweepOutcome::NeedsTrx { have_sun: trx, need_sun: MIN_TRX_SUN_FOR_TRANSFER });
        }

        let built: BuildResponse = serde_json::from_value(
            self.post(
                "/wallet/triggersmartcontract",
                serde_json::json!({
                    "owner_address": from,
                    "contract_address": self.cfg.usdt_contract,
                    "function_selector": "transfer(address,uint256)",
                    "parameter": transfer_parameter(&self.cfg.treasury_address, amount)?,
                    "fee_limit": self.cfg.fee_limit,
                    "call_value": 0,
                    "visible": true,
                }),
            )
            .await?,
        )
        .map_err(|e| format!("unexpected build response: {e}"))?;
        let tx = built.transaction.ok_or("trongrid returned no transaction to sign")?;

        // Recompute the id rather than trusting the node's: it is the thing being signed, and a
        // node that returned a txID for different raw_data would otherwise get a valid signature
        // over a transaction we never inspected.
        let computed = hex::encode(Sha256::digest(hex::decode(&tx.raw_data_hex).map_err(|e| e.to_string())?));
        if computed != tx.tx_id {
            return Err(format!("txID mismatch: node said {} but raw_data hashes to {computed}", tx.tx_id));
        }

        let signature = sign_txid(&signer.signing_key_at(index)?, &tx.tx_id)?;
        let mut broadcast = serde_json::to_value(&serde_json::json!({
            "txID": tx.tx_id,
            "raw_data_hex": tx.raw_data_hex,
            "signature": [signature],
        }))
        .map_err(|e| e.to_string())?;
        // TronGrid wants the full transaction object back; carry through whatever else it built.
        if let Some(obj) = broadcast.as_object_mut() {
            obj.insert("visible".into(), serde_json::Value::Bool(true));
        }

        let res = self.post("/wallet/broadcasttransaction", broadcast).await?;
        if res["result"].as_bool() != Some(true) {
            let code = res["code"].as_str().unwrap_or("");
            let msg = res["message"].as_str().unwrap_or("");
            let decoded = hex::decode(msg).ok().and_then(|b| String::from_utf8(b).ok()).unwrap_or_default();
            return Err(format!("broadcast rejected: {code} {decoded}"));
        }
        Ok(SweepOutcome::Swept { tx_id: tx.tx_id, amount_usdt: amount })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREASURY: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";
    const OTHER: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";
    const MNEMONIC: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn abi_encodes_an_address_into_one_right_aligned_word() {
        let w = abi_address(TREASURY).unwrap();
        assert_eq!(w.len(), 64);
        assert!(w.starts_with(&"0".repeat(24)), "20 bytes right-aligned in 32: {w}");
    }

    /// A corrupted destination must fail here rather than encode whatever the malformed string
    /// happened to decode to — that would be a transfer to an address nobody controls.
    #[test]
    fn a_corrupted_address_is_refused_not_encoded() {
        let mut chars: Vec<char> = TREASURY.chars().collect();
        chars[10] = if chars[10] == 'a' { 'b' } else { 'a' };
        let bad: String = chars.into_iter().collect();
        assert_ne!(bad, TREASURY);
        assert!(abi_address(&bad).is_err(), "a bad checksum must not produce a word");
    }

    /// The parameter must encode the RECIPIENT then the amount, in that order. Reversed, the funds
    /// would go to an address derived from the amount.
    #[test]
    fn transfer_parameter_places_recipient_then_amount() {
        let p = transfer_parameter(TREASURY, 1_000_000).unwrap();
        assert_eq!(p.len(), 128, "two 32-byte words");
        assert_eq!(&p[..64], abi_address(TREASURY).unwrap(), "first word is the recipient");
        assert_eq!(i64::from_str_radix(p[64..].trim_start_matches('0'), 16).unwrap(), 1_000_000);
    }

    /// Encoding two different recipients must differ — a parameter builder that ignored its
    /// argument would send every sweep to one place and still pass a single-address test.
    #[test]
    fn different_recipients_encode_differently() {
        assert_ne!(
            transfer_parameter(TREASURY, 1_000_000).unwrap(),
            transfer_parameter(OTHER, 1_000_000).unwrap()
        );
    }

    #[test]
    fn refuses_to_build_a_non_positive_transfer() {
        assert!(transfer_parameter(TREASURY, 0).is_err());
        assert!(transfer_parameter(TREASURY, -1).is_err());
    }

    /// Tron's `v` is the recovery id itself (0 or 1), not recovery + 27. Getting this wrong yields
    /// a signature the network rejects, or worse, one that recovers a different address.
    #[test]
    fn signature_is_65_bytes_with_a_bare_recovery_id() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let key = s.signing_key_at(0).unwrap();
        let sig = sign_txid(&key, &"ab".repeat(32)).unwrap();
        assert_eq!(sig.len(), 130, "65 bytes as hex");
        let v = u8::from_str_radix(&sig[128..], 16).unwrap();
        assert!(v == 0 || v == 1, "Tron v must be a bare recovery id, got {v}");
    }

    #[test]
    fn a_malformed_txid_is_refused() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let key = s.signing_key_at(0).unwrap();
        assert!(sign_txid(&key, "nothex").is_err());
        assert!(sign_txid(&key, "abcd").is_err(), "a 2-byte digest is not a txID");
    }

    /// Signing is deterministic per (key, digest) — RFC6979. Two calls must agree, or a retry would
    /// produce a second distinct transaction for one sweep.
    #[test]
    fn signing_is_deterministic() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let key = s.signing_key_at(3).unwrap();
        let digest = "cd".repeat(32);
        assert_eq!(sign_txid(&key, &digest).unwrap(), sign_txid(&key, &digest).unwrap());
    }

    /// Different indices must produce different signatures over the same digest — proof the sweep
    /// signs with the key for the address it is actually emptying.
    #[test]
    fn each_index_signs_with_its_own_key() {
        let s = Signer::from_mnemonic(MNEMONIC, "").unwrap();
        let digest = "ef".repeat(32);
        let a = sign_txid(&s.signing_key_at(0).unwrap(), &digest).unwrap();
        let b = sign_txid(&s.signing_key_at(1).unwrap(), &digest).unwrap();
        assert_ne!(a, b);
    }
}

/// Is this address worth sweeping yet?
///
/// Threshold rather than per-deposit, because on Tron a sweep is not free: the address must first
/// be funded with TRX for energy, then the transfer itself costs. Against a $1 minimum deposit,
/// sweeping each one as it arrives can cost more than it moves.
///
/// The age escape valve is what stops that becoming "small deposits never move". Without it a
/// balance below the threshold sits at its address indefinitely, permanently fragmenting the
/// reserve across addresses nobody revisits — the reserve sum would stay correct, but the funds
/// would be unusable in practice and the fragmentation would only grow.
pub fn should_sweep(balance_usdt: i64, threshold_usdt: i64, age_hours: i64, max_age_hours: i64) -> bool {
    if balance_usdt <= 0 {
        return false;
    }
    balance_usdt >= threshold_usdt || age_hours >= max_age_hours
}

#[cfg(test)]
mod threshold_tests {
    use super::should_sweep;

    const THRESHOLD: i64 = 100_000_000; // $100
    const MAX_AGE: i64 = 168; // a week

    #[test]
    fn sweeps_once_the_balance_reaches_the_threshold() {
        assert!(should_sweep(THRESHOLD, THRESHOLD, 0, MAX_AGE));
        assert!(should_sweep(THRESHOLD + 1, THRESHOLD, 0, MAX_AGE));
    }

    #[test]
    fn leaves_a_small_fresh_balance_alone() {
        assert!(!should_sweep(THRESHOLD - 1, THRESHOLD, 0, MAX_AGE));
    }

    /// The escape valve. Without it, anything under the threshold sits at its address forever and
    /// the reserve fragments permanently across addresses nobody revisits.
    #[test]
    fn sweeps_a_small_balance_once_it_is_old_enough() {
        assert!(should_sweep(1, THRESHOLD, MAX_AGE, MAX_AGE));
        assert!(should_sweep(1, THRESHOLD, MAX_AGE + 100, MAX_AGE));
    }

    /// An empty address is never worth a transaction, however old. A sweep here would spend TRX to
    /// move nothing.
    #[test]
    fn never_sweeps_an_empty_address_however_old() {
        assert!(!should_sweep(0, THRESHOLD, MAX_AGE * 10, MAX_AGE));
        assert!(!should_sweep(-1, THRESHOLD, MAX_AGE * 10, MAX_AGE), "a negative balance is nonsense, not a sweep");
    }

    /// A zero threshold means "sweep everything immediately" — valid, and worth pinning so nobody
    /// assumes a floor is implied.
    #[test]
    fn a_zero_threshold_sweeps_any_positive_balance() {
        assert!(should_sweep(1, 0, 0, MAX_AGE));
    }
}
