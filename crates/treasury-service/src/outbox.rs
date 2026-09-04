use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clutch_chain::node_client::NodeClient;
use clutch_chain::signer::ChainSigner;
use clutch_chain::tx::{build_raw_transaction, FunctionData};
use sqlx::PgPool;
use uuid::Uuid;

use crate::breakers;
use crate::configuration::AppConfig;
use crate::ledger::alert;

/// Node error substring for "this credit_ref already landed" — pinned against
/// clutch-node's own text (src/node/transactions/mint.rs, `ref_already_processed` branch
/// of `Mint::verify_state`): `"Mint rejected: credit_ref '{ref}' already processed (exactly-once)"`.
/// Matching on the stable middle substring, not the whole message, since the ref itself is
/// interpolated per-call.
const ALREADY_PROCESSED_SUBSTR: &str = "already processed";

const MAX_ATTEMPTS: i32 = 10;

struct OutboxRow {
    outbox_id: i64,
    intent_id: Uuid,
    attempts: i32,
    beneficiary: String,
    amount_clt: i64,
    credit_ref: String,
    client_ref: Option<String>,
}

/// Picks due `pending` outbox rows, re-checks breakers (approval alone is never
/// authorisation to mint — spec §7.2), signs, and submits. Returns the count processed.
/// Whether the "node is behind" alert has already fired for the current stale episode.
///
/// Process-local by design: this de-duplicates a live condition rather than recording anything, and
/// a restart re-alerting is correct — whoever restarted the service should be told the node is
/// still behind.
static STALE_ALERTED: AtomicBool = AtomicBool::new(false);

pub async fn drain_once(
    pool: &PgPool,
    node: &Arc<NodeClient>,
    peers: &[Arc<NodeClient>],
    signer: &dyn ChainSigner,
    config: &AppConfig,
) -> Result<u32, String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let rows: Vec<OutboxRow> = sqlx::query_as::<_, (i64, Uuid, i32, String, i64, String, Option<String>)>(
        "SELECT o.id, o.intent_id, o.attempts, i.beneficiary, i.amount_clt, i.credit_ref, i.client_ref
         FROM chain_outbox o
         JOIN mint_intents i ON i.id = o.intent_id
         WHERE o.status = 'pending' AND o.next_attempt_at <= now()
         ORDER BY o.id
         FOR UPDATE OF o SKIP LOCKED",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| e.to_string())?
    .into_iter()
    .map(|(outbox_id, intent_id, attempts, beneficiary, amount_clt, credit_ref, client_ref)| OutboxRow {
        outbox_id,
        intent_id,
        attempts,
        beneficiary,
        amount_clt,
        credit_ref,
        client_ref,
    })
    .collect();
    tx.commit().await.map_err(|e| e.to_string())?;

    // Is the node we are about to submit into actually at the tip?
    //
    // Checked once per pass, before any row: a node that has fallen behind accepts the mint and
    // reports success, and the transaction lands on a chain nobody else is following. On stage the
    // primary sat 115,000 blocks behind while the outbox recorded `submitted` with zero attempts
    // and no error. Parking is the right response, not failing -- the intent is fine, the node is
    // not, and attempts must not burn down toward permanent failure over someone else's sync.
    if !peers.is_empty() {
        match crate::chain_sync::check(node, peers, config.max_node_lag_blocks).await {
            crate::chain_sync::SyncState::Behind { lag, primary, best_peer } => {
                let reason = format!(
                    "node is {lag} blocks behind its peers (primary {primary}, best peer {best_peer}) \
                     — not submitting mints into a stale chain"
                );
                // One alert per stale EPISODE, not per pass. The outbox runs every 2 seconds, so
                // alerting each time buried the alerts table under hundreds of identical P1 rows
                // within minutes of this guard first firing — which is how a useful signal becomes
                // one people filter out. The flag resets when the node comes back in sync, so a
                // second episode alerts again.
                if !STALE_ALERTED.swap(true, Ordering::Relaxed) {
                    alert(pool, "p1", "outbox", &reason).await;
                } else {
                    tracing::warn!("{reason}");
                }
                for row in &rows {
                    park_row(pool, row.outbox_id, &reason).await?;
                }
                return Ok(0);
            }
            crate::chain_sync::SyncState::Unknown => {
                // No peer answered, so there is nothing to compare against. Proceeding is no worse
                // than the behaviour before this check existed, and blocking would stop minting
                // every time a peer restarts -- which, on a three-node stack, is every deploy.
                tracing::warn!("outbox: could not confirm the node is at the tip (no peer answered)");
            }
            crate::chain_sync::SyncState::InSync { .. } => {
                // Back in sync: re-arm the alert so the next episode is reported.
                if STALE_ALERTED.swap(false, Ordering::Relaxed) {
                    tracing::info!("outbox: node is back in sync with its peers");
                }
            }
        }
    }

    let mut processed = 0u32;
    for row in rows {
        // Authoritative gate: re-checked immediately before submission, not only at approval
        // time. Between approval and here the backing ratio can fall, reconciliation can go
        // stale, or the daily cap can fill. `_excluding` because this intent is already
        // `approved` and therefore already inside the daily-cap sum the plain check_mint reads.
        if let Err(denial) = breakers::check_mint_excluding(pool, config, row.amount_clt, row.intent_id).await {
            // `client_ref` is set only by the orchestrator, for an intent backed by a verified
            // on-chain deposit. That USDT is real and already at a derived address, so a cap
            // window being tight right now is not a reason to give up on it: park for retry with
            // attempts untouched. Failing it would strand the deposit — a `failed` row leaves the
            // reserve sum and the sweeper ignores it — which is exactly what happened to a 1,000
            // USDT deposit on 2026-09-03 while this read was still a placeholder `false`.
            if row.client_ref.is_some() {
                park_row(pool, row.outbox_id, &denial.reason).await?;
            } else {
                fail_or_backoff(pool, row.outbox_id, row.intent_id, row.attempts, &denial.reason).await?;
            }
            continue;
        }

        let nonce = match node.get_next_nonce(&signer.address()).await {
            Ok(n) => n,
            Err(e) => {
                fail_or_backoff(pool, row.outbox_id, row.intent_id, row.attempts, &e).await?;
                continue;
            }
        };

        let signed = match build_raw_transaction(
            signer,
            nonce,
            config.chain_id,
            &FunctionData::Mint {
                to: row.beneficiary.clone(),
                amount: row.amount_clt as u64,
                credit_ref: row.credit_ref.clone(),
            },
        ) {
            Ok(s) => s,
            Err(e) => {
                fail_or_backoff(pool, row.outbox_id, row.intent_id, row.attempts, &e).await?;
                continue;
            }
        };

        match node.send_raw_transaction(&signed.raw_hex).await {
            Ok(_) => {
                submit_ok(pool, row.outbox_id, row.intent_id, &signed.tx_hash).await?;
                processed += 1;
            }
            Err(e) if e.contains(ALREADY_PROCESSED_SUBSTR) => {
                // Not a failure: a previous attempt landed and we only lost the response
                // (e.g. a crash between send and recording chain_tx_hash). Leave `submitted`
                // with no hash — the watcher will credit it by credit_ref and backfill the
                // hash from the block. Retrying here would double-submit a nonce that's
                // already spent; marking it failed would abandon a mint that already happened.
                mark_submitted_no_hash(pool, row.outbox_id, row.intent_id).await?;
            }
            Err(e) => {
                fail_or_backoff(pool, row.outbox_id, row.intent_id, row.attempts, &e).await?;
            }
        }
    }

    Ok(processed)
}

async fn submit_ok(pool: &PgPool, outbox_id: i64, intent_id: Uuid, tx_hash: &str) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("UPDATE mint_intents SET status = 'submitted', chain_tx_hash = $2, updated_at = now() WHERE id = $1")
        .bind(intent_id)
        .bind(tx_hash)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE chain_outbox SET status = 'submitted', last_error = NULL WHERE id = $1")
        .bind(outbox_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

async fn mark_submitted_no_hash(pool: &PgPool, outbox_id: i64, intent_id: Uuid) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("UPDATE mint_intents SET status = 'submitted', updated_at = now() WHERE id = $1")
        .bind(intent_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE chain_outbox SET status = 'submitted', last_error = NULL WHERE id = $1")
        .bind(outbox_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// Deposit-backed denial: the cap window slides, so park without touching `attempts` —
/// this must never count toward the 10-attempt permanent-failure ceiling.
async fn park_row(pool: &PgPool, outbox_id: i64, reason: &str) -> Result<(), String> {
    sqlx::query(
        "UPDATE chain_outbox SET next_attempt_at = now() + interval '1 hour', last_error = $2 WHERE id = $1",
    )
    .bind(outbox_id)
    .bind(reason)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Ordinary failure path: attempts+1, exponential backoff capped at 10 minutes; the 10th
/// attempt fails the row permanently and pages P1.
async fn fail_or_backoff(
    pool: &PgPool,
    outbox_id: i64,
    intent_id: Uuid,
    prior_attempts: i32,
    reason: &str,
) -> Result<(), String> {
    let attempts = prior_attempts + 1;
    if attempts >= MAX_ATTEMPTS {
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        sqlx::query("UPDATE chain_outbox SET status = 'failed', attempts = $2, last_error = $3 WHERE id = $1")
            .bind(outbox_id)
            .bind(attempts)
            .bind(reason)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query("UPDATE mint_intents SET status = 'failed', updated_at = now() WHERE id = $1")
            .bind(intent_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        alert(pool, "p1", "outbox", &format!("mint intent {intent_id} failed after {attempts} attempts: {reason}")).await;
    } else {
        // 5s * 2^attempts, capped at 10 minutes.
        let backoff_secs = (5i64.saturating_mul(1i64 << attempts.min(20))).min(600);
        sqlx::query(
            "UPDATE chain_outbox
             SET attempts = $2, next_attempt_at = now() + make_interval(secs => $3), last_error = $4
             WHERE id = $1",
        )
        .bind(outbox_id)
        .bind(attempts)
        .bind(backoff_secs as f64)
        .bind(reason)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}
