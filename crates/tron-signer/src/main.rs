//! The signer's HTTP surface.
//!
//! The sweep route's shape IS its security argument: it accepts an INDEX and nothing else, so no
//! field a caller sets can redirect funds. That holds on the GasFree rail too — every field of a
//! GasFree permit comes from this service's config or from the chain.
//!
//! `/internal/addresses/:index` and `/internal/xpub` move nothing; they publish derived addresses.
//!
//! The payout route cannot make that claim and does not pretend to — it takes a destination and an
//! amount because a redemption has no other way to express them. Its bound is different: the source
//! is always the payout float, so the most a hostile caller moves is the float balance, and the
//! per-tx cap bounds a single request. Here the bearer token is load-bearing, not defence in depth.
//!
//! The fund-float route goes further than sweep: it takes NO parameters at all. Source, destination
//! and token are fixed and both addresses are derived from the mnemonic, so the only thing it can
//! be asked for is the one correction it exists to make.
//!
//! The activate-float route takes no parameters either, and it costs at most one activation fee, once:
//! after the first call the float answers `already_active`, or "in flight" while that call is pending.
//! The rule that the reserve's surplus must cover that fee lives in the clutch-deploy workflow that
//! calls it, not here — so for that rule the bearer token is load-bearing.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use tron_signer::keys::Signer;
use tron_signer::sweep::{
    activate_float_response, fund_float_response, load_gasfree_config, payout_response, sweep_response, trace_response, validate_payout_cap, ActivateFloatOutcome, FundFloatOutcome,
    PayoutOutcome, SelfTest, SweepClient, SweepConfig,
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

/// The account xpub, the fee address, and the payout address, so all can be read off the
/// service that owns the private half rather than transcribed by hand. Public material — a
/// mistyped xpub over there means every deposit address is one this service cannot sweep.
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
    // Where redemptions are paid from on the GasFree rail. The treasury derives the same address
    // from its PAYOUT_FLOAT_ADDRESS, which is the plain float above (payout_address), so the reserve
    // counts the float the signer spends from.
    let payout_gasfree_address = s.sweeper.gasfree_address_for(&payout_address).map_err(|e| {
        tracing::error!("GasFree float address derivation failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(json!({
        "account_xpub": s.signer.account_xpub(),
        "fee_address": fee_address,
        "payout_address": payout_address,
        "payout_gasfree_address": payout_gasfree_address,
    })))
}

/// Both deposit addresses of `index`: the plain one and, when GasFree is on, its GasFree account.
///
/// Public material, and it moves nothing. The treasury reads it to decide how much of a deposit to
/// hold back: USDT paid to the GasFree address pays a relay fee when it is swept, USDT paid to the
/// plain address does not. It asks here, where the addresses are derived, rather than trusting the
/// address the orchestrator reported, because that fee must not be the orchestrator's choice.
async fn addresses(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(index): Path<u32>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    authed(&headers, &s.token)?;
    // Hardened indexes are never handed out: the orchestrator derives from an xpub and cannot
    // reach them, so a question about one is about an address no depositor has.
    if index >= 0x8000_0000 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let plain = s.signer.address_at(index).map_err(|e| {
        tracing::error!("address derivation for index {index} failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let gasfree = s.sweeper.gasfree_address_for(&plain).map_err(|e| {
        tracing::error!("GasFree address derivation for index {index} failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(json!({"index": index, "plain": plain, "gasfree": gasfree})))
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
        // Not errors, any of them: a worker must be able to tell "already empty", "funded, sweep
        // next pass", "permit with the relay" and "wait" apart from a genuine failure.
        Ok(outcome) => {
            tracing::info!(index = req.index, ?outcome, "sweep");
            Ok(Json(sweep_response(&outcome)))
        }
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
                PayoutOutcome::Submitted { trace_id, .. } => tracing::info!(intent_id = %req.intent_id, to = %req.to, amount_usdt = req.amount_usdt, %trace_id, "payout permit submitted"),
                PayoutOutcome::RelayRefused { reason, nonce, deadline } => tracing::warn!(intent_id = %req.intent_id, %reason, nonce, deadline, "payout permit refused by the relay"),
                PayoutOutcome::FloatNotActive { float_address } => tracing::warn!(intent_id = %req.intent_id, %float_address, "the GasFree float is not activated yet"),
                PayoutOutcome::Refused(reason) => tracing::warn!(intent_id = %req.intent_id, %reason, "payout refused pre-broadcast"),
            }
            Ok(Json(payout_response(&outcome)))
        }
        Err(e) => {
            tracing::error!(intent_id = %req.intent_id, "payout failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// The relay's record of a permit, so the treasury can find a GasFree payout's transaction without
/// holding the relay's API key. Moves nothing. The id is checked before it reaches a URL.
async fn gasfree_trace(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(trace_id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    authed(&headers, &s.token)?;
    if !tron_signer::relay::is_trace_id(&trace_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    match s.sweeper.gasfree_trace(&trace_id).await {
        Ok(Some(trace)) => Ok(Json(trace_response(&trace))),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::warn!(%trace_id, "trace lookup failed: {e}");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

/// No request struct, deliberately: there is nothing to deserialize.
///
/// A `to`, an `amount` or a `contract` here would each turn "move the fee account's misplaced USDT
/// to where it belongs" into "move what you are told" — and unlike payout, there is no redemption
/// that needs to express them. The route is POST because it moves money, not because it carries a
/// body; a body sent anyway is ignored rather than read.
async fn fund_float(State(s): State<AppState>, headers: HeaderMap) -> Result<Json<serde_json::Value>, StatusCode> {
    authed(&headers, &s.token)?;
    match s.sweeper.fund_float(&s.signer).await {
        Ok(outcome) => {
            match &outcome {
                FundFloatOutcome::Funded { tx_id, amount_usdt } => {
                    tracing::info!(%tx_id, amount_usdt, "moved the fee account's USDT to the payout float")
                }
                FundFloatOutcome::NothingToMove => tracing::info!("fee account holds no USDT; nothing to move"),
                FundFloatOutcome::FeeAccountDry { fee_address, have_sun, need_sun } => {
                    tracing::warn!(%fee_address, have_sun, need_sun, "fee account cannot pay for its own transfer")
                }
                FundFloatOutcome::Refused(reason) => tracing::warn!(%reason, "fund-float refused pre-broadcast"),
            }
            Ok(Json(fund_float_response(&outcome)))
        }
        // Only reachable at or after the broadcast call, so the operator must treat it as "may have
        // moved" and read the chain — every provable non-broadcast is a `Refused` above.
        Err(e) => {
            tracing::error!("fund-float failed, possibly after broadcasting: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// No request struct, like fund-float: source, receiver, value and fee are all fixed.
async fn activate_float(State(s): State<AppState>, headers: HeaderMap) -> Result<Json<serde_json::Value>, StatusCode> {
    authed(&headers, &s.token)?;
    match s.sweeper.activate_float(&s.signer).await {
        Ok(outcome) => {
            match &outcome {
                ActivateFloatOutcome::Submitted { trace_id } => tracing::info!(%trace_id, "GasFree float activation submitted"),
                ActivateFloatOutcome::AlreadyActive { float_address } => tracing::info!(%float_address, "GasFree float already active"),
                ActivateFloatOutcome::FloatDry { float_address, have_usdt, need_usdt } => {
                    tracing::warn!(%float_address, have_usdt, need_usdt, "GasFree float cannot pay for its activation")
                }
                ActivateFloatOutcome::Refused(reason) => tracing::warn!(%reason, "GasFree float activation refused"),
            }
            Ok(Json(activate_float_response(&outcome)))
        }
        // Only after a permit was sent: the operator must read the float on chain.
        Err(e) => {
            tracing::error!("GasFree float activation gave no clear answer: {e}");
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

    // Off unless APP_GASFREE_API_KEY is set. A half-configured rail stops the signer here, not at
    // the first deposit.
    let gasfree = load_gasfree_config(|name| std::env::var(name).ok()).unwrap_or_else(|e| panic!("{e}"));

    let mut sweeper = SweepClient::new(SweepConfig {
        trongrid_url: env("APP_TRONGRID_URL"),
        trongrid_api_key: std::env::var("APP_TRONGRID_API_KEY").unwrap_or_default(),
        treasury_address: env("APP_TREASURY_ADDRESS"),
        usdt_contract: env("APP_USDT_CONTRACT"),
        fee_limit: std::env::var("APP_FEE_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(150_000_000),
        per_tx_payout_cap_usdt: validate_payout_cap(&env("APP_PER_TX_PAYOUT_CAP_USDT")).unwrap_or_else(|e| panic!("{e}")),
    });
    if let Some(cfg) = gasfree {
        tracing::info!(chain_id = cfg.chain.chain_id, payouts = cfg.payouts, "GasFree rail on");
        sweeper = sweeper.with_gasfree(cfg);
    }
    let sweeper = Arc::new(sweeper);
    match sweeper.gasfree_self_test_within(&signer, std::time::Duration::from_secs(30)).await {
        SelfTest::Passed => {}
        SelfTest::Failed(e) => panic!("GasFree self-test failed: {e}"),
        SelfTest::Unreachable(e) => tracing::warn!(
            "GasFree self-test could not reach TronGrid, so it did not run; every permit is still checked \
             before it is signed: {e}"
        ),
    }

    let state = AppState { signer, sweeper, token: env("APP_SIGNER_TOKEN") };
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/internal/xpub", get(xpub))
        .route("/internal/addresses/:index", get(addresses))
        .route("/internal/sweep", post(sweep))
        .route("/internal/payout", post(payout))
        .route("/internal/gasfree/trace/:trace_id", get(gasfree_trace))
        .route("/internal/fund-float", post(fund_float))
        .route("/internal/activate-float", post(activate_float))
        .with_state(state);

    let addr = std::env::var("APP_HTTP_ADDR").unwrap_or_else(|_| "0.0.0.0:8093".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    tracing::info!("tron-signer listening on {addr}");
    axum::serve(listener, app).await.expect("serve");
}
