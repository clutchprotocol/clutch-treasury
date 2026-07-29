//! Bitcart IPN intake + the refetch-and-apply logic the poller (`poller.rs`) also calls.
//!
//! Two facts from Bitcart's reality shape everything here (T3's module docs, the T4 brief):
//! 1. The IPN is `{"id", "status"}`, UNSIGNED, and never retried on delivery failure. So the
//!    payload is trusted for exactly one thing — "invoice id X may have changed" — and NOTHING
//!    else. Every handler here refetches via `adapter.get_invoice` and acts only on that
//!    refetched state; no amount, status, or tx hash is ever read out of the webhook body.
//! 2. Because a failed IPN is silently swallowed, the poller (not this file) is the
//!    reliability path. `apply_invoice_update` is written once and called from both places so
//!    that property holds by construction: nothing here can ever be a state the poller alone
//!    can't also reach.
//!
//! Spam resistance: `POST /webhooks/bitcart` has no auth (Bitcart cannot sign it), so the
//! indexed lookup by `invoice_id` happens BEFORE any DB write or Bitcart call. An unknown id
//! writes nothing and calls nothing — deliberately no amount-based fallback (see
//! `handle_webhook`'s doc comment for why that would be a DoS amplifier pointed at our own
//! refetch call).

use sqlx::PgPool;

use crate::adapter::{InvoiceState, PaymentAdapter};
use crate::alerts::alert;
use crate::deposits;

/// Every status this table can produce, sanity-checked against the schema's CHECK constraint
/// (`migrations/0001_orchestrator.sql`) — kept here as from-sets rather than a shared "all
/// non-terminal" constant, since which statuses are legal varies per target and that's the
/// part worth reading explicitly at each call site.
const NON_TERMINAL_EXCEPT_CREDITED: &[&str] =
    &["created", "invoiced", "paying", "confirmed", "mint_requested", "expired", "failed", "needs_manual"];

/// Refetch `invoice_id` through the adapter (never trust the webhook payload beyond the id
/// itself) and drive the intent's guarded `transition` from the result. Called from both the
/// webhook processing task and the poller — this is the single place Bitcart state becomes our
/// state, so a webhook can never reach anything the poller alone couldn't also produce.
///
/// `transition`'s guarded `WHERE status = ANY($from)` already absorbs out-of-order delivery: a
/// stale event that no longer matches the intent's current status is a silent no-op, not a
/// regression (deposits.rs's own doc comment on `transition`).
pub async fn apply_invoice_update(pool: &PgPool, adapter: &dyn PaymentAdapter, invoice_id: &str) {
    let intent = match deposits::find_by_invoice_id(pool, invoice_id).await {
        Ok(Some(i)) => i,
        Ok(None) => return, // no intent holds this invoice_id — nothing to update.
        Err(e) => {
            alert(pool, "warn", "webhook", &format!("db error looking up invoice {invoice_id}: {e}")).await;
            return;
        }
    };

    let status = match adapter.get_invoice(invoice_id).await {
        Ok(s) => s,
        Err(e) => {
            // Transient (Bitcart down/timeout) — the poller will retry this invoice on its
            // next pass; nothing to persist from a failed refetch.
            alert(pool, "warn", "webhook", &format!("refetch failed for invoice {invoice_id}: {e}")).await;
            return;
        }
    };

    match status.state {
        InvoiceState::Pending => {} // nothing to do yet.

        InvoiceState::Paid => {
            let _ = deposits::transition(pool, intent.id, &["invoiced", "expired"], "paying").await;
        }

        // Late-honour is deliberate (Decisions block, par rate = no FX risk): an intent that
        // soft-expired on OUR side can still legally confirm if Bitcart's invoice does.
        InvoiceState::Confirmed => {
            confirm_and_credit(pool, &intent.id.to_string(), status.tron_tx_id.as_deref()).await;
        }

        // PaidOver credits the INTENDED amount (par — crediting what arrived would mint CLT
        // the user's intended deposit didn't back); the surplus is a manual-refund alert, not
        // a ledger entry this crate can compute — the adapter surfaces no observed amount
        // (T3's InvoiceStatus carries only state + tron_tx_id), so the alert names the invoice
        // for a human to look up in Bitcart directly rather than guessing a figure.
        InvoiceState::PaidOver => {
            confirm_and_credit(pool, &intent.id.to_string(), status.tron_tx_id.as_deref()).await;
            alert(
                pool,
                "warn",
                "webhook",
                &format!(
                    "deposit {} (invoice {invoice_id}) overpaid — credited intended amount_clt={}, \
                     surplus stays in custody pending manual refund",
                    intent.id, intent.amount_clt
                ),
            )
            .await;
        }

        // Bitcart-side terminal: this is what finally frees the discriminator slot (the
        // migration's invariant — amount is the only thing Bitcart can match a payment by on
        // the shared static address, so the slot must stay reserved until Bitcart itself can
        // no longer match a payment to it). Our own status only moves to `expired` if it
        // hasn't already progressed past that (a `paying` intent has a payment in flight;
        // Bitcart calling the invoice "expired" doesn't undo that).
        InvoiceState::Expired => {
            mark_bitcart_terminal(pool, intent.id).await;
            let _ = deposits::transition(pool, intent.id, &["created", "invoiced"], "expired").await;
        }

        // Terminal on Bitcart's side and ours — but NEVER from `credited` (once minted, a
        // later "refunded" cannot walk the row backwards; spec money-safety rule).
        InvoiceState::Invalid | InvoiceState::Refunded => {
            mark_bitcart_terminal(pool, intent.id).await;
            let _ = deposits::transition(pool, intent.id, NON_TERMINAL_EXCEPT_CREDITED, "failed").await;
        }

        // Money may have moved; a human decides. Never a benign path — Unknown especially,
        // since it's a Bitcart status string this adapter has never seen mapped.
        InvoiceState::PaidPartial | InvoiceState::FailedConfirm | InvoiceState::Unknown(_) => {
            let reason = match &status.state {
                InvoiceState::PaidPartial => "paid_partial".to_string(),
                InvoiceState::FailedConfirm => "failed_confirm".to_string(),
                InvoiceState::Unknown(raw) => format!("unknown bitcart status '{raw}'"),
                _ => unreachable!(),
            };
            let applied =
                deposits::transition(pool, intent.id, NON_TERMINAL_EXCEPT_CREDITED, "needs_manual").await;
            if matches!(applied, Ok(true)) {
                alert(
                    pool,
                    "p1",
                    "webhook",
                    &format!("deposit {} (invoice {invoice_id}) needs manual review: {reason}", intent.id),
                )
                .await;
            }
        }
    }
}

/// `Confirmed` and `PaidOver` share this from-set and this side effect (store tx id, guarded
/// transition to `confirmed`) — the only difference is `PaidOver`'s extra surplus alert.
async fn confirm_and_credit(pool: &PgPool, intent_id: &str, tron_tx_id: Option<&str>) {
    let id = match uuid::Uuid::parse_str(intent_id) {
        Ok(id) => id,
        Err(_) => return,
    };
    if let Some(tx_id) = tron_tx_id {
        let _ = deposits::set_tron_tx_id(pool, id, tx_id).await;
    }
    // ponytail: T5's bridge picks up `confirmed` intents from here (treasury_bridge.rs, out
    // of this task's scope) — nothing to call, it polls this status itself.
    let _ = deposits::transition(pool, id, &["invoiced", "paying", "expired"], "confirmed").await;
}

async fn mark_bitcart_terminal(pool: &PgPool, id: uuid::Uuid) {
    if let Err(e) = deposits::mark_bitcart_terminal(pool, id).await {
        tracing::error!("failed to set bitcart_terminal for {id}: {e}");
    }
}

/// `POST /webhooks/bitcart` — no auth (Bitcart can't sign). Indexed lookup FIRST: only a
/// payload whose `id` matches an existing intent's `invoice_id` gets a `webhook_events` row
/// and processing. An unknown id writes nothing and calls nothing — deliberately no
/// amount-based fallback here even though it might look like it would rescue a payment against
/// an invoice id we failed to record: the payload's amount is attacker-controlled, and
/// refetching every unknown id turns this guard into a DoS amplifier pointed at our own
/// payment processor. That case is Task 5's TronGrid verifier, which scans on-chain transfers
/// and matches by amount — off the attacker's path entirely.
///
/// The route handler (`api.rs`) spawns this whole function so the HTTP response returns 200
/// immediately either way — this function itself runs the lookup, dedup insert, and refetch
/// straight through (no further spawn needed once the caller has already backgrounded it).
pub async fn handle_webhook(pool: PgPool, adapter: std::sync::Arc<dyn PaymentAdapter>, id: String, status: String) {
    let known = match deposits::find_by_invoice_id(&pool, &id).await {
        Ok(intent) => intent.is_some(),
        Err(e) => {
            tracing::error!("webhook lookup failed for invoice {id}: {e}");
            return;
        }
    };
    if !known {
        return; // Unknown invoice id: no storage, no upstream call.
    }

    let event_key = format!("{id}:{status}");
    let inserted = sqlx::query(
        "INSERT INTO webhook_events (provider, event_key, payload) VALUES ('bitcart', $1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(&event_key)
    .bind(serde_json::json!({"id": id, "status": status}))
    .execute(&pool)
    .await;

    match inserted {
        Ok(result) if result.rows_affected() == 1 => {
            // Fresh event key — this pair hasn't been processed before.
            apply_invoice_update(&pool, adapter.as_ref(), &id).await;
        }
        Ok(_) => {} // ON CONFLICT DO NOTHING: this exact (invoice_id, status) pair was already processed.
        Err(e) => tracing::error!("failed to record webhook_event for invoice {id}: {e}"),
    }
}
