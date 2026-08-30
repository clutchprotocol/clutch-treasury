//! The signer's HTTP surface.
//!
//! Three routes. The sweep route's shape IS its security argument: it accepts an INDEX and nothing
//! else, so no field a caller sets can redirect funds.
//!
//! The payout route cannot make that claim and does not pretend to — it takes a destination and an
//! amount because a redemption has no other way to express them. Its bound is different: the source
//! is always the payout float, so the most a hostile caller moves is the float balance, and the
//! per-tx cap bounds a single request. Here the bearer token is load-bearing, not defence in depth.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use tron_signer::keys::Signer;
use tron_signer::sweep::{
    payout_response, validate_payout_cap, PayoutOutcome, SweepClient, SweepConfig, SweepOutcome,
};

#[derive(Clone)]
struct AppState {
    signer: Arc<Signer>,
    sweeper: Arc<SweepClient>,
    token: String,
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

fn authed(headers: &HeaderMap, expected: &str) -> Result<(), StatusCode> {
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    // Length-independent comparison is overkill for an internal-only token, but the cost is one
    // line and the alternative is explaining why it was fine.
    if got.len() == expected.len() && got.bytes().zip(expected.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0 {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// The account xpub, the fee address, and the payout address, so all can be read off the service that owns the private
/// half rather than transcribed by hand. Public material — a mistyped xpub over there means every
/// deposit address is one this service cannot sweep.
///
/// `fee_address` is where an operator sends the TRX float. It is here rather than only in the log
/// line that fires when the account runs dry, because it is needed BEFORE the first sweep: an
/// unfunded fee account means no deposit can ever be moved.
///
/// `payout_address` is where an operator sends the USDT float. Like `fee_address`, it is needed
/// before the first payout.
async fn xpub(State(s): State<AppState>, headers: HeaderMap) -> Result<Json<serde_json::Value>, StatusCode> {
    authed(&headers, &s.token)?;
    let fee_address = s.signer.fee_address().map_err(|e| {
        tracing::error!("fee address derivation failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let payout_address = s.signer.payout_address().map_err(|e| {
        tracing::error!("payout address derivation failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(json!({
        "account_xpub": s.signer.account_xpub(),
        "fee_address": fee_address,
        "payout_address": payout_address,
    })))
}

/// ONLY an index. Adding `to`, `contract` or `amount` here would delete the reason this service
/// exists — see sweep.rs's module docs before changing this struct.
#[derive(Deserialize)]
struct SweepRequest {
    index: u32,
}

/// Unlike `SweepRequest` this DOES carry a destination and an amount, because a payout has no other
/// way to know them. What it does NOT carry is a contract or a source: the token is config and the
/// source is always the float. See the spec before widening this.
///
/// `intent_id` is not used for signing. It is logged so a broadcast can be tied back to the
/// redemption that caused it — which is the only way to resolve an ambiguous payout later.
#[derive(Deserialize)]
struct PayoutRequest {
    intent_id: String,
    to: String,
    amount_usdt: i64,
}

async fn sweep(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SweepRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    authed(&headers, &s.token)?;
    match s.sweeper.sweep(&s.signer, req.index).await {
        Ok(SweepOutcome::Swept { tx_id, amount_usdt }) => {
            tracing::info!("swept index {} : {amount_usdt} micro-USDT in {tx_id}", req.index);
            Ok(Json(json!({"status": "swept", "tx_id": tx_id, "amount_usdt": amount_usdt})))
        }
        // Not errors: a worker re-running over an already-empty address, or one that just had its
        // fee funded, must be able to tell those apart from a genuine failure and act differently.
        Ok(SweepOutcome::NothingToSweep) => Ok(Json(json!({"status": "nothing_to_sweep"}))),
        Ok(SweepOutcome::Funded { tx_id, amount_sun }) => {
            Ok(Json(json!({"status": "funded", "tx_id": tx_id, "amount_sun": amount_sun})))
        }
        // The one outcome no retry resolves: only an operator can top the account up.
        Ok(SweepOutcome::FeeAccountDry { fee_address, have_sun, need_sun }) => Ok(Json(json!({
            "status": "fee_account_dry",
            "fee_address": fee_address,
            "have_sun": have_sun,
            "need_sun": need_sun,
        }))),
        Err(e) => {
            tracing::error!("sweep of index {} failed: {e}", req.index);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn payout(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PayoutRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    authed(&headers, &s.token)?;
    if req.amount_usdt <= 0 {
        tracing::warn!(intent_id = %req.intent_id, "payout refused: non-positive amount");
        return Err(StatusCode::BAD_REQUEST);
    }
    match s.sweeper.payout(&s.signer, &req.to, req.amount_usdt).await {
        Ok(outcome) => {
            match &outcome {
                PayoutOutcome::Paid { tx_id } => tracing::info!(intent_id = %req.intent_id, to = %req.to, amount_usdt = req.amount_usdt, %tx_id, "paid"),
                PayoutOutcome::CapExceeded { limit_usdt } => tracing::warn!(intent_id = %req.intent_id, amount_usdt = req.amount_usdt, limit_usdt, "payout over cap"),
                PayoutOutcome::FloatDry { float_address, have_usdt, need_usdt } => tracing::warn!(intent_id = %req.intent_id, %float_address, have_usdt, need_usdt, "payout float dry"),
                PayoutOutcome::NeedsTrx { tx_id, amount_sun } => tracing::info!(intent_id = %req.intent_id, %tx_id, amount_sun, "funded the payout float with TRX"),
            }
            Ok(Json(payout_response(&outcome)))
        }
        Err(e) => {
            tracing::error!(intent_id = %req.intent_id, "payout failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    dotenv::dotenv().ok();

    // Fail at boot on a bad mnemonic. A typo is a valid but DIFFERENT wallet, and the first symptom
    // would be deposits arriving at addresses this service cannot sweep.
    let signer = Arc::new(
        Signer::from_mnemonic(&env("APP_DEPOSIT_MNEMONIC"), &std::env::var("APP_DEPOSIT_PASSPHRASE").unwrap_or_default())
            .expect("APP_DEPOSIT_MNEMONIC must be a valid BIP39 mnemonic"),
    );
    tracing::info!("deposit wallet account xpub: {}", signer.account_xpub());

    let sweeper = Arc::new(SweepClient::new(SweepConfig {
        trongrid_url: env("APP_TRONGRID_URL"),
        trongrid_api_key: std::env::var("APP_TRONGRID_API_KEY").unwrap_or_default(),
        treasury_address: env("APP_TREASURY_ADDRESS"),
        usdt_contract: env("APP_USDT_CONTRACT"),
        fee_limit: std::env::var("APP_FEE_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(150_000_000),
        per_tx_payout_cap_usdt: validate_payout_cap(&env("APP_PER_TX_PAYOUT_CAP_USDT")).unwrap_or_else(|e| panic!("{e}")),
    }));

    let state = AppState { signer, sweeper, token: env("APP_SIGNER_TOKEN") };
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/internal/xpub", get(xpub))
        .route("/internal/sweep", post(sweep))
        .route("/internal/payout", post(payout))
        .with_state(state);

    let addr = std::env::var("APP_HTTP_ADDR").unwrap_or_else(|_| "0.0.0.0:8093".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    tracing::info!("tron-signer listening on {addr}");
    axum::serve(listener, app).await.expect("serve");
}
