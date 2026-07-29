use std::sync::Arc;

use clutch_chain::node_client::NodeClient;
use sqlx::PgPool;
use uuid::Uuid;

/// Idempotent credit: the unique (intent_id, kind) index on treasury_events makes the
/// ledger write exactly-once; ON CONFLICT DO NOTHING absorbs watcher replays.
pub async fn credit_mint(
    pool: &PgPool,
    intent_id: Uuid,
    amount_clt: i64,
    tx_hash: &str,
) -> Result<(), String> {
    let mut db = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO treasury_events (kind, amount_clt, amount_usdt, intent_id, chain_tx_hash, description)
         VALUES ('mint_executed', $1, 0, $2, $3, 'mint confirmed on-chain')
         ON CONFLICT (intent_id, kind) WHERE intent_id IS NOT NULL DO NOTHING",
    )
    .bind(amount_clt)
    .bind(intent_id)
    .bind(tx_hash)
    .execute(&mut *db)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE mint_intents SET status = 'credited', updated_at = now() WHERE id = $1 AND status IN ('submitted','approved')")
        .bind(intent_id)
        .execute(&mut *db)
        .await
        .map_err(|e| e.to_string())?;
    // Backfill chain_tx_hash from the block: the outbox worker may have crashed between
    // send_raw_transaction and recording it, so this is the only reliable place it lands.
    sqlx::query("UPDATE mint_intents SET chain_tx_hash = $2 WHERE id = $1 AND chain_tx_hash IS NULL")
        .bind(intent_id)
        .bind(tx_hash)
        .execute(&mut *db)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE chain_outbox SET status = 'confirmed' WHERE intent_id = $1")
        .bind(intent_id)
        .execute(&mut *db)
        .await
        .map_err(|e| e.to_string())?;
    db.commit().await.map_err(|e| e.to_string())
}

/// Processes confirmed blocks since the cursor, crediting Mints by `credit_ref` — never by
/// tx hash (see module doc). Advances `chain_cursor` only after a block is fully processed,
/// so a crash mid-block simply reprocesses it next run; `credit_mint`'s idempotency makes
/// that safe.
///
/// Match-by-credit_ref, not hash, for two independent reasons: the node stores the wire hash
/// WITHOUT `0x` while `SignedTx.tx_hash` is 0x-prefixed (a naive comparison never matches),
/// and a crash between `send_raw_transaction` and recording `chain_tx_hash` leaves no hash to
/// match at all — `credit_ref` is derived from the intent id and is known before submission.
/// Pure range computation, split out so the fresh-chain underflow guard is unit-testable
/// without a DB or node connection. Returns `None` when there is nothing new to process.
fn process_range(cursor: u64, head: u64, confirmations: u64) -> Option<std::ops::RangeInclusive<u64>> {
    // saturating_sub: on a fresh chain head < confirmations, and the naive subtraction would
    // underflow (u64) into a bound near u64::MAX, sending the watcher off fetching billions
    // of nonexistent blocks.
    let bound = head.saturating_sub(confirmations);
    if bound <= cursor {
        None
    } else {
        Some((cursor + 1)..=bound)
    }
}

pub async fn poll_once(pool: &PgPool, node: &Arc<NodeClient>, confirmations: u64) -> Result<(), String> {
    let (cursor,): (i64,) =
        sqlx::query_as("SELECT last_processed_height FROM chain_cursor")
            .fetch_one(pool)
            .await
            .map_err(|e| e.to_string())?;
    let cursor = cursor as u64;

    let info = node.get_chain_info().await?;
    let head = info.latest_block_index;
    let Some(range) = process_range(cursor, head, confirmations) else {
        return Ok(());
    };

    for height in range {
        let block = node.get_block_by_index(height).await?;
        let txs = block.get("transactions").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        for tx in &txs {
            let function_call_type = tx.get("data").and_then(|d| d.get("function_call_type")).and_then(|v| v.as_str());
            if function_call_type != Some("Mint") {
                continue; // Burn txs handled in Task 7.
            }
            let credit_ref = tx
                .get("data")
                .and_then(|d| d.get("arguments"))
                .and_then(|a| a.get("credit_ref"))
                .and_then(|v| v.as_str());
            let Some(credit_ref) = credit_ref else { continue };
            let tx_hash = tx.get("hash").and_then(|v| v.as_str()).unwrap_or_default();

            let intent: Option<(Uuid, i64)> = sqlx::query_as(
                "SELECT id, amount_clt FROM mint_intents WHERE credit_ref = $1 AND status <> 'credited'",
            )
            .bind(credit_ref)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;

            if let Some((intent_id, amount_clt)) = intent {
                credit_mint(pool, intent_id, amount_clt, tx_hash).await?;
            }
        }

        sqlx::query("UPDATE chain_cursor SET last_processed_height = $1")
            .bind(height as i64)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::process_range;

    /// The exact scenario the brief calls out: a fresh chain has fewer blocks than the
    /// confirmation depth. head=1, confirmations=2 would naively underflow to bound
    /// ~u64::MAX; the guard must return None instead of a billion-block range.
    #[test]
    fn fresh_chain_shorter_than_confirmations_does_not_underflow() {
        assert_eq!(process_range(0, 1, 2), None);
        assert_eq!(process_range(0, 0, 2), None);
    }

    #[test]
    fn bound_equal_to_cursor_is_a_noop() {
        // Nothing new past what's already processed.
        assert_eq!(process_range(5, 7, 2), None);
    }

    #[test]
    fn processes_newly_confirmed_blocks_once() {
        // head=10, confirmations=2 -> confirmed up to 8; cursor at 5 -> process 6..=8.
        assert_eq!(process_range(5, 10, 2), Some(6..=8));
    }

    #[test]
    fn zero_confirmations_processes_up_to_head() {
        assert_eq!(process_range(0, 3, 0), Some(1..=3));
    }
}
