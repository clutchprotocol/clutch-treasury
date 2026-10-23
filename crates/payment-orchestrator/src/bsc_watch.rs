//! BEP-20 USDT top-ups on BNB Smart Chain, phase 1 (docs/superpowers/specs/2026-10-23-bsc-usdt-design.md):
//! a user pays USDT from their own wallet to one BSC pay-in address, and is credited by the SENDER.
//!
//! The argument is `direct.rs`'s, with one step fewer. A user's BSC address IS their CLT address: the
//! same key, the same 20 bytes, the same `0x` spelling. So a Transfer from `0xX` can only have been
//! signed by the holder of CLT address `0xX`'s key, and minting its value to `0xX` pays the person who
//! paid. The treasury checks the same thing again on its own node before it mints (`bsc_verifier.rs`).
//!
//! The same "ask first" rule as TronLink, for the same reason: an exchange withdrawal comes from the
//! exchange's wallet, whose CLT twin is the exchange's. A transfer is credited only when its sender
//! has a `bsc_payers` row and landed after it; anything else is recorded once in `bsc_unattributed`,
//! alerted, and left for a human. Nothing is swept: the money is already where it stays.
//!
//! Detection reads `Transfer(*, pay_in)` logs block range by block range, up to `confirmations` below
//! the head, so a shallow reorg never reaches the ledger. The cursor advances only after a whole range
//! was handled; a range read twice is harmless, because the transaction hash is the idempotency key.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::alerts::alert;

/// Below this, a transfer nobody claims is recorded without paging anyone (address-poisoning dust,
/// as in `direct.rs`). One USDT, in micro-USDT.
const ALERT_FROM_USDT: i64 = 1_000_000;

/// Blocks read per `eth_getLogs`. Public BSC nodes refuse wide ranges; at under a second a block this
/// is a few minutes of chain, so a quiet pay-in address catches up quickly.
const MAX_RANGE: u64 = 500;

/// Where a fresh watch starts below the safe head: enough to catch a transfer made while the service
/// was starting, small enough not to replay old history into unattributed alerts.
const INITIAL_LOOKBACK: u64 = 200;

/// The three reads the watcher needs, so tests can script a chain.
#[async_trait]
pub trait BscChain: Send + Sync {
    async fn head(&self) -> Result<u64, String>;
    async fn transfers_to(&self, contract: &str, to: &str, from_block: u64, to_block: u64) -> Result<Vec<bsc::Transfer>, String>;
    /// Seconds.
    async fn block_timestamp(&self, number: u64) -> Result<u64, String>;
}

#[async_trait]
impl BscChain for bsc::Client {
    async fn head(&self) -> Result<u64, String> {
        self.block_number().await
    }
    async fn transfers_to(&self, contract: &str, to: &str, from_block: u64, to_block: u64) -> Result<Vec<bsc::Transfer>, String> {
        bsc::Client::transfers_to(self, contract, to, from_block, to_block).await
    }
    async fn block_timestamp(&self, number: u64) -> Result<u64, String> {
        bsc::Client::block_timestamp(self, number).await
    }
}

/// Record that `user_pk` will pay from their own wallet, `address` (their CLT address). Idempotent: a
/// second call keeps the first row, and with it the first `created_at`.
pub async fn register_payer(pool: &PgPool, user_pk: &str, address: &str) -> Result<String, String> {
    let address = bsc::normalize_address(address).ok_or_else(|| format!("{address} is not a CLT address"))?;
    sqlx::query("INSERT INTO bsc_payers (user_pk, address) VALUES ($1, $2) ON CONFLICT (user_pk) DO NOTHING")
        .bind(user_pk)
        .bind(&address)
        .execute(pool)
        .await
        .map_err(|e| format!("registering the BSC payer {address}: {e}"))?;
    Ok(address)
}

/// One transaction's USDT to the pay-in address, by sender.
struct Payment {
    tx_hash: String,
    block_number: u64,
    from: String,
    /// Base units.
    value: u128,
}

/// The Transfers of a range, one entry per transaction. A transaction carrying USDT from two senders
/// (a contract batching payments) is returned with `from` empty: it cannot be credited to one person.
fn by_transaction(transfers: &[bsc::Transfer]) -> Vec<Payment> {
    let mut txs: BTreeMap<(u64, String), Payment> = BTreeMap::new();
    for t in transfers {
        let p = txs.entry((t.block_number, t.tx_hash.clone())).or_insert_with(|| Payment {
            tx_hash: t.tx_hash.clone(),
            block_number: t.block_number,
            from: t.from.clone(),
            value: 0,
        });
        if p.from != t.from {
            p.from.clear();
        }
        p.value = p.value.saturating_add(t.value);
    }
    txs.into_values().collect()
}

/// One pass: read the confirmed blocks since the cursor, at most `MAX_RANGE` of them, and credit or
/// report every payment in them.
pub async fn watch_once(pool: &PgPool, chain: &dyn BscChain, settings: &bsc::Settings) -> Result<(), String> {
    let head = chain.head().await?;
    let Some(safe) = head.checked_sub(settings.confirmations) else {
        return Ok(());
    };
    let last: Option<i64> = sqlx::query_scalar("SELECT last_block FROM bsc_watch WHERE pay_in = $1")
        .bind(&settings.pay_in_address)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("bsc_watch: {e}"))?;
    let from_block = match last {
        Some(n) => n as u64 + 1,
        None => safe.saturating_sub(INITIAL_LOOKBACK),
    };
    if from_block > safe {
        return Ok(());
    }
    let to_block = safe.min(from_block + MAX_RANGE - 1);

    let transfers = chain.transfers_to(&settings.usdt_contract, &settings.pay_in_address, from_block, to_block).await?;
    let mut timestamps: HashMap<u64, u64> = HashMap::new();
    for p in by_transaction(&transfers) {
        let seconds = match timestamps.get(&p.block_number) {
            Some(s) => *s,
            None => {
                let s = chain.block_timestamp(p.block_number).await?;
                timestamps.insert(p.block_number, s);
                s
            }
        };
        handle(pool, settings, &p, seconds).await?;
    }

    sqlx::query(
        "INSERT INTO bsc_watch (pay_in, last_block) VALUES ($1, $2)
         ON CONFLICT (pay_in) DO UPDATE SET last_block = EXCLUDED.last_block",
    )
    .bind(&settings.pay_in_address)
    .bind(to_block as i64)
    .execute(pool)
    .await
    .map_err(|e| format!("stamping bsc_watch: {e}"))?;
    Ok(())
}

async fn handle(pool: &PgPool, settings: &bsc::Settings, p: &Payment, block_seconds: u64) -> Result<(), String> {
    // Flooring: what is below a micro-USDT stays in the reserve (the treasury does the same).
    let amount = bsc::to_micro(p.value, settings.usdt_decimals).unwrap_or(0);
    if amount <= 0 || p.from == settings.pay_in_address {
        return Ok(());
    }
    let reported: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM bsc_unattributed WHERE tx_hash = $1)")
        .bind(&p.tx_hash)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("reading bsc_unattributed for {}: {e}", p.tx_hash))?;
    if reported {
        return Ok(());
    }
    if !p.from.is_empty() {
        let payer: Option<(String, chrono::DateTime<chrono::Utc>)> =
            sqlx::query_as("SELECT user_pk, created_at FROM bsc_payers WHERE address = $1")
                .bind(&p.from)
                .fetch_optional(pool)
                .await
                .map_err(|e| format!("looking up the BSC payer {}: {e}", p.from))?;
        if let Some((user_pk, since)) = payer {
            if block_seconds as i64 >= since.timestamp() {
                return credit(pool, &user_pk, p, amount, block_seconds).await;
            }
        }
    }
    report_unattributed(pool, settings, p, amount).await
}

/// A `confirmed` deposit, which `treasury_bridge` carries to the treasury like any other.
async fn credit(pool: &PgPool, user_pk: &str, p: &Payment, amount: i64, block_seconds: u64) -> Result<(), String> {
    let transfer_at = chrono::DateTime::from_timestamp(block_seconds as i64, 0).unwrap_or_else(chrono::Utc::now);
    let done = sqlx::query(
        "INSERT INTO deposit_intents
            (id, user_pk, clt_address, amount_usdt, amount_clt, status, client_key,
             tron_tx_id, received_usdt, expires_at, direct_from, transfer_at, network)
         VALUES ($1, $2, $3, $4, $4, 'confirmed', $5, $5, $4, now(), $3, $6, 'bsc')
         ON CONFLICT (tron_tx_id) WHERE tron_tx_id IS NOT NULL DO NOTHING",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(user_pk)
    .bind(&p.from)
    .bind(amount)
    .bind(&p.tx_hash)
    .bind(transfer_at)
    .execute(pool)
    .await
    .map_err(|e| format!("crediting BSC top-up {}: {e}", p.tx_hash))?;
    if done.rows_affected() == 1 {
        tracing::info!("bsc top-up {}: {amount} micro-USDT from {} credited", p.tx_hash, p.from);
    }
    Ok(())
}

async fn report_unattributed(pool: &PgPool, settings: &bsc::Settings, p: &Payment, amount: i64) -> Result<(), String> {
    let from = if p.from.is_empty() { "several senders" } else { p.from.as_str() };
    let inserted = sqlx::query(
        "INSERT INTO bsc_unattributed (tx_hash, from_address, amount_usdt) VALUES ($1, $2, $3)
         ON CONFLICT (tx_hash) DO NOTHING",
    )
    .bind(&p.tx_hash)
    .bind(from)
    .bind(amount)
    .execute(pool)
    .await
    .map_err(|e| format!("recording unattributed BSC transfer {}: {e}", p.tx_hash))?
    .rows_affected()
        == 1;
    if inserted && amount >= ALERT_FROM_USDT {
        alert(
            pool,
            "p1",
            "bsc_topup",
            &format!(
                "{amount} micro-USDT arrived at the BSC pay-in address {} in {} from {from}, a sender that did not ask \
                 for a BNB Chain top-up first (an exchange withdrawal, or a payment sent before pressing the button). \
                 Nothing was minted, and nothing will be: the USDT is in the reserve as surplus. Refund the sender \
                 by hand, from the pay-in address's own key.",
                settings.pay_in_address, p.tx_hash
            ),
        )
        .await;
    }
    Ok(())
}

/// Spawned once from `main.rs` while BSC top-ups are on; runs forever on the poll interval.
pub async fn run(pool: PgPool, settings: bsc::Settings, poll_interval_secs: u64) {
    let chain = bsc::Client::new(&settings.rpc_url);
    let mut interval = tokio::time::interval(Duration::from_secs(poll_interval_secs));
    loop {
        interval.tick().await;
        if let Err(e) = watch_once(&pool, &chain, &settings).await {
            tracing::warn!("bsc top-ups: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(tx: u8, block: u64, from: &str, value: u128) -> bsc::Transfer {
        bsc::Transfer {
            tx_hash: format!("0x{}", format!("{tx:02x}").repeat(32)),
            log_index: 0,
            block_number: block,
            contract: "0xc".into(),
            from: from.into(),
            to: "0xp".into(),
            value,
        }
    }

    #[test]
    fn one_payment_per_transaction_and_two_senders_are_nobodys() {
        let ps = by_transaction(&[t(1, 5, "0xa", 3), t(1, 5, "0xa", 4), t(2, 6, "0xa", 1), t(2, 6, "0xb", 1)]);
        assert_eq!(ps.len(), 2);
        assert_eq!((ps[0].from.as_str(), ps[0].value), ("0xa", 7));
        assert_eq!((ps[1].from.as_str(), ps[1].value), ("", 2));
    }
}
