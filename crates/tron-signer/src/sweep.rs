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
    /// The most one payout may move, in micro-USDT.
    ///
    /// Stateless and checked before anything else. NOT the same number as the treasury's
    /// `daily_payout_cap_clt`: that one is a rolling 24h total in CLT base units and lives where
    /// the database is. Configuring both from one value conflates two units that only happen to be
    /// equal at 1:1 par.
    pub per_tx_payout_cap_usdt: i64,
}

/// A payout cap of zero or less is a misconfiguration, not a limit: it refuses every payout while
/// looking like a working service, which presents as "redemptions all mysteriously fail" and costs
/// an investigation to trace back to one env var. Rejected at boot so the signer dies loudly with
/// the reason instead of stalling redemptions quietly.
pub fn validate_payout_cap(raw: &str) -> Result<i64, String> {
    let parsed: i64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("APP_PER_TX_PAYOUT_CAP_USDT must be an integer number of micro-USDT, got {raw:?}"))?;
    if parsed <= 0 {
        return Err(format!("APP_PER_TX_PAYOUT_CAP_USDT must be positive, got {parsed} — a non-positive cap refuses every payout"));
    }
    Ok(parsed)
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

/// What one payout attempt did. Mirrors `SweepOutcome`: the caller must be able to tell "refused,
/// nothing broadcast" apart from "broadcast", because the treasury retries the first and never
/// retries the second.
#[derive(Debug, PartialEq)]
pub enum PayoutOutcome {
    /// Broadcast accepted; `tx_id` is the on-chain transfer.
    Paid { tx_id: String },
    /// The float does not hold enough USDT. Proof that nothing was broadcast. Only an operator
    /// topping the float up resolves it.
    FloatDry { float_address: String, have_usdt: i64, need_usdt: i64 },
    /// Above `per_tx_payout_cap_usdt`. Proof that nothing was broadcast.
    CapExceeded { limit_usdt: i64 },
    /// The float was just sent TRX so it can pay for its own transfer. Not a failure and not yet a
    /// payout — the funding has to confirm first, so the next pass does the transfer.
    NeedsTrx { tx_id: String, amount_sun: i64 },
    /// Provably no broadcast was attempted: a key derivation failed, a TronGrid read (balance,
    /// building the transaction) never got a usable response, or the recipient address itself was
    /// bad — all before anything existed to sign. Safe to retry, unlike a 500: this is what a
    /// TronGrid `balanceOf` blip or a dry fee account (spec §3) actually are.
    ///
    /// Never used for anything at or after `sign_and_broadcast` is called — a failure there may
    /// have followed a real broadcast, so it stays a plain `Err` (500 on the wire, `Ambiguous` to
    /// the treasury) even though some of ITS internal failures (a txID mismatch, a node rejecting
    /// the broadcast) are, in principle, also provable non-broadcasts. Drawing the line at the
    /// call rather than inside it keeps that guarantee simple enough to trust.
    Refused(String),
}

/// The wire form of a payout outcome.
///
/// Separate from the handler so the status strings are testable without an HTTP rig. These
/// literals are a contract with treasury-service's `HttpPayoutSigner`: it matches on them exactly,
/// and anything it does not recognise it must treat as "may have broadcast" — so a typo here does
/// not fail loudly, it parks every redemption as ambiguous and waits for a human.
pub fn payout_response(outcome: &PayoutOutcome) -> serde_json::Value {
    match outcome {
        PayoutOutcome::Paid { tx_id } => serde_json::json!({"status": "paid", "tx_id": tx_id}),
        PayoutOutcome::CapExceeded { limit_usdt } => {
            serde_json::json!({"status": "cap_exceeded", "limit_usdt": limit_usdt})
        }
        PayoutOutcome::FloatDry { float_address, have_usdt, need_usdt } => serde_json::json!({
            "status": "float_dry",
            "float_address": float_address,
            "have_usdt": have_usdt,
            "need_usdt": need_usdt,
        }),
        PayoutOutcome::NeedsTrx { tx_id, amount_sun } => {
            serde_json::json!({"status": "needs_trx", "tx_id": tx_id, "amount_sun": amount_sun})
        }
        PayoutOutcome::Refused(reason) => serde_json::json!({"status": "refused", "reason": reason}),
    }
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

/// The body for a TRC-20 transfer out of the payout float.
///
/// Separate from the call so the argument order is testable. `owner_address` PAYS and is always the
/// float; `to` receives and is the only address the caller chose. Swapped, this would ask the
/// redeemer to pay us — which fails at broadcast for lack of a signature we do not hold, but only
/// after the transaction has been built and signed against the wrong account.
fn payout_body(
    from: &str,
    usdt_contract: &str,
    to: &str,
    amount_usdt: i64,
    fee_limit: i64,
) -> Result<serde_json::Value, String> {
    Ok(serde_json::json!({
        "owner_address": from,
        "contract_address": usdt_contract,
        "function_selector": "transfer(address,uint256)",
        // Propagated, not defaulted: an empty parameter is a transfer TronGrid will build and sign
        // against zero recipient bytes and a zero amount — a transaction that does nothing while
        // looking exactly like one that does. sweep() treats the same failure the same way.
        "parameter": transfer_parameter(to, amount_usdt)?,
        "fee_limit": fee_limit,
        "call_value": 0,
        "visible": true,
    }))
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

/// The body `/wallet/broadcasttransaction` expects: the transaction EXACTLY as the node built it,
/// with a signature added.
///
/// Rebuilding it from `txID` and `raw_data_hex` alone does not work, and this is how that failed:
/// the node rejected every broadcast with an empty code and an empty message, because what it
/// received was not a transaction it could parse. `raw_data` -- the structured object -- has to
/// survive the round trip. The first real sweep on stage died here.
fn signed_body(mut built: serde_json::Value, signature: &str) -> Result<serde_json::Value, String> {
    let obj = built.as_object_mut().ok_or("built transaction is not a JSON object")?;
    obj.insert("signature".into(), serde_json::json!([signature]));
    Ok(built)
}

/// Say something useful about a rejection even when the node says nothing useful.
///
/// Tron reports failures as `code` plus a hex-encoded `message`, but not always -- and when both
/// were missing this returned the bare string "broadcast rejected:  ", which names a failure and
/// gives nothing to act on. Falling back to the raw body means the next failure is at least
/// attributable.
fn describe_rejection(res: &serde_json::Value) -> String {
    let code = res["code"].as_str().unwrap_or("");
    let decoded = res["message"]
        .as_str()
        .and_then(|m| hex::decode(m).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();
    if code.is_empty() && decoded.is_empty() {
        let raw = res.to_string();
        let clipped: String = raw.chars().take(400).collect();
        return format!("node gave no code or message; raw response: {clipped}");
    }
    format!("{code} {decoded}").trim().to_string()
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

    /// Send `MIN_TRX_SUN_FOR_TRANSFER` to `target_address` from the fee account so it can pay for
    /// its own TRC-20 transfer.
    ///
    /// Two callers now: a deposit address about to be swept, and the payout float about to send a
    /// redemption. Both are addresses of ours that hold tokens but no TRX, and the fee account is
    /// the single TRX float behind both.
    ///
    /// Sends the full minimum rather than the shortfall. Topping up the difference would, for an
    /// address already near the floor, broadcast a transaction worth less than its own bandwidth.
    async fn fund(&self, signer: &Signer, target_address: &str) -> Result<SweepOutcome, String> {
        let fee_address = signer.fee_address()?;
        let have = self.trx_balance_sun(&fee_address).await?;
        let need = MIN_TRX_SUN_FOR_TRANSFER + FEE_ACCOUNT_RESERVE_SUN;
        if have < need {
            return Ok(SweepOutcome::FeeAccountDry { fee_address, have_sun: have, need_sun: need });
        }

        let built = self
            .post(
                "/wallet/createtransaction",
                funding_body(&fee_address, target_address, MIN_TRX_SUN_FOR_TRANSFER),
            )
            .await?;
        // createtransaction returns the transaction at the top level and reports refusals as
        // {"Error": ...} — checked explicitly so a rejection is not reported as a parse failure.
        if let Some(err) = built["Error"].as_str() {
            return Err(format!("trongrid refused to build the funding transfer: {err}"));
        }
        let tx_id = self.sign_and_broadcast(&signer.fee_signing_key()?, built).await?;
        tracing::info!("funded {target_address} with {MIN_TRX_SUN_FOR_TRANSFER} sun from {fee_address} in {tx_id}");
        Ok(SweepOutcome::Funded { tx_id, amount_sun: MIN_TRX_SUN_FOR_TRANSFER })
    }

    /// Verify, sign and broadcast a transaction TronGrid built. Shared by the sweep and its funding
    /// so both get the txID recomputation — a node that returned a txID for different raw_data would
    /// otherwise walk away with a valid signature over a transaction nobody inspected.
    async fn sign_and_broadcast(&self, key: &SigningKey, built: serde_json::Value) -> Result<String, String> {
        let tx_id = built["txID"].as_str().ok_or("built transaction has no txID")?.to_string();
        let raw_hex = built["raw_data_hex"].as_str().ok_or("built transaction has no raw_data_hex")?.to_string();

        // Recompute the id rather than trusting the node's: it is the thing being signed, and a
        // node that returned a txID for different raw_data would otherwise walk away with a valid
        // signature over a transaction nobody inspected.
        let computed = hex::encode(Sha256::digest(hex::decode(&raw_hex).map_err(|e| e.to_string())?));
        if computed != tx_id {
            return Err(format!("txID mismatch: node said {tx_id} but raw_data hashes to {computed}"));
        }

        let signature = sign_txid(key, &tx_id)?;
        let body = signed_body(built, &signature)?;

        let res = self.post("/wallet/broadcasttransaction", body).await?;
        if res["result"].as_bool() != Some(true) {
            return Err(format!("broadcast rejected: {}", describe_rejection(&res)));
        }
        Ok(tx_id)
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

        let built: serde_json::Value = self
            .post(
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
            .await?;
        // triggersmartcontract nests the transaction; createtransaction does not.
        let tx = built
            .get("transaction")
            .cloned()
            .ok_or_else(|| format!("trongrid returned no transaction to sign: {}", describe_rejection(&built)))?;

        let tx_id = self.sign_and_broadcast(&signer.signing_key_at(index)?, tx).await?;
        Ok(SweepOutcome::Swept { tx_id, amount_usdt: amount })
    }

    /// Send `amount_usdt` from the payout float to `to`.
    ///
    /// Unlike `sweep`, this DOES take a destination and an amount — see the spec at
    /// docs/superpowers/specs/2026-08-30-redemption-payout-rail-design.md. The property that
    /// survives is narrower but still real: the source is always the float and the token is always
    /// config, so the most a compromised caller moves is the float balance.
    pub async fn payout(
        &self,
        signer: &Signer,
        to: &str,
        amount_usdt: i64,
    ) -> Result<PayoutOutcome, String> {
        // FIRST, before any network call: a refusal must be provable as "nothing was broadcast",
        // and the cheapest proof is not having talked to anything yet.
        if amount_usdt > self.cfg.per_tx_payout_cap_usdt {
            return Ok(PayoutOutcome::CapExceeded { limit_usdt: self.cfg.per_tx_payout_cap_usdt });
        }

        // Beside the cap check and before ANY network call, for the same reason: a refusal must be
        // provably a non-broadcast. Without this, a non-positive amount still reaches the balance
        // reads and can trigger a real TRX funding broadcast before failing at transfer_parameter.
        if amount_usdt <= 0 {
            return Ok(PayoutOutcome::Refused(format!("payout amount must be positive, got {amount_usdt}")));
        }

        // Everything from here down to the call to `sign_and_broadcast` is provably pre-broadcast:
        // a key derivation or a read-only TronGrid call, never a transaction. A failure in that
        // stretch is `Refused`, not `Err` — see `PayoutOutcome::Refused`'s doc comment for why the
        // line is drawn at the call to `sign_and_broadcast` and not inside it.
        let from = match signer.payout_address() {
            Ok(a) => a,
            Err(e) => return Ok(PayoutOutcome::Refused(format!("could not derive the payout float address: {e}"))),
        };

        let have = match self.usdt_balance(&from).await {
            Ok(v) => v,
            Err(e) => return Ok(PayoutOutcome::Refused(format!("could not read the payout float's USDT balance: {e}"))),
        };
        if have < amount_usdt {
            return Ok(PayoutOutcome::FloatDry {
                float_address: from,
                have_usdt: have,
                need_usdt: amount_usdt,
            });
        }

        // Same ordering as sweep: balance first, TRX second. An underfunded float reports FloatDry
        // without dispersing TRX to an address that was never going to send anything.
        let trx = match self.trx_balance_sun(&from).await {
            Ok(v) => v,
            Err(e) => return Ok(PayoutOutcome::Refused(format!("could not read the payout float's TRX balance: {e}"))),
        };
        if trx < MIN_TRX_SUN_FOR_TRANSFER {
            return match self.fund(signer, &from).await {
                Ok(SweepOutcome::Funded { tx_id, amount_sun }) => Ok(PayoutOutcome::NeedsTrx { tx_id, amount_sun }),
                // Only a balance check ran before this outcome — no sign_and_broadcast call was
                // made for any transaction, so this is provably a non-broadcast (spec §3).
                Ok(SweepOutcome::FeeAccountDry { fee_address, have_sun, need_sun }) => {
                    Ok(PayoutOutcome::Refused(format!(
                        "payout float {from} has no TRX and the fee account {fee_address} is dry \
                         ({have_sun} sun, needs {need_sun}) — an operator must top it up"
                    )))
                }
                Ok(other) => Err(format!("funding the payout float returned {other:?}")),
                // fund() may already have reached its OWN sign_and_broadcast call for the TRX
                // top-up transfer before failing — conservatively ambiguous, same rule as below.
                Err(e) => Err(e),
            };
        }

        let body = match payout_body(&from, &self.cfg.usdt_contract, to, amount_usdt, self.cfg.fee_limit) {
            Ok(b) => b,
            Err(e) => return Ok(PayoutOutcome::Refused(format!("could not build the payout transfer: {e}"))),
        };
        let built: serde_json::Value = match self.post("/wallet/triggersmartcontract", body).await {
            Ok(v) => v,
            Err(e) => {
                return Ok(PayoutOutcome::Refused(format!("could not build the payout transfer via TronGrid: {e}")))
            }
        };
        let tx = match built.get("transaction").cloned() {
            Some(t) => t,
            None => {
                return Ok(PayoutOutcome::Refused(format!(
                    "trongrid returned no transaction to sign: {}",
                    describe_rejection(&built)
                )))
            }
        };
        let payout_key = match signer.payout_signing_key() {
            Ok(k) => k,
            Err(e) => return Ok(PayoutOutcome::Refused(format!("could not derive the payout signing key: {e}"))),
        };

        // Past this point a failure may follow a real broadcast attempt — genuinely ambiguous,
        // never Refused. This is the only `?` left in the function.
        let tx_id = self.sign_and_broadcast(&payout_key, tx).await?;
        Ok(PayoutOutcome::Paid { tx_id })
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

    const RECIPIENT: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";
    const USDT_FIXTURE: &str = "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf";
    const FLOAT_ADDR: &str = "TKTuTvBn4qZpeYFuXz1SuL1B94NgtK5EnT";

    fn payout_test_config(cap: i64) -> SweepConfig {
        SweepConfig {
            trongrid_url: "http://127.0.0.1:1".into(),
            trongrid_api_key: String::new(),
            treasury_address: "TQwgeRaDt4FSJSsncmFNcbMNTfFpjvjwFX".into(),
            usdt_contract: "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf".into(),
            fee_limit: 150_000_000,
            per_tx_payout_cap_usdt: cap,
        }
    }

    // keys.rs's own test modules each construct Signer::from_mnemonic(MNEMONIC, "") locally rather
    // than sharing a fixture helper; this crate has no such public helper, so this is the same
    // pattern, just factored out because payout tests need it twice.
    fn fixture_signer() -> Signer {
        Signer::from_mnemonic(MNEMONIC, "").unwrap()
    }

    #[test]
    fn a_payout_cap_of_zero_or_less_is_rejected_at_boot() {
        // A cap that resolves to 0 or negative would refuse every payout while the service looks up
        // and healthy. validate_payout_cap is what turns that into a boot-time panic instead of a
        // silent, hard-to-trace outage.
        assert!(validate_payout_cap("0").is_err(), "zero must be rejected");
        assert!(validate_payout_cap("-5").is_err(), "negative must be rejected");
        assert!(validate_payout_cap("not a number").is_err(), "non-numeric must be rejected");
        assert_eq!(validate_payout_cap("25000000").unwrap(), 25_000_000, "a valid cap must parse through");
    }

    #[tokio::test]
    async fn payout_above_the_cap_refuses_without_touching_the_network() {
        // trongrid_url points at a dead port: if the implementation reaches TronGrid before
        // checking the cap, this fails with a connection error instead of CapExceeded.
        let client = SweepClient::new(payout_test_config(1_000_000));
        let signer = fixture_signer();
        let outcome = client.payout(&signer, RECIPIENT, 1_000_001).await.unwrap();
        assert_eq!(outcome, PayoutOutcome::CapExceeded { limit_usdt: 1_000_000 });
    }

    /// A TronGrid failure while reading the float's OWN balance happens before anything exists to
    /// sign — provably no broadcast was attempted. Before this fix `payout()` propagated this as a
    /// plain `Err`, which the treasury client maps to `Ambiguous` and never retries: a dead
    /// TronGrid endpoint would permanently wedge every redemption instead of leaving it retryable.
    #[tokio::test]
    async fn a_dead_trongrid_before_any_broadcast_is_refused_not_ambiguous() {
        let client = SweepClient::new(payout_test_config(1_000_000)); // trongrid_url: dead port
        let signer = fixture_signer();
        let outcome = client.payout(&signer, RECIPIENT, 5).await.unwrap();
        assert!(matches!(outcome, PayoutOutcome::Refused(_)), "got {outcome:?}");
    }

    #[test]
    fn a_payout_always_spends_from_the_float_and_never_a_deposit() {
        // The security property of the whole endpoint, tested the way funding_body's argument order
        // is: as a pure body builder, because tron-signer has no dev-dependencies and no HTTP mock.
        // owner_address is the payer. If owner and recipient are ever swapped, or if owner is taken
        // from anything the caller supplied, this fails.
        let s = fixture_signer();
        let float = s.payout_address().unwrap();
        let body = payout_body(&float, USDT_FIXTURE, RECIPIENT, 5, 150_000_000).unwrap();

        assert_eq!(body["owner_address"], float, "the float must be the payer");
        assert_ne!(body["owner_address"], RECIPIENT, "the recipient must never be the payer");
        for i in 0..50u32 {
            assert_ne!(body["owner_address"], s.address_at(i).unwrap(),
                "a payout must never spend from deposit index {i}");
        }
        assert_eq!(body["contract_address"], USDT_FIXTURE, "the token comes from config, not the caller");
    }

    #[test]
    fn payout_body_propagates_a_bad_parameter_instead_of_defaulting() {
        // Guards the fix, not the helper: an empty transfer parameter is a transaction that looks
        // built and moves nothing. sweep() propagates here; so must this.
        let err = payout_body(FLOAT_ADDR, USDT_FIXTURE, "not-a-tron-address", 5, 150_000_000);
        assert!(err.is_err(), "an undecodable recipient must propagate, not default to empty");
    }

    #[test]
    fn every_payout_status_string_is_pinned() {
        // These five literals are the contract with treasury-service's HttpPayoutSigner. A typo
        // does not fail loudly on either side: the treasury treats an unrecognised status as
        // "may have broadcast" and parks the redemption for a human, forever.
        assert_eq!(payout_response(&PayoutOutcome::Paid { tx_id: "t".into() })["status"], "paid");
        assert_eq!(payout_response(&PayoutOutcome::CapExceeded { limit_usdt: 1 })["status"], "cap_exceeded");
        assert_eq!(
            payout_response(&PayoutOutcome::FloatDry { float_address: "a".into(), have_usdt: 0, need_usdt: 1 })["status"],
            "float_dry"
        );
        assert_eq!(
            payout_response(&PayoutOutcome::NeedsTrx { tx_id: "t".into(), amount_sun: 1 })["status"],
            "needs_trx"
        );
        assert_eq!(payout_response(&PayoutOutcome::Refused("x".into()))["status"], "refused");
    }

    #[test]
    fn a_paid_response_always_carries_its_tx_id() {
        // The treasury refuses to record a payout it cannot point at on chain, so a `paid` reply
        // without a tx_id becomes Ambiguous and parks the intent. Cheap to guarantee here.
        let v = payout_response(&PayoutOutcome::Paid { tx_id: "abc123".into() });
        assert_eq!(v["tx_id"], "abc123");
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

#[cfg(test)]
mod broadcast_tests {
    use super::*;

    /// What the node built must survive to the broadcast INTACT.
    ///
    /// This is the bug that killed the first real sweep on stage: the body was rebuilt from txID and
    /// raw_data_hex alone, `raw_data` was dropped, and the node rejected it with an empty code and
    /// an empty message. Nothing in the unit tests noticed, because nothing checked what got sent.
    #[test]
    fn the_broadcast_keeps_every_field_the_node_built() {
        let built = serde_json::json!({
            "txID": "aa".repeat(32),
            "raw_data": { "contract": [{"type": "TransferContract"}], "timestamp": 1 },
            "raw_data_hex": "0a02",
            "visible": true,
        });
        let body = signed_body(built.clone(), "sig").unwrap();

        assert_eq!(body["raw_data"], built["raw_data"], "raw_data must survive -- dropping it is the bug");
        assert_eq!(body["txID"], built["txID"]);
        assert_eq!(body["raw_data_hex"], built["raw_data_hex"]);
        assert_eq!(body["visible"], serde_json::json!(true), "visible decides how addresses are read");
        assert_eq!(body["signature"], serde_json::json!(["sig"]));
    }

    /// A rejection with no code and no message must still say something actionable. The bare
    /// "broadcast rejected:  " named a failure and gave nothing to act on.
    #[test]
    fn an_empty_rejection_still_reports_the_raw_response() {
        let res = serde_json::json!({"result": false});
        let d = describe_rejection(&res);
        assert!(d.contains("raw response"), "must fall back to the body, got: {d}");
        assert!(d.contains("result"), "the body itself must appear, got: {d}");
    }

    /// The normal case still decodes Tron's hex message.
    #[test]
    fn a_normal_rejection_decodes_the_hex_message() {
        let res = serde_json::json!({
            "result": false,
            "code": "SIGERROR",
            "message": hex::encode("validate signature error"),
        });
        let d = describe_rejection(&res);
        assert!(d.contains("SIGERROR"), "{d}");
        assert!(d.contains("validate signature error"), "{d}");
    }
}
