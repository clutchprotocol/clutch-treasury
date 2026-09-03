use std::sync::Arc;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, HeaderName, Method, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use tower_http::cors::{AllowHeaders, AllowOrigin, Any, CorsLayer};
use uuid::Uuid;

use crate::addresses;
use crate::auth::authenticated_pk;
use crate::configuration::OrchConfig;
use crate::derive::AddressDeriver;
use crate::deposits;
use crate::redemptions::{self, RedemptionOutcome};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: OrchConfig,
    /// Shared so the account xpub is parsed once, at startup.
    pub deriver: Arc<AddressDeriver>,
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
}

/// `POST /api/v1/deposits` — where to send USDT.
///
/// No amount, and no intent. The user has one permanent address; whatever arrives at it is credited
/// in full by the poller. This is idempotent by nature rather than by an idempotency key: a user has
/// exactly one address, so a repeat call is the same answer.
///
/// On a repeat call the stored `clt_address` wins: `addresses::address_for_user`'s `ON CONFLICT
/// (user_pk) DO NOTHING` means a user who sends a different one later keeps their original
/// destination. That is deliberate — silently re-pointing where someone's future deposits mint is
/// worse than ignoring the change.
///
/// Marking the address hot here is the whole reason the tiered poller can stay cheap — this call IS
/// the signal that a deposit is imminent.
async fn create_deposit_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateDepositBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let user_pk = authenticated_pk(&headers, &state.config)?;

    let address =
        addresses::address_for_user(&state.pool, state.deriver.as_ref(), &user_pk, &body.clt_address)
            .await
            .map_err(|e| {
                tracing::error!("deposit address for {user_pk}: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

    if let Err(e) =
        addresses::mark_hot(&state.pool, &user_pk, state.config.deposit_hot_window_hours).await
    {
        // Not fatal: the address is still watched on the cold rotation, so a deposit is credited
        // late rather than lost. Worth an error line because a persistent failure here quietly
        // degrades every deposit to the slow tier.
        tracing::error!("marking {user_pk} hot: {e}");
    }

    Ok((StatusCode::OK, Json(serde_json::json!({ "address": address }))))
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
        "pay_amount_usdt": intent.amount_usdt,
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
/// 503s while `config.redemptions_enabled` is false (default) — the treasury's payout rail is a
/// real TRC-20 transfer now, but the rollout that funds and verifies its float hasn't run yet;
/// see `OrchConfig::redemptions_enabled`'s doc comment for why this gate exists before anything
/// else in this handler runs.
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
        // 503, not 400: this isn't a malformed request, it's a feature not turned on yet — the
        // treasury's payout rail itself is real, but redemptions_enabled stays false until its
        // rollout finishes (see OrchConfig::redemptions_enabled). Naming that plainly in the body
        // keeps a caller from retrying forever thinking it's transient backpressure, or from
        // concluding their address/amount was somehow invalid.
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

// There is no webhook route any more.
//
// `POST /webhooks/bitcart` existed because Bitcart's IPN was unsigned and never retried, so it was
// treated as a wake-up ping and every state it could reach had to be reachable by the poller alone
// anyway. With detection moved onto our own custody watcher (custody.rs) the ping has nothing to
// announce, and removing it also removes the only unauthenticated route this service exposed —
// which was a standing DoS surface against a 5-connection pool shared with the deposit routes.

pub fn router(pool: PgPool, config: OrchConfig, deriver: Arc<AddressDeriver>) -> Router {
    let cors = build_cors(&config.allowed_origins);
    let state = AppState { pool, config, deriver };
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/deposits", post(create_deposit_handler))
        .route("/api/v1/deposits/:id", get(get_deposit_handler))
        .route("/api/v1/redemptions", post(create_redemption_handler))
        .route("/api/v1/redemptions/:id", get(get_redemption_handler))
        .layer(cors)
        .with_state(state)
}
