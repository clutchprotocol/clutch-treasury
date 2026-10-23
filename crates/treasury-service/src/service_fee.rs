//! The operator's service fee on top-ups (decided 2026-10-23: 1%, at least $0.50, never on a
//! user's first top-up).
//!
//! It is taken out of the CLT a verified deposit mints, never out of the USDT: the deposit's mint
//! is split into two intents, the user's and the operator's, that together mint exactly what the
//! deposit alone would have. The reserve, the ledger's liability and every cap see the same total,
//! so reconciliation needs nothing new. The operator's CLT is redeemable like anyone else's.
//!
//! Separate from the relay's fee on purpose. That one is the most GasFree may charge, held back so
//! the reserve always covers what is minted, and it moves with GasFree's prices. This one is a
//! price the operator sets, and it is off (0 bps) unless a deployment chooses one.

use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use clutch_chain::tx::intent_ref;

/// Who an intent made by this module says created it. The verifier approves it, so the four-eyes
/// CHECK holds, and an operator reading the table can tell it from a manual mint.
pub const CREATED_BY: &str = "service-fee";

/// The fee on a mint of `minted` CLT, in CLT: `bps` of it, at least `min`. Zero when the fee is off
/// or would take the whole mint, so a deposit always mints something for the user.
pub fn service_fee(minted: i64, bps: u32, min: i64) -> i64 {
    if bps == 0 || minted <= 0 {
        return 0;
    }
    let share = (minted as i128 * bps as i128 / 10_000) as i64;
    let fee = share.max(min);
    if fee >= minted {
        0
    } else {
        fee
    }
}

/// The fee as configured: `None` while it is off.
#[derive(Debug, Clone)]
pub struct Settings {
    pub bps: u32,
    pub min_clt: i64,
    /// Where the fee is minted: `0x` + 40 lowercase hex.
    pub address: String,
}

/// Split the service fee off the approved deposit intent `intent_id`, inside the caller's
/// transaction (the verifier's approval), so the split and the approval land together or not at
/// all. Returns the fee taken, 0 when none was.
///
/// No fee on the user's first verified deposit, nor on a deposit to the operator's own address.
pub async fn split_off(
    tx: &mut Transaction<'_, Postgres>,
    settings: &Settings,
    intent_id: Uuid,
) -> Result<i64, String> {
    let (beneficiary, amount_clt): (String, i64) =
        sqlx::query_as("SELECT beneficiary, amount_clt FROM mint_intents WHERE id = $1")
            .bind(intent_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| e.to_string())?;
    if beneficiary.to_ascii_lowercase() == settings.address {
        return Ok(0);
    }
    // Deposit-backed intents only (`client_ref`), and only ones that mint or minted: a rejected or
    // parked deposit is not a top-up the user has had.
    let (earlier,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM mint_intents
         WHERE lower(beneficiary) = lower($1) AND client_ref IS NOT NULL AND id <> $2
           AND status IN ('approved', 'submitted', 'credited')",
    )
    .bind(&beneficiary)
    .bind(intent_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| e.to_string())?;
    if earlier == 0 {
        return Ok(0);
    }
    let fee = service_fee(amount_clt, settings.bps, settings.min_clt);
    if fee == 0 {
        return Ok(0);
    }

    sqlx::query("UPDATE mint_intents SET amount_clt = amount_clt - $2, service_fee_clt = $2 WHERE id = $1")
        .bind(intent_id)
        .bind(fee)
        .execute(&mut **tx)
        .await
        .map_err(|e| e.to_string())?;
    let fee_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents
            (id, beneficiary, amount_clt, status, credit_ref, created_by, approved_by, verified_at, service_fee_for)
         VALUES ($1, $2, $3, 'approved', $4, $5, 'tron-verifier', now(), $6)",
    )
    .bind(fee_id)
    .bind(&settings.address)
    .bind(fee)
    .bind(intent_ref(&fee_id.to_string()))
    .bind(CREATED_BY)
    .bind(intent_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query("INSERT INTO chain_outbox (intent_id) VALUES ($1)")
        .bind(fee_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| e.to_string())?;
    Ok(fee)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USD: i64 = 1_000_000;

    #[test]
    fn off_takes_nothing() {
        assert_eq!(service_fee(50 * USD, 0, USD / 2), 0);
    }

    #[test]
    fn one_percent_with_a_fifty_cent_floor() {
        assert_eq!(service_fee(48 * USD, 100, USD / 2), USD / 2, "1% of $48 is $0.48, under the floor");
        assert_eq!(service_fee(98 * USD, 100, USD / 2), 980_000, "1% of $98");
        assert_eq!(service_fee(50 * USD, 100, USD / 2), USD / 2, "exactly 1%");
    }

    #[test]
    fn rounds_down_to_the_base_unit() {
        assert_eq!(service_fee(50_000_099, 100, 0), 500_000);
    }

    #[test]
    fn never_takes_the_whole_mint() {
        assert_eq!(service_fee(USD / 2, 100, USD / 2), 0);
        assert_eq!(service_fee(USD / 4, 100, USD / 2), 0);
        assert_eq!(service_fee(500_001, 100, USD / 2), USD / 2);
    }
}
