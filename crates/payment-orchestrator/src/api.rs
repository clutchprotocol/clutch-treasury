use axum::{
    extract::{Path, State},
    http::{HeaderMap, HeaderName, Method, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use tower_http::cors::{AllowHeaders, AllowOrigin, Any, CorsLayer};
use uuid::Uuid;

use crate::adapter::PaymentAdapter;
use crate::auth::authenticated_pk;
use crate::configuration::OrchConfig;
use crate::deposits::{self, DepositOutcome};
use crate::redemptions::{self, RedemptionOutcome};
use crate::webhook;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: OrchConfig,
    pub adapter: Arc<dyn PaymentAdapter>,
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

/// `"*"` allows any origin (local/dev default); otherwise a comma-separated allowlist.
/// Same config style as clutch-hub-api's `build_cors` (`hub/server.rs`) — different crate
/// because this service is Axum, not Actix, but the origin-parsing rule is identical.
/// Must explicitly allow `Idempotency-Key` (deposits require it) and `Authorization` (the JWT
/// bearer token) since the specific-origins branch can't use a header wildcard.
fn build_cors(allowed_origins: &str) -> CorsLayer {
    let layer = CorsLayer::new().allow_methods([Method::GET, Method::POST, Method::OPTIONS]);

    if allowed_origins.trim() == "*" {
        layer.allow_origin(Any).allow_headers(Any)
    } else {
        let origins: Vec<_> = allowed_origins
            .split(',')
            .map(str::trim)
            .filter(|o| !o.is_empty())
            .filter_map(|o| o.parse().ok())
            .collect();
        layer
            .allow_origin(AllowOrigin::list(origins))
            .allow_headers(AllowHeaders::list([
                HeaderName::from_static("authorization"),
                HeaderName::from_static("content-type"),
                HeaderName::from_static("idempotency-key"),
            ]))
    }
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
        // Fall back to 500, not 200: an unparseable stored status is a bug in whatever
        // wrote it, and replaying it as success would tell the client their deposit is
        // fine on the strength of a value we couldn't read.
        DepositOutcome::Respond { status, body } => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            body,
        ),
        DepositOutcome::Conflict => (StatusCode::CONFLICT, json!({"error": "idempotency key already used with a different request body"})),
        DepositOutcome::StillProcessing => {
            resp_headers.insert("retry-after", "2".parse().unwrap());
            (StatusCode::CONFLICT, json!({"error": "a request with this idempotency key is still being processed"}))
        }
        DepositOutcome::OutOfBounds { min, max } => (
            StatusCode::BAD_REQUEST,
            json!({"error": format!("amount_usdt must be between {min} and {max}")}),
        ),
        // Fail closed (T2b's deferred headroom check, landed in 5b): the treasury couldn't be
        // asked whether it could mint against this deposit — 503 + Retry-After, same shape as
        // any other "ask again shortly" backpressure signal.
        DepositOutcome::TreasuryUnavailable => {
            resp_headers.insert("retry-after", "30".parse().unwrap());
            (StatusCode::SERVICE_UNAVAILABLE, json!({"error": "treasury unreachable — cannot verify mint headroom, try again shortly"}))
        }
        // A clear 4xx, not a 503: the treasury DID answer, and the answer is "not enough room
        // today" — retrying immediately won't change that, so this isn't the same kind of
        // "try again shortly" signal a 503 implies.
        DepositOutcome::InsufficientHeadroom { headroom_clt } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"error": format!("insufficient daily mint headroom ({headroom_clt} CLT remaining) to cover this deposit")}),
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

/// Deliberately has NO `redeemer_address` field — see `redemptions.rs` module docs. The
/// redeemer is always the caller's authenticated `user_pk`; there is nothing in this struct
/// a client could set to name someone else's balance.
#[derive(Deserialize)]
struct CreateRedemptionBody {
    payout_tron_address: String,
    amount_clt: i64,
}

/// `POST /api/v1/redemptions` — validates the payout address (base58check checksum + Tron
/// version byte, `redemptions::is_valid_tron_address` — NOT a shape regex) and bounds, then
/// forwards to the treasury with `redeemer_address` = the JWT's `pk`, never the request body.
/// 503s while `config.redemptions_enabled` is false (default) — the treasury's payout rail is
/// still `payout::StubRail`; see `OrchConfig::redemptions_enabled`'s doc comment for why this
/// gate exists before anything else in this handler runs.
async fn create_redemption_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateRedemptionBody>,
) -> Result<(StatusCode, HeaderMap, Json<serde_json::Value>), StatusCode> {
    // Gated before auth, same ordering as get_redemption_handler below: a disabled feature
    // 503s uniformly regardless of whether the caller's JWT would otherwise have been valid.
    if !state.config.redemptions_enabled {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            HeaderMap::new(),
            Json(json!({"error": "redemptions are not yet available — the treasury payout rail is not live"})),
        ));
    }
    let user_pk = authenticated_pk(&headers, &state.config)?;

    let outcome = redemptions::create_redemption(
        &state.pool,
        &state.config,
        &user_pk,
        &body.payout_tron_address,
        body.amount_clt,
    )
    .await;

    let mut resp_headers = HeaderMap::new();
    let (status, payload) = match outcome {
        RedemptionOutcome::Created { id, redemption_ref, amount_clt, status } => (
            StatusCode::CREATED,
            json!({"id": id, "redemption_ref": redemption_ref, "amount_clt": amount_clt, "status": status}),
        ),
        // 503, not 400: this isn't a malformed request, it's a feature that isn't live yet —
        // the treasury's payout rail is a stub (see OrchConfig::redemptions_enabled). Naming
        // that plainly in the body keeps a caller from retrying forever thinking it's transient
        // backpressure, or from concluding their address/amount was somehow invalid.
        RedemptionOutcome::Disabled => (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "redemptions are not yet available — the treasury payout rail is not live"}),
        ),
        RedemptionOutcome::InvalidAddress => (
            StatusCode::BAD_REQUEST,
            json!({"error": "payout_tron_address failed base58check validation (bad checksum or version byte)"}),
        ),
        RedemptionOutcome::OutOfBounds { min, max } => (
            StatusCode::BAD_REQUEST,
            json!({"error": format!("amount_clt must be between {min} and {max}")}),
        ),
        RedemptionOutcome::TreasuryUnavailable => {
            resp_headers.insert("retry-after", "30".parse().unwrap());
            (StatusCode::SERVICE_UNAVAILABLE, json!({"error": "treasury unreachable, try again shortly"}))
        }
        RedemptionOutcome::TreasuryRejected => (
            StatusCode::BAD_GATEWAY,
            json!({"error": "treasury refused the redemption request"}),
        ),
        RedemptionOutcome::Failed(msg) => {
            tracing::error!("create_redemption failed: {msg}");
            (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "internal error"}))
        }
    };
    Ok((status, resp_headers, Json(payload)))
}

/// `GET /api/v1/redemptions/:id` — owner-checked exactly like `get_deposit_handler`: a caller
/// whose `user_pk` does not match the mapping row's gets 404, not 403 (don't confirm a
/// resource exists to someone not allowed to see it). Also 503s while `redemptions_enabled`
/// is false, for the same reason creation does.
///
/// Status comes from a LIVE treasury re-fetch, with the stored creation-time status as the
/// fallback only when the treasury can't be reached. The stored value alone would be actively
/// misleading: `watcher::confirm_burn` is what advances a redemption, so a user polling their own
/// redemption would see `created` forever — including after their CLT was burned and the payout
/// made. `GET /internal/redemption-intents/:id` was added treasury-side for exactly this.
///
/// A stale-status fallback is the right failure mode here, unlike the create path's fail-closed
/// 503: reading a status moves no money, and the alternative is telling a worried user nothing at
/// all. `status_live` marks which they got, so a client can tell "not yet" from "we couldn't ask".
async fn get_redemption_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !state.config.redemptions_enabled {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let user_pk = authenticated_pk(&headers, &state.config)?;
    let mapping = redemptions::find_by_id(&state.pool, id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if mapping.user_pk != user_pk {
        return Err(StatusCode::NOT_FOUND);
    }
    let live = redemptions::fetch_treasury_status(&state.config, mapping.treasury_intent_id).await;
    Ok(Json(json!({
        "id": mapping.id,
        "redemption_ref": mapping.redemption_ref,
        "payout_tron_address": mapping.payout_tron_address,
        "amount_clt": mapping.amount_clt,
        "status": live.as_deref().unwrap_or(&mapping.status),
        "status_live": live.is_some(),
    })))
}

#[derive(Deserialize)]
struct BitcartIpn {
    id: String,
    status: String,
}

/// `POST /webhooks/bitcart` — deliberately NO auth (Bitcart cannot sign its IPN). The route
/// itself does nothing trusting: it hands the raw `{id, status}` straight to
/// `webhook::handle_webhook`, which does the indexed lookup BEFORE any DB write or Bitcart
/// call (T4 brief's spam-resistance requirement) and refetches through the adapter rather
/// than trusting anything in this body beyond the id. Always 200 — Bitcart doesn't retry, so
/// there's no failure code worth sending back.
///
/// The known-invoice lookup is AWAITED here rather than inside the spawned task, and only real
/// work is backgrounded. It's one indexed SELECT, so 200 is still effectively immediate, but a
/// spammer now has to hold a TCP connection for each in-flight lookup instead of firing and
/// disconnecting. Spawning first would let unauthenticated traffic grow detached tasks that each
/// take a connection from a 5-connection pool — starving the deposit routes through the one
/// route on this service that has no auth in front of it.
async fn bitcart_webhook_handler(State(state): State<AppState>, Json(body): Json<BitcartIpn>) -> StatusCode {
    if webhook::is_known_invoice(&state.pool, &body.id).await {
        tokio::spawn(webhook::process_known_webhook(state.pool, state.adapter, body.id, body.status));
    }
    StatusCode::OK
}

pub fn router(pool: PgPool, config: OrchConfig, adapter: Arc<dyn PaymentAdapter>) -> Router {
    let cors = build_cors(&config.allowed_origins);
    let state = AppState { pool, config, adapter };
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/deposits", post(create_deposit_handler))
        .route("/api/v1/deposits/:id", get(get_deposit_handler))
        .route("/api/v1/redemptions", post(create_redemption_handler))
        .route("/api/v1/redemptions/:id", get(get_redemption_handler))
        .route("/webhooks/bitcart", post(bitcart_webhook_handler))
        .layer(cors)
        .with_state(state)
}
