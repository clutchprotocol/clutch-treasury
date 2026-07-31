use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::configuration::AppConfig;
use crate::{breakers, intents, ledger};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: AppConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Approver,
    ReadOnly,
}

/// Static bearer tokens, one per role — disjoint on purpose (four-eyes, spec §5).
/// Returns the caller's role; the handler decides which roles it accepts.
pub fn caller_role(headers: &HeaderMap, config: &AppConfig) -> Result<Role, StatusCode> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s).trim())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if token == config.initiator_token {
        Ok(Role::Initiator)
    } else if token == config.approver_token {
        Ok(Role::Approver)
    } else if token == config.readonly_token {
        Ok(Role::ReadOnly)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Single shared token per role (no per-person identity yet — see docs/keys.md and Plan C's
/// note that per-person token names arrive later): the role name is what the audit trail can
/// truthfully say tagged the action.
fn actor_name(role: Role) -> &'static str {
    match role {
        Role::Initiator => "initiator",
        Role::Approver => "approver",
        Role::ReadOnly => "readonly",
    }
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

#[derive(Deserialize)]
struct CreateMintIntentBody {
    beneficiary: String,
    amount_clt: i64,
    /// Plan C T5: the orchestrator's deposit-intent id, its idempotency key for this call.
    /// `None` for Plan B's direct/manual mint intents.
    client_ref: Option<String>,
    /// The Tron transfer the tron_verifier should check first (may be absent — Bitcart
    /// sometimes returns no hash; the verifier's fallback match then backfills this).
    deposit_tx_id: Option<String>,
    /// What the depositor was told to PAY: `amount_clt` plus the orchestrator's discriminator.
    /// Mandatory whenever `client_ref` is set — the verifier matches on-chain transfers against
    /// this, and on a shared custody address it is the only thing separating one user's payment
    /// from another's. Rejected with 400 rather than defaulted, so a bridge that forgets to send
    /// it fails loudly instead of silently widening the verifier's match to `amount_clt`.
    expected_amount_usdt: Option<i64>,
    deposit_address: Option<String>,
}

/// `created_by` is derived from the AUTHENTICATED ROLE (`actor_name`), never from the request
/// body — a body field would be spoofable within a role and would collapse four-eyes down to
/// whatever string an attacker chose to write next to their own approval (brief's explicit
/// requirement). This is what keeps the `four_eyes` DB CHECK meaningful: initiator-token calls
/// always record literally `"initiator"` (renamed `'orchestrator'` in Plan C's bridge worker,
/// which authenticates as this same Role::Initiator), and only `tron_verifier.rs` ever writes
/// `approved_by = 'tron-verifier'` — neither string is ever read off anything a caller sent.
///
/// Duplicate `client_ref`: replays the EXISTING intent rather than creating a second row
/// (spec — this is the treasury-side half of the bridge worker's idempotent retry). No
/// separate CAS/lock needed beyond the schema's `client_ref UNIQUE`: the lookup-then-insert
/// below has a race window (two concurrent creates with the same never-before-seen
/// client_ref), but the loser's INSERT simply fails the unique constraint and 500s into a
/// caller retry, which re-runs this handler and finds the winner's row via `find_by_client_ref`
/// — no duplicate intent and no double mint can result either way.
async fn create_mint_intent_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateMintIntentBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let role = caller_role(&headers, &state.config)?;
    if role != Role::Initiator {
        return Err(StatusCode::FORBIDDEN);
    }
    if let Some(client_ref) = &body.client_ref {
        if let Some(existing) = intents::find_by_client_ref(&state.pool, client_ref)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        {
            return Ok((StatusCode::OK, Json(intent_json(&existing))));
        }
        // A deposit-backed intent without the discriminated pay amount is unverifiable: the
        // verifier would have nothing to match on-chain transfers against but `amount_clt`,
        // which every user depositing the same round number shares. The DB CHECK also refuses
        // this; failing here makes the reason legible instead of a 500.
        // A deposit-backed intent with no address is unverifiable, and unverifiable must never
        // be approvable — refuse at the door rather than let it age into manual review.
        if body.deposit_address.as_deref().is_none_or(str::is_empty) {
            return Err(StatusCode::BAD_REQUEST);
        }
        if body.expected_amount_usdt.is_none_or(|a| a <= 0) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    let intent = intents::create_mint_intent(
        &state.pool,
        &body.beneficiary,
        body.amount_clt,
        actor_name(role),
        body.client_ref.as_deref(),
        body.deposit_tx_id.as_deref(),
        body.expected_amount_usdt,
        body.deposit_address.clone(),
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((StatusCode::CREATED, Json(intent_json(&intent))))
}

/// `GET /internal/mint-intents/:id` — any role (same convention as reserve-status/reconciliation
/// reads below): Plan C 5b's bridge worker polls this with the readonly token to learn what
/// became of a deposit-backed intent it created (`credited` / `rejected` / `failed` / still
/// pending). Reuses the exact same `intent_json` shape the create/approve routes already return,
/// so the bridge parses one response shape everywhere.
async fn get_mint_intent_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    caller_role(&headers, &state.config)?; // any role
    let intent = intents::find_by_id(&state.pool, id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(intent_json(&intent)))
}

async fn approve_mint_intent_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let role = caller_role(&headers, &state.config)?;
    if role != Role::Approver {
        return Err(StatusCode::FORBIDDEN);
    }
    // Early feedback only — NOT the authoritative gate. The outbox worker re-checks with
    // check_mint_excluding immediately before every submission; approval must never itself
    // be treated as authorisation to mint (spec §7.2).
    let intent_amount: Option<(i64,)> = sqlx::query_as("SELECT amount_clt FROM mint_intents WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some((amount_clt,)) = intent_amount else {
        return Err(StatusCode::NOT_FOUND);
    };
    if let Err(denial) = breakers::check_mint(&state.pool, &state.config, amount_clt).await {
        tracing::warn!("approval-time breaker denial for {id}: {}", denial.reason);
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // By this point role == Approver and created_by is always the literal string "initiator"
    // (see actor_name) — a real cross-person four-eyes collision is a schema-level guarantee
    // (db_ledger.rs::four_eyes_enforced_in_db), not reachable through this single-shared-
    // token-per-role API. Any error here is a state conflict (already approved/submitted/etc).
    let approved = intents::approve_mint_intent(&state.pool, id, actor_name(role))
        .await
        .map_err(|_| StatusCode::CONFLICT)?;
    Ok(Json(intent_json(&approved)))
}

#[derive(Deserialize)]
struct CreateRedemptionIntentBody {
    redeemer_address: String,
    payout_address: String,
    amount_clt: i64,
}

/// Initiator only, same role shape as mint-intents — the orchestrator/SDK takes the
/// returned `redemption_ref` and puts it in the user's on-chain Burn tx. The intent starts
/// `created`; `watcher::confirm_burn` is the only path that advances it, once a matching
/// on-chain burn is confirmed.
async fn create_redemption_intent_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateRedemptionIntentBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let role = caller_role(&headers, &state.config)?;
    if role != Role::Initiator {
        return Err(StatusCode::FORBIDDEN);
    }
    let intent = intents::create_redemption_intent(
        &state.pool,
        &body.redeemer_address,
        &body.payout_address,
        body.amount_clt,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((StatusCode::CREATED, Json(redemption_intent_json(&intent))))
}

/// `GET /internal/redemption-intents/:id` — any role, same convention as the mint read above.
/// Plan C T6's redemption proxy polls this so a user checking their redemption sees the CURRENT
/// status; without it the proxy could only echo the status captured at creation, showing `created`
/// forever even after the burn was confirmed and the payout made.
async fn get_redemption_intent_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    caller_role(&headers, &state.config)?; // any role
    let intent = intents::find_redemption_by_id(&state.pool, id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(redemption_intent_json(&intent)))
}

#[derive(Deserialize)]
struct HaltBody {
    reason: String,
}

async fn halt_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<HaltBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let role = caller_role(&headers, &state.config)?;
    if role != Role::Approver {
        return Err(StatusCode::FORBIDDEN);
    }
    breakers::manual_halt(&state.pool, &body.reason, actor_name(role))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({"minting_halted": true})))
}

async fn resume_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let role = caller_role(&headers, &state.config)?;
    if role != Role::Approver {
        return Err(StatusCode::FORBIDDEN);
    }
    breakers::manual_resume(&state.pool, actor_name(role))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({"minting_halted": false})))
}

#[derive(Deserialize)]
struct CustodyDepositBody {
    amount_usdt: i64,
    reference: String,
}

/// Manual custody-deposit entry — the only writer of custody inflows until Plan C's TronGrid
/// verifier takes over. Without this route the backing-ratio breaker reads custody 0 and
/// correctly refuses the very first mint (there is no recorded reserve to back it). Approver
/// only, and every use writes a `warn` alert: an operator asserting "reserve arrived" without
/// an independent on-chain observation is exactly the kind of action that must leave a trail,
/// never a silent override.
///
/// `config.custody_stub_balance_usdt` (reconciliation's separate custody input, read at
/// startup) is NOT updated by this route — it must be kept equal to the sum of recorded
/// deposits by the same operator action (i.e. whoever calls this route also updates that
/// config value) until the real Tron custody watcher replaces both.
async fn custody_deposits_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CustodyDepositBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let role = caller_role(&headers, &state.config)?;
    if role != Role::Approver {
        return Err(StatusCode::FORBIDDEN);
    }
    ledger::append_event(
        &state.pool,
        "custody_deposit",
        0,
        body.amount_usdt,
        None,
        None,
        &format!("manual custody entry ({}): {}", actor_name(role), body.reference),
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    ledger::alert(
        &state.pool,
        "warn",
        "custody",
        &format!(
            "manual custody entry by {}: {} USDT, ref '{}' — no silent overrides, verify independently",
            actor_name(role),
            body.amount_usdt,
            body.reference
        ),
    )
    .await;
    Ok((StatusCode::CREATED, Json(json!({"amount_usdt": body.amount_usdt, "reference": body.reference}))))
}

async fn reserve_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    caller_role(&headers, &state.config)?; // any role
    let balances = ledger::balances(&state.pool).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let (minting_halted, halt_reason): (bool, Option<String>) =
        sqlx::query_as("SELECT minting_halted, halt_reason FROM breaker_state")
            .fetch_one(&state.pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let last_reconciliation: Option<(String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT status, run_at FROM reconciliation_runs ORDER BY run_at DESC LIMIT 1")
            .fetch_optional(&state.pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let (pending_outbox,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM chain_outbox WHERE status = 'pending'")
        .fetch_one(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Plan C T2b deliberately deferred its headroom check waiting for this field (5b consumes
    // it before proposing a new mint). Reuses breakers::daily_mint_total — the exact sum the
    // daily-cap gate itself checks against — rather than recomputing the 24h window here.
    let day_total = breakers::daily_mint_total(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let daily_headroom_clt = (state.config.daily_mint_cap_clt - day_total).max(0);

    Ok(Json(json!({
        "balances": {
            "clt_liability": balances.clt_liability,
            "custody_usdt": balances.custody_usdt,
        },
        "breaker": {
            "minting_halted": minting_halted,
            "halt_reason": halt_reason,
        },
        "last_reconciliation": last_reconciliation.map(|(status, run_at)| json!({
            "status": status,
            "run_at": run_at,
        })),
        "pending_outbox": pending_outbox,
        "daily_headroom_clt": daily_headroom_clt,
    })))
}

#[derive(Deserialize)]
struct ReconciliationReportQuery {
    limit: Option<i64>,
}

async fn reconciliation_report_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ReconciliationReportQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    caller_role(&headers, &state.config)?; // any role
    let limit = q.limit.unwrap_or(30).clamp(1, 500);
    let rows: Vec<(i64, chrono::DateTime<chrono::Utc>, i64, i64, i64, i64, String, serde_json::Value)> =
        sqlx::query_as(
            "SELECT id, run_at, onchain_supply, genesis_allocation, ledger_liability, custody_reported, status, detail
             FROM reconciliation_runs ORDER BY run_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let items: Vec<_> = rows
        .into_iter()
        .map(|(id, run_at, onchain_supply, genesis_allocation, ledger_liability, custody_reported, status, detail)| {
            json!({
                "id": id,
                "run_at": run_at,
                "onchain_supply": onchain_supply,
                "genesis_allocation": genesis_allocation,
                "ledger_liability": ledger_liability,
                "custody_reported": custody_reported,
                "status": status,
                "detail": detail,
            })
        })
        .collect();
    Ok(Json(json!({"items": items})))
}

fn intent_json(intent: &intents::MintIntent) -> serde_json::Value {
    json!({
        "id": intent.id,
        "beneficiary": intent.beneficiary,
        "amount_clt": intent.amount_clt,
        "status": intent.status,
        "credit_ref": intent.credit_ref,
        "created_by": intent.created_by,
        "approved_by": intent.approved_by,
        "chain_tx_hash": intent.chain_tx_hash,
        "client_ref": intent.client_ref,
        "deposit_tx_id": intent.deposit_tx_id,
        "verified_at": intent.verified_at,
    })
}

fn redemption_intent_json(intent: &intents::RedemptionIntent) -> serde_json::Value {
    json!({
        "id": intent.id,
        "redeemer_address": intent.redeemer_address,
        "payout_address": intent.payout_address,
        "amount_clt": intent.amount_clt,
        "status": intent.status,
        "redemption_ref": intent.redemption_ref,
        "burn_tx_hash": intent.burn_tx_hash,
    })
}

pub fn router(pool: PgPool, config: AppConfig) -> Router {
    let state = AppState { pool, config };
    Router::new()
        .route("/health", get(health))
        .route("/internal/mint-intents", post(create_mint_intent_handler))
        .route("/internal/mint-intents/:id", get(get_mint_intent_handler))
        .route("/internal/mint-intents/:id/approve", post(approve_mint_intent_handler))
        .route("/internal/redemption-intents", post(create_redemption_intent_handler))
        .route("/internal/redemption-intents/:id", get(get_redemption_intent_handler))
        .route("/internal/halt", post(halt_handler))
        .route("/internal/resume", post(resume_handler))
        .route("/internal/custody-deposits", post(custody_deposits_handler))
        .route("/internal/reserve-status", get(reserve_status_handler))
        .route("/internal/reconciliation-report", get(reconciliation_report_handler))
        .with_state(state)
}
