//! T4: webhook intake (dedupe, refetch, async apply) + poll backstop, against a real database
//! and a scriptable `FakeAdapter` (tests/support/mod.rs) — no live Bitcart, no wiremock. Every
//! assertion here is about the REFETCHED state driving the transition, never the payload.
//!
//! Same shared-database convention as db_deposits.rs / db_deposit_api.rs: a sibling
//! `_orchestrator` database (sqlx's `_sqlx_migrations` bookkeeping table has no configurable
//! name, so two crates' migrators would corrupt each other's history on ONE shared DB).

mod support;

use payment_orchestrator::adapter::InvoiceState;
use payment_orchestrator::configuration::OrchConfig;
use payment_orchestrator::deposits::{self, CreateOutcome};
use payment_orchestrator::{poller, webhook};
use sqlx::migrate::MigrateDatabase;
use sqlx::{PgPool, Postgres};
use support::FakeAdapter;
use uuid::Uuid;

async fn pool() -> PgPool {
    let base_url = std::env::var("DATABASE_URL").expect("DATABASE_URL (run via docker-compose.test.yml)");
    let (prefix, dbname) = base_url.rsplit_once('/').expect("DATABASE_URL must contain a database name");
    let url = format!("{prefix}/{dbname}_orchestrator");

    if !Postgres::database_exists(&url).await.unwrap_or(false) {
        Postgres::create_database(&url).await.unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query("TRUNCATE deposit_intents, webhook_events, alerts RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

fn test_config() -> OrchConfig {
    OrchConfig {
        http_addr: "0.0.0.0:0".into(),
        database_url: std::env::var("DATABASE_URL").unwrap(),
        jwt_secret: "test-jwt-secret".into(),
        bitcart_url: "http://unused".into(),
        bitcart_token: "t".into(),
        bitcart_store_id: "s".into(),
        public_base_url: "http://unused".into(),
        treasury_url: "http://unused".into(),
        treasury_initiator_token: "i".into(),
        treasury_readonly_token: "r".into(),
        custody_tron_address: "Tunused".into(),
        deposit_ttl_minutes: 30,
        min_deposit_usdt: 1_000_000,
        max_deposit_usdt: 50_000_000,
        poll_interval_secs: 30,
        // This file never exercises the redemption routes (Plan C T6) — off by default,
        // same as production, and unused otherwise.
        redemptions_enabled: false,
        min_redemption_clt: 1_000_000,
        max_redemption_clt: 50_000_000,
    }
}

/// Creates an intent and runs it through `store_invoice` exactly like T2b's create-flow would,
/// so every test here starts from a real `invoiced` row with a real `invoice_id` — not a
/// hand-built fixture that could silently drift from what the create-flow actually produces.
async fn invoiced_intent(pool: &PgPool, cfg: &OrchConfig, user_pk: &str, amount: i64, key: &str, invoice_id: &str) -> Uuid {
    let CreateOutcome::Created(intent) = deposits::create(pool, cfg, user_pk, "clt-addr", amount, key).await.unwrap()
    else {
        panic!("expected Created");
    };
    let body = serde_json::json!({"id": intent.id, "status": "invoiced"});
    let stored = deposits::store_invoice(pool, intent.id, invoice_id, 201, &body).await.unwrap();
    assert!(stored, "test setup: store_invoice should win uncontested");
    intent.id
}

async fn status_of(pool: &PgPool, id: Uuid) -> String {
    deposits::find_by_id(pool, id).await.unwrap().unwrap().status
}

/// Core webhook flow: a webhook for a KNOWN invoice id is recorded and processed, and the
/// resulting status comes from the REFETCHED state, never the payload's own `status` field
/// (the payload here even lies — says "confirmed" — to prove the point; FakeAdapter is
/// scripted to `Paid`, and `Paid` is what must win).
#[tokio::test]
async fn webhook_drives_transition_from_refetched_state_not_payload() {
    let pool = pool().await;
    let cfg = test_config();
    let adapter = std::sync::Arc::new(FakeAdapter::new());
    let id = invoiced_intent(&pool, &cfg, "user-a", 2_000_000, "key-a", "inv-a").await;

    adapter.script("inv-a", InvoiceState::Paid, None);
    webhook::handle_webhook(pool.clone(), adapter.clone(), "inv-a".to_string(), "confirmed".to_string()).await;

    assert_eq!(status_of(&pool, id).await, "paying", "must reflect the REFETCHED Paid state, not the payload's claimed status");
    assert_eq!(adapter.call_count("inv-a"), 1, "handler must actually refetch, not trust the payload");
}

/// Duplicate webhook event key (same invoice_id + same status delivered twice — Bitcart can
/// double-fire, or a retry from an upstream proxy) must be processed exactly once. The second
/// delivery's `ON CONFLICT DO NOTHING` must short-circuit before any refetch happens.
#[tokio::test]
async fn duplicate_webhook_event_key_processed_once() {
    let pool = pool().await;
    let cfg = test_config();
    let adapter = std::sync::Arc::new(FakeAdapter::new());
    let id = invoiced_intent(&pool, &cfg, "user-b", 2_000_000, "key-b", "inv-b").await;

    adapter.script("inv-b", InvoiceState::Paid, None);
    webhook::handle_webhook(pool.clone(), adapter.clone(), "inv-b".to_string(), "paid".to_string()).await;
    webhook::handle_webhook(pool.clone(), adapter.clone(), "inv-b".to_string(), "paid".to_string()).await;

    assert_eq!(status_of(&pool, id).await, "paying");
    assert_eq!(adapter.call_count("inv-b"), 1, "the second delivery of the SAME event_key must never reach the refetch");

    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM webhook_events WHERE event_key = 'inv-b:paid'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "ON CONFLICT DO NOTHING must keep exactly one row for this event_key");
}

/// Spam resistance, the money-safety property: a webhook payload naming an invoice_id NO
/// intent holds must store nothing (no webhook_events row) and call nothing (zero refetches) —
/// the indexed lookup happens before any DB write or upstream call.
#[tokio::test]
async fn unknown_invoice_id_writes_nothing_and_calls_nothing() {
    let pool = pool().await;
    let adapter = std::sync::Arc::new(FakeAdapter::new());
    // Deliberately unscripted — if the handler ever calls get_invoice for this id, FakeAdapter
    // panics-via-Err loudly rather than the test having to notice a silent extra call.

    webhook::handle_webhook(pool.clone(), adapter.clone(), "inv-never-recorded".to_string(), "paid".to_string()).await;

    assert_eq!(adapter.call_count("inv-never-recorded"), 0, "an unknown id must never reach the refetch");
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM webhook_events").fetch_one(&pool).await.unwrap();
    assert_eq!(count, 0, "an unknown id must insert no webhook_events row at all");
}

/// Out-of-order delivery: a stale `Paid` refetch arriving AFTER the intent already reached
/// `confirmed` must not regress it back to `paying`. `Paid`'s from-set is `invoiced|expired`
/// (deposits.rs's `transition` guard) — `confirmed` isn't in it, so this is refused by
/// construction; the test proves that property survives through the whole webhook entrypoint,
/// not just at the `transition` call site in isolation.
///
/// (`Invalid`/`Refunded` moving a `confirmed`-but-not-yet-`credited` row to `failed` is legal
/// per the brief — the guard is specifically "never FROM `credited`", since that's the only
/// status meaning CLT has actually been minted; `invalid_never_moves_a_credited_row` below
/// covers that boundary.)
#[tokio::test]
async fn out_of_order_stale_paid_after_confirmed_does_not_regress_status() {
    let pool = pool().await;
    let cfg = test_config();
    let adapter = std::sync::Arc::new(FakeAdapter::new());
    let id = invoiced_intent(&pool, &cfg, "user-c", 2_000_000, "key-c", "inv-c").await;

    adapter.script("inv-c", InvoiceState::Confirmed, Some("tron-tx-c"));
    webhook::handle_webhook(pool.clone(), adapter.clone(), "inv-c".to_string(), "confirmed".to_string()).await;
    assert_eq!(status_of(&pool, id).await, "confirmed");

    // A stale/out-of-order event now claims Paid — refetch is re-scripted to simulate a late,
    // superseded signal arriving after the real confirmation already landed.
    adapter.script("inv-c", InvoiceState::Paid, None);
    webhook::handle_webhook(pool.clone(), adapter.clone(), "inv-c".to_string(), "paid".to_string()).await;

    assert_eq!(status_of(&pool, id).await, "confirmed", "confirmed must never regress to paying on a stale out-of-order Paid signal");
}

/// The property named first in the brief: an intent sitting in `expired` (our soft expiry)
/// that refetches as `Confirmed` must move to `confirmed` — it is a real user's real money,
/// and par rate means there is no FX reason to refuse a late payment.
#[tokio::test]
async fn late_confirm_from_expired_reaches_confirmed() {
    let pool = pool().await;
    let cfg = test_config();
    let adapter = std::sync::Arc::new(FakeAdapter::new());
    let id = invoiced_intent(&pool, &cfg, "user-d", 2_000_000, "key-d", "inv-d").await;

    // Soft-expire it exactly as the poller's sweep would, WITHOUT bitcart_terminal — the
    // dangerous-looking window the whole discriminator design exists to keep safe through.
    assert!(deposits::transition(&pool, id, &["created", "invoiced", "paying"], "expired").await.unwrap());
    assert_eq!(status_of(&pool, id).await, "expired");

    adapter.script("inv-d", InvoiceState::Confirmed, Some("tron-tx-d"));
    webhook::apply_invoice_update(&pool, adapter.as_ref(), "inv-d").await;

    let row = deposits::find_by_id(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "confirmed", "expired -> confirmed must be honoured on a late refetch");
    assert_eq!(row.tron_tx_id.as_deref(), Some("tron-tx-d"), "the tx id from the late confirm must be recorded");
}

/// The second property named first in the brief: the discriminator slot must stay reserved
/// while `expired && !bitcart_terminal` (a second user cannot claim the amount a possibly-live
/// invoice still carries), and must free the moment a refetch shows Bitcart-side terminality.
/// Driven through `apply_invoice_update` end to end, not by setting `bitcart_terminal` by hand.
#[tokio::test]
async fn slot_reserved_while_expired_not_terminal_then_frees_on_bitcart_terminal() {
    let pool = pool().await;
    let cfg = test_config();
    let adapter = std::sync::Arc::new(FakeAdapter::new());
    let id = invoiced_intent(&pool, &cfg, "user-e", 3_000_000, "key-e", "inv-e").await;
    let claimed_amount = deposits::find_by_id(&pool, id).await.unwrap().unwrap().pay_amount_usdt;

    assert!(deposits::transition(&pool, id, &["created", "invoiced", "paying"], "expired").await.unwrap());

    // Bitcart's refetch still says Pending (invoice not yet dead on Bitcart's side) — slot
    // must stay reserved.
    adapter.script("inv-e", InvoiceState::Pending, None);
    webhook::apply_invoice_update(&pool, adapter.as_ref(), "inv-e").await;

    let still_reserved = sqlx::query(
        "INSERT INTO deposit_intents
            (id, user_pk, clt_address, amount_usdt, pay_amount_usdt, amount_clt, client_key, expires_at)
         VALUES ($1, 'other-user', 'clt-other', 3000000, $2, 3000000, 'other-key', now() + interval '30 minutes')",
    )
    .bind(Uuid::new_v4())
    .bind(claimed_amount)
    .execute(&pool)
    .await;
    assert!(still_reserved.is_err(), "slot must stay reserved while expired && !bitcart_terminal");
    assert_eq!(
        still_reserved.unwrap_err().as_database_error().and_then(|e| e.constraint()),
        Some("uq_active_pay_amount")
    );

    // NOW Bitcart's refetch says the invoice is Expired on its own side — this is what must
    // free the slot, via a genuine refetch through apply_invoice_update, not a hand-set flag.
    adapter.script("inv-e", InvoiceState::Expired, None);
    webhook::apply_invoice_update(&pool, adapter.as_ref(), "inv-e").await;

    let row = deposits::find_by_id(&pool, id).await.unwrap().unwrap();
    assert!(row.bitcart_terminal, "a Bitcart-side Expired refetch must set bitcart_terminal");

    let now_free = sqlx::query(
        "INSERT INTO deposit_intents
            (id, user_pk, clt_address, amount_usdt, pay_amount_usdt, amount_clt, client_key, expires_at)
         VALUES ($1, 'other-user', 'clt-other', 3000000, $2, 3000000, 'other-key', now() + interval '30 minutes')",
    )
    .bind(Uuid::new_v4())
    .bind(claimed_amount)
    .execute(&pool)
    .await;
    assert!(now_free.is_ok(), "once bitcart_terminal = TRUE, the slot must be free for a new user to claim");
}

/// An underpaid deposit (`paid_partial`) goes to `needs_manual`, but Bitcart's invoice is STILL
/// LIVE and can still take the remainder at that amount — so the slot must stay reserved
/// (migration 0003). Freeing it on status alone would hand a stranger's live partially-paid
/// amount to a later user, the same defect class as a32a101.
///
/// Second property, same test because it's the other half of one invariant: the poller keeps
/// refetching such a row so terminality eventually releases the slot, but the refetch must NOT
/// resolve the row off `needs_manual` — a human was asked to look at money that may be sitting
/// in custody, and Bitcart calling the invoice invalid is not that human answering.
#[tokio::test]
async fn paid_partial_holds_slot_and_is_not_auto_resolved_off_needs_manual() {
    let pool = pool().await;
    let cfg = test_config();
    let adapter = std::sync::Arc::new(FakeAdapter::new());
    let id = invoiced_intent(&pool, &cfg, "user-pp", 7_000_000, "key-pp", "inv-pp").await;
    let claimed_amount = deposits::find_by_id(&pool, id).await.unwrap().unwrap().pay_amount_usdt;

    adapter.script("inv-pp", InvoiceState::PaidPartial, None);
    webhook::apply_invoice_update(&pool, adapter.as_ref(), "inv-pp").await;
    assert_eq!(status_of(&pool, id).await, "needs_manual");

    let claim = |amount: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO deposit_intents
                    (id, user_pk, clt_address, amount_usdt, pay_amount_usdt, amount_clt, client_key, expires_at)
                 VALUES ($1, 'other-user', 'clt-other', 7000000, $2, 7000000, 'other-key', now() + interval '30 minutes')",
            )
            .bind(Uuid::new_v4())
            .bind(amount)
            .execute(&pool)
            .await
        }
    };

    let blocked = claim(claimed_amount).await;
    assert_eq!(
        blocked.unwrap_err().as_database_error().and_then(|e| e.constraint()),
        Some("uq_active_pay_amount"),
        "a partially-paid invoice is still live at this amount — the slot must stay reserved"
    );

    // The poller must still pick this row up, purely to reach terminality.
    let due: Vec<String> = sqlx::query_scalar(
        "SELECT invoice_id FROM deposit_intents
         WHERE invoice_id IS NOT NULL AND status IN ('expired','needs_manual','failed') AND NOT bitcart_terminal",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(due.contains(&"inv-pp".to_string()), "poller must keep refetching needs_manual until terminal");

    // Bitcart finally calls it invalid: terminality releases the slot, the flag survives.
    adapter.script("inv-pp", InvoiceState::Invalid, None);
    webhook::apply_invoice_update(&pool, adapter.as_ref(), "inv-pp").await;

    assert!(deposits::find_by_id(&pool, id).await.unwrap().unwrap().bitcart_terminal);
    assert_eq!(
        status_of(&pool, id).await,
        "needs_manual",
        "a Bitcart refetch must not auto-resolve a row a human was asked to review"
    );
    assert!(claim(claimed_amount).await.is_ok(), "terminality must release the slot");
}

/// PaidOver must credit the INTENDED amount (par rate — crediting what arrived would mint CLT
/// the user's intended deposit didn't back) and raise an alert for the surplus, never silently
/// treat the overpayment as an exact match.
#[tokio::test]
async fn paid_over_credits_intended_amount_and_alerts_surplus() {
    let pool = pool().await;
    let cfg = test_config();
    let adapter = std::sync::Arc::new(FakeAdapter::new());
    let id = invoiced_intent(&pool, &cfg, "user-f", 4_000_000, "key-f", "inv-f").await;

    adapter.script("inv-f", InvoiceState::PaidOver, Some("tron-tx-f"));
    webhook::apply_invoice_update(&pool, adapter.as_ref(), "inv-f").await;

    let row = deposits::find_by_id(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "confirmed", "PaidOver must reach confirmed, crediting the intended amount");
    assert_eq!(row.amount_clt, 4_000_000, "amount_clt is untouched by the overpayment — it stays the INTENDED amount");

    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM alerts WHERE message LIKE '%overpaid%'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "an overpayment must raise exactly one alert recording the surplus for manual refund");
}

/// PaidPartial, FailedConfirm, and Unknown(_) must ALL land on needs_manual with an alert —
/// never a benign path, since money may have moved and a human must decide.
#[tokio::test]
async fn paid_partial_failed_confirm_and_unknown_all_need_manual_review() {
    let pool = pool().await;
    let cfg = test_config();
    let adapter = std::sync::Arc::new(FakeAdapter::new());

    for (key, invoice_id, state) in [
        ("key-g1", "inv-g1", InvoiceState::PaidPartial),
        ("key-g2", "inv-g2", InvoiceState::FailedConfirm),
        ("key-g3", "inv-g3", InvoiceState::Unknown("some_new_bitcart_status".to_string())),
    ] {
        let id = invoiced_intent(&pool, &cfg, "user-g", 2_000_000, key, invoice_id).await;
        adapter.script(invoice_id, state, None);
        webhook::apply_invoice_update(&pool, adapter.as_ref(), invoice_id).await;
        assert_eq!(status_of(&pool, id).await, "needs_manual", "{invoice_id} must reach needs_manual, never a benign status");
    }

    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM alerts WHERE severity = 'p1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 3, "all three exception states must raise a p1 alert — none silently absorbed");
}

/// Invalid/Refunded can never move a `credited` row — once minted, a later "refunded" cannot
/// walk the row backwards. Simulates the post-mint state directly (T5 owns the real path
/// there; this test only proves the guard on THIS webhook's transition, per `transition`'s
/// own from-set contract).
#[tokio::test]
async fn invalid_never_moves_a_credited_row() {
    let pool = pool().await;
    let cfg = test_config();
    let adapter = std::sync::Arc::new(FakeAdapter::new());
    let id = invoiced_intent(&pool, &cfg, "user-h", 2_000_000, "key-h", "inv-h").await;

    // Fast-forward straight to credited, as T5's bridge would eventually leave it.
    sqlx::query("UPDATE deposit_intents SET status = 'credited' WHERE id = $1").bind(id).execute(&pool).await.unwrap();

    adapter.script("inv-h", InvoiceState::Invalid, None);
    webhook::apply_invoice_update(&pool, adapter.as_ref(), "inv-h").await;

    assert_eq!(status_of(&pool, id).await, "credited", "Invalid must never move a credited row toward failed");
}

/// The poller must reach the exact same states the webhook does, with NO webhook delivery at
/// all — proving the reliability-path property by running `poll_once` directly rather than
/// asserting it by reading the code.
#[tokio::test]
async fn poller_alone_reaches_confirmed_with_no_webhook_delivered() {
    let pool = pool().await;
    let cfg = test_config();
    let adapter = std::sync::Arc::new(FakeAdapter::new());
    let id = invoiced_intent(&pool, &cfg, "user-i", 2_000_000, "key-i", "inv-i").await;

    // No call to webhook::handle_webhook anywhere in this test — the poller alone must find
    // and refetch this `invoiced` intent.
    adapter.script("inv-i", InvoiceState::Confirmed, Some("tron-tx-i"));
    poller::poll_once(&pool, adapter.as_ref()).await;

    assert_eq!(status_of(&pool, id).await, "confirmed", "the poller alone, with zero webhooks, must reach every state the webhook can");
    assert_eq!(adapter.call_count("inv-i"), 1);
}

/// `poll_once`'s expiry sweep: a non-terminal intent past its `expires_at` must move to
/// `expired` — the mechanism that puts an intent into the late-payment window this whole task
/// is built to honour correctly.
#[tokio::test]
async fn poller_sweeps_past_expiry_to_expired() {
    let pool = pool().await;
    let cfg = test_config();
    let adapter = std::sync::Arc::new(FakeAdapter::new());
    let id = invoiced_intent(&pool, &cfg, "user-j", 2_000_000, "key-j", "inv-j").await;

    sqlx::query("UPDATE deposit_intents SET expires_at = now() - interval '1 minute' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

    // Script Pending so the same pass's refetch (this intent is also `invoiced` before the
    // sweep runs) doesn't itself move the status — isolates the sweep as the mechanism under
    // test rather than a race between refetch-first and sweep-first within one poll_once.
    adapter.script("inv-j", InvoiceState::Pending, None);
    poller::poll_once(&pool, adapter.as_ref()).await;

    assert_eq!(status_of(&pool, id).await, "expired", "poll_once must sweep a past-expiry intent to expired");
}

/// The 30-day webhook_events retention sweep: an old row must be deleted, a recent one kept.
#[tokio::test]
async fn poller_sweeps_old_webhook_events_past_30_days() {
    let pool = pool().await;
    let cfg = test_config();
    let adapter = std::sync::Arc::new(FakeAdapter::new());
    let _ = invoiced_intent(&pool, &cfg, "user-k", 2_000_000, "key-k", "inv-k").await;

    sqlx::query(
        "INSERT INTO webhook_events (provider, event_key, payload, received_at)
         VALUES ('bitcart', 'inv-old:paid', '{}', now() - interval '31 days'),
                ('bitcart', 'inv-recent:paid', '{}', now() - interval '1 day')",
    )
    .execute(&pool)
    .await
    .unwrap();

    poller::poll_once(&pool, adapter.as_ref()).await;

    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM webhook_events WHERE event_key = 'inv-old:paid'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "a webhook_events row older than 30 days must be swept");

    let (recent_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM webhook_events WHERE event_key = 'inv-recent:paid'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(recent_count, 1, "a recent webhook_events row must survive the sweep");
}
