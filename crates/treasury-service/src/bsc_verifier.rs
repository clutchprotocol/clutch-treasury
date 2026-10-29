//! The independent on-chain check of a BEP-20 USDT top-up, and the BSC part of the reserve
//! (docs/superpowers/specs/2026-10-23-bsc-usdt-design.md, phase 1).
//!
//! The orchestrator proposes; this approves, on evidence it reads itself, through its own node. What
//! decides the mint is all this service's own: the USDT contract and the pay-in address come from its
//! settings, never from the intent, and the sender must be the beneficiary.
//!
//! Since 2026-10-29 a BSC intent can also be a deposit at the user's own BSC address
//! (docs/superpowers/specs/2026-10-29-bsc-deposit-addresses-design.md). Then the sender does not
//! matter, and the address does: the signer must derive it at the intent's index, or no sweep could
//! ever move what backs the CLT. Which user an address belongs to is the orchestrator's record, as for
//! TRON deposit addresses. A BSC address and a CLT
//! address are the same 20 bytes in the same spelling, so a Transfer from `0xX` can only have been
//! signed by the holder of CLT address `0xX`'s key, and the orchestrator can at worst ask to pay
//! someone their own money back.
//!
//! The same two outcomes as `tron_verifier.rs`, kept apart for the same reason: a HARD mismatch is the
//! transaction itself saying it is not a top-up (reverted, another sender, no USDT to us, less than
//! proposed); a TRANSIENT one is our node or the chain not having caught up. Only the first rejects.

use uuid::Uuid;

use crate::configuration::AppConfig;
use crate::tron_verifier::{Evidence, Verdict};

/// The node the treasury reads BSC through, and whether it is on the configured chain.
pub struct Witness<'a> {
    settings: &'a bsc::Settings,
    client: bsc::Client,
    /// `Err` when the node could not be asked, or answered for another chain: then nothing on BSC is
    /// decided this pass.
    on_chain: Result<(), String>,
}

impl<'a> Witness<'a> {
    pub async fn new(settings: &'a bsc::Settings) -> Self {
        let client = bsc::Client::new(&settings.rpc_url);
        let on_chain = match client.chain_id().await {
            Ok(id) if id == settings.chain_id => Ok(()),
            Ok(id) => Err(format!("the BSC node is on chain {id}, not {}", settings.chain_id)),
            Err(e) => Err(e),
        };
        Self { settings, client, on_chain }
    }
}

/// One BSC intent, as the verifier reads it.
pub(crate) struct BscIntent<'a> {
    pub id: Uuid,
    pub beneficiary: &'a str,
    pub direct_from: Option<&'a str>,
    pub deposit_address: Option<&'a str>,
    pub derivation_index: Option<i64>,
    pub deposit_tx_id: Option<&'a str>,
    pub expected_amount_usdt: Option<i64>,
}

/// Where the USDT of a BSC intent must have gone, and from whom.
enum Route {
    /// From the beneficiary's own wallet to the pay-in address.
    Direct { from: String },
    /// From anyone to the user's own deposit address.
    Address { address: String },
}

/// The evidence for one BSC intent. `witness` is `None` while BSC top-ups are off here.
pub(crate) async fn evaluate(witness: Option<&Witness<'_>>, signer: &crate::sweeper::HttpSigner, intent: &BscIntent<'_>) -> Evidence {
    let intent_id = intent.id;
    let beneficiary = intent.beneficiary;
    let direct_from = intent.direct_from;
    // Off here: our settings, not evidence against the user's money, so never a rejection.
    let Some(w) = witness else {
        return Evidence::Transient(format!("intent {intent_id} is a BSC top-up, and BSC top-ups are off here"));
    };
    if let Err(e) = &w.on_chain {
        return Evidence::Transient(format!("BSC node: {e}"));
    }
    let Some(expected) = intent.expected_amount_usdt else {
        return Evidence::Transient(format!("intent {intent_id} is deposit-backed but has no expected_amount_usdt"));
    };
    let route = match (direct_from, intent.deposit_address) {
        (Some(_), _) => match (bsc::normalize_address(beneficiary), direct_from.and_then(bsc::normalize_address)) {
            (Some(b), Some(f)) if b == f => Route::Direct { from: f },
            _ => {
                return Evidence::HardMismatch(format!(
                    "BSC top-up sender {direct_from:?} is not the beneficiary {beneficiary}"
                ))
            }
        },
        (None, Some(address)) => {
            let (Some(address), Some(index)) = (bsc::normalize_address(address), intent.derivation_index) else {
                return Evidence::HardMismatch(format!("BSC deposit address {address:?} with index {:?} is unusable", intent.derivation_index));
            };
            // Asked where the keys are, not taken from the orchestrator: an address the signer does not
            // derive at this index is one no sweep could ever move.
            match signer.bsc_address(index).await {
                Ok(ours) if ours == address => Route::Address { address },
                Ok(ours) => {
                    return Evidence::HardMismatch(format!(
                        "BSC deposit address {address} is not the signer's address {ours} of index {index}"
                    ))
                }
                Err(e) => return Evidence::Transient(format!("asking the signer for the BSC address of index {index}: {e}")),
            }
        }
        (None, None) => return Evidence::HardMismatch(format!("intent {intent_id} is a BSC top-up with neither a sender nor an address")),
    };
    let deposit_tx_id = intent.deposit_tx_id;
    let Some(tx_hash) = deposit_tx_id.and_then(bsc::normalize_tx_hash) else {
        return Evidence::HardMismatch(format!("a BSC top-up names no BSC transaction ({deposit_tx_id:?})"));
    };

    let receipt = match w.client.receipt(&tx_hash).await {
        Ok(Some(r)) => r,
        // Unknown or not mined: a node behind, or a hash the orchestrator saw early. The stuck sweep
        // hands a hash that never appears to a human.
        Ok(None) => return Evidence::Transient(format!("BSC transaction {tx_hash} has no receipt yet")),
        Err(e) => return Evidence::Transient(e),
    };
    if !receipt.succeeded {
        return Evidence::HardMismatch(format!("BSC transaction {tx_hash} reverted"));
    }
    let head = match w.client.block_number().await {
        Ok(h) => h,
        Err(e) => return Evidence::Transient(e),
    };
    if head < receipt.block_number.saturating_add(w.settings.confirmations) {
        return Evidence::Transient("not yet confirmed".to_string());
    }

    let s = w.settings;
    let (sum, route_text) = match &route {
        Route::Direct { from } => {
            (bsc::sum_transfers(&receipt.transfers, &s.usdt_contract, from, &s.pay_in_address), format!("from {from} to the pay-in address {}", s.pay_in_address))
        }
        Route::Address { address } => (
            receipt
                .transfers
                .iter()
                .filter(|t| t.contract == s.usdt_contract && &t.to == address)
                .try_fold(0u128, |sum, t| sum.checked_add(t.value)),
            format!("to the deposit address {address}"),
        ),
    };
    let arrived = sum.and_then(|v| bsc::to_micro(v, s.usdt_decimals));
    match arrived {
        None => Evidence::HardMismatch(format!("BSC transaction {tx_hash}: the USDT it moved does not fit a CLT amount")),
        Some(0) => Evidence::HardMismatch(format!("BSC transaction {tx_hash} moved no USDT {route_text}")),
        Some(observed) if observed < expected => Evidence::HardMismatch(format!(
            "BSC transaction {tx_hash} moved {observed} micro-USDT {route_text}, below the {expected} proposed"
        )),
        Some(observed) => Evidence::Pass { tx_id: tx_hash, observed_amount_usdt: observed },
    }
}

/// What a verified BSC top-up may mint: what arrived, never more. Below the minimum deposit it is
/// held for a human, like a TRON top-up; from the pay-in address itself it is not a top-up at all.
pub(crate) fn verdict(config: &AppConfig, direct_from: Option<&str>, deposit_address: Option<&str>, observed_amount_usdt: i64) -> Verdict {
    let pay_in = config.bsc.as_ref().map(|s| s.pay_in_address.as_str());
    let from = direct_from.and_then(bsc::normalize_address);
    // A deposit at the user's own address has no sender to check: the address said whose it is.
    let at_address = direct_from.is_none() && deposit_address.is_some();
    if !at_address && (from.is_none() || from.as_deref() == pay_in) {
        return Verdict::Reject(format!("BSC top-up sender {direct_from:?} is the pay-in address itself"));
    }
    if let Some(min) = config.gasfree.as_ref().map(|g| g.min_deposit_usdt) {
        if observed_amount_usdt < min {
            return Verdict::Hold {
                cap: observed_amount_usdt,
                reason: format!(
                    "a BSC top-up of {observed_amount_usdt} micro-USDT is below the minimum deposit of {min}, so nothing \
                     was minted. The USDT is at the BSC pay-in address or the user's deposit address, counted in the reserve. Approving this intent \
                     (mint-intent-approve) mints at most what arrived."
                ),
            };
        }
    }
    Verdict::Approve { cap: Some(observed_amount_usdt) }
}

/// The USDT at the pay-in address and at every BSC deposit address in `unswept`, in micro-USDT
/// (each floored), for reconciliation. `Err` on any failure, a node on another chain included: a
/// partial reserve would read as a shortfall. `unswept` must hold each address once.
pub async fn reserve_micro(settings: &bsc::Settings, unswept: &[String]) -> Result<i64, String> {
    let client = bsc::Client::new(&settings.rpc_url);
    let chain = client.chain_id().await?;
    if chain != settings.chain_id {
        return Err(format!("the BSC node is on chain {chain}, not {}", settings.chain_id));
    }
    let mut total: i64 = 0;
    for address in std::iter::once(&settings.pay_in_address).chain(unswept.iter()) {
        let raw = client.balance_of(&settings.usdt_contract, address).await?;
        let micro = bsc::to_micro(raw, settings.usdt_decimals).ok_or_else(|| format!("BSC balance {raw} of {address} does not fit a CLT amount"))?;
        total = total.checked_add(micro).ok_or("the BSC reserve overflows")?;
    }
    Ok(total)
}
