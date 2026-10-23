//! TronLink direct top-ups: a user pays USDT from their own TRON address straight to the treasury's
//! pay-in address (the GasFree float), and is credited by the SENDER.
//!
//! # Why crediting by sender is sound here, when it was not for Bitcart
//!
//! `custody.rs` explains why Bitcart's sender matching could never work: payers were anonymous until
//! they paid. A user signed in to the app is not. Their CLT address `0xX` is 20 bytes of the keccak of
//! their public key, and a TRON address is the same 20 bytes behind a `0x41` version byte. So the TRON
//! address `tron(X)` is controlled by exactly the key that controls CLT address `X`, and a transfer
//! FROM `tron(X)` can only have been signed by that key's holder. Minting its value to `X` pays the
//! person who paid. No signature beyond the login is needed, and the treasury's verifier re-derives the
//! same sender from the beneficiary on its own before it approves (`tron_verifier.rs` there).
//!
//! # Why a user must ask first
//!
//! The key argument says who owns a sending address; it says nothing about who MEANT to top up. An
//! exchange withdrawal is sent from the exchange's hot wallet, whose CLT twin belongs to the exchange.
//! So a transfer is credited only when its sender has a `direct_payers` row, which exists only once
//! the user pressed the button in the app, and only for transfers that landed after that. Anything
//! else that reaches the pay-in address is recorded once in `direct_unattributed`, alerted, and left
//! for a human to refund: never credited later, so a refund cannot be followed by a mint.
//!
//! Sweeps from per-user deposit addresses land at the same address. Their senders are our own
//! addresses, which are skipped without an alert.

use std::time::Duration;

use sqlx::PgPool;

use crate::alerts::alert;
use crate::custody::{CustodyWatcher, ObservedTransfer};

/// Tron address version byte.
const TRON_ADDRESS_VERSION: u8 = 0x41;

/// Below this, a transfer nobody claims is recorded without an alert. Address-poisoning scams send
/// fractions of a cent to every busy address; paging a human for each one would bury the real case,
/// a user's money with no credit. One USDT, in micro-USDT.
const ALERT_FROM_USDT: i64 = 1_000_000;

/// How far before the watermark each read starts, as in `poller::TieredPoller`: a transfer landing
/// between the read and the stamp is seen again next pass, and crediting is idempotent on the tx id.
const OVERLAP_HOURS: i64 = 1;

/// `0x` + 40 hex (any case) in, the TRON address of the same 20 bytes out. `None` for anything else.
pub fn tron_address_of(clt_address: &str) -> Option<String> {
    let hex = clt_address.strip_prefix("0x").or_else(|| clt_address.strip_prefix("0X")).unwrap_or(clt_address);
    if hex.len() != 40 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bytes = hex::decode(hex).ok()?;
    let mut payload = Vec::with_capacity(21);
    payload.push(TRON_ADDRESS_VERSION);
    payload.extend_from_slice(&bytes);
    Some(bs58::encode(payload).with_check().into_string())
}

/// Record that `user_pk` will pay from their own TRON address, and return that address. Idempotent:
/// a second call keeps the first row, and with it the first `created_at`.
pub async fn register_payer(pool: &PgPool, user_pk: &str, clt_address: &str) -> Result<String, String> {
    let tron_address = tron_address_of(clt_address).ok_or_else(|| format!("{clt_address} is not a CLT address"))?;
    sqlx::query(
        "INSERT INTO direct_payers (user_pk, tron_address, clt_address) VALUES ($1, $2, $3)
         ON CONFLICT (user_pk) DO NOTHING",
    )
    .bind(user_pk)
    .bind(&tron_address)
    .bind(clt_address)
    .execute(pool)
    .await
    .map_err(|e| format!("registering the direct payer {tron_address}: {e}"))?;
    Ok(tron_address)
}

/// The payer row for a sending address: who to credit, and since when.
async fn payer_for(pool: &PgPool, from: &str) -> Result<Option<(String, String, chrono::DateTime<chrono::Utc>)>, String> {
    sqlx::query_as("SELECT user_pk, clt_address, created_at FROM direct_payers WHERE tron_address = $1")
        .bind(from)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("looking up the direct payer {from}: {e}"))
}

/// Whether `address` is one of our own deposit addresses: a sweep from it is not a top-up.
async fn is_our_deposit_address(pool: &PgPool, address: &str) -> Result<bool, String> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM deposit_addresses WHERE address = $1)
             OR EXISTS (SELECT 1 FROM deposit_intents WHERE deposit_address = $1)",
    )
    .bind(address)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("checking whether {address} is a deposit address: {e}"))
}

/// Whether this transfer was already reported as unattributed. It then stays a human's to settle.
async fn is_unattributed(pool: &PgPool, tx_id: &str) -> Result<bool, String> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM direct_unattributed WHERE tron_tx_id = $1)")
        .bind(tx_id)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("reading direct_unattributed for {tx_id}: {e}"))
}

/// Record a direct top-up as a `confirmed` deposit, which `treasury_bridge` then carries to the
/// treasury like any other. `Ok(false)` when the transaction was already recorded.
async fn credit(pool: &PgPool, user_pk: &str, clt_address: &str, t: &ObservedTransfer) -> Result<bool, String> {
    let transfer_at = chrono::DateTime::from_timestamp_millis(t.block_timestamp).unwrap_or_else(chrono::Utc::now);
    // No deposit address and no derivation index: there is nothing to sweep. The tx id is the
    // idempotency key, as in `poller::credit_transfer`.
    let done = sqlx::query(
        "INSERT INTO deposit_intents
            (id, user_pk, clt_address, amount_usdt, amount_clt, status, client_key,
             tron_tx_id, received_usdt, expires_at, direct_from, transfer_at)
         VALUES ($1, $2, $3, $4, $4, 'confirmed', $5, $5, $4, now(), $6, $7)
         ON CONFLICT (tron_tx_id) WHERE tron_tx_id IS NOT NULL DO NOTHING",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(user_pk)
    .bind(clt_address)
    .bind(t.amount_usdt)
    .bind(&t.tx_id)
    .bind(&t.from)
    .bind(transfer_at)
    .execute(pool)
    .await
    .map_err(|e| format!("crediting direct top-up {}: {e}", t.tx_id))?;
    Ok(done.rows_affected() == 1)
}

/// Record a transfer nobody claims, and alert once if it is more than dust.
async fn report_unattributed(pool: &PgPool, pay_to: &str, t: &ObservedTransfer) -> Result<(), String> {
    let inserted = sqlx::query(
        "INSERT INTO direct_unattributed (tron_tx_id, from_address, amount_usdt) VALUES ($1, $2, $3)
         ON CONFLICT (tron_tx_id) DO NOTHING",
    )
    .bind(&t.tx_id)
    .bind(&t.from)
    .bind(t.amount_usdt)
    .execute(pool)
    .await
    .map_err(|e| format!("recording unattributed transfer {}: {e}", t.tx_id))?
    .rows_affected()
        == 1;
    if inserted && t.amount_usdt >= ALERT_FROM_USDT {
        alert(
            pool,
            "p1",
            "direct_topup",
            &format!(
                "{} micro-USDT arrived at the pay-in address {pay_to} in {} from {}, a sender that did not ask for a \
                 direct top-up first (an exchange withdrawal, or a payment sent before pressing the button). Nothing \
                 was minted, and nothing will be: the USDT is in the reserve as surplus. If it was an operator \
                 top-up of the float, nothing is owed. Otherwise refund the sender by hand.",
                t.amount_usdt, t.tx_id, t.from
            ),
        )
        .await;
    }
    Ok(())
}

/// One pass over the pay-in address.
pub async fn credit_direct(
    pool: &PgPool,
    watcher: &dyn CustodyWatcher,
    pay_to: &str,
    usdt_contract: &str,
) -> Result<(), String> {
    sqlx::query("INSERT INTO direct_watch (pay_to) VALUES ($1) ON CONFLICT (pay_to) DO NOTHING")
        .bind(pay_to)
        .execute(pool)
        .await
        .map_err(|e| format!("direct_watch: {e}"))?;
    let watermark: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_polled_at FROM direct_watch WHERE pay_to = $1")
            .bind(pay_to)
            .fetch_one(pool)
            .await
            .map_err(|e| format!("direct_watch: {e}"))?;
    let since = watermark.map(|t| (t - chrono::Duration::hours(OVERLAP_HOURS)).timestamp_millis());
    let read_at = chrono::Utc::now();

    // An unread chain advances nothing, as in `TieredPoller`.
    let transfers = watcher.transfers_to(pay_to, since).await?;

    for t in &transfers {
        // Re-checked here for the reason `poller::credit_from_addresses` gives.
        if t.contract != usdt_contract || t.to != pay_to || t.amount_usdt <= 0 || t.from.is_empty() {
            continue;
        }
        if let Err(e) = handle(pool, pay_to, t).await {
            tracing::error!("direct top-up {}: {e}", t.tx_id);
        }
    }

    sqlx::query("UPDATE direct_watch SET last_polled_at = $2 WHERE pay_to = $1")
        .bind(pay_to)
        .bind(read_at)
        .execute(pool)
        .await
        .map_err(|e| format!("stamping direct_watch: {e}"))?;
    Ok(())
}

async fn handle(pool: &PgPool, pay_to: &str, t: &ObservedTransfer) -> Result<(), String> {
    if is_unattributed(pool, &t.tx_id).await? {
        return Ok(());
    }
    if let Some((user_pk, clt_address, since)) = payer_for(pool, &t.from).await? {
        if t.block_timestamp >= since.timestamp_millis() {
            if credit(pool, &user_pk, &clt_address, t).await? {
                tracing::info!("direct top-up {}: {} micro-USDT from {} credited", t.tx_id, t.amount_usdt, t.from);
            }
            return Ok(());
        }
    }
    if is_our_deposit_address(pool, &t.from).await? {
        return Ok(()); // a sweep
    }
    report_unattributed(pool, pay_to, t).await
}

/// Spawned once from `main.rs` when `direct_topup_address` is set; runs forever on the poll interval.
pub async fn run(
    pool: PgPool,
    watcher: std::sync::Arc<dyn CustodyWatcher>,
    pay_to: String,
    usdt_contract: String,
    poll_interval_secs: u64,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(poll_interval_secs));
    loop {
        interval.tick().await;
        if let Err(e) = credit_direct(&pool, watcher.as_ref(), &pay_to, &usdt_contract).await {
            tracing::warn!("direct top-ups: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two addresses known from outside this code: TRON's all-zero "black hole" address, and the
    /// all-"abandon" mnemonic's index-0 address that `derive.rs` pins (a real mainnet account), whose
    /// 20 bytes were read off it with an independent base58 decoder.
    #[test]
    fn the_same_20_bytes_give_the_tron_address() {
        assert_eq!(
            tron_address_of("0x0000000000000000000000000000000000000000").as_deref(),
            Some("T9yD14Nj9j7xAB4dbGeiX9h8unkKHxuWwb")
        );
        assert_eq!(
            tron_address_of("0xc8599111f29c1e1e061265b4af93ea1f274ad78a").as_deref(),
            Some("TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH")
        );
    }

    #[test]
    fn case_and_prefix_do_not_matter() {
        let a = tron_address_of("0xABCDEFabcdef0123456789abcdef0123456789ab");
        assert!(a.is_some());
        assert_eq!(a, tron_address_of("abcdefabcdef0123456789abcdef0123456789ab"));
        assert_eq!(a, tron_address_of("0Xabcdefabcdef0123456789abcdef0123456789AB"));
    }

    #[test]
    fn anything_but_20_bytes_of_hex_is_refused() {
        assert_eq!(tron_address_of("0x1234"), None);
        assert_eq!(tron_address_of("0xzz00000000000000000000000000000000000001"), None);
        assert_eq!(tron_address_of(""), None);
    }
}
