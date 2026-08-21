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

/// TRX the fee account must hold ON TOP of what it is about to send.
///
/// Funding is itself a transaction. A fee account allowed to drain to exactly the amount it sends
/// would broadcast a transfer it cannot pay the bandwidth for — and the failure would land on the
/// deposit address's sweep, several steps away from the account that actually ran dry.
const FEE_ACCOUNT_RESERVE_SUN: i64 = 1_000_000;

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
    /// TRX was just sent to the address so it can pay for its own transfer. Not a failure and not
    /// yet a sweep: the funding has to confirm first, so the next pass does the actual sweep.
    ///
    /// Two passes rather than waiting inline, because waiting means holding a request open across
    /// Tron's confirmation time for every address in a batch.
    Funded { tx_id: String, amount_sun: i64 },
    /// The fee account has run out of TRX. The only outcome here that no automation can resolve —
    /// an operator has to top the account up, and until they do every sweep stalls.
    FeeAccountDry { fee_address: String, have_sun: i64, need_sun: i64 },
}

/// The body for a native TRX transfer from the fee account to a deposit address.
///
/// Separate from the call so the argument order is testable. `owner_address` pays and `to_address`
/// receives; swapped, this asks a deposit address that holds no TRX to fund the account that was
/// supposed to fund it — which fails at broadcast, reads as "insufficient balance", and names the
/// wrong account entirely.
fn funding_body(fee_address: &str, deposit_address: &str, amount_sun: i64) -> serde_json::Value {
    serde_json::json!({
        "owner_address": fee_address,
        "to_address": deposit_address,
        "amount": amount_sun,
        "visible": true,
    })
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

    /// Send `from` enough TRX to pay for its own sweep, out of the wallet's fee account.
    ///
    /// A fresh deposit address holds no TRX — receiving tokens does not create a balance — so it
    /// cannot move the USDT sitting on it. The fee account (`<account>/1/0`) is the wallet's TRX
    /// float, topped up by an operator, and this is the only thing that spends from it.
    ///
    /// Sends the full minimum rather than the shortfall. Topping up the difference would, for an
    /// address already near the floor, broadcast a transaction worth less than its own bandwidth.
    async fn fund(&self, signer: &Signer, deposit_address: &str) -> Result<SweepOutcome, String> {
        let fee_address = signer.fee_address()?;
        let have = self.trx_balance_sun(&fee_address).await?;
        let need = MIN_TRX_SUN_FOR_TRANSFER + FEE_ACCOUNT_RESERVE_SUN;
        if have < need {
            return Ok(SweepOutcome::FeeAccountDry { fee_address, have_sun: have, need_sun: need });
        }

        let built = self
            .post(
                "/wallet/createtransaction",
                funding_body(&fee_address, deposit_address, MIN_TRX_SUN_FOR_TRANSFER),
            )
            .await?;
        // createtransaction returns the transaction at the top level and reports refusals as
        // {"Error": ...} — checked explicitly so a rejection is not reported as a parse failure.
        if let Some(err) = built["Error"].as_str() {
            return Err(format!("trongrid refused to build the funding transfer: {err}"));
        }
        let tx: BuiltTx =
            serde_json::from_value(built).map_err(|e| format!("unexpected createtransaction response: {e}"))?;

        let tx_id = self.sign_and_broadcast(&signer.fee_signing_key()?, tx).await?;
        tracing::info!("funded {deposit_address} with {MIN_TRX_SUN_FOR_TRANSFER} sun from {fee_address} in {tx_id}");
        Ok(SweepOutcome::Funded { tx_id, amount_sun: MIN_TRX_SUN_FOR_TRANSFER })
    }

    /// Verify, sign and broadcast a transaction TronGrid built. Shared by the sweep and its funding
    /// so both get the txID recomputation — a node that returned a txID for different raw_data would
    /// otherwise walk away with a valid signature over a transaction nobody inspected.
    async fn sign_and_broadcast(&self, key: &SigningKey, tx: BuiltTx) -> Result<String, String> {
        let computed = hex::encode(Sha256::digest(hex::decode(&tx.raw_data_hex).map_err(|e| e.to_string())?));
        if computed != tx.tx_id {
            return Err(format!("txID mismatch: node said {} but raw_data hashes to {computed}", tx.tx_id));
        }

        let signature = sign_txid(key, &tx.tx_id)?;
        let broadcast = serde_json::json!({
            "txID": tx.tx_id,
            "raw_data_hex": tx.raw_data_hex,
            "signature": [signature],
            "visible": true,
        });

        let res = self.post("/wallet/broadcasttransaction", broadcast).await?;
        if res["result"].as_bool() != Some(true) {
            let code = res["code"].as_str().unwrap_or("");
            let msg = res["message"].as_str().unwrap_or("");
            let decoded = hex::decode(msg).ok().and_then(|b| String::from_utf8(b).ok()).unwrap_or_default();
            return Err(format!("broadcast rejected: {code} {decoded}"));
        }
        Ok(tx.tx_id)
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

        // Deliberately AFTER the balance check above: an address holding no USDT returns
        // NothingToSweep without moving a single sun. That ordering is what bounds a caller who can
        // reach this service — sweeping an arbitrary empty index costs nothing, so the TRX float
        // cannot be dispersed across addresses by asking for sweeps that were never owed.
        let trx = self.trx_balance_sun(&from).await?;
        if trx < MIN_TRX_SUN_FOR_TRANSFER {
            return self.fund(signer, &from).await;
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

        let tx_id = self.sign_and_broadcast(&signer.signing_key_at(index)?, tx).await?;
        Ok(SweepOutcome::Swept { tx_id, amount_usdt: amount })
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

// NOTE: the "is this address worth sweeping yet" decision deliberately does NOT live here.
//
// This service knows HOW to sweep; it has no idea WHEN. The threshold needs a balance, an age and
// the sweep bookkeeping, all of which live in treasury-service — and putting the decision here
// would mean linking the mnemonic-handling code into whatever else wanted to reason about it. See
// treasury-service's `sweeper.rs`.

#[cfg(test)]
mod funding_tests {
    use super::*;

    const FEE: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";
    const DEPOSIT: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";

    /// The fee account PAYS and the deposit address RECEIVES. Reversed, this asks an address that
    /// holds no TRX (that being the entire reason we are funding it) to fund the account that was
    /// supposed to fund it — which fails at broadcast complaining about a balance, pointing at the
    /// wrong account, on a path that only runs against a live chain.
    #[test]
    fn funding_pays_from_the_fee_account_to_the_deposit_address() {
        let body = funding_body(FEE, DEPOSIT, MIN_TRX_SUN_FOR_TRANSFER);
        assert_eq!(body["owner_address"], FEE, "the fee account pays");
        assert_eq!(body["to_address"], DEPOSIT, "the deposit address receives");
        assert_eq!(body["amount"], MIN_TRX_SUN_FOR_TRANSFER);
    }

    /// The fee account must be required to keep more than it sends. Funding is itself a transaction:
    /// an account holding exactly the send amount would broadcast a transfer it cannot pay the
    /// bandwidth for, and the failure would surface on some deposit address's sweep instead.
    #[test]
    fn the_fee_account_must_hold_more_than_it_sends() {
        assert!(FEE_ACCOUNT_RESERVE_SUN > 0, "a zero reserve lets the account drain to unusable");
    }
}
