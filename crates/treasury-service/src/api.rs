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
}

async fn create_mint_intent_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateMintIntentBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let role = caller_role(&headers, &state.config)?;
    if role != Role::Initiator {
        return Err(StatusCode::FORBIDDEN);
    }
    let intent = intents::create_mint_intent(&state.pool, &body.beneficiary, body.amount_clt, actor_name(role))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((StatusCode::CREATED, Json(intent_json(&intent))))
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
    })
}

pub fn router(pool: PgPool, config: AppConfig) -> Router {
    let state = AppState { pool, config };
    Router::new()
        .route("/health", get(health))
        .route("/internal/mint-intents", post(create_mint_intent_handler))
        .route("/internal/mint-intents/:id/approve", post(approve_mint_intent_handler))
        .route("/internal/halt", post(halt_handler))
        .route("/internal/resume", post(resume_handler))
        .route("/internal/custody-deposits", post(custody_deposits_handler))
        .route("/internal/reserve-status", get(reserve_status_handler))
        .route("/internal/reconciliation-report", get(reconciliation_report_handler))
        .with_state(state)
}
