//! One fake TronGrid and one fake GasFree relay on the same port, and every GasFree test.
//!
//! wiremock is deliberately not a dependency of the crate that holds the mnemonic (see
//! `fund_float_tests`); axum already is, so a stand-in costs a few lines and nothing in Cargo.toml.

use super::*;
use crate::keys::Signer;
use crate::relay::RelayConfig;
use crate::sweep::SweepConfig;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

const MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const USDT: &str = "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf";
const CUSTODY: &str = "TQwgeRaDt4FSJSsncmFNcbMNTfFpjvjwFX";
const PROVIDER: &str = "TDbJyQ6g1Lx9BAfEEeN5S5TMjjDRAVFCaA";
const TRACE_ID: &str = "6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60";
/// Read from Nile on 2026-09-24.
const BEACON_IMPL: &str = "b8eda40b467b45af107f198e94cc2fa1378adf50";
const CONTROLLER_IMPL: &str = "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441";
const ACTIVATE_MAX: i64 = 1_500_000;
const TRANSFER_MAX: i64 = 500_000;
const FLOAT_TARGET: i64 = 50_000_000;

/// Everything the fake chain and relay know. Tests set it up, run one call, then read `calls`.
#[derive(Default)]
struct WorldState {
    /// balanceOf, by address.
    usdt: HashMap<String, i64>,
    /// TRX in sun, by address. Only the plain-address path reads it.
    trx: HashMap<String, i64>,
    /// Addresses `getcontract` reports as deployed.
    contracts: HashSet<String>,
    /// `nonces(user)`, by the user's ABI word.
    nonces: HashMap<String, u64>,
    /// `implementation()`, by proxy address, as 40 hex.
    implementations: HashMap<String, String>,
    /// `getGasFreeAddress(user)`, by the user's ABI word, as 40 hex.
    gasfree_of: HashMap<String, String>,
    /// The relay's account `data`, by owner address.
    accounts: HashMap<String, serde_json::Value>,
    /// The relay's whole reply to a submit.
    submit_reply: String,
    /// The relay's whole reply to a trace lookup.
    trace_reply: String,
    /// Every request in order: a short name and what was sent.
    calls: Vec<(String, serde_json::Value)>,
}

#[derive(Clone)]
struct World(Arc<Mutex<WorldState>>);

async fn constant(State(w): State<World>, Json(b): Json<serde_json::Value>) -> Json<serde_json::Value> {
    let mut s = w.0.lock().unwrap();
    let selector = b["function_selector"].as_str().unwrap_or_default().to_string();
    s.calls.push((selector.clone(), b.clone()));
    let parameter = b["parameter"].as_str().unwrap_or_default().to_string();
    let contract = b["contract_address"].as_str().unwrap_or_default().to_string();
    let owner = b["owner_address"].as_str().unwrap_or_default().to_string();
    let word = match selector.as_str() {
        "balanceOf(address)" => format!("{:064x}", s.usdt.get(&owner).copied().unwrap_or(0)),
        "nonces(address)" => format!("{:064x}", s.nonces.get(&parameter).copied().unwrap_or(0)),
        "implementation()" => format!("{:0>64}", s.implementations.get(&contract).cloned().unwrap_or_default()),
        "getGasFreeAddress(address)" => format!("{:0>64}", s.gasfree_of.get(&parameter).cloned().unwrap_or_default()),
        other => panic!("the fake chain was asked for {other}"),
    };
    Json(serde_json::json!({"result": {"result": true}, "constant_result": [word]}))
}

async fn getcontract(State(w): State<World>, Json(b): Json<serde_json::Value>) -> Json<serde_json::Value> {
    let mut s = w.0.lock().unwrap();
    s.calls.push(("getcontract".into(), b.clone()));
    let address = b["value"].as_str().unwrap_or_default().to_string();
    // Shaped like the live reply for an activated GasFree account: a contract record whose
    // bytecode is EMPTY. An address with no contract gets `{}`.
    Json(if s.contracts.contains(&address) {
        serde_json::json!({"contract_address": address, "code_hash": "c0de", "bytecode": ""})
    } else {
        serde_json::json!({})
    })
}

async fn trx_account(State(w): State<World>, Path(address): Path<String>) -> Json<serde_json::Value> {
    let mut s = w.0.lock().unwrap();
    s.calls.push(("trx_balance".into(), serde_json::json!(address)));
    let balance = s.trx.get(&address).copied().unwrap_or(0);
    Json(serde_json::json!({"data": [{"balance": balance}]}))
}

async fn relay_account(State(w): State<World>, Path(owner): Path<String>) -> String {
    let mut s = w.0.lock().unwrap();
    s.calls.push(("relay_account".into(), serde_json::json!(owner)));
    match s.accounts.get(&owner) {
        Some(data) => serde_json::json!({"code": 200, "reason": null, "message": null, "data": data}).to_string(),
        None => serde_json::json!({"code": 400, "reason": "GasFreeAddressNotFoundException", "message": owner, "data": null})
            .to_string(),
    }
}

async fn relay_submit(State(w): State<World>, Json(b): Json<serde_json::Value>) -> String {
    let mut s = w.0.lock().unwrap();
    s.calls.push(("submit".into(), b));
    s.submit_reply.clone()
}

async fn relay_trace(State(w): State<World>, Path(id): Path<String>) -> String {
    let mut s = w.0.lock().unwrap();
    s.calls.push(("trace".into(), serde_json::json!(id)));
    s.trace_reply.clone()
}

async fn spawn(state: WorldState) -> (String, World) {
    let world = World(Arc::new(Mutex::new(state)));
    let app = Router::new()
        .route("/wallet/triggerconstantcontract", post(constant))
        .route("/wallet/getcontract", post(getcontract))
        .route("/v1/accounts/:address", get(trx_account))
        .route("/nile/api/v1/address/:owner", get(relay_account))
        .route("/nile/api/v1/gasfree/submit", post(relay_submit))
        .route("/nile/api/v1/gasfree/:trace", get(relay_trace))
        .with_state(world.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, world)
}

fn signer() -> Signer {
    Signer::from_mnemonic(MNEMONIC, "").unwrap()
}

/// The GasFree account of deposit index 0.
fn g0(s: &Signer) -> String {
    gasfree::gasfree_address(&gasfree::NILE, &s.address_at(0).unwrap()).unwrap()
}

/// The GasFree float, F = gasfree(2/0).
fn float_of(s: &Signer) -> String {
    gasfree::gasfree_address(&gasfree::NILE, &s.payout_address().unwrap()).unwrap()
}

fn relay_account_json(owner: &str, gasfree_address: &str, nonce: u64) -> serde_json::Value {
    serde_json::json!({
        "accountAddress": owner, "gasFreeAddress": gasfree_address, "active": false, "nonce": nonce,
        "allowSubmit": true, "assets": [{"tokenAddress": USDT, "frozen": 0}],
    })
}

/// A Nile world with nothing in flight: GasFree runs the reviewed code, the relay agrees with this
/// signer about both addresses, every nonce is 0, and a submit is accepted.
fn healthy(s: &Signer) -> WorldState {
    let mut w = WorldState::default();
    w.implementations.insert(gasfree::NILE.beacon.into(), BEACON_IMPL.into());
    w.implementations.insert(gasfree::NILE.controller.into(), CONTROLLER_IMPL.into());
    w.contracts.insert(gasfree::NILE.controller.into());
    for owner in [s.address_at(0).unwrap(), s.payout_address().unwrap()] {
        let g = gasfree::gasfree_address(&gasfree::NILE, &owner).unwrap();
        w.accounts.insert(owner.clone(), relay_account_json(&owner, &g, 0));
        w.gasfree_of.insert(abi_address(&owner).unwrap(), abi_address(&g).unwrap()[24..].to_string());
    }
    w.submit_reply =
        serde_json::json!({"code": 200, "reason": null, "message": null, "data": {"id": TRACE_ID, "state": "WAITING"}})
            .to_string();
    w
}

fn sweep_config(url: &str) -> SweepConfig {
    SweepConfig {
        trongrid_url: url.to_string(),
        trongrid_api_key: String::new(),
        treasury_address: CUSTODY.into(),
        usdt_contract: USDT.into(),
        fee_limit: 150_000_000,
        per_tx_payout_cap_usdt: 25_000_000,
    }
}

fn gasfree_config(url: &str, payouts: bool) -> GasFreeConfig {
    GasFreeConfig {
        chain: &gasfree::NILE,
        relay: RelayConfig { base_url: format!("{url}/nile"), api_key: "k".into(), api_secret: "s".into() },
        service_provider: PROVIDER.into(),
        activate_fee_max_usdt: ACTIVATE_MAX,
        transfer_fee_max_usdt: TRANSFER_MAX,
        expected_beacon_implementation: BEACON_IMPL.into(),
        expected_controller_implementation: CONTROLLER_IMPL.into(),
        payout_float_target_usdt: FLOAT_TARGET,
        deadline_secs: 180,
        payouts,
    }
}

/// A client with GasFree on, and redemptions paid from the GasFree float.
fn client(url: &str) -> SweepClient {
    SweepClient::new(sweep_config(url)).with_gasfree(gasfree_config(url, true))
}

fn named(world: &World, name: &str) -> Vec<serde_json::Value> {
    world.0.lock().unwrap().calls.iter().filter(|(n, _)| n == name).map(|(_, b)| b.clone()).collect()
}

// ---- settings ----

const FULL: [(&str, &str); 11] = [
    ("APP_TRANSFER_RAIL", "gasfree"),
    ("APP_GASFREE_API_KEY", "key"),
    ("APP_GASFREE_API_SECRET", "secret"),
    ("APP_GASFREE_API_URL", "https://open-test.gasfree.io/nile/"),
    ("APP_GASFREE_NETWORK", "nile"),
    ("APP_GASFREE_SERVICE_PROVIDER", PROVIDER),
    ("APP_GASFREE_ACTIVATE_FEE_MAX_USDT", "1500000"),
    ("APP_GASFREE_TRANSFER_FEE_MAX_USDT", "500000"),
    ("APP_GASFREE_EXPECTED_IMPLEMENTATION", "0xB8EDA40B467B45AF107F198E94CC2FA1378ADF50"),
    ("APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION", "0x2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441"),
    ("APP_PAYOUT_FLOAT_TARGET_USDT", "50000000"),
];

/// `FULL` with `changes` applied. An empty value removes the setting.
fn setting(changes: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let mut map: HashMap<String, String> = FULL.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    for (k, v) in changes {
        if v.is_empty() {
            map.remove(*k);
        } else {
            map.insert(k.to_string(), v.to_string());
        }
    }
    move |k: &str| map.get(k).cloned()
}

#[test]
fn without_an_api_key_the_rail_stays_off() {
    assert!(matches!(load_gasfree_config(|_| None), Ok(None)), "an empty environment is today's signer");
    assert!(matches!(
        load_gasfree_config(setting(&[("APP_TRANSFER_RAIL", "trx"), ("APP_GASFREE_API_KEY", "")])),
        Ok(None)
    ));
}

#[test]
fn the_gasfree_rail_without_an_api_key_is_refused() {
    assert!(load_gasfree_config(setting(&[("APP_GASFREE_API_KEY", "")])).is_err());
}

#[test]
fn a_full_setting_parses_and_normalises() {
    let cfg = load_gasfree_config(setting(&[])).unwrap().expect("an API key turns GasFree on");
    assert_eq!(cfg.chain.chain_id, gasfree::NILE.chain_id);
    assert_eq!(cfg.relay.base_url, "https://open-test.gasfree.io/nile", "a trailing slash would double up in every path");
    assert_eq!(cfg.service_provider, PROVIDER);
    assert_eq!((cfg.activate_fee_max_usdt, cfg.transfer_fee_max_usdt), (1_500_000, 500_000));
    assert_eq!(cfg.expected_beacon_implementation, BEACON_IMPL, "lowercase, without 0x");
    assert_eq!(cfg.expected_controller_implementation, CONTROLLER_IMPL);
    assert_eq!(cfg.payout_float_target_usdt, 50_000_000);
    assert_eq!(cfg.deadline_secs, 180, "the relay's recommended default");
    assert!(cfg.payouts, "APP_TRANSFER_RAIL=gasfree pays redemptions from the GasFree float");

    let trx = load_gasfree_config(setting(&[("APP_TRANSFER_RAIL", "trx")])).unwrap().unwrap();
    assert!(!trx.payouts, "GasFree can be on for sweeps while redemptions still go by TRX");
}

#[test]
fn each_bad_setting_is_refused_at_boot() {
    for change in [
        ("APP_GASFREE_TRANSFER_FEE_MAX_USDT", "0"),
        ("APP_GASFREE_ACTIVATE_FEE_MAX_USDT", "-1"),
        ("APP_GASFREE_EXPECTED_IMPLEMENTATION", "0x1234"),
        ("APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION", ""),
        ("APP_GASFREE_DEADLINE_SECS", "59"),
        ("APP_GASFREE_DEADLINE_SECS", "601"),
        ("APP_GASFREE_NETWORK", "shasta"),
        ("APP_TRANSFER_RAIL", "both"),
        ("APP_GASFREE_API_URL", ""),
        ("APP_GASFREE_API_SECRET", ""),
        ("APP_GASFREE_SERVICE_PROVIDER", "not-an-address"),
        ("APP_PAYOUT_FLOAT_TARGET_USDT", ""),
    ] {
        assert!(load_gasfree_config(setting(&[change])).is_err(), "{change:?} must stop the signer at boot");
    }
}

// ---- chain reads ----

#[tokio::test]
async fn activation_is_read_from_the_contract_record_not_the_bytecode() {
    let s = signer();
    let mut w = healthy(&s);
    w.contracts.insert(g0(&s));
    let (url, _) = spawn(w).await;
    let c = client(&url);
    assert!(c.has_contract(&g0(&s)).await.unwrap(), "a contract record with empty bytecode is an activated account");
    assert!(!c.has_contract(&float_of(&s)).await.unwrap(), "`{}` is an address with no contract");
}

// ---- the boot check ----

#[tokio::test]
async fn the_self_test_passes_on_the_right_network() {
    let s = signer();
    let (url, world) = spawn(healthy(&s)).await;
    assert_eq!(client(&url).gasfree_self_test(&s).await, SelfTest::Passed);
    let asked = &named(&world, "getGasFreeAddress(address)")[0];
    assert_eq!(asked["contract_address"], gasfree::NILE.controller, "the chain's own answer, from the controller");
    assert_eq!(asked["parameter"], abi_address(&s.address_at(0).unwrap()).unwrap(), "about deposit index 0");
}

#[tokio::test]
async fn the_self_test_fails_when_the_controller_disagrees() {
    let s = signer();
    let mut w = healthy(&s);
    w.gasfree_of.insert(abi_address(&s.address_at(0).unwrap()).unwrap(), "33".repeat(20));
    let (url, _) = spawn(w).await;
    match client(&url).gasfree_self_test(&s).await {
        SelfTest::Failed(why) => assert!(why.contains("derives"), "{why}"),
        other => panic!("got {other:?}"),
    }
}

#[tokio::test]
async fn the_self_test_fails_when_the_controller_is_not_a_contract() {
    let s = signer();
    let mut w = healthy(&s);
    w.contracts.clear();
    let (url, _) = spawn(w).await;
    match client(&url).gasfree_self_test(&s).await {
        SelfTest::Failed(why) => assert!(why.contains("not a contract"), "{why}"),
        other => panic!("got {other:?}"),
    }
}

#[tokio::test]
async fn the_self_test_is_not_fatal_when_trongrid_is_down() {
    // A dead port: nothing answers.
    let result = client("http://127.0.0.1:1").gasfree_self_test(&signer()).await;
    assert!(matches!(result, SelfTest::Unreachable(_)), "got {result:?}");
}

#[test]
fn the_gasfree_address_is_only_known_when_gasfree_is_on() {
    let s = signer();
    let owner = s.address_at(0).unwrap();
    let off = SweepClient::new(sweep_config("http://127.0.0.1:1"));
    assert_eq!(off.gasfree_address_for(&owner).unwrap(), None);
    assert_eq!(client("http://127.0.0.1:1").gasfree_address_for(&owner).unwrap(), Some(g0(&s)));
}
