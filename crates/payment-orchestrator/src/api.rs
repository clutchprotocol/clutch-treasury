use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::adapter::PaymentAdapter;
use crate::auth::authenticated_pk;
use crate::configuration::OrchConfig;
use crate::deposits::{self, DepositOutcome};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: OrchConfig,
    pub adapter: Arc<dyn PaymentAdapter>,
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

#[derive(Deserialize)]
struct CreateDepositBody {
    clt_address: String,
    amount_usdt: i64,
}

/// `POST /api/v1/deposits` — the create-flow: idempotency layer 1 (client-key dedup,
/// Task 2) meets layer 4 (invoice-store compare-and-set) meets the Bitcart adapter (T3).
/// Thin by design: every actual decision (replay/conflict/still-processing/bounds/CAS) is
/// made in `deposits::create_and_invoice`; this handler only extracts the request and
/// translates the resulting `DepositOutcome` into a status code, headers, and body.
async fn create_deposit_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateDepositBody>,
) -> Result<(StatusCode, HeaderMap, Json<serde_json::Value>), StatusCode> {
    let user_pk = authenticated_pk(&headers, &state.config)?;
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::BAD_REQUEST)?;

    let notification_url = format!("{}/webhooks/bitcart", state.config.public_base_url);
    let outcome = deposits::create_and_invoice(
        &state.pool,
        &state.config,
        state.adapter.as_ref(),
        &user_pk,
        &body.clt_address,
        body.amount_usdt,
        idempotency_key,
        &notification_url,
    )
    .await;

    let mut resp_headers = HeaderMap::new();
    let (status, payload) = match outcome {
        DepositOutcome::Respond { status, body } => {
            (StatusCode::from_u16(status).unwrap_or(StatusCode::OK), body)
        }
        DepositOutcome::Conflict => (StatusCode::CONFLICT, json!({"error": "idempotency key already used with a different request body"})),
        DepositOutcome::StillProcessing => {
            resp_headers.insert("retry-after", "2".parse().unwrap());
            (StatusCode::CONFLICT, json!({"error": "a request with this idempotency key is still being processed"}))
        }
        DepositOutcome::OutOfBounds { min, max } => (
            StatusCode::BAD_REQUEST,
            json!({"error": format!("amount_usdt must be between {min} and {max}")}),
        ),
        DepositOutcome::Failed(msg) => {
            tracing::error!("create_and_invoice failed: {msg}");
            (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "internal error"}))
        }
    };
    Ok((status, resp_headers, Json(payload)))
}

/// `GET /api/v1/deposits/:id` — owner-checked: a JWT that authenticates fine but names a
/// different `user_pk` than the one on the intent gets 404, not 403 — same reasoning as
/// not confirming a resource exists to a caller who isn't allowed to see it.
async fn get_deposit_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let user_pk = authenticated_pk(&headers, &state.config)?;
    let intent = deposits::find_by_id(&state.pool, id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if intent.user_pk != user_pk {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(json!({
        "id": intent.id,
        "clt_address": intent.clt_address,
        "amount_usdt": intent.amount_usdt,
        "pay_amount_usdt": intent.pay_amount_usdt,
        "status": intent.status,
        "invoice_id": intent.invoice_id,
        "expires_at": intent.expires_at,
    })))
}

pub fn router(pool: PgPool, config: OrchConfig, adapter: Arc<dyn PaymentAdapter>) -> Router {
    let state = AppState { pool, config, adapter };
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/deposits", post(create_deposit_handler))
        .route("/api/v1/deposits/:id", get(get_deposit_handler))
        .with_state(state)
}
