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
    /// When true, `getcontract` answers like a rate-limited TronGrid: `{"Error": "request rate exceeded"}`.
    getcontract_error: bool,
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
    if s.getcontract_error {
        return Json(serde_json::json!({"Error": "request rate exceeded"}));
    }
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
    assert!(!c.has_contract(&float_of(&s)).await.unwrap(), "`{{}}` is an address with no contract");
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

// ---- what every permit needs: the nonce, the tripwire, the signature ----

/// The key that signed a submitted permit, recovered from the permit's own fields and `sig`. So a
/// test that uses it proves the relay got a permit signed over exactly what it was sent.
fn signer_of(body: &serde_json::Value) -> k256::ecdsa::VerifyingKey {
    let permit = gasfree::Permit {
        token: body["token"].as_str().unwrap(),
        service_provider: body["serviceProvider"].as_str().unwrap(),
        user: body["user"].as_str().unwrap(),
        receiver: body["receiver"].as_str().unwrap(),
        value: body["value"].as_u64().unwrap(),
        max_fee: body["maxFee"].as_u64().unwrap(),
        deadline: body["deadline"].as_u64().unwrap(),
        version: body["version"].as_u64().unwrap(),
        nonce: body["nonce"].as_u64().unwrap(),
    };
    let hash = gasfree::permit_hash(&gasfree::NILE, &permit).unwrap();
    let sig = hex::decode(body["sig"].as_str().unwrap()).unwrap();
    assert_eq!(sig.len(), 65, "r, s and v");
    assert!(sig[64] == 27 || sig[64] == 28, "v must be 27 or 28, got {}", sig[64]);
    let signature = Signature::from_slice(&sig[..64]).unwrap();
    let recid = RecoveryId::from_byte(sig[64] - 27).unwrap();
    k256::ecdsa::VerifyingKey::recover_from_prehash(&hash, &signature, recid).unwrap()
}

#[tokio::test]
async fn the_nonce_is_the_controllers_not_the_relays() {
    let s = signer();
    let owner = s.address_at(0).unwrap();
    let mut w = healthy(&s);
    w.nonces.insert(abi_address(&owner).unwrap(), 7);
    let (url, world) = spawn(w).await;
    assert_eq!(client(&url).chain_nonce(&gasfree::NILE, &owner).await.unwrap(), 7);
    let call = &named(&world, "nonces(address)")[0];
    assert_eq!(call["contract_address"], gasfree::NILE.controller, "nonces live on the controller");
}

#[tokio::test]
async fn a_changed_beacon_or_controller_is_named() {
    let s = signer();
    let (url, _) = spawn(healthy(&s)).await;
    let cfg = gasfree_config(&url, true);
    assert_eq!(client(&url).code_changed(&cfg).await.unwrap(), None, "the reviewed code passes");

    let mut w = healthy(&s);
    w.implementations.insert(gasfree::NILE.beacon.into(), "11".repeat(20));
    let (url, _) = spawn(w).await;
    let why = client(&url).code_changed(&gasfree_config(&url, true)).await.unwrap().expect("a new beacon implementation");
    assert!(why.contains("beacon") && why.contains(&"11".repeat(20)), "{why}");

    let mut w = healthy(&s);
    w.implementations.insert(gasfree::NILE.controller.into(), "22".repeat(20));
    let (url, _) = spawn(w).await;
    let why = client(&url).code_changed(&gasfree_config(&url, true)).await.unwrap().expect("a new controller implementation");
    assert!(why.contains("controller"), "{why}");
}

#[test]
fn a_permit_signature_recovers_to_the_signing_key_with_v_27_or_28() {
    let s = signer();
    let key = s.signing_key_at(0).unwrap();
    let owner = s.address_at(0).unwrap();
    let permit = gasfree::Permit {
        token: USDT,
        service_provider: PROVIDER,
        user: &owner,
        receiver: CUSTODY,
        value: 8_000_000,
        max_fee: 2_000_000,
        deadline: 1_790_000_000,
        version: 1,
        nonce: 0,
    };
    let sig = sign_permit(&key, &gasfree::NILE, &permit).unwrap();
    assert_eq!(sig.len(), 130, "65 bytes as hex, no 0x");
    let body = serde_json::json!({
        "token": USDT, "serviceProvider": PROVIDER, "user": owner, "receiver": CUSTODY,
        "value": 8_000_000u64, "maxFee": 2_000_000u64, "deadline": 1_790_000_000u64, "version": 1, "nonce": 0, "sig": sig,
    });
    assert_eq!(&signer_of(&body), key.verifying_key(), "the signature must be over this permit, by this key");
}

// ---- the GasFree sweep ----

/// The whole rail in one test: everything above the fee, to the float while it is below its
/// target, by a permit that D signed over exactly what the relay received.
#[tokio::test]
async fn a_deposit_is_swept_by_a_permit_for_everything_above_the_fee() {
    let s = signer();
    let owner = s.address_at(0).unwrap();
    let mut w = healthy(&s);
    w.usdt.insert(g0(&s), 10_000_000);
    w.nonces.insert(abi_address(&owner).unwrap(), 0);
    let (url, world) = spawn(w).await;

    let outcome = client(&url).sweep(&s, 0).await.unwrap();

    let fee = ACTIVATE_MAX + TRANSFER_MAX; // never activated: the first transfer also activates
    assert_eq!(
        outcome,
        SweepOutcome::Pending {
            trace_id: TRACE_ID.into(),
            gasfree_address: g0(&s),
            receiver: float_of(&s),
            value_usdt: 10_000_000 - fee,
            max_fee_usdt: fee,
        }
    );
    let submits = named(&world, "submit");
    assert_eq!(submits.len(), 1, "exactly one permit");
    let p = &submits[0];
    assert_eq!(p["user"], owner, "the permit's user is the plain wallet D, never G");
    assert_eq!(p["receiver"], float_of(&s), "the float is below its target, so it receives");
    assert_eq!(p["token"], USDT, "the token is config, never a parameter");
    assert_eq!(p["serviceProvider"], PROVIDER, "the pinned relay");
    assert_eq!(p["value"], 10_000_000 - fee, "value = balance − maxFee: the receiver gets exactly what was minted");
    assert_eq!(p["maxFee"], fee);
    assert_eq!(p["version"], 1);
    assert_eq!(p["nonce"], 0, "the controller's nonce");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let deadline = p["deadline"].as_u64().unwrap();
    assert!(deadline > now + 170 && deadline <= now + 185, "a short deadline, minutes: {deadline} vs {now}");
    assert_eq!(&signer_of(p), s.signing_key_at(0).unwrap().verifying_key(), "signed by D's key over these fields");
}

#[tokio::test]
async fn an_activated_account_holds_back_only_the_transfer_fee() {
    let s = signer();
    let mut w = healthy(&s);
    w.usdt.insert(g0(&s), 10_000_000);
    w.contracts.insert(g0(&s));
    let (url, world) = spawn(w).await;

    client(&url).sweep(&s, 0).await.unwrap();

    let p = &named(&world, "submit")[0];
    assert_eq!(p["maxFee"], TRANSFER_MAX);
    assert_eq!(p["value"], 10_000_000 - TRANSFER_MAX);
}

#[tokio::test]
async fn the_float_stops_receiving_once_it_reaches_its_target() {
    let s = signer();
    let mut w = healthy(&s);
    w.usdt.insert(g0(&s), 10_000_000);
    w.usdt.insert(float_of(&s), FLOAT_TARGET);
    let (url, world) = spawn(w).await;

    client(&url).sweep(&s, 0).await.unwrap();

    assert_eq!(named(&world, "submit")[0]["receiver"], CUSTODY);
}

#[tokio::test]
async fn with_trx_payouts_every_sweep_goes_to_custody() {
    let s = signer();
    let mut w = healthy(&s);
    w.usdt.insert(g0(&s), 10_000_000);
    let (url, world) = spawn(w).await;
    let c = SweepClient::new(sweep_config(&url)).with_gasfree(gasfree_config(&url, false));

    c.sweep(&s, 0).await.unwrap();

    assert_eq!(named(&world, "submit")[0]["receiver"], CUSTODY, "a float that pays nothing is not filled");
    let reads: Vec<_> = named(&world, "balanceOf(address)").iter().map(|b| b["owner_address"].clone()).collect();
    assert!(!reads.contains(&serde_json::json!(float_of(&s))), "the GasFree float is not even read");
}

#[tokio::test]
async fn a_changed_beacon_halts_before_the_relay_is_asked() {
    let s = signer();
    let mut w = healthy(&s);
    w.usdt.insert(g0(&s), 10_000_000);
    w.implementations.insert(gasfree::NILE.beacon.into(), "44".repeat(20));
    let (url, world) = spawn(w).await;

    let outcome = client(&url).sweep(&s, 0).await.unwrap();

    assert!(matches!(outcome, SweepOutcome::Halted { .. }), "got {outcome:?}");
    assert!(named(&world, "relay_account").is_empty() && named(&world, "submit").is_empty(), "nothing signed or sent");
}

#[tokio::test]
async fn a_relay_that_disagrees_about_the_address_halts() {
    let s = signer();
    let owner = s.address_at(0).unwrap();
    let mut w = healthy(&s);
    w.usdt.insert(g0(&s), 10_000_000);
    w.accounts.insert(owner.clone(), relay_account_json(&owner, "TJM1BE5wq1VdHh3gwjUeyaVkvZp9DVYCfC", 0));
    let (url, world) = spawn(w).await;

    let outcome = client(&url).sweep(&s, 0).await.unwrap();

    assert!(matches!(outcome, SweepOutcome::Halted { .. }), "got {outcome:?}");
    assert!(named(&world, "submit").is_empty());
}

#[tokio::test]
async fn a_transfer_in_flight_makes_the_sweep_wait() {
    let s = signer();
    let owner = s.address_at(0).unwrap();
    let in_flight = [
        // The relay's nonce is ahead of the chain's: a permit is queued.
        relay_account_json(&owner, &g0(&s), 1),
        serde_json::json!({"gasFreeAddress": g0(&s), "nonce": 0, "allowSubmit": false}),
        serde_json::json!({"gasFreeAddress": g0(&s), "nonce": 0, "assets": [{"tokenAddress": USDT, "frozen": 2_000_000}]}),
    ];
    for account in in_flight {
        let mut w = healthy(&s);
        w.usdt.insert(g0(&s), 10_000_000);
        w.accounts.insert(owner.clone(), account.clone());
        let (url, world) = spawn(w).await;

        let outcome = client(&url).sweep(&s, 0).await.unwrap();

        assert_eq!(outcome, SweepOutcome::Busy { gasfree_address: g0(&s) }, "for {account}");
        assert!(named(&world, "submit").is_empty(), "two permits in flight would collide on one nonce");
    }
}

#[tokio::test]
async fn a_relay_refusal_is_reported_as_rejected() {
    let s = signer();
    let mut w = healthy(&s);
    w.usdt.insert(g0(&s), 10_000_000);
    w.submit_reply = serde_json::json!({
        "code": 400, "reason": "MaxFeeExceededException", "message": "estimated fee exceeds the limit", "data": null,
    })
    .to_string();
    let (url, _) = spawn(w).await;

    assert_eq!(
        client(&url).sweep(&s, 0).await.unwrap(),
        SweepOutcome::Rejected {
            reason: "MaxFeeExceededException".into(),
            message: "estimated fee exceeds the limit".into(),
        }
    );
}

#[tokio::test]
async fn dust_that_cannot_pay_its_fee_does_not_block_the_plain_address() {
    let s = signer();
    let owner = s.address_at(0).unwrap();
    let mut w = healthy(&s);
    w.usdt.insert(g0(&s), 1_000_000); // below ACTIVATE_MAX + TRANSFER_MAX
    w.usdt.insert(owner, 5_000_000);
    let (url, world) = spawn(w).await;

    let outcome = client(&url).sweep(&s, 0).await.unwrap();

    // The plain address has no TRX and neither does the fee account, so today's path answers
    // FeeAccountDry — which proves it ran.
    assert!(matches!(outcome, SweepOutcome::FeeAccountDry { .. }), "got {outcome:?}");
    assert!(named(&world, "submit").is_empty(), "no permit for a balance that cannot pay its fee");
}

#[tokio::test]
async fn dust_alone_is_reported_below_fee() {
    let s = signer();
    let mut w = healthy(&s);
    w.usdt.insert(g0(&s), 1_000_000);
    let (url, _) = spawn(w).await;

    assert_eq!(
        client(&url).sweep(&s, 0).await.unwrap(),
        SweepOutcome::BelowFee {
            gasfree_address: g0(&s),
            balance_usdt: 1_000_000,
            max_fee_usdt: ACTIVATE_MAX + TRANSFER_MAX,
        }
    );
}

/// The property that makes merging this safe: without GasFree settings, a sweep reads exactly one
/// balance, the plain address's, as it always has.
#[tokio::test]
async fn without_gasfree_the_sweep_never_looks_at_a_gasfree_address() {
    let s = signer();
    let mut w = healthy(&s);
    w.usdt.insert(g0(&s), 10_000_000);
    let (url, world) = spawn(w).await;

    let outcome = SweepClient::new(sweep_config(&url)).sweep(&s, 0).await.unwrap();

    assert_eq!(outcome, SweepOutcome::NothingToSweep);
    let reads = named(&world, "balanceOf(address)");
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0]["owner_address"], s.address_at(0).unwrap());
}

#[test]
fn every_sweep_status_string_is_pinned() {
    use crate::sweep::sweep_response;
    let pending = sweep_response(&SweepOutcome::Pending {
        trace_id: "t".into(),
        gasfree_address: "g".into(),
        receiver: "r".into(),
        value_usdt: 8,
        max_fee_usdt: 2,
    });
    assert_eq!(
        pending,
        serde_json::json!({"status": "pending", "trace_id": "t", "gasfree_address": "g", "receiver": "r", "value_usdt": 8, "max_fee_usdt": 2})
    );
    assert_eq!(
        sweep_response(&SweepOutcome::Busy { gasfree_address: "g".into() }),
        serde_json::json!({"status": "busy", "gasfree_address": "g"})
    );
    assert_eq!(
        sweep_response(&SweepOutcome::Rejected { reason: "r".into(), message: "m".into() }),
        serde_json::json!({"status": "rejected", "reason": "r", "message": "m"})
    );
    assert_eq!(
        sweep_response(&SweepOutcome::Halted { reason: "r".into() }),
        serde_json::json!({"status": "halted", "reason": "r"})
    );
    assert_eq!(
        sweep_response(&SweepOutcome::BelowFee { gasfree_address: "g".into(), balance_usdt: 1, max_fee_usdt: 2 }),
        serde_json::json!({"status": "below_fee", "gasfree_address": "g", "balance_usdt": 1, "max_fee_usdt": 2})
    );
    // The four the treasury already parses must not change.
    assert_eq!(
        sweep_response(&SweepOutcome::Swept { tx_id: "t".into(), amount_usdt: 5 }),
        serde_json::json!({"status": "swept", "tx_id": "t", "amount_usdt": 5})
    );
    assert_eq!(sweep_response(&SweepOutcome::NothingToSweep), serde_json::json!({"status": "nothing_to_sweep"}));
    assert_eq!(
        sweep_response(&SweepOutcome::Funded { tx_id: "t".into(), amount_sun: 5 }),
        serde_json::json!({"status": "funded", "tx_id": "t", "amount_sun": 5})
    );
    assert_eq!(
        sweep_response(&SweepOutcome::FeeAccountDry { fee_address: "a".into(), have_sun: 1, need_sun: 2 }),
        serde_json::json!({"status": "fee_account_dry", "fee_address": "a", "have_sun": 1, "need_sun": 2})
    );
}

// ---- the GasFree payout ----

const REDEEMER: &str = "TJM1BE5wq1VdHh3gwjUeyaVkvZp9DVYCfC";

/// A world where the GasFree float is activated and holds 100 USDT.
fn with_float(s: &Signer) -> WorldState {
    let mut w = healthy(s);
    w.contracts.insert(float_of(s));
    w.usdt.insert(float_of(s), 100_000_000);
    w
}

#[tokio::test]
async fn a_gasfree_payout_is_a_permit_from_the_float_for_exactly_the_amount() {
    let s = signer();
    let (url, world) = spawn(with_float(&s)).await;

    let outcome = client(&url).payout(&s, REDEEMER, 20_000_000).await.unwrap();

    assert_eq!(outcome, PayoutOutcome::Submitted { trace_id: TRACE_ID.into() });
    let p = &named(&world, "submit")[0];
    assert_eq!(p["user"], s.payout_address().unwrap(), "the float's owner, 2/0 — never a deposit key");
    assert_eq!(p["receiver"], REDEEMER);
    assert_eq!(p["value"], 20_000_000, "the redeemer gets exactly the amount; the fee comes on top");
    assert_eq!(p["maxFee"], TRANSFER_MAX, "an activated float pays one transfer fee, never an activation");
    assert_eq!(p["token"], USDT);
    assert_eq!(&signer_of(p), s.payout_signing_key().unwrap().verifying_key(), "signed by the 2/0 key");
}

#[tokio::test]
async fn an_unactivated_float_answers_float_not_active_and_signs_nothing() {
    let s = signer();
    let mut w = with_float(&s);
    w.contracts.remove(&float_of(&s));
    let (url, world) = spawn(w).await;

    assert_eq!(
        client(&url).payout(&s, REDEEMER, 20_000_000).await.unwrap(),
        PayoutOutcome::FloatNotActive { float_address: float_of(&s) }
    );
    assert!(named(&world, "submit").is_empty());
}

#[tokio::test]
async fn a_float_short_of_amount_plus_fee_is_dry() {
    let s = signer();
    let mut w = with_float(&s);
    w.usdt.insert(float_of(&s), 20_000_000 + TRANSFER_MAX - 1);
    let (url, world) = spawn(w).await;

    assert_eq!(
        client(&url).payout(&s, REDEEMER, 20_000_000).await.unwrap(),
        PayoutOutcome::FloatDry {
            float_address: float_of(&s),
            have_usdt: 20_000_000 + TRANSFER_MAX - 1,
            need_usdt: 20_000_000 + TRANSFER_MAX,
        }
    );
    assert!(named(&world, "submit").is_empty());
}

#[tokio::test]
async fn a_documented_relay_refusal_is_a_provable_non_payment() {
    let s = signer();
    let mut w = with_float(&s);
    w.submit_reply = serde_json::json!({
        "code": 400, "reason": "InsufficientBalanceException", "message": "insufficient balance", "data": null,
    })
    .to_string();
    let (url, _) = spawn(w).await;

    let outcome = client(&url).payout(&s, REDEEMER, 20_000_000).await.unwrap();

    assert!(matches!(outcome, PayoutOutcome::Refused(ref why) if why.contains("InsufficientBalanceException")), "got {outcome:?}");
}

#[tokio::test]
async fn any_other_relay_answer_after_signing_is_ambiguous() {
    let s = signer();
    for reply in [
        serde_json::json!({"code": 500, "reason": "RuntimeException", "message": "boom", "data": null}).to_string(),
        serde_json::json!({"code": 400, "reason": "SomethingNewException", "message": "?", "data": null}).to_string(),
        "Bad Gateway".to_string(),
    ] {
        let mut w = with_float(&s);
        w.submit_reply = reply.clone();
        let (url, _) = spawn(w).await;
        // Err is a 500 on the wire, which the treasury records as ambiguous and hands to a human.
        assert!(client(&url).payout(&s, REDEEMER, 20_000_000).await.is_err(), "{reply} must not read as a clear answer");
    }
}

#[tokio::test]
async fn a_changed_beacon_refuses_the_payout() {
    let s = signer();
    let mut w = with_float(&s);
    w.implementations.insert(gasfree::NILE.beacon.into(), "55".repeat(20));
    let (url, world) = spawn(w).await;

    let outcome = client(&url).payout(&s, REDEEMER, 20_000_000).await.unwrap();

    assert!(matches!(outcome, PayoutOutcome::Refused(_)), "got {outcome:?}");
    assert!(named(&world, "submit").is_empty());
}

/// Redemptions by TRX stay exactly as they were: the plain 2/0 float, and not one relay call.
#[tokio::test]
async fn with_trx_payouts_the_gasfree_float_is_never_touched() {
    let s = signer();
    let (url, world) = spawn(with_float(&s)).await;
    let c = SweepClient::new(sweep_config(&url)).with_gasfree(gasfree_config(&url, false));

    let outcome = c.payout(&s, REDEEMER, 20_000_000).await.unwrap();

    // The plain float holds nothing in this world, so today's path answers FloatDry for 2/0.
    assert!(
        matches!(outcome, PayoutOutcome::FloatDry { ref float_address, .. } if *float_address == s.payout_address().unwrap()),
        "got {outcome:?}"
    );
    assert!(named(&world, "relay_account").is_empty() && named(&world, "submit").is_empty());
}

#[test]
fn the_new_payout_status_strings_are_pinned() {
    use crate::sweep::payout_response;
    assert_eq!(
        payout_response(&PayoutOutcome::Submitted { trace_id: "t".into() }),
        serde_json::json!({"status": "submitted", "trace_id": "t"})
    );
    assert_eq!(
        payout_response(&PayoutOutcome::FloatNotActive { float_address: "f".into() }),
        serde_json::json!({"status": "float_not_active", "float_address": "f"})
    );
}

#[tokio::test]
async fn a_trace_is_reported_in_the_signers_own_words() {
    let s = signer();
    let mut w = healthy(&s);
    w.trace_reply = serde_json::json!({"code": 200, "reason": null, "message": null, "data": {
        "id": TRACE_ID, "state": "SUCCEED", "txnHash": "ab", "txnState": "SOLIDITY", "txnAmount": 20000000, "txnTotalFee": 300000,
    }})
    .to_string();
    let (url, _) = spawn(w).await;

    let trace = client(&url).gasfree_trace(TRACE_ID).await.unwrap().expect("GasFree is on");
    assert_eq!(
        trace_response(&trace),
        serde_json::json!({"state": "SUCCEED", "txn_hash": "ab", "txn_state": "SOLIDITY", "txn_amount": 20000000, "txn_total_fee": 300000})
    );
    assert_eq!(SweepClient::new(sweep_config(&url)).gasfree_trace(TRACE_ID).await.unwrap(), None, "off means no trace");
}

// ---- activating the float ----

#[tokio::test]
async fn activation_sends_the_smallest_amount_from_the_float_to_custody() {
    let s = signer();
    let mut w = healthy(&s);
    w.usdt.insert(float_of(&s), 5_000_000); // not activated: no contract record
    let (url, world) = spawn(w).await;

    let outcome = client(&url).activate_float(&s).await.unwrap();

    assert_eq!(outcome, ActivateFloatOutcome::Submitted { trace_id: TRACE_ID.into() });
    let p = &named(&world, "submit")[0];
    assert_eq!(p["user"], s.payout_address().unwrap());
    assert_eq!(p["receiver"], CUSTODY, "custody gets the value back; only the fee leaves the reserve");
    assert_eq!(p["value"], 1);
    assert_eq!(p["maxFee"], ACTIVATE_MAX + TRANSFER_MAX, "the first transfer pays activation too");
    assert_eq!(&signer_of(p), s.payout_signing_key().unwrap().verifying_key());
}

#[tokio::test]
async fn an_activated_float_is_left_alone() {
    let s = signer();
    let (url, world) = spawn(with_float(&s)).await;

    assert_eq!(
        client(&url).activate_float(&s).await.unwrap(),
        ActivateFloatOutcome::AlreadyActive { float_address: float_of(&s) }
    );
    assert!(named(&world, "submit").is_empty(), "re-running the workflow must cost nothing");
}

#[tokio::test]
async fn a_float_that_cannot_pay_for_its_activation_is_dry() {
    let s = signer();
    let mut w = healthy(&s);
    w.usdt.insert(float_of(&s), ACTIVATE_MAX + TRANSFER_MAX); // one micro-USDT short
    let (url, world) = spawn(w).await;

    assert_eq!(
        client(&url).activate_float(&s).await.unwrap(),
        ActivateFloatOutcome::FloatDry {
            float_address: float_of(&s),
            have_usdt: ACTIVATE_MAX + TRANSFER_MAX,
            need_usdt: 1 + ACTIVATE_MAX + TRANSFER_MAX,
        }
    );
    assert!(named(&world, "submit").is_empty());
}

#[tokio::test]
async fn activation_without_gasfree_is_refused() {
    let outcome = SweepClient::new(sweep_config("http://127.0.0.1:1")).activate_float(&signer()).await.unwrap();
    assert!(matches!(outcome, ActivateFloatOutcome::Refused(_)), "got {outcome:?}");
}

#[test]
fn every_activation_status_string_is_pinned() {
    assert_eq!(
        activate_float_response(&ActivateFloatOutcome::Submitted { trace_id: "t".into() }),
        serde_json::json!({"status": "submitted", "trace_id": "t"})
    );
    assert_eq!(
        activate_float_response(&ActivateFloatOutcome::AlreadyActive { float_address: "f".into() }),
        serde_json::json!({"status": "already_active", "float_address": "f"})
    );
    assert_eq!(
        activate_float_response(&ActivateFloatOutcome::FloatDry { float_address: "f".into(), have_usdt: 1, need_usdt: 2 }),
        serde_json::json!({"status": "float_dry", "float_address": "f", "have_usdt": 1, "need_usdt": 2})
    );
    assert_eq!(
        activate_float_response(&ActivateFloatOutcome::Refused("r".into())),
        serde_json::json!({"status": "refused", "reason": "r"})
    );
}

// ---- found by the final review ----

#[tokio::test]
async fn a_trongrid_error_is_never_read_as_not_activated() {
    // G is activated, so the treasury held back one transfer fee. Read as "not activated", a
    // rate-limited answer would sign a maxFee of activation plus transfer: more than was held back.
    let s = signer();
    let mut w = healthy(&s);
    w.usdt.insert(g0(&s), 10_000_000);
    w.contracts.insert(g0(&s));
    w.getcontract_error = true;
    let (url, world) = spawn(w).await;

    let outcome = client(&url).sweep(&s, 0).await;

    assert!(outcome.is_err(), "an error is no answer about activation, got {outcome:?}");
    assert!(named(&world, "submit").is_empty(), "no permit is signed on a guess");
}

#[tokio::test]
async fn the_self_test_is_not_fatal_when_trongrid_answers_an_error() {
    // A rate limit at boot says nothing about which network this is, so it must not stop the signer.
    let s = signer();
    let mut w = healthy(&s);
    w.getcontract_error = true;
    let (url, _) = spawn(w).await;

    let result = client(&url).gasfree_self_test(&s).await;

    assert!(matches!(result, SelfTest::Unreachable(_)), "got {result:?}");
}

#[tokio::test]
async fn any_other_relay_answer_to_the_activation_is_ambiguous() {
    let s = signer();
    for reply in [
        serde_json::json!({"code": 500, "reason": "RuntimeException", "message": "boom", "data": null}).to_string(),
        serde_json::json!({"code": 400, "reason": "SomethingNewException", "message": "?", "data": null}).to_string(),
        "Bad Gateway".to_string(),
    ] {
        let mut w = healthy(&s);
        w.usdt.insert(float_of(&s), 5_000_000); // not activated: no contract record
        w.submit_reply = reply.clone();
        let (url, world) = spawn(w).await;

        let outcome = client(&url).activate_float(&s).await;

        // Refused tells the operator the permit did not run, and only a documented refusal proves
        // that. After any other answer it may still run before its deadline: Err, read the chain.
        assert!(outcome.is_err(), "{reply} must not read as a clear answer, got {outcome:?}");
        assert_eq!(named(&world, "submit").len(), 1, "exactly one permit for {reply}");
    }
}

#[test]
fn a_blank_optional_setting_counts_as_unset() {
    // The deploy repo passes an unset optional value as an empty string (`${X:-}`), so a blank must
    // mean "not set", never "invalid" — an invalid rail would stop the TRX rail with it.
    let blank = |k: &str| match k {
        "APP_TRANSFER_RAIL" | "APP_GASFREE_API_KEY" => Some(String::new()),
        _ => None,
    };
    assert!(matches!(load_gasfree_config(blank), Ok(None)), "a blank rail and a blank key: GasFree stays off");

    let base = setting(&[]);
    let with_blanks = move |k: &str| match k {
        "APP_TRANSFER_RAIL" | "APP_GASFREE_DEADLINE_SECS" => Some(String::new()),
        _ => base(k),
    };
    let cfg = load_gasfree_config(with_blanks).unwrap().expect("the API key still turns GasFree on");
    assert!(!cfg.payouts, "a blank rail is trx");
    assert_eq!(cfg.deadline_secs, 180, "a blank deadline is the default");
}
