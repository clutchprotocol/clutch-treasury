//! The user-facing off-ramp (Plan C T6): a thin proxy in front of the treasury's existing
//! `POST /internal/redemption-intents`. This module owns Tron address validation, the bounds
//! check, the HTTP call to the treasury, and the `(user_pk, treasury_intent_id)` mapping row —
//! `api.rs` only extracts the request and translates the outcome into a status code, same split
//! `api.rs` uses for the deposit routes (`addresses::address_for_user` owns that logic).
//!
//! **`redeemer_address` is never read from the request body.** It comes from the caller's
//! authenticated JWT `pk` (same class of requirement as treasury's own `created_by` on mint
//! intents — see `treasury-service/src/api.rs`'s doc comment on `create_mint_intent_handler`):
//! if a client could name the redeemer, it could burn against and redeem someone else's
//! balance, and the treasury has no way to detect the substitution since it only sees whatever
//! address this service sends it. `api.rs`'s handler enforces this by construction — the
//! request body type has no `redeemer_address` field at all, so there is nothing to ignore.
//!
//! ## Idempotency — deliberately not layered here
//!
//! ponytail: no client-key/Idempotency-Key dedup layer on this endpoint, unlike the deposit
//! path. A duplicate POST creates a second treasury redemption intent with its own
//! `redemption_ref`, but payout only happens against a matching ON-CHAIN BURN carrying that
//! specific ref (`watcher::confirm_burn`) — the user can only ever burn once, so at most one of
//! the two intents is ever fulfilled. The other sits `created` forever: litter, not a double
//! spend or a lost-money bug, so the complexity of a second CAS/lock layer here buys nothing.
//! An unfulfilled-intent sweep (alerting on `created` redemption intents past some age) is a
//! later concern, not this task's.

use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::configuration::OrchConfig;

/// Tron mainnet/Shasta address version byte (base58check payload's first byte).
const TRON_ADDRESS_VERSION: u8 = 0x41;
/// Version byte (1) + address bytes (20) — the length `bs58`'s `with_check` hands back once
/// the trailing 4-byte checksum is stripped. `bs58` verifies the checksum and the version byte
/// but does NOT itself enforce total length, so a checksum-valid string decoding to any other
/// length must still be rejected here.
const DECODED_LEN_WITH_VERSION: usize = 21;

/// Verifies both the base58check double-SHA256 checksum AND the `0x41` Tron version byte —
/// not a shape/regex check. A single mistyped character in a valid address changes the
/// decoded payload, which fails the checksum here (it does not merely fail a length/charset
/// check), which is exactly the case a `"T…", base58, 34 chars` shape check would let through.
pub fn is_valid_tron_address(address: &str) -> bool {
    match bs58::decode(address).with_check(Some(TRON_ADDRESS_VERSION)).into_vec() {
        Ok(bytes) => bytes.len() == DECODED_LEN_WITH_VERSION,
        Err(_) => false,
    }
}

#[derive(Debug)]
pub enum RedemptionOutcome {
    Created {
        id: Uuid,
        redemption_ref: String,
        /// What the caller must burn.
        amount_clt: i64,
        /// What they will be paid, after the treasury's redemption fee. Equal to `amount_clt`
        /// when no fee is configured. Carried separately because the caller has to see it BEFORE
        /// they burn — afterwards there is nothing they can do about it.
        payout_amount_usdt: i64,
        fee_usdt: i64,
        status: String,
    },
    InvalidAddress,
    OutOfBounds { min: i64, max: i64 },
    Disabled,
    TreasuryUnavailable,
    /// The treasury answered but refused the request (e.g. rejected the initiator role, or a
    /// shape mismatch) — distinct from `TreasuryUnavailable` for the same reason
    /// `treasury_bridge.rs`'s `FailureCause::Rejected` is: retrying an unchanged request against
    /// a treasury that is up and answering "no" will not succeed on a later attempt.
    TreasuryRejected,
    Failed(String),
}

#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct RedemptionMapRow {
    pub id: Uuid,
    pub user_pk: String,
    pub treasury_intent_id: Uuid,
    pub payout_tron_address: String,
    pub amount_clt: i64,
    pub redemption_ref: String,
    pub status: String,
}

/// Validate → bounds-check → POST to the treasury with the **initiator** token → store the
/// mapping row. `redeemer_address` is `user_pk`, taken from the caller — see module docs.
///
/// The treasury's response IS the mapping row's `status`/`redemption_ref` — captured once, at
/// creation. There is no `GET /internal/redemption-intents/:id` on the treasury side to refresh
/// this from later (only the `POST` exists; see `treasury-service/src/api.rs`'s router), so
/// `find_by_id`'s caller reads back this same stored snapshot rather than a live proxy. Flagged
/// in the task report — out of scope to add here per the brief (treasury changes excluded).
pub async fn create_redemption(
    pool: &PgPool,
    config: &OrchConfig,
    user_pk: &str,
    payout_tron_address: &str,
    amount_clt: i64,
) -> RedemptionOutcome {
    // The payout rail behind this is real now (see OrchConfig::redemptions_enabled); the flag
    // stays false only until its rollout — float funded, reconciliation verified — finishes.
    if !config.redemptions_enabled {
        return RedemptionOutcome::Disabled;
    }
    if !is_valid_tron_address(payout_tron_address) {
        return RedemptionOutcome::InvalidAddress;
    }
    if amount_clt < config.min_redemption_clt || amount_clt > config.max_redemption_clt {
        return RedemptionOutcome::OutOfBounds { min: config.min_redemption_clt, max: config.max_redemption_clt };
    }

    let resp = reqwest::Client::new()
        .post(format!("{}/internal/redemption-intents", config.treasury_url))
        .bearer_auth(&config.treasury_initiator_token)
        .json(&json!({
            "redeemer_address": user_pk,
            "payout_address": payout_tron_address,
            "amount_clt": amount_clt,
        }))
        .send()
        .await;

    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            tracing::error!("redemptions: treasury rejected create for user {user_pk}: {status} {text}");
            return RedemptionOutcome::TreasuryRejected;
        }
        Err(e) => {
            tracing::warn!("redemptions: treasury unreachable for user {user_pk}: {e}");
            return RedemptionOutcome::TreasuryUnavailable;
        }
    };

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return RedemptionOutcome::Failed(format!("treasury response unparseable: {e}")),
    };

    let treasury_intent_id = match body.get("id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok()) {
        Some(id) => id,
        None => return RedemptionOutcome::Failed(format!("treasury response had no parseable id: {body}")),
    };
    let redemption_ref = match body.get("redemption_ref").and_then(|v| v.as_str()) {
        Some(r) => r.to_string(),
        None => return RedemptionOutcome::Failed(format!("treasury response had no redemption_ref: {body}")),
    };
    let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("created").to_string();
    // Falls back to par rather than failing the request. A treasury too old to send these fields
    // is one that charges no fee, so par is its true answer — and refusing here would break
    // redemptions outright during the window where the two services deploy at different times.
    let payout_amount_usdt = body
        .get("payout_amount_usdt")
        .and_then(|v| v.as_i64())
        .unwrap_or(amount_clt);
    let fee_usdt = amount_clt - payout_amount_usdt;

    let id = Uuid::new_v4();
    let insert = sqlx::query(
        "INSERT INTO redemption_map
            (id, user_pk, treasury_intent_id, payout_tron_address, amount_clt, redemption_ref, status)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(user_pk)
    .bind(treasury_intent_id)
    .bind(payout_tron_address)
    .bind(amount_clt)
    .bind(&redemption_ref)
    .bind(&status)
    .execute(pool)
    .await;

    match insert {
        Ok(_) => RedemptionOutcome::Created {
            id,
            redemption_ref,
            amount_clt,
            payout_amount_usdt,
            fee_usdt,
            status,
        },
        Err(e) => RedemptionOutcome::Failed(format!("failed to store redemption mapping: {e}")),
    }
}

/// Live status from the treasury, or `None` if it couldn't be asked. Readonly token — reading a
/// status must never need the initiator credential.
///
/// Returns `None` rather than an error on every failure path, because the caller's fallback (the
/// stored creation-time status) is a better answer for the user than a 5xx: this read moves no
/// money. The caller surfaces which one it used so nobody mistakes "we couldn't ask" for "nothing
/// has happened yet".
/// What the treasury currently says about a redemption: its status, and the Tron transaction
/// that paid it if one has been broadcast. Both travel together because they are read in one
/// call and a caller showing `paid` without the receipt is the case this exists to fix.
pub struct TreasuryRedemption {
    pub status: String,
    pub payout_ref: Option<String>,
}

pub async fn fetch_treasury_status(
    config: &OrchConfig,
    treasury_intent_id: Uuid,
) -> Option<TreasuryRedemption> {
    let resp = reqwest::Client::new()
        .get(format!("{}/internal/redemption-intents/{treasury_intent_id}", config.treasury_url))
        .bearer_auth(&config.treasury_readonly_token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        tracing::warn!("redemptions: treasury status read for {treasury_intent_id} returned {}", resp.status());
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let status = body.get("status").and_then(|v| v.as_str())?.to_string();
    // Absent on every redemption that has not been paid yet, which is most of them — so a
    // missing field is normal here and must not discard the status we did get.
    let payout_ref = body.get("payout_ref").and_then(|v| v.as_str()).map(str::to_string);
    Some(TreasuryRedemption { status, payout_ref })
}

pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<RedemptionMapRow>, sqlx::Error> {
    sqlx::query_as::<_, RedemptionMapRow>(
        "SELECT id, user_pk, treasury_intent_id, payout_tron_address, amount_clt, redemption_ref, status
         FROM redemption_map WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A genuine Tron mainnet address (USDT-TRC20 contract address itself, widely published —
    /// a real base58check-valid `0x41`-version address, not a fixture invented for this test).
    const VALID_TRON_ADDRESS: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";

    #[test]
    fn accepts_a_genuinely_valid_address() {
        assert!(is_valid_tron_address(VALID_TRON_ADDRESS));
    }

    /// THE required case per the brief: a shape check ("T…", base58, 34 chars) passes this —
    /// same length, same alphabet, same leading character — but the checksum no longer matches
    /// because the payload bytes changed. Only a real base58check verification catches it.
    #[test]
    fn rejects_a_one_character_corruption_of_a_valid_address() {
        let mut corrupted: Vec<char> = VALID_TRON_ADDRESS.chars().collect();
        // Flip one character in the middle of the payload (not the checksum tail) to something
        // else in the base58 alphabet, keeping length and leading 'T' identical.
        let idx = 10;
        let original = corrupted[idx];
        corrupted[idx] = if original == 'a' { 'b' } else { 'a' };
        let corrupted: String = corrupted.into_iter().collect();

        assert_ne!(corrupted, VALID_TRON_ADDRESS, "test bug: corruption didn't change anything");
        assert_eq!(corrupted.len(), VALID_TRON_ADDRESS.len(), "shape check would still pass: same length");
        assert!(corrupted.starts_with('T'), "shape check would still pass: still starts with T");
        assert!(!is_valid_tron_address(&corrupted), "checksum must catch what a shape regex would miss");
    }

    #[test]
    fn rejects_wrong_version_byte() {
        // A syntactically valid base58check string (correct checksum) but for a DIFFERENT
        // version byte than Tron's 0x41 — e.g. a Bitcoin mainnet P2PKH address (version 0x00).
        assert!(!is_valid_tron_address("1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2"));
    }

    /// `bs58`'s `with_check(Some(0x41))` verifies the checksum AND the version byte, but NOT
    /// the total payload length — a base58check string with a genuinely valid checksum and the
    /// correct 0x41 version byte, but only 10 address bytes instead of Tron's 20, decodes
    /// successfully through `with_check` alone. Independently constructed and confirmed
    /// (version 0x41 + 10 zero-derived address bytes + real double-SHA256 checksum) to prove
    /// `DECODED_LEN_WITH_VERSION`'s length check is load-bearing, not redundant with the
    /// checksum/version check `bs58` already does.
    #[test]
    fn rejects_correct_checksum_and_version_but_wrong_payload_length() {
        assert!(!is_valid_tron_address("2pUoQw6qT8ob6Jjopz1Lg"));
    }

    #[test]
    fn rejects_non_base58_characters() {
        // '0', 'O', 'I', 'l' are excluded from the base58 alphabet.
        assert!(!is_valid_tron_address("T0OIl0000000000000000000000000000"));
    }

    #[test]
    fn rejects_empty_and_garbage_strings() {
        assert!(!is_valid_tron_address(""));
        assert!(!is_valid_tron_address("not-an-address"));
        assert!(!is_valid_tron_address("T"));
    }
}
