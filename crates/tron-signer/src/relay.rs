//! The GasFree relay (GasFree's word is "service provider"): read an account, submit a signed
//! permit, and follow the permit afterwards.
//!
//! Holds no key and signs nothing but its own API requests. A permit reaches the relay already
//! signed, and the relay can do nothing with it except execute exactly what it says: the token, the
//! receiver, the value, the most it may charge and a deadline are all inside the signature.
//!
//! Field names are from the GasFree developer docs (`gasfreeio/developer-docs`, `docs/index.md` at
//! a84fee8). Where the docs disagree with themselves — `allowSubmit` in the field list,
//! `allow_submit` in the example — both spellings are read.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

#[derive(Clone)]
pub struct RelayConfig {
    /// The API root INCLUDING the network prefix: `https://open-test.gasfree.io/nile` or
    /// `https://open.gasfree.io/tron`. The prefix is part of the signed path, so it is kept.
    pub base_url: String,
    pub api_key: String,
    pub api_secret: String,
}

/// A GasFree account as the relay reports it.
#[derive(Debug, PartialEq)]
pub struct Account {
    /// Where the relay thinks this owner's GasFree account lives. The signer compares it with its
    /// own derivation and signs nothing when they differ.
    pub gasfree_address: String,
    /// The nonce the relay recommends for the next permit. It counts permits waiting in the relay's
    /// queue, which is why it can be ahead of the chain's.
    pub nonce: u64,
    pub allow_submit: bool,
    /// The token amount tied up in transfers the relay accepted and has not finished, fees included.
    pub frozen: i64,
}

/// What became of a submitted permit.
#[derive(Debug, PartialEq)]
pub struct Trace {
    /// `WAITING`, `INPROGRESS`, `CONFIRMING`, `SUCCEED` or `FAILED`.
    pub state: String,
    /// The on-chain transaction, once there is one.
    pub txn_hash: Option<String>,
    /// `INIT`, `NOT_ON_CHAIN`, `ON_CHAIN`, `SOLIDITY` or `ON_CHAIN_FAILED`.
    pub txn_state: Option<String>,
    /// What actually reached the receiver.
    pub txn_amount: Option<i64>,
    /// What the relay actually charged: activation plus transfer.
    pub txn_total_fee: Option<i64>,
}

#[derive(Debug, PartialEq)]
pub enum RelayError {
    /// The relay answered and said no: its body `code` was 400. `reason` is its exception name,
    /// for example `MaxFeeExceededException`.
    Refused { reason: String, message: String },
    /// No usable answer: the request failed, the reply was not the relay's JSON envelope, or the
    /// envelope reported a runtime error.
    Unavailable(String),
}

/// `base64(HMAC-SHA256(secret, METHOD + PATH + TIMESTAMP))`, the relay's request signature.
///
/// PATH is the URL path INCLUDING the network prefix (`/nile/api/v1/...`). The body is not signed.
pub fn signature(secret: &str, method: &str, path: &str, timestamp: u64) -> String {
    todo!("Task 1 Step 5")
}

/// A trace id is a UUID. Checked before one goes into a URL path, so a caller cannot make this
/// service call some other relay path with its credentials.
pub fn is_trace_id(s: &str) -> bool {
    todo!("Task 1 Step 5")
}

pub struct Relay {
    http: reqwest::Client,
    cfg: RelayConfig,
}

impl Relay {
    pub fn new(cfg: RelayConfig) -> Self {
        todo!("Task 1 Step 5")
    }

    /// The GasFree account of the wallet `owner` — its EOA address, not its GasFree address.
    pub async fn account(&self, owner: &str, token: &str) -> Result<Account, RelayError> {
        todo!("Task 1 Step 5")
    }

    /// Hand a signed permit to the relay. Returns its trace id.
    pub async fn submit(&self, permit: &gasfree::Permit<'_>, sig: &str) -> Result<String, RelayError> {
        todo!("Task 1 Step 5")
    }

    pub async fn trace(&self, trace_id: &str) -> Result<Trace, RelayError> {
        todo!("Task 1 Step 5")
    }
}

fn parse_account(data: &serde_json::Value, token: &str) -> Result<Account, String> {
    todo!("Task 1 Step 5")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::{HeaderMap, Method, Uri},
        Router,
    };
    use std::sync::{Arc, Mutex};

    const USDT: &str = "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf";
    const OWNER: &str = "TMVQGm1qAQYVdetCeGRRkTWYYrLXuHK2HC";

    /// Shaped like the docs' example, with the fees the Nile relay reported on 2026-09-24.
    const OK_ACCOUNT: &str = r#"{"code":200,"reason":null,"message":null,"data":{"accountAddress":"TMVQGm1qAQYVdetCeGRRkTWYYrLXuHK2HC","gasFreeAddress":"TUGC4eNuEgbaLotwxzzXEck1fRWru6n8ye","active":false,"nonce":3,"allowSubmit":true,"assets":[{"tokenAddress":"TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf","tokenSymbol":"USDT","activateFee":1000000,"transferFee":300000,"decimal":6,"frozen":0}]}}"#;

    /// One request as the fake relay saw it.
    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        path: String,
        timestamp: String,
        authorization: String,
        body: String,
    }

    #[derive(Clone)]
    struct Fake {
        reply: String,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    async fn record(State(f): State<Fake>, method: Method, uri: Uri, headers: HeaderMap, body: String) -> String {
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        f.seen.lock().unwrap().push(Seen {
            method: method.to_string(),
            path: uri.path().to_string(),
            timestamp: header("timestamp"),
            authorization: header("authorization"),
            body,
        });
        f.reply.clone()
    }

    /// A relay that answers every request with `reply`, verbatim, and remembers what it was sent.
    async fn fake_relay(reply: &str) -> (Relay, Arc<Mutex<Vec<Seen>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .fallback(record)
            .with_state(Fake { reply: reply.to_string(), seen: seen.clone() });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let relay = Relay::new(RelayConfig {
            base_url: format!("{url}/nile"),
            api_key: "test-key".into(),
            api_secret: "test-secret".into(),
        });
        (relay, seen)
    }

    /// Computed independently with Node's `crypto.createHmac` and with `openssl dgst -hmac` — the
    /// same openssl call clutch-deploy's `PROBE=gasfree` makes against the live relay. Both agree.
    #[test]
    fn the_request_signature_matches_an_independent_hmac() {
        assert_eq!(
            signature("test-secret", "GET", "/nile/api/v1/config/token/all", 1731912286),
            "sYfwaLSnifcL/MTLFDmfORdCVceFLckEPL1mMKEjTHQ="
        );
        assert_eq!(
            signature("test-secret", "POST", "/nile/api/v1/gasfree/submit", 1731912286),
            "FvhbpUfD8fWggPc0cjP523ng+M/EoYsJ0q5T40cImHg="
        );
    }

    /// The prefix is the part that is easy to lose: signing `/api/v1/...` instead of
    /// `/nile/api/v1/...` gets every request refused, on every network.
    #[tokio::test]
    async fn every_request_is_signed_over_its_full_path_including_the_network_prefix() {
        let (relay, seen) = fake_relay(OK_ACCOUNT).await;
        relay.account(OWNER, USDT).await.unwrap();
        let s = seen.lock().unwrap()[0].clone();
        assert_eq!(s.method, "GET");
        assert_eq!(s.path, format!("/nile/api/v1/address/{OWNER}"));
        let ts: u64 = s.timestamp.parse().expect("the Timestamp header is whole seconds");
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert!(ts.abs_diff(now) <= 5, "Timestamp {ts} is not the current time {now}");
        assert_eq!(s.authorization, format!("ApiKey test-key:{}", signature("test-secret", "GET", &s.path, ts)));
    }

    #[tokio::test]
    async fn an_account_reply_is_read_field_by_field() {
        let (relay, _) = fake_relay(OK_ACCOUNT).await;
        assert_eq!(
            relay.account(OWNER, USDT).await.unwrap(),
            Account {
                gasfree_address: "TUGC4eNuEgbaLotwxzzXEck1fRWru6n8ye".into(),
                nonce: 3,
                allow_submit: true,
                frozen: 0,
            }
        );
    }

    #[test]
    fn both_spellings_of_allow_submit_are_read_and_frozen_is_per_token() {
        let snake = serde_json::json!({
            "gasFreeAddress": "G", "nonce": 1, "allow_submit": false,
            "assets": [{"tokenAddress": "OTHER", "frozen": 9}, {"tokenAddress": USDT, "frozen": 5}],
        });
        let a = parse_account(&snake, USDT).unwrap();
        assert!(!a.allow_submit, "the docs' example spelling must be read");
        assert_eq!(a.frozen, 5, "frozen is the configured token's, not the first asset's");

        let camel = serde_json::json!({"gasFreeAddress": "G", "nonce": 1, "allowSubmit": false});
        assert!(!parse_account(&camel, USDT).unwrap().allow_submit, "the docs' field-list spelling must be read");

        let neither = serde_json::json!({"gasFreeAddress": "G", "nonce": 1});
        assert!(parse_account(&neither, USDT).unwrap().allow_submit, "a missing field is not a refusal");

        assert!(parse_account(&serde_json::json!({"nonce": 1}), USDT).is_err(), "no gasFreeAddress, no account");
        assert!(parse_account(&serde_json::json!({"gasFreeAddress": "G"}), USDT).is_err(), "no nonce, no account");
    }

    #[tokio::test]
    async fn a_relay_refusal_carries_its_reason() {
        let (relay, _) = fake_relay(
            r#"{"code":400,"reason":"MaxFeeExceededException","message":"estimated fee exceeds the limit","data":null}"#,
        )
        .await;
        assert_eq!(
            relay.account(OWNER, USDT).await,
            Err(RelayError::Refused {
                reason: "MaxFeeExceededException".into(),
                message: "estimated fee exceeds the limit".into(),
            })
        );
    }

    #[tokio::test]
    async fn a_runtime_error_or_a_reply_that_is_not_the_envelope_is_unavailable() {
        let (relay, _) = fake_relay(r#"{"code":500,"reason":"RuntimeException","message":"boom","data":null}"#).await;
        assert!(matches!(relay.account(OWNER, USDT).await, Err(RelayError::Unavailable(_))));

        // What the live relay answers a request without credentials: plain text, not JSON.
        let (relay, _) = fake_relay("Authorization or timestamp not found.").await;
        match relay.account(OWNER, USDT).await {
            Err(RelayError::Unavailable(msg)) => {
                assert!(msg.contains("Authorization or timestamp not found."), "the relay's words must survive: {msg}")
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn submit_sends_the_permit_as_json_numbers_and_returns_the_trace_id() {
        let (relay, seen) = fake_relay(
            r#"{"code":200,"reason":null,"message":null,"data":{"id":"6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60","state":"WAITING"}}"#,
        )
        .await;
        let permit = gasfree::Permit {
            token: USDT,
            service_provider: "TDbJyQ6g1Lx9BAfEEeN5S5TMjjDRAVFCaA",
            user: OWNER,
            receiver: "TJM1BE5wq1VdHh3gwjUeyaVkvZp9DVYCfC",
            value: 10_000,
            max_fee: 2_000,
            deadline: 1_726_207_632,
            version: 1,
            nonce: 2,
        };
        assert_eq!(relay.submit(&permit, "ab").await.unwrap(), "6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60");

        let s = seen.lock().unwrap()[0].clone();
        assert_eq!((s.method.as_str(), s.path.as_str()), ("POST", "/nile/api/v1/gasfree/submit"));
        let body: serde_json::Value = serde_json::from_str(&s.body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "token": USDT,
                "serviceProvider": "TDbJyQ6g1Lx9BAfEEeN5S5TMjjDRAVFCaA",
                "user": OWNER,
                "receiver": "TJM1BE5wq1VdHh3gwjUeyaVkvZp9DVYCfC",
                "value": 10000,
                "maxFee": 2000,
                "deadline": 1726207632,
                "version": 1,
                "nonce": 2,
                "sig": "ab",
            })
        );
    }

    #[tokio::test]
    async fn a_trace_reports_the_transaction_once_there_is_one() {
        let (relay, seen) = fake_relay(
            r#"{"code":200,"reason":null,"message":null,"data":{"id":"6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60","state":"SUCCEED","txnHash":"332789a76fa3d39bfeebf0ce7ab81108ed8c82fa76be573719cc5931452fb7c7","txnState":"SOLIDITY","txnAmount":8000000,"txnTotalFee":1300000}}"#,
        )
        .await;
        assert_eq!(
            relay.trace("6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60").await.unwrap(),
            Trace {
                state: "SUCCEED".into(),
                txn_hash: Some("332789a76fa3d39bfeebf0ce7ab81108ed8c82fa76be573719cc5931452fb7c7".into()),
                txn_state: Some("SOLIDITY".into()),
                txn_amount: Some(8_000_000),
                txn_total_fee: Some(1_300_000),
            }
        );
        assert_eq!(seen.lock().unwrap()[0].path, "/nile/api/v1/gasfree/6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60");

        // Before the relay has put it on chain, the transaction fields are empty.
        let (relay, _) = fake_relay(r#"{"code":200,"reason":null,"message":null,"data":{"id":"6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60","state":"WAITING","txnHash":""}}"#).await;
        let t = relay.trace("6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60").await.unwrap();
        assert_eq!((t.state.as_str(), t.txn_hash), ("WAITING", None), "an empty hash is no hash");
    }

    #[tokio::test]
    async fn a_trace_id_that_is_not_a_uuid_never_reaches_the_relay() {
        let (relay, seen) = fake_relay("{}").await;
        assert!(relay.trace("../address/TMVQGm1qAQYVdetCeGRRkTWYYrLXuHK2HC").await.is_err());
        assert!(seen.lock().unwrap().is_empty(), "nothing may be requested for a malformed id");
        assert!(is_trace_id("6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60"));
        assert!(!is_trace_id("6ab4c27c-f66b-4328-b40f-ffdc6cf1ca6/"));
    }
}
