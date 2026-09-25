//! Sweeping deposits out of GasFree accounts (spec §3), and deciding from the chain when a sweep ran
//! (spec §5).
//!
//! A GasFree sweep ends as a permit with the relay, not a finished transfer. The account is swept
//! when the controller's `nonces(owner)` moves past the permit's nonce. The account's balance never
//! says it: the relay's unused margin stays there, so it never falls to zero.

use sqlx::PgPool;

use super::{SignerReply, SweepSigner};
use crate::configuration::AppConfig;
use crate::ledger::{alert, alert_once};
use crate::tron_verifier::TronClient;

/// After a permit's deadline the controller refuses it, but a block at the deadline may take this
/// long to show in what TronGrid answers.
const DEADLINE_GRACE_SECS: i64 = 60;

/// How often one unchanged condition pages again.
fn hourly() -> chrono::Duration {
    chrono::Duration::hours(1)
}

/// The tripwire, read once per pass (spec §5). `false`: no GasFree sweep is asked for this pass.
///
/// The signer refuses to sign after a change anyway. What this adds is a human being told.
pub(super) async fn code_unchanged(pool: &PgPool, settings: &gasfree::Settings, client: &TronClient) -> bool {
    match crate::gasfree_rail::code_changed(client, settings).await {
        Ok(None) => true,
        Ok(Some(reason)) => {
            alert_once(
                pool,
                "p1",
                "sweeper",
                &format!(
                    "{reason}. GasFree sweeps have stopped until someone reviews the new code and updates the \
                     setting. Money already in GasFree accounts is exposed either way; no more should go in."
                ),
                hourly(),
            )
            .await;
            false
        }
        Err(e) => {
            tracing::warn!("sweeper: GasFree's implementation() is unreadable: {e}");
            alert_once(
                pool,
                "warn",
                "sweeper",
                "GasFree's implementation() could not be read; GasFree sweeps wait until it can be",
                hourly(),
            )
            .await;
            false
        }
    }
}

/// Every permit in flight: did it run, and can it still run?
pub(super) async fn settle(pool: &PgPool, settings: &gasfree::Settings, client: &TronClient, signer: &dyn SweepSigner) {
    let pending: Vec<(i64, String, String, String, i64, i64, i64, chrono::DateTime<chrono::Utc>)> = match sqlx::query_as(
        "SELECT derivation_index, gasfree_address, owner_address, pending_trace_id, pending_nonce,
                pending_deadline, pending_value_usdt, pending_requested_at
         FROM gasfree_accounts WHERE pending_nonce IS NOT NULL
         ORDER BY derivation_index",
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("sweeper: could not list GasFree permits in flight: {e}");
            return;
        }
    };

    for (index, account, owner, trace_id, nonce, deadline, value_usdt, requested_at) in pending {
        let chain_nonce = match client.gasfree_nonce(settings.chain.controller, &owner).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("sweeper: nonces({owner}) is unreadable, so the permit for {account} waits: {e}");
                continue;
            }
        };

        if chain_nonce > nonce as u64 {
            // It ran: the controller moves past a nonce only by running a permit that carries it.
            // Every deposit verified before the permit was asked for was in the balance it moved.
            let swept: Vec<i64> = match sqlx::query_scalar(
                "UPDATE mint_intents SET swept_at = now()
                 WHERE derivation_index = $1 AND deposit_address = $2 AND swept_at IS NULL
                   AND verified_at <= $3
                   AND status IN ('approved', 'submitted', 'credited', 'needs_manual')
                 RETURNING amount_clt",
            )
            .bind(index)
            .bind(&account)
            .bind(requested_at)
            .fetch_all(pool)
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    alert(pool, "p1", "sweeper", &format!("the permit for {account} ran, but recording its deposits as swept failed: {e}. The next pass retries.")).await;
                    continue;
                }
            };
            // From here on a deposit at this account holds back one transfer fee (spec §2). The record
            // is the treasury's own: this service asked for the permit that ran.
            if let Err(e) = sqlx::query(
                "UPDATE gasfree_accounts
                    SET first_transfer_at = COALESCE(first_transfer_at, now()),
                        pending_trace_id = NULL, pending_nonce = NULL, pending_deadline = NULL,
                        pending_value_usdt = NULL, pending_requested_at = NULL
                  WHERE derivation_index = $1 AND pending_nonce = $2",
            )
            .bind(index)
            .bind(nonce)
            .execute(pool)
            .await
            {
                alert(pool, "p1", "sweeper", &format!("the permit for {account} ran, but clearing it failed: {e}. The next pass retries.")).await;
                continue;
            }
            tracing::info!(
                "sweeper: GasFree account {account} (index {index}) swept by permit {trace_id}; {} deposit(s) recorded",
                swept.len()
            );

            // Open question 1 of the design, checked on every sweep: the receiver must get exactly
            // `value`, with the relay's fee on top of it.
            match signer.trace(&trace_id).await {
                Ok(t) => {
                    if let Some(got) = t.txn_amount.filter(|got| *got != value_usdt) {
                        alert(
                            pool,
                            "p1",
                            "sweeper",
                            &format!(
                                "the relay says {got} micro-USDT of the sweep of {account} reached the receiver; the \
                                 permit said {value_usdt}. If the fee came out of the value, every GasFree mint is too \
                                 large by the fee: stop GasFree deposits and read open question 1 of the GasFree design."
                            ),
                        )
                        .await;
                    }
                }
                Err(e) => tracing::warn!("sweeper: the relay's record of permit {trace_id} is unreadable: {e}"),
            }
        } else if chrono::Utc::now().timestamp() > deadline + DEADLINE_GRACE_SECS {
            // It can no longer run, and it did not: a fresh permit on this pass or a later one.
            match sqlx::query(
                "UPDATE gasfree_accounts
                    SET pending_trace_id = NULL, pending_nonce = NULL, pending_deadline = NULL,
                        pending_value_usdt = NULL, pending_requested_at = NULL
                  WHERE derivation_index = $1 AND pending_nonce = $2",
            )
            .bind(index)
            .bind(nonce)
            .execute(pool)
            .await
            {
                Ok(_) => tracing::info!("sweeper: the permit for {account} (index {index}) expired without running; a fresh one follows"),
                Err(e) => tracing::error!("sweeper: could not clear the expired permit for {account}: {e}"),
            }
        }
    }
}

/// One GasFree account holding credited deposits: ask for a permit if one could move anything.
pub(super) async fn sweep_account(
    pool: &PgPool,
    config: &AppConfig,
    settings: &gasfree::Settings,
    client: &TronClient,
    signer: &dyn SweepSigner,
    index: i64,
    address: &str,
) {
    let account: Option<(bool, bool)> = match sqlx::query_as(
        "SELECT first_transfer_at IS NOT NULL, pending_nonce IS NOT NULL FROM gasfree_accounts
         WHERE derivation_index = $1 AND gasfree_address = $2",
    )
    .bind(index)
    .bind(address)
    .fetch_optional(pool)
    .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("sweeper: could not read GasFree account {address}: {e}");
            return;
        }
    };
    let Some((seen_first_transfer, in_flight)) = account else {
        // The verifier records every GasFree account it approves a deposit at, so this deposit's
        // account was never recorded, and nothing here can size its fee.
        alert_once(
            pool,
            "warn",
            "sweeper",
            &format!("GasFree account {address} (index {index}) holds credited deposits but is not in gasfree_accounts, so it is not swept"),
            hourly(),
        )
        .await;
        return;
    };
    // One permit at a time (spec §3): a second would carry the same nonce.
    if in_flight {
        return;
    }

    let balance = match client.get_custody_balance(address, &config.usdt_contract).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("sweeper: balance read failed for {address}: {e}");
            return;
        }
    };
    // No more than the relay may take: a sweep would move nothing. The line the signer draws too,
    // here on the treasury's own record, so activation counts until its own first sweep ran.
    let fee = gasfree::fee_to_hold(seen_first_transfer, settings.activate_fee_max_usdt, settings.transfer_fee_max_usdt);
    if balance <= fee {
        return;
    }

    let requested_at = chrono::Utc::now();
    match signer.sweep(index).await {
        SignerReply::Pending { trace_id, gasfree_address, value_usdt, max_fee_usdt, nonce, deadline } => {
            if gasfree_address != address {
                alert(
                    pool,
                    "p1",
                    "sweeper",
                    &format!(
                        "asked to sweep index {index}, the signer sent a permit for GasFree account {gasfree_address}; the \
                         books say {address}. Check the GasFree settings in both services."
                    ),
                )
                .await;
            }
            // Never a higher maxFee than was held back (spec §2): the most any deposit here held is
            // the most the relay may take.
            let held: Option<i64> = sqlx::query_scalar(
                "SELECT MAX(fee_held_usdt) FROM mint_intents
                 WHERE derivation_index = $1 AND deposit_address = $2 AND swept_at IS NULL AND verified_at <= $3",
            )
            .bind(index)
            .bind(address)
            .bind(requested_at)
            .fetch_one(pool)
            .await
            .unwrap_or(None);
            if let Some(h) = held.filter(|h| max_fee_usdt > *h) {
                alert(
                    pool,
                    "p1",
                    "sweeper",
                    &format!(
                        "the signer set maxFee {max_fee_usdt} on the permit for {address}, above the {h} held back for \
                         any deposit it moves. The permit is signed; if it runs, the reserve may fall below supply by \
                         the difference. Check the GasFree maxima in both services."
                    ),
                )
                .await;
            }
            match sqlx::query(
                "UPDATE gasfree_accounts
                    SET pending_trace_id = $2, pending_nonce = $3, pending_deadline = $4,
                        pending_value_usdt = $5, pending_requested_at = $6
                  WHERE derivation_index = $1",
            )
            .bind(index)
            .bind(&trace_id)
            .bind(nonce as i64)
            .bind(deadline as i64)
            .bind(value_usdt)
            .bind(requested_at)
            .execute(pool)
            .await
            {
                Ok(_) => tracing::info!(
                    "sweeper: permit {trace_id} for {address} (index {index}) is with the relay: {value_usdt} \
                     micro-USDT, maxFee {max_fee_usdt}, nonce {nonce}"
                ),
                // Not recorded, so never settled: the deposits stay unswept on the books, still
                // counted, and the next pass finds the relay's transfer in flight and waits.
                Err(e) => {
                    alert(pool, "p1", "sweeper", &format!("permit {trace_id} for {address} is with the relay, but recording it failed: {e}")).await;
                }
            }
        }
        SignerReply::Busy => tracing::info!("sweeper: a transfer from {address} is already in flight; next pass"),
        SignerReply::BelowFee => tracing::info!("sweeper: {address} holds no more than the relay's fee; left for a later deposit"),
        SignerReply::Rejected { reason } => {
            alert_once(
                pool,
                "p1",
                "sweeper",
                &format!(
                    "the relay refused the sweep of GasFree account {address}: {reason}. The deposit stays there, still \
                     counted. If the live fee is above the configured maximum, raise the maximum in the signer and the \
                     treasury together, never in the signer alone."
                ),
                hourly(),
            )
            .await;
        }
        SignerReply::Halted { reason } => {
            alert_once(pool, "p1", "sweeper", &format!("the signer halted GasFree for {address}: {reason}"), hourly()).await;
        }
        SignerReply::Failed(e) => super::sweep_failed(pool, config, address, index, &e).await,
        other => {
            tracing::warn!("sweeper: asked to sweep GasFree account {address} (index {index}), the signer answered {other:?}");
            alert_once(
                pool,
                "warn",
                "sweeper",
                &format!(
                    "asked to sweep GasFree account {address} (index {index}), the signer answered for the plain \
                     address instead: is GasFree on in the signer?"
                ),
                hourly(),
            )
            .await;
        }
    }
}
