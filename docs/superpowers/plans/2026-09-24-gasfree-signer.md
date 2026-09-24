# GasFree Signer Implementation Plan (Plan 2 of 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Teach `tron-signer` the GasFree rail: sweep a deposit out of a user's GasFree account, pay a redemption out of the GasFree float, and activate that float, each with a permit signed here and handed to the pinned relay.

**Architecture:** A key-free relay client (`relay.rs`) and a child module of the sweep code (`sweep/gasfree_rail.rs`) that builds, checks and signs permits. It is off unless `APP_GASFREE_API_KEY` is set, so a signer without GasFree settings behaves exactly as today. Every number that decides how much moves comes from config or the chain; the relay's replies are only ever used to wait.

**Tech Stack:** Rust 2021, axum 0.7, reqwest 0.12, k256 0.13, the workspace `gasfree` crate (Plan 1), `hmac` 0.12 and `base64` 0.22 (both already in the lockfile).

**Spec:** `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md`. This plan implements the signer's half of §3 (sweeping), §4 (payouts, the float, its activation) and the signer's part of §5 (the tripwire). The treasury and orchestrator halves are Plan 3; configuration, invariants, the deposit panel and the Nile rollout are Plan 4.

## Global Constraints

- **No local builds.** "Do not run `cargo`, `npm`, `docker`, or any build/test/lint command on this Windows host, and forbid it in every subagent prompt. Verify code by dispatching CI and reading the run log."
- A test counts only when the CI log shows it **by name**. A green badge is not evidence. Pick the CI run by head SHA.
- One implementer per checkout at a time. Commit with `git commit -F <file>`; never put backticks in `-m`.
- "**`tron-signer`'s SWEEP API takes an INDEX and nothing else** — the destination is its own config. Do not add a `to`, `contract`, or `amount` parameter there." (workspace CLAUDE.md)
- The payout endpoint "can only spend from the payout float at `2/0` — never a deposit address, never custody"; on this rail the float is `F = gasfree(the 2/0 address)` (spec §4). `contract` is never a parameter.
- **Merging changes nothing.** Without `APP_GASFREE_API_KEY` the signer never derives, reads or signs for a GasFree address, and `APP_TRANSFER_RAIL` defaults to `trx` (spec: "defaulting to `trx`").
- "**Never sign a higher `maxFee` than was held back**" (spec §2). `maxFee` is always `gasfree::fee_to_hold(activated, activate_max, transfer_max)` — the one function the treasury will also use (Plan 3).
- "Activation has one source of truth: whether `G` has contract code on-chain" (spec §2), "never the relay's `active` field" (spec §3).
- "The chain is the truth, not the relay." (spec §5)
- One relay, "**pinned** in `GASFREE_SERVICE_PROVIDER`"; "a short `deadline`, minutes"; "never two sweeps of one address at once" (spec §3).
- The signer is the only service that holds the GasFree API key and secret (spec §6).
- `wiremock` is deliberately not a dependency of this crate, the one that holds the mnemonic. Test fakes are small axum servers, as the existing `fund_float_tests` are.
- A fenced code block in a doc comment must be marked `text`, or `cargo test` compiles it as a doctest.

## Facts this plan relies on, checked 2026-09-24

Read live through TronGrid and the GasFree docs (`gasfreeio/developer-docs`, `docs/index.md` at `a84fee8`), not assumed:

1. **The fee is charged on top of `value`** (spec open question 1). The docs define `txnTotalCost` as "actual amount paid by the user, including the total fee and the transferred amount", and a real mainnet GasFree transfer (tx `332789a7…`) shows the fee as a separate USDT Transfer next to the value. So `value = balance − maxFee`, as spec §3 assumed. Rollout step 3 (Plan 4) still confirms it with test money.
2. **An activated GasFree account has an EMPTY `bytecode`.** `POST /wallet/getcontract` for the activated mainnet account `TBdkSW3VkKsA8RxmZFxMvNezimndUEymgg` returned `contract_address` and `code_hash` with `bytecode` empty; an unused account returns `{}`. So activation is read from `contract_address`, never from `bytecode`.
3. **The controller is an upgradeable proxy too**, not only the beacon. It has `admin()`, `implementation()` and `upgradeTo`. Mainnet controller implementation `0xc8b13e3104f8a2d6e915ac132bdeda7faaf84d7d`, admin `TLntW9Z59LYY5KEi9cmwk3PKjQga828ird` (the same address that received the relay fee in the transfer above). Nile controller implementation `0x2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441`.
4. **Beacon implementations:** mainnet `0xa3b0edffa1b94e93d297dcc9b6860175e9b537ec` (unchanged since the spec), Nile `0xb8eda40b467b45af107f198e94cc2fa1378adf50` (spec open question 2).
5. **The controller exposes `nonces(address)`** and `getGasFreeAddress(address)` as views, so the nonce and the address can be read from the chain.
6. **The relay's API**: every reply is HTTP 200 with a `{code, reason, message, data}` body, `code` 400 for a refusal; an unsigned request gets the plain text "Authorization or timestamp not found."; the signature is `base64(HMAC-SHA256(secret, METHOD + PATH + TIMESTAMP))` with PATH including the `/nile` or `/tron` prefix (the stage probe already uses this successfully). The account reply's field list says `allowSubmit`, its example shows `allow_submit`. The provider config allows deadlines of 60 to 600 seconds.
7. **A mainnet transfer fee of 1.5 USDT** was charged in that transaction. Mainnet maxima (Plan 4) must be at least that.

## Decisions this plan makes where the spec is silent

1. **The signer enforces the tripwire itself, before every permit**, and it watches the controller's `implementation()` as well as the beacon's (fact 3). New setting `APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION`. The treasury's paging and the orchestrator's stop are Plan 3.
2. **The nonce comes from the chain** (`nonces(owner)` on the controller). The relay's recommended nonce must equal it, and `allowSubmit` must not be false and `frozen` must be 0, or the signer waits a pass. So at most one unexecuted permit per nonce can exist.
3. **The relay's `gasFreeAddress` must equal this signer's derivation** or nothing is signed, and at boot the signer asks the controller's `getGasFreeAddress` for index 0 and refuses to start on a mismatch (the final Plan 1 review's recommendation). A TronGrid outage at boot is not fatal; the per-permit checks still run.
4. **Two new read-only endpoints for Plan 3:** `GET /internal/addresses/:index` (both addresses of an index, so the treasury decides the fee itself instead of trusting the orchestrator's address), and `GET /internal/gasfree/trace/:trace_id` (the relay's record of a permit, so the treasury can find a payout's transaction without holding the API key).
5. **Sweeps fill the float only when payouts use GasFree.** With `APP_TRANSFER_RAIL=trx` the GasFree float pays nothing, so every GasFree sweep goes to custody.
6. **A relay refusal of a payout is a provable non-payment only for the nine refusals the docs list** (`ProviderAddressNotMatchException`, `DeadlineExceededException`, `InvalidSignatureException`, `UnsupportedTokenException`, `TooManyPendingTransferException`, `VersionNotSupportedException`, `NonceNotMatchException`, `MaxFeeExceededException`, `InsufficientBalanceException`). Any other answer after a payout permit was signed is ambiguous and goes to a human, as a TRX payout's does today.
7. **The float's activation moves 1 micro-USDT** from the float to custody. The relay's minimum is not documented; rollout step 4 (Plan 4) finds out.
8. **A missing `allowSubmit` counts as allowed.** The nonce comparison and the relay's own refusal still catch a transfer in flight, and treating a renamed field as "never allowed" would stop every sweep. The probe in clutch-deploy#99 prints a live account reply so the field name can be checked before Task 1 starts.

---

## File Structure

- `crates/tron-signer/Cargo.toml` — add `gasfree`, `hmac`, `base64` (Task 1)
- `Cargo.lock` — the signer's dependency list (Task 1)
- `crates/tron-signer/src/lib.rs` — `pub mod relay;` (Task 1)
- `crates/tron-signer/src/relay.rs` — the relay client: signed requests, account, submit, trace. No key. (Task 1)
- `crates/tron-signer/src/sweep/gasfree_rail.rs` — settings, chain reads, permit signing, the boot check, and the GasFree sweep, payout and activation. A child module of `sweep`, so it can use `SweepClient`'s private TronGrid helpers. (Tasks 2-5)
- `crates/tron-signer/src/sweep/gasfree_rail/tests.rs` — one fake TronGrid-plus-relay and every GasFree test (Tasks 2-5)
- `crates/tron-signer/src/sweep.rs` — the module declaration, the `SweepClient.gasfree` field, new outcome variants and wire forms, and the dispatch into GasFree (Tasks 2-4)
- `crates/tron-signer/src/main.rs` — settings, the boot check, three routes (Tasks 2-5)
- `crates/gasfree/src/lib.rs` — `fee_to_hold` (Task 3)
- `README.md` — the signer's row (Task 3)

Commit message files go in `.superpowers/sdd/2026-09-24-gasfree-signer/`, which git ignores.

## How CI is run in this plan (controller steps)

Every "run CI" step is the controller's, never the implementer's:

```bash
cd /d/source/clutch/clutch-treasury
git push -u origin feat/gasfree-signer
gh workflow run test.yml --repo clutchprotocol/clutch-treasury --ref feat/gasfree-signer
SHA=$(git rev-parse HEAD)
gh run list --repo clutchprotocol/clutch-treasury --workflow test.yml --branch feat/gasfree-signer --event workflow_dispatch --json databaseId,headSha --jq ".[] | select(.headSha==\"$SHA\") | .databaseId"
```

Repeat the last command until it prints an id, then:

```bash
RUN=<the id>
gh run watch "$RUN" --repo clutchprotocol/clutch-treasury --exit-status
gh run view "$RUN" --repo clutchprotocol/clutch-treasury --log | sed -e 's/\^\[\[[0-9;]*m//g' | grep -E "test (relay|sweep::gasfree_rail)::tests::|test tests::the_fee_to_hold|test result:|^warning|--> crates/tron-signer|--> crates/gasfree"
```

`gh` prints cargo's colour codes as the literal characters `^[`, which split `--> crates/...` apart; the `sed` removes them so warnings can be found. Cargo stops at the first failing test binary, so a red run may not reach other crates. That is expected.

---

### Task 1: The relay client

**Files:**
- Create: `crates/tron-signer/src/relay.rs`
- Modify: `crates/tron-signer/src/lib.rs`
- Modify: `crates/tron-signer/Cargo.toml`
- Modify: `Cargo.lock`
- Test: `crates/tron-signer/src/relay.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `gasfree::Permit<'a>` (Plan 1): public fields `token`, `service_provider`, `user`, `receiver: &'a str` and `value`, `max_fee`, `deadline`, `version`, `nonce: u64`.
- Produces:
  - `pub struct RelayConfig { pub base_url: String, pub api_key: String, pub api_secret: String }` (derives `Clone`)
  - `pub struct Account { pub gasfree_address: String, pub nonce: u64, pub allow_submit: bool, pub frozen: i64 }` (derives `Debug, PartialEq`)
  - `pub struct Trace { pub state: String, pub txn_hash: Option<String>, pub txn_state: Option<String>, pub txn_amount: Option<i64>, pub txn_total_fee: Option<i64> }` (derives `Debug, PartialEq`)
  - `pub enum RelayError { Refused { reason: String, message: String }, Unavailable(String) }` (derives `Debug, PartialEq`)
  - `pub fn signature(secret: &str, method: &str, path: &str, timestamp: u64) -> String`
  - `pub fn is_trace_id(s: &str) -> bool`
  - `pub struct Relay` with `pub fn new(cfg: RelayConfig) -> Self`, `pub async fn account(&self, owner: &str, token: &str) -> Result<Account, RelayError>`, `pub async fn submit(&self, permit: &gasfree::Permit<'_>, sig: &str) -> Result<String, RelayError>` (the trace id), `pub async fn trace(&self, trace_id: &str) -> Result<Trace, RelayError>`

- [ ] **Step 1: Branch, and check the live account reply**

```bash
cd /d/source/clutch/clutch-treasury
git checkout main
git pull --ff-only origin main
git checkout -b feat/gasfree-signer
```

Controller, before dispatching this task: if clutch-deploy#99 is merged, run `gh workflow run inspect-stage.yml --repo clutchprotocol/clutch-deploy -f probe=gasfree` and read the Nile "account reply (raw)" line. If its field names differ from `gasFreeAddress`, `nonce`, `allowSubmit`/`allow_submit`, `assets[].tokenAddress`, `assets[].frozen`, rule on the difference in the ledger and give the implementer the live names.

- [ ] **Step 2: Add the dependencies**

In `crates/tron-signer/Cargo.toml`, add after the `tracing-subscriber` line:

```toml
# The GasFree rail (sweep/gasfree_rail.rs, relay.rs). The pure crate derives GasFree addresses and
# permit hashes; hmac and base64 sign the relay's API requests. Both versions are already in the
# lockfile, pulled in by other crates.
gasfree = { path = "../gasfree" }
hmac = "0.12"
base64 = "0.22"
```

In `Cargo.lock`, replace the `tron-signer` package's `dependencies` list with:

```toml
dependencies = [
 "axum",
 "base64 0.22.1",
 "bip32",
 "bip39",
 "bs58",
 "config",
 "dotenv",
 "gasfree",
 "hex",
 "hmac",
 "k256",
 "reqwest",
 "serde",
 "serde_json",
 "sha2",
 "sha3",
 "tokio",
 "tracing",
 "tracing-subscriber",
]
```

(`base64` has two versions in the lockfile, so cargo writes it with its version; `gasfree` and `hmac` have one each.)

Replace the whole of `crates/tron-signer/src/lib.rs` with:

```rust
pub mod keys;
pub mod relay;
pub mod sweep;
```

- [ ] **Step 3: Write the tests, with stub bodies**

Create `crates/tron-signer/src/relay.rs`. The function bodies are `todo!()` on purpose, so CI shows every test failing by name; Step 5 replaces them.

```rust
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
```

- [ ] **Step 4: Commit, and the controller confirms red**

Create `.superpowers/sdd/2026-09-24-gasfree-signer/commit-msg.txt`:

```text
test(signer): GasFree relay client tests, against stubs

The relay client signs each request with HMAC-SHA256 over method, full
path and timestamp, reads an account, submits a permit and follows a
trace. The bodies are todo!() in this commit so CI shows every test
failing by name.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add Cargo.lock crates/tron-signer/Cargo.toml crates/tron-signer/src/lib.rs crates/tron-signer/src/relay.rs
git commit -F .superpowers/sdd/2026-09-24-gasfree-signer/commit-msg.txt
```

Controller: run CI as described in "How CI is run". Expected: the run fails, and all nine fail by name:

```text
test relay::tests::a_relay_refusal_carries_its_reason ... FAILED
test relay::tests::a_runtime_error_or_a_reply_that_is_not_the_envelope_is_unavailable ... FAILED
test relay::tests::a_trace_id_that_is_not_a_uuid_never_reaches_the_relay ... FAILED
test relay::tests::a_trace_reports_the_transaction_once_there_is_one ... FAILED
test relay::tests::an_account_reply_is_read_field_by_field ... FAILED
test relay::tests::both_spellings_of_allow_submit_are_read_and_frozen_is_per_token ... FAILED
test relay::tests::every_request_is_signed_over_its_full_path_including_the_network_prefix ... FAILED
test relay::tests::submit_sends_the_permit_as_json_numbers_and_returns_the_trace_id ... FAILED
test relay::tests::the_request_signature_matches_an_independent_hmac ... FAILED
```

Unused-import and unused-variable warnings from `relay.rs` are expected in this run only.

- [ ] **Step 5: Replace the stubs**

In `crates/tron-signer/src/relay.rs`, replace the bodies of `signature`, `is_trace_id`, `Relay::new`, `account`, `submit`, `trace` and `parse_account`, and add the private helpers, so that everything above `#[cfg(test)]` from `pub fn signature` down reads:

```rust
pub fn signature(secret: &str, method: &str, path: &str, timestamp: u64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes a key of any length");
    mac.update(format!("{method}{path}{timestamp}").as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

pub fn is_trace_id(s: &str) -> bool {
    s.len() == 36 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

pub struct Relay {
    http: reqwest::Client,
    cfg: RelayConfig,
}

impl Relay {
    pub fn new(cfg: RelayConfig) -> Self {
        // A bound on every call. Without one a relay that stops answering holds a sweep pass, or a
        // payout request, open for as long as the TCP connection survives.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("a reqwest client with only a timeout set always builds");
        Self { http, cfg }
    }

    pub async fn account(&self, owner: &str, token: &str) -> Result<Account, RelayError> {
        let data = self.call(reqwest::Method::GET, &format!("/api/v1/address/{owner}"), None).await?;
        parse_account(&data, token).map_err(RelayError::Unavailable)
    }

    pub async fn submit(&self, permit: &gasfree::Permit<'_>, sig: &str) -> Result<String, RelayError> {
        let data = self.call(reqwest::Method::POST, "/api/v1/gasfree/submit", Some(submit_body(permit, sig))).await?;
        data["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| RelayError::Unavailable(format!("the submit reply has no id: {}", clip(&data.to_string()))))
    }

    pub async fn trace(&self, trace_id: &str) -> Result<Trace, RelayError> {
        if !is_trace_id(trace_id) {
            return Err(RelayError::Unavailable(format!("{trace_id:?} is not a trace id")));
        }
        let data = self.call(reqwest::Method::GET, &format!("/api/v1/gasfree/{trace_id}"), None).await?;
        parse_trace(&data).map_err(RelayError::Unavailable)
    }

    /// One signed request. Returns the envelope's `data` when `code` is 200.
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, RelayError> {
        let url = format!("{}{path}", self.cfg.base_url);
        let signed_path = reqwest::Url::parse(&url)
            .map_err(|e| RelayError::Unavailable(format!("bad relay URL {url}: {e}")))?
            .path()
            .to_string();
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let authorization = format!(
            "ApiKey {}:{}",
            self.cfg.api_key,
            signature(&self.cfg.api_secret, method.as_str(), &signed_path, timestamp)
        );
        let mut request = self
            .http
            .request(method, &url)
            .header("Timestamp", timestamp.to_string())
            .header("Authorization", authorization);
        if let Some(b) = body {
            request = request.json(&b);
        }
        let response = request
            .send()
            .await
            .map_err(|e| RelayError::Unavailable(format!("the relay request failed: {e}")))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|e| RelayError::Unavailable(format!("the relay reply was unreadable: {e}")))?;
        envelope(status, &text)
    }
}

/// Unwrap the relay's `{code, reason, message, data}` envelope.
///
/// The relay answers HTTP 200 to its own errors and puts the real result in `code`, so HTTP 200
/// alone proves nothing. Anything that is not the envelope — like the plain-text "Authorization or
/// timestamp not found." an unsigned request gets — is Unavailable, with the text kept.
fn envelope(status: u16, text: &str) -> Result<serde_json::Value, RelayError> {
    let body: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| RelayError::Unavailable(format!("the relay replied HTTP {status}: {}", clip(text))))?;
    match body["code"].as_i64() {
        Some(200) => Ok(body["data"].clone()),
        Some(400) => Err(RelayError::Refused {
            reason: body["reason"].as_str().unwrap_or("").to_string(),
            message: body["message"].as_str().unwrap_or("").to_string(),
        }),
        _ => Err(RelayError::Unavailable(format!("the relay replied HTTP {status}: {}", clip(text)))),
    }
}

fn parse_account(data: &serde_json::Value, token: &str) -> Result<Account, String> {
    let gasfree_address = data["gasFreeAddress"].as_str().ok_or("the relay's account has no gasFreeAddress")?.to_string();
    let nonce = data["nonce"].as_u64().ok_or("the relay's account has no nonce")?;
    // Missing counts as allowed: the nonce comparison and the relay's own refusal still catch a
    // transfer in flight, while "never allowed" for a renamed field would stop every sweep.
    let allow_submit = data["allowSubmit"].as_bool().or_else(|| data["allow_submit"].as_bool()).unwrap_or(true);
    let frozen = data["assets"]
        .as_array()
        .and_then(|assets| assets.iter().find(|a| a["tokenAddress"].as_str() == Some(token)))
        .and_then(|a| a["frozen"].as_i64())
        .unwrap_or(0);
    Ok(Account { gasfree_address, nonce, allow_submit, frozen })
}

fn parse_trace(data: &serde_json::Value) -> Result<Trace, String> {
    Ok(Trace {
        state: data["state"].as_str().ok_or("the relay's trace has no state")?.to_string(),
        txn_hash: data["txnHash"].as_str().filter(|h| !h.is_empty()).map(str::to_string),
        txn_state: data["txnState"].as_str().map(str::to_string),
        txn_amount: data["txnAmount"].as_i64(),
        txn_total_fee: data["txnTotalFee"].as_i64(),
    })
}

/// The submit request. Every number is a JSON number, as in the docs' example.
fn submit_body(permit: &gasfree::Permit<'_>, sig: &str) -> serde_json::Value {
    serde_json::json!({
        "token": permit.token,
        "serviceProvider": permit.service_provider,
        "user": permit.user,
        "receiver": permit.receiver,
        "value": permit.value,
        "maxFee": permit.max_fee,
        "deadline": permit.deadline,
        "version": permit.version,
        "nonce": permit.nonce,
        "sig": sig,
    })
}

fn clip(s: &str) -> String {
    s.chars().take(300).collect()
}
```

Do not change the tests.

- [ ] **Step 6: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(signer): a GasFree relay client

Signs each request as the relay requires, base64 HMAC-SHA256 over the
method, the full path including the network prefix, and the timestamp.
Reads an account (both spellings of allowSubmit), submits a signed
permit, and follows it by trace id. Holds no key, and nothing uses it
yet.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/tron-signer/src/relay.rs
git commit -F .superpowers/sdd/2026-09-24-gasfree-signer/commit-msg.txt
```

Controller: run CI. Expected: the run succeeds, the same nine tests say `ok` by name, every `test result:` line says `ok`, and there is no warning located in `crates/tron-signer`.

---

### Task 2: Settings, the activation read and the boot check

**Files:**
- Create: `crates/tron-signer/src/sweep/gasfree_rail.rs`
- Create: `crates/tron-signer/src/sweep/gasfree_rail/tests.rs`
- Modify: `crates/tron-signer/src/sweep.rs` (module declaration, the `SweepClient` field, two methods)
- Modify: `crates/tron-signer/src/main.rs` (settings, boot check, `/internal/addresses/:index`, the xpub reply)

This task adds only code that is used by the end of it; the chain reads and signing that only a permit needs arrive with the sweep in Task 3. Otherwise the lib build between the two tasks would warn about unused functions.

**Interfaces:**
- Consumes: Task 1's `RelayConfig`; `sweep.rs`'s `pub fn abi_address(address: &str) -> Result<String, String>` (64 hex, lowercase) and its private `SweepClient::post` and `describe_rejection`; `crate::keys::Signer` (`address_at(u32)`, `payout_address()`); `gasfree::{Chain, NILE, MAINNET, gasfree_address}`.
- Produces:
  - `pub struct GasFreeConfig { pub chain: &'static gasfree::Chain, pub relay: RelayConfig, pub service_provider: String, pub activate_fee_max_usdt: i64, pub transfer_fee_max_usdt: i64, pub expected_beacon_implementation: String, pub expected_controller_implementation: String, pub payout_float_target_usdt: i64, pub deadline_secs: u64, pub payouts: bool }` (the two implementations are 40 lowercase hex characters without `0x`)
  - `pub fn load_gasfree_config(var: impl Fn(&str) -> Option<String>) -> Result<Option<GasFreeConfig>, String>`
  - `pub enum SelfTest { Passed, Failed(String), Unreachable(String) }` (derives `Debug, PartialEq`)
  - `pub(super) struct GasFree { pub(super) cfg: GasFreeConfig }` (Task 3 adds a `relay` field)
  - on `SweepClient`: `pub fn with_gasfree(self, cfg: GasFreeConfig) -> Self`, `pub fn gasfree_address_for(&self, plain: &str) -> Result<Option<String>, String>`, `pub async fn gasfree_self_test(&self, signer: &Signer) -> SelfTest`, `pub(super) async fn has_contract(&self, address: &str) -> Result<bool, String>`, and the private `async fn view_word(&self, contract: &str, selector: &str, parameter: Option<&str>) -> Result<String, String>` (64 lowercase hex)
  - `GET /internal/addresses/:index` → `{"index": <u32>, "plain": "<T…>", "gasfree": "<T…>" | null}`; `GET /internal/xpub` gains `"payout_gasfree_address": "<T…>" | null`

- [ ] **Step 1: Wire the module into `sweep.rs`**

In `crates/tron-signer/src/sweep.rs`, directly below the line `use crate::keys::Signer;`, add:

```rust

mod gasfree_rail;

pub use gasfree_rail::{load_gasfree_config, GasFreeConfig, SelfTest};
```

Replace:

```rust
pub struct SweepClient {
    http: reqwest::Client,
    cfg: SweepConfig,
}

impl SweepClient {
    pub fn new(cfg: SweepConfig) -> Self {
        Self { http: reqwest::Client::new(), cfg }
    }
```

with:

```rust
pub struct SweepClient {
    http: reqwest::Client,
    cfg: SweepConfig,
    /// `None` unless GasFree is configured. While it is `None` nothing in this service derives,
    /// reads or signs for a GasFree address — the state every signer is in until someone sets
    /// `APP_GASFREE_API_KEY`.
    gasfree: Option<gasfree_rail::GasFree>,
}

impl SweepClient {
    pub fn new(cfg: SweepConfig) -> Self {
        Self { http: reqwest::Client::new(), cfg, gasfree: None }
    }

    /// Turn the GasFree rail on.
    pub fn with_gasfree(mut self, cfg: GasFreeConfig) -> Self {
        self.gasfree = Some(gasfree_rail::GasFree::new(cfg));
        self
    }

    /// The GasFree address of the wallet `plain`, when GasFree is on.
    pub fn gasfree_address_for(&self, plain: &str) -> Result<Option<String>, String> {
        self.gasfree.as_ref().map(|gf| gasfree::gasfree_address(gf.cfg.chain, plain)).transpose()
    }
```

- [ ] **Step 2: Write the module with stub bodies**

Create `crates/tron-signer/src/sweep/gasfree_rail.rs`:

```rust
//! The GasFree rail: sweeping a deposit out of a GasFree account, paying a redemption out of the
//! GasFree float, and activating that float — each with a permit signed here and handed to the
//! pinned relay.
//!
//! # What does not change
//!
//! A sweep still takes an index and nothing else. Every field of a permit comes from this
//! service's config or from the chain: the token and custody from config, the receiver from the
//! float's balance, `maxFee` from the configured maxima and the chain's record of activation, the
//! nonce from the controller. A payout still spends only from the float, which on this rail is the
//! float's GasFree account, `F = gasfree(the 2/0 address)`.
//!
//! # What the relay is trusted with
//!
//! The relay submits permits and pays the network. It is never the source of a number that decides
//! how much moves: activation is read from `getcontract`, the nonce from the controller's
//! `nonces`, balances from `balanceOf`. The relay's replies are only ever used to wait — a nonce
//! ahead of the chain's, `allowSubmit` false or a `frozen` amount means a transfer is in flight.
//! And the relay's idea of a GasFree address is compared with this service's own derivation before
//! anything is signed.
//!
//! # The tripwire
//!
//! GasFree accounts are beacon proxies, and the controller that moves money out of them is an
//! upgradeable proxy too. Before every permit this service reads both `implementation()`s and signs
//! nothing when either differs from the reviewed one it was configured with. See spec §5 in
//! docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md.

use super::{abi_address, describe_rejection, SweepClient};
use crate::keys::Signer;
use crate::relay::RelayConfig;

#[cfg(test)]
mod tests;

pub struct GasFreeConfig {
    /// Which GasFree deployment: `gasfree::NILE` or `gasfree::MAINNET`.
    pub chain: &'static gasfree::Chain,
    pub relay: RelayConfig,
    /// The one relay every permit names. A permit is only valid for the provider it names, so this
    /// is pinned rather than picked at runtime.
    pub service_provider: String,
    /// The most a first transfer may pay for activation, in micro-USDT. Above the live fee.
    pub activate_fee_max_usdt: i64,
    /// The most any transfer may pay the relay, in micro-USDT. Above the live fee.
    pub transfer_fee_max_usdt: i64,
    /// The beacon's `implementation()` when it was reviewed: 40 lowercase hex, no `0x`.
    pub expected_beacon_implementation: String,
    /// The controller's `implementation()` when it was reviewed: 40 lowercase hex, no `0x`.
    pub expected_controller_implementation: String,
    /// Sweeps go to the GasFree float until it holds this much, in micro-USDT.
    pub payout_float_target_usdt: i64,
    /// How long a signed permit stays valid, in seconds. The relay accepts 60 to 600.
    pub deadline_secs: u64,
    /// Redemptions are paid from the GasFree float (`APP_TRANSFER_RAIL=gasfree`), not from 2/0 in TRX.
    pub payouts: bool,
}

/// The GasFree half of a `SweepClient`.
pub(super) struct GasFree {
    pub(super) cfg: GasFreeConfig,
}

impl GasFree {
    pub(super) fn new(cfg: GasFreeConfig) -> Self {
        Self { cfg }
    }
}

/// What the boot check found.
#[derive(Debug, PartialEq)]
pub enum SelfTest {
    Passed,
    /// The chain answered, and the answer is wrong for this configuration. The signer must not start.
    Failed(String),
    /// TronGrid did not answer. Not fatal: the checks before each permit still run.
    Unreachable(String),
}

/// Read the GasFree settings. `Ok(None)` means GasFree is off, which is the default.
///
/// GasFree is on when `APP_GASFREE_API_KEY` is set, and then every other setting is required: a
/// missing one stops the signer at boot, not at the first deposit.
pub fn load_gasfree_config(var: impl Fn(&str) -> Option<String>) -> Result<Option<GasFreeConfig>, String> {
    todo!("Task 2 Step 5")
}

impl SweepClient {
    /// Whether `address` holds a deployed contract; for a GasFree account, whether it is activated.
    pub(super) async fn has_contract(&self, address: &str) -> Result<bool, String> {
        todo!("Task 2 Step 5")
    }

    /// The first 32-byte word a view function returns, as 64 lowercase hex characters.
    async fn view_word(&self, contract: &str, selector: &str, parameter: Option<&str>) -> Result<String, String> {
        todo!("Task 2 Step 5")
    }

    /// Once at boot: are these GasFree constants the ones deployed on the network this TronGrid
    /// serves? A Nile setting on a mainnet signer fails here instead of handing out addresses that
    /// nobody controls.
    pub async fn gasfree_self_test(&self, signer: &Signer) -> SelfTest {
        todo!("Task 2 Step 5")
    }
}
```

- [ ] **Step 3: Write the fake chain and relay, and the tests**

Create `crates/tron-signer/src/sweep/gasfree_rail/tests.rs`. Tasks 3 to 5 add tests to the end of this file; the fake below serves all of them.

```rust
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
```

- [ ] **Step 4: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test(signer): GasFree settings, the activation read and a boot check, against stubs

A fake TronGrid and a fake relay on one port serve every GasFree test in
this plan. The settings loader, the activation read and the boot
self-test are todo!() in this commit.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/tron-signer/src/sweep.rs crates/tron-signer/src/sweep/gasfree_rail.rs crates/tron-signer/src/sweep/gasfree_rail/tests.rs
git commit -F .superpowers/sdd/2026-09-24-gasfree-signer/commit-msg.txt
```

Controller: run CI. Expected: the run fails; these 9 fail by name:

```text
test sweep::gasfree_rail::tests::a_full_setting_parses_and_normalises ... FAILED
test sweep::gasfree_rail::tests::activation_is_read_from_the_contract_record_not_the_bytecode ... FAILED
test sweep::gasfree_rail::tests::each_bad_setting_is_refused_at_boot ... FAILED
test sweep::gasfree_rail::tests::the_gasfree_rail_without_an_api_key_is_refused ... FAILED
test sweep::gasfree_rail::tests::the_self_test_fails_when_the_controller_disagrees ... FAILED
test sweep::gasfree_rail::tests::the_self_test_fails_when_the_controller_is_not_a_contract ... FAILED
test sweep::gasfree_rail::tests::the_self_test_is_not_fatal_when_trongrid_is_down ... FAILED
test sweep::gasfree_rail::tests::the_self_test_passes_on_the_right_network ... FAILED
test sweep::gasfree_rail::tests::without_an_api_key_the_rail_stays_off ... FAILED
```

and `the_gasfree_address_is_only_known_when_gasfree_is_on ... ok` — it tests Step 1's real code, not a stub. The nine relay tests stay `ok`.

- [ ] **Step 5: Replace the stubs**

In `crates/tron-signer/src/sweep/gasfree_rail.rs`, replace `load_gasfree_config` with the following, and add the two helpers after it:

```rust
pub fn load_gasfree_config(var: impl Fn(&str) -> Option<String>) -> Result<Option<GasFreeConfig>, String> {
    let rail = var("APP_TRANSFER_RAIL").unwrap_or_else(|| "trx".to_string());
    let payouts = match rail.trim() {
        "trx" => false,
        "gasfree" => true,
        other => return Err(format!("APP_TRANSFER_RAIL must be trx or gasfree, got {other:?}")),
    };
    let api_key = var("APP_GASFREE_API_KEY").map(|v| v.trim().to_string()).unwrap_or_default();
    if api_key.is_empty() {
        return if payouts {
            Err("APP_TRANSFER_RAIL=gasfree needs APP_GASFREE_API_KEY and the other APP_GASFREE_* settings".into())
        } else {
            Ok(None)
        };
    }

    let required = |name: &str| {
        var(name)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("{name} must be set when APP_GASFREE_API_KEY is"))
    };
    let chain: &'static gasfree::Chain = match required("APP_GASFREE_NETWORK")?.as_str() {
        "nile" => &gasfree::NILE,
        "mainnet" => &gasfree::MAINNET,
        other => return Err(format!("APP_GASFREE_NETWORK must be nile or mainnet, got {other:?}")),
    };
    let service_provider = required("APP_GASFREE_SERVICE_PROVIDER")?;
    abi_address(&service_provider).map_err(|e| format!("APP_GASFREE_SERVICE_PROVIDER: {e}"))?;
    let deadline_secs = match var("APP_GASFREE_DEADLINE_SECS") {
        None => 180,
        Some(raw) => raw
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("APP_GASFREE_DEADLINE_SECS must be whole seconds, got {raw:?}"))?,
    };
    // The relay's published limits. A permit outside them is refused at submit, every time.
    if !(60..=600).contains(&deadline_secs) {
        return Err(format!("APP_GASFREE_DEADLINE_SECS must be 60 to 600, got {deadline_secs}"));
    }

    Ok(Some(GasFreeConfig {
        chain,
        relay: RelayConfig {
            base_url: required("APP_GASFREE_API_URL")?.trim_end_matches('/').to_string(),
            api_key,
            api_secret: required("APP_GASFREE_API_SECRET")?,
        },
        service_provider,
        activate_fee_max_usdt: positive_micro_usdt(
            "APP_GASFREE_ACTIVATE_FEE_MAX_USDT",
            &required("APP_GASFREE_ACTIVATE_FEE_MAX_USDT")?,
        )?,
        transfer_fee_max_usdt: positive_micro_usdt(
            "APP_GASFREE_TRANSFER_FEE_MAX_USDT",
            &required("APP_GASFREE_TRANSFER_FEE_MAX_USDT")?,
        )?,
        expected_beacon_implementation: implementation_hex(
            "APP_GASFREE_EXPECTED_IMPLEMENTATION",
            &required("APP_GASFREE_EXPECTED_IMPLEMENTATION")?,
        )?,
        expected_controller_implementation: implementation_hex(
            "APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION",
            &required("APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION")?,
        )?,
        payout_float_target_usdt: positive_micro_usdt(
            "APP_PAYOUT_FLOAT_TARGET_USDT",
            &required("APP_PAYOUT_FLOAT_TARGET_USDT")?,
        )?,
        deadline_secs,
        payouts,
    }))
}

/// A zero maximum would sign permits the relay refuses, stopping every sweep while looking set up.
fn positive_micro_usdt(name: &str, raw: &str) -> Result<i64, String> {
    match raw.trim().parse::<i64>() {
        Ok(v) if v > 0 => Ok(v),
        _ => Err(format!("{name} must be a positive whole number of micro-USDT, got {raw:?}")),
    }
}

/// `0xA3B0…` or `a3b0…` in, 40 lowercase hex characters out.
fn implementation_hex(name: &str, raw: &str) -> Result<String, String> {
    let hex = raw.trim().trim_start_matches("0x").trim_start_matches("0X").to_ascii_lowercase();
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{name} must be a 20-byte hex address like 0xa3b0edff…, got {raw:?}"));
    }
    Ok(hex)
}
```

In the same file, replace the three stub methods in `impl SweepClient` with:

```rust
    pub(super) async fn has_contract(&self, address: &str) -> Result<bool, String> {
        // `contract_address`, NOT `bytecode`: on 2026-09-24 the activated mainnet GasFree account
        // TBdkSW3VkKsA8RxmZFxMvNezimndUEymgg returned its contract record with an EMPTY bytecode,
        // and an address with no contract returns `{}`.
        let resp = self.post("/wallet/getcontract", serde_json::json!({"value": address, "visible": true})).await?;
        Ok(resp["contract_address"].as_str().is_some_and(|a| !a.is_empty()))
    }

    async fn view_word(&self, contract: &str, selector: &str, parameter: Option<&str>) -> Result<String, String> {
        // The contract is also the caller. TronGrid wants an `owner_address`, and a view call's
        // caller changes nothing.
        let mut body = serde_json::json!({
            "owner_address": contract,
            "contract_address": contract,
            "function_selector": selector,
            "visible": true,
        });
        if let Some(p) = parameter {
            body["parameter"] = serde_json::Value::from(p);
        }
        let resp = self.post("/wallet/triggerconstantcontract", body).await?;
        let word = resp["constant_result"][0]
            .as_str()
            .ok_or_else(|| format!("{selector} on {contract} returned nothing: {}", describe_rejection(&resp)))?;
        if word.len() != 64 || !word.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{selector} on {contract} returned {word:?}, not one 32-byte word"));
        }
        Ok(word.to_ascii_lowercase())
    }

    pub async fn gasfree_self_test(&self, signer: &Signer) -> SelfTest {
        let Some(gf) = &self.gasfree else { return SelfTest::Passed };
        let chain = gf.cfg.chain;
        match self.has_contract(chain.controller).await {
            Ok(true) => {}
            Ok(false) => {
                return SelfTest::Failed(format!(
                    "the GasFree controller {} is not a contract on this TronGrid: APP_GASFREE_NETWORK does \
                     not match APP_TRONGRID_URL",
                    chain.controller
                ))
            }
            Err(e) => return SelfTest::Unreachable(e),
        }
        let owner = match signer.address_at(0) {
            Ok(a) => a,
            Err(e) => return SelfTest::Failed(e),
        };
        let ours = match gasfree::gasfree_address(chain, &owner).and_then(|g| abi_address(&g)) {
            Ok(word) => word[24..].to_string(),
            Err(e) => return SelfTest::Failed(e),
        };
        let parameter = match abi_address(&owner) {
            Ok(p) => p,
            Err(e) => return SelfTest::Failed(e),
        };
        match self.view_word(chain.controller, "getGasFreeAddress(address)", Some(&parameter)).await {
            Ok(word) if word[24..] == ours => SelfTest::Passed,
            Ok(word) => SelfTest::Failed(format!(
                "the controller puts the GasFree account of {owner} at 0x{}, this signer derives 0x{ours}",
                &word[24..]
            )),
            Err(e) => SelfTest::Unreachable(e),
        }
    }
```

Do not change the tests.

- [ ] **Step 6: Wire it into `main.rs`**

In `crates/tron-signer/src/main.rs`:

a) Replace the `use axum::{ ... };` block's `extract::State,` line with `extract::{Path, State},`.

b) Replace the `use tron_signer::sweep::{ ... };` block with:

```rust
use tron_signer::sweep::{
    fund_float_response, load_gasfree_config, payout_response, validate_payout_cap, FundFloatOutcome,
    PayoutOutcome, SelfTest, SweepClient, SweepConfig, SweepOutcome,
};
```

c) In the `xpub` handler, replace:

```rust
    Ok(Json(json!({
        "account_xpub": s.signer.account_xpub(),
        "fee_address": fee_address,
        "payout_address": payout_address,
    })))
```

with:

```rust
    // Where redemptions are paid from on the GasFree rail, and what provisioning writes into the
    // treasury's PAYOUT_FLOAT_ADDRESS so the reserve counts the same float the signer spends from.
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
```

d) Directly above `/// ONLY an index.`, add the handler:

```rust
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
```

e) In `main`, replace:

```rust
    let sweeper = Arc::new(SweepClient::new(SweepConfig {
```

with:

```rust
    // Off unless APP_GASFREE_API_KEY is set. A half-configured rail stops the signer here, not at
    // the first deposit.
    let gasfree = load_gasfree_config(|name| std::env::var(name).ok()).unwrap_or_else(|e| panic!("{e}"));

    let mut sweeper = SweepClient::new(SweepConfig {
```

and replace the line that closes that call:

```rust
    }));
```

with:

```rust
    });
    if let Some(cfg) = gasfree {
        tracing::info!(chain_id = cfg.chain.chain_id, payouts = cfg.payouts, "GasFree rail on");
        sweeper = sweeper.with_gasfree(cfg);
    }
    let sweeper = Arc::new(sweeper);
    match sweeper.gasfree_self_test(&signer).await {
        SelfTest::Passed => {}
        SelfTest::Failed(e) => panic!("GasFree self-test failed: {e}"),
        SelfTest::Unreachable(e) => tracing::warn!(
            "GasFree self-test could not reach TronGrid, so it did not run; every permit is still checked \
             before it is signed: {e}"
        ),
    }
```

f) Add the route after `.route("/internal/xpub", get(xpub))`:

```rust
        .route("/internal/addresses/:index", get(addresses))
```

g) In the module doc at the top of `main.rs`, replace these two lines:

```rust
//! Four routes. The sweep route's shape IS its security argument: it accepts an INDEX and nothing
//! else, so no field a caller sets can redirect funds.
```

with:

```rust
//! The sweep route's shape IS its security argument: it accepts an INDEX and nothing else, so no
//! field a caller sets can redirect funds. That holds on the GasFree rail too — every field of a
//! GasFree permit comes from this service's config or from the chain.
//!
//! `/internal/addresses/:index` and `/internal/xpub` move nothing; they publish derived addresses.
```

- [ ] **Step 7: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(signer): GasFree settings, the activation read and a boot check

GasFree stays off unless APP_GASFREE_API_KEY is set; then every setting
is required and checked at boot. Activation is read from getcontract's
contract_address, because an activated GasFree account has an empty
bytecode. At boot the controller must be a contract on this TronGrid
and its getGasFreeAddress must match this signer's derivation. Adds
GET /internal/addresses/:index, and the GasFree float to /internal/xpub.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/tron-signer/src/sweep/gasfree_rail.rs crates/tron-signer/src/main.rs
git commit -F .superpowers/sdd/2026-09-24-gasfree-signer/commit-msg.txt
```

Controller: run CI. Expected: success; all 10 `sweep::gasfree_rail::tests::` tests and the nine `relay::tests::` say `ok` by name; no warning located in `crates/tron-signer`.

---

### Task 3: The GasFree sweep

**Files:**
- Modify: `crates/gasfree/src/lib.rs` (`fee_to_hold`, and its test)
- Modify: `crates/tron-signer/src/sweep.rs` (`SweepOutcome` variants, `sweep_response`, the dispatch in `sweep`)
- Modify: `crates/tron-signer/src/sweep/gasfree_rail.rs` (the `relay` field, `chain_nonce`, `code_changed`, `sign_permit`, `sweep_gasfree`, `sweep_receiver`, `deadline_after`)
- Modify: `crates/tron-signer/src/main.rs` (the sweep handler)
- Modify: `README.md` (the signer's row)
- Test: `crates/tron-signer/src/sweep/gasfree_rail/tests.rs`, `crates/gasfree/src/lib.rs`

**Interfaces:**
- Consumes: Task 2's `has_contract`, `view_word`, `GasFree`; Task 1's `Relay`, `Relay::account`, `Relay::submit`, `RelayError`; `SweepClient`'s private `usdt_balance`; `Signer::signing_key_at(u32)` and `payout_address()`; `gasfree::{permit_hash, Permit}`.
- Produces:
  - `GasFree` gains `pub(super) relay: Relay`
  - on `SweepClient`, for Tasks 4-5: `pub(super) async fn chain_nonce(&self, chain: &gasfree::Chain, user: &str) -> Result<u64, String>` and `pub(super) async fn code_changed(&self, cfg: &GasFreeConfig) -> Result<Option<String>, String>`
  - `pub(super) fn sign_permit(key: &SigningKey, chain: &gasfree::Chain, permit: &gasfree::Permit<'_>) -> Result<String, String>` — 130 hex characters, `r ‖ s ‖ v` with `v` 27 or 28
  - `fn deadline_after(secs: u64) -> u64`
  - `pub fn gasfree::fee_to_hold(activated: bool, activate_max_usdt: i64, transfer_max_usdt: i64) -> i64` (Plan 3's treasury uses it for `fee_reserved`)
  - `SweepOutcome` gains `Pending { trace_id: String, gasfree_address: String, receiver: String, value_usdt: i64, max_fee_usdt: i64 }`, `Busy { gasfree_address: String }`, `Rejected { reason: String, message: String }`, `Halted { reason: String }`, `BelowFee { gasfree_address: String, balance_usdt: i64, max_fee_usdt: i64 }`
  - `pub fn sweep_response(outcome: &SweepOutcome) -> serde_json::Value` with statuses `swept`, `nothing_to_sweep`, `funded`, `fee_account_dry` (unchanged), and `pending`, `busy`, `rejected`, `halted`, `below_fee` with the fields named above (Plan 3's `HttpSigner` parses these)

- [ ] **Step 1: The shared fee rule, with its test**

This one is written real at once, not as a stub. It is two lines, and a failing test in the `gasfree` crate would stop cargo before it reaches the signer's red tests in Step 4, which are the ones that matter.

In `crates/gasfree/src/lib.rs`, directly above `const TRON_ADDRESS_VERSION: u8 = 0x41;`, add:

```rust
/// How much one transfer out of a GasFree account may pay the relay: the transfer fee, plus the
/// activation fee when the account has never made a transfer.
///
/// One copy, used by the treasury to size what it holds back from a mint and by the signer to set
/// the permit's `maxFee`. The two must be the same number — a `maxFee` above what was held back is
/// the one way this rail can leave CLT under-reserved — so neither service computes it on its own.
/// `activated` must come from the chain, never from the relay's `active` field.
pub fn fee_to_hold(activated: bool, activate_max_usdt: i64, transfer_max_usdt: i64) -> i64 {
    if activated {
        transfer_max_usdt
    } else {
        activate_max_usdt + transfer_max_usdt
    }
}

#[cfg(test)]
mod tests {
    use super::fee_to_hold;

    #[test]
    fn the_fee_to_hold_includes_activation_only_before_it() {
        assert_eq!(fee_to_hold(false, 1_500_000, 500_000), 2_000_000, "a first transfer also activates");
        assert_eq!(fee_to_hold(true, 1_500_000, 500_000), 500_000, "an activated account pays one transfer fee");
    }
}
```

- [ ] **Step 2: The new outcomes and their wire form, as stubs**

In `crates/tron-signer/src/sweep.rs`, inside `pub enum SweepOutcome`, after the `FeeAccountDry` variant, add:

```rust
    /// A GasFree permit is with the relay. NOT yet swept: the chain decides that on a later pass,
    /// when the GasFree account's balance is gone. `value_usdt` is what `receiver` will get and
    /// `max_fee_usdt` the most the relay may take on top.
    Pending { trace_id: String, gasfree_address: String, receiver: String, value_usdt: i64, max_fee_usdt: i64 },
    /// A transfer from this GasFree account is already in flight. Nothing was signed; try next pass.
    Busy { gasfree_address: String },
    /// The relay refused the permit. `reason` is its exception name. `MaxFeeExceededException`
    /// means the live fee rose above the configured maximum: the deposit stays put, still counted,
    /// and a human must decide — never a higher `maxFee` than the treasury held back.
    Rejected { reason: String, message: String },
    /// Nothing was signed, and nothing will be until a human acts: GasFree's code changed, or the
    /// relay and this signer disagree about an address.
    Halted { reason: String },
    /// The GasFree account holds less than one transfer's fee, and the plain address holds nothing.
    /// Not an error on its own; a later deposit to the same account lifts it over the fee.
    BelowFee { gasfree_address: String, balance_usdt: i64, max_fee_usdt: i64 },
```

Directly above `/// The wire form of a payout outcome.`, add:

```rust
/// The wire form of a sweep outcome.
///
/// A contract with treasury-service's `HttpSigner`, which treats any status it does not know as a
/// failure — so a typo here stalls sweeps rather than moving money. The first four are the same
/// literals the handler sent before GasFree existed.
pub fn sweep_response(outcome: &SweepOutcome) -> serde_json::Value {
    todo!("Task 3 Step 5")
}
```

In `pub async fn sweep`, replace:

```rust
        let from = signer.address_at(index)?;

        let amount = self.usdt_balance(&from).await?;
        if amount == 0 {
            return Ok(SweepOutcome::NothingToSweep);
        }
```

with:

```rust
        let from = signer.address_at(index)?;

        // The GasFree account first, and only when GasFree is on. Its dust — a balance that cannot
        // pay its own fee — must not stop the plain address below from being swept.
        let mut gasfree_dust = None;
        if let Some(gf) = &self.gasfree {
            let g = gasfree::gasfree_address(gf.cfg.chain, &from)?;
            let balance = self.usdt_balance(&g).await?;
            if balance > 0 {
                match self.sweep_gasfree(gf, signer, index, &from, &g, balance).await? {
                    dust @ SweepOutcome::BelowFee { .. } => gasfree_dust = Some(dust),
                    other => return Ok(other),
                }
            }
        }

        let amount = self.usdt_balance(&from).await?;
        if amount == 0 {
            return Ok(gasfree_dust.unwrap_or(SweepOutcome::NothingToSweep));
        }
```

In `crates/tron-signer/src/sweep/gasfree_rail.rs`:

a) Replace the three `use` lines at the top:

```rust
use super::{abi_address, describe_rejection, SweepClient};
use crate::keys::Signer;
use crate::relay::RelayConfig;
```

with:

```rust
use k256::ecdsa::{signature::hazmat::PrehashSigner, RecoveryId, Signature, SigningKey};

use super::{abi_address, describe_rejection, SweepClient, SweepOutcome};
use crate::keys::Signer;
use crate::relay::{Relay, RelayConfig, RelayError};
```

b) Replace the `GasFree` struct and its `impl` with:

```rust
/// The GasFree half of a `SweepClient`: its settings and its relay.
pub(super) struct GasFree {
    pub(super) cfg: GasFreeConfig,
    pub(super) relay: Relay,
}

impl GasFree {
    pub(super) fn new(cfg: GasFreeConfig) -> Self {
        let relay = Relay::new(cfg.relay.clone());
        Self { cfg, relay }
    }
}
```

c) Directly after `implementation_hex`, add:

```rust
/// Sign a permit as TIP-712 wallets do: the permit hash itself, no prefix, and `v` as 27 or 28.
///
/// Not `sign_txid`'s convention, where TRON wants the bare recovery id 0 or 1. The GasFree docs'
/// own example signature ends in `1b`, which is 27.
pub(super) fn sign_permit(key: &SigningKey, chain: &gasfree::Chain, permit: &gasfree::Permit<'_>) -> Result<String, String> {
    todo!("Task 3 Step 5")
}
```

d) At the end of `impl SweepClient` (after `gasfree_self_test`), add:

```rust
    /// The next nonce the controller will accept from `user`: the chain's count, not the relay's.
    pub(super) async fn chain_nonce(&self, chain: &gasfree::Chain, user: &str) -> Result<u64, String> {
        todo!("Task 3 Step 5")
    }

    /// Why permits must stop, when GasFree's code is not the reviewed code; `None` when it is.
    pub(super) async fn code_changed(&self, cfg: &GasFreeConfig) -> Result<Option<String>, String> {
        todo!("Task 3 Step 5")
    }

    /// Sweep the GasFree account `g` of the wallet `owner` at `index`, which holds `balance`.
    pub(super) async fn sweep_gasfree(
        &self,
        gf: &GasFree,
        signer: &Signer,
        index: u32,
        owner: &str,
        g: &str,
        balance: i64,
    ) -> Result<SweepOutcome, String> {
        todo!("Task 3 Step 5")
    }

    /// Where a GasFree sweep sends its value.
    async fn sweep_receiver(&self, gf: &GasFree, signer: &Signer) -> Result<String, String> {
        todo!("Task 3 Step 5")
    }
```

In `crates/tron-signer/src/main.rs`, replace the whole `sweep` handler with:

```rust
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
```

and add `sweep_response,` to the `use tron_signer::sweep::{ ... }` list (after `payout_response,`), and remove `SweepOutcome` from that list (the handler no longer names a variant).

- [ ] **Step 3: The sweep tests**

Append to `crates/tron-signer/src/sweep/gasfree_rail/tests.rs`:

```rust
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
```

- [ ] **Step 4: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test(signer): the GasFree sweep, against stubs

Sweep tests against the fake chain and relay: the permit's every field,
the fee held back before and after activation, where the value goes,
the tripwire, a relay that disagrees, a transfer in flight, a refusal,
and dust. The chain nonce, the tripwire read, permit signing,
sweep_gasfree, sweep_receiver and sweep_response are todo!() in this
commit; gasfree::fee_to_hold is real.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/gasfree/src/lib.rs crates/tron-signer/src/sweep.rs crates/tron-signer/src/sweep/gasfree_rail.rs crates/tron-signer/src/sweep/gasfree_rail/tests.rs crates/tron-signer/src/main.rs
git commit -F .superpowers/sdd/2026-09-24-gasfree-signer/commit-msg.txt
```

Controller: run CI. Expected: the run fails. `tests::the_fee_to_hold_includes_activation_only_before_it ... ok` in the `gasfree` crate (written real in Step 1), and these fail by name in the signer:

```text
test sweep::gasfree_rail::tests::a_changed_beacon_halts_before_the_relay_is_asked ... FAILED
test sweep::gasfree_rail::tests::a_changed_beacon_or_controller_is_named ... FAILED
test sweep::gasfree_rail::tests::a_permit_signature_recovers_to_the_signing_key_with_v_27_or_28 ... FAILED
test sweep::gasfree_rail::tests::the_nonce_is_the_controllers_not_the_relays ... FAILED
test sweep::gasfree_rail::tests::a_deposit_is_swept_by_a_permit_for_everything_above_the_fee ... FAILED
test sweep::gasfree_rail::tests::a_relay_refusal_is_reported_as_rejected ... FAILED
test sweep::gasfree_rail::tests::a_relay_that_disagrees_about_the_address_halts ... FAILED
test sweep::gasfree_rail::tests::a_transfer_in_flight_makes_the_sweep_wait ... FAILED
test sweep::gasfree_rail::tests::an_activated_account_holds_back_only_the_transfer_fee ... FAILED
test sweep::gasfree_rail::tests::dust_alone_is_reported_below_fee ... FAILED
test sweep::gasfree_rail::tests::dust_that_cannot_pay_its_fee_does_not_block_the_plain_address ... FAILED
test sweep::gasfree_rail::tests::every_sweep_status_string_is_pinned ... FAILED
test sweep::gasfree_rail::tests::the_float_stops_receiving_once_it_reaches_its_target ... FAILED
test sweep::gasfree_rail::tests::with_trx_payouts_every_sweep_goes_to_custody ... FAILED
```

`without_gasfree_the_sweep_never_looks_at_a_gasfree_address ... ok` is expected: it guards today's behaviour, which Step 2's dispatch already keeps.

- [ ] **Step 5: Replace the stubs**

In `crates/tron-signer/src/sweep.rs`, replace the body of `sweep_response`:

```rust
pub fn sweep_response(outcome: &SweepOutcome) -> serde_json::Value {
    use serde_json::json;
    match outcome {
        SweepOutcome::Swept { tx_id, amount_usdt } => json!({"status": "swept", "tx_id": tx_id, "amount_usdt": amount_usdt}),
        SweepOutcome::NothingToSweep => json!({"status": "nothing_to_sweep"}),
        SweepOutcome::Funded { tx_id, amount_sun } => json!({"status": "funded", "tx_id": tx_id, "amount_sun": amount_sun}),
        SweepOutcome::FeeAccountDry { fee_address, have_sun, need_sun } => json!({
            "status": "fee_account_dry",
            "fee_address": fee_address,
            "have_sun": have_sun,
            "need_sun": need_sun,
        }),
        SweepOutcome::Pending { trace_id, gasfree_address, receiver, value_usdt, max_fee_usdt } => json!({
            "status": "pending",
            "trace_id": trace_id,
            "gasfree_address": gasfree_address,
            "receiver": receiver,
            "value_usdt": value_usdt,
            "max_fee_usdt": max_fee_usdt,
        }),
        SweepOutcome::Busy { gasfree_address } => json!({"status": "busy", "gasfree_address": gasfree_address}),
        SweepOutcome::Rejected { reason, message } => json!({"status": "rejected", "reason": reason, "message": message}),
        SweepOutcome::Halted { reason } => json!({"status": "halted", "reason": reason}),
        SweepOutcome::BelowFee { gasfree_address, balance_usdt, max_fee_usdt } => json!({
            "status": "below_fee",
            "gasfree_address": gasfree_address,
            "balance_usdt": balance_usdt,
            "max_fee_usdt": max_fee_usdt,
        }),
    }
}
```

In `crates/tron-signer/src/sweep/gasfree_rail.rs`, replace the body of `sign_permit`:

```rust
pub(super) fn sign_permit(key: &SigningKey, chain: &gasfree::Chain, permit: &gasfree::Permit<'_>) -> Result<String, String> {
    let hash = gasfree::permit_hash(chain, permit)?;
    let (sig, recid): (Signature, RecoveryId) =
        key.sign_prehash(&hash).map_err(|e| format!("signing the permit failed: {e}"))?;
    Ok(format!("{}{:02x}", hex::encode(sig.to_bytes()), recid.to_byte() + 27))
}
```

and replace the four stub methods — `chain_nonce`, `code_changed`, `sweep_gasfree`, `sweep_receiver` — with:

```rust
    pub(super) async fn chain_nonce(&self, chain: &gasfree::Chain, user: &str) -> Result<u64, String> {
        let word = self.view_word(chain.controller, "nonces(address)", Some(&abi_address(user)?)).await?;
        let (high, low) = word.split_at(48);
        if high.bytes().any(|b| b != b'0') {
            return Err(format!("nonces({user}) returned 0x{word}, more than a u64"));
        }
        u64::from_str_radix(low, 16).map_err(|e| format!("nonces({user}) returned 0x{word}: {e}"))
    }

    pub(super) async fn code_changed(&self, cfg: &GasFreeConfig) -> Result<Option<String>, String> {
        let checks = [
            ("beacon", cfg.chain.beacon, &cfg.expected_beacon_implementation),
            ("controller", cfg.chain.controller, &cfg.expected_controller_implementation),
        ];
        for (what, proxy, expected) in checks {
            let word = self.view_word(proxy, "implementation()", None).await?;
            let now = &word[24..];
            if now != expected.as_str() {
                return Ok(Some(format!(
                    "the GasFree {what} {proxy} now runs 0x{now}, not the reviewed 0x{expected}; nothing \
                     will be signed until someone reviews the new code and updates the setting"
                )));
            }
        }
        Ok(None)
    }

    pub(super) async fn sweep_gasfree(
        &self,
        gf: &GasFree,
        signer: &Signer,
        index: u32,
        owner: &str,
        g: &str,
        balance: i64,
    ) -> Result<SweepOutcome, String> {
        // The same number the treasury held back when it minted: the same maxima, the same
        // function, and the same on-chain fact about activation.
        let activated = self.has_contract(g).await?;
        let max_fee = gasfree::fee_to_hold(activated, gf.cfg.activate_fee_max_usdt, gf.cfg.transfer_fee_max_usdt);
        if balance <= max_fee {
            return Ok(SweepOutcome::BelowFee { gasfree_address: g.to_string(), balance_usdt: balance, max_fee_usdt: max_fee });
        }

        if let Some(reason) = self.code_changed(&gf.cfg).await? {
            return Ok(SweepOutcome::Halted { reason });
        }

        let account = gf
            .relay
            .account(owner, &self.cfg.usdt_contract)
            .await
            .map_err(|e| format!("reading the GasFree account of {owner} from the relay: {e:?}"))?;
        if account.gasfree_address != g {
            return Ok(SweepOutcome::Halted {
                reason: format!(
                    "the relay puts the GasFree account of {owner} at {}, this signer derives {g}",
                    account.gasfree_address
                ),
            });
        }
        let nonce = self.chain_nonce(gf.cfg.chain, owner).await?;
        if !account.allow_submit || account.frozen > 0 || account.nonce != nonce {
            return Ok(SweepOutcome::Busy { gasfree_address: g.to_string() });
        }

        let receiver = self.sweep_receiver(gf, signer).await?;
        let value = balance - max_fee;
        let permit = gasfree::Permit {
            token: &self.cfg.usdt_contract,
            service_provider: &gf.cfg.service_provider,
            user: owner,
            receiver: &receiver,
            value: u64::try_from(value).map_err(|_| format!("sweep value {value} is negative"))?,
            max_fee: u64::try_from(max_fee).map_err(|_| format!("maxFee {max_fee} is negative"))?,
            deadline: deadline_after(gf.cfg.deadline_secs),
            version: 1,
            nonce,
        };
        let sig = sign_permit(&signer.signing_key_at(index)?, gf.cfg.chain, &permit)?;
        // Taken out of the match so the permit's borrow of `receiver` has ended before `receiver`
        // moves into the outcome.
        let reply = gf.relay.submit(&permit, &sig).await;
        match reply {
            Ok(trace_id) => Ok(SweepOutcome::Pending {
                trace_id,
                gasfree_address: g.to_string(),
                receiver,
                value_usdt: value,
                max_fee_usdt: max_fee,
            }),
            Err(RelayError::Refused { reason, message }) => Ok(SweepOutcome::Rejected { reason, message }),
            // A sweep can only move money into this service's own float or custody, so an unclear
            // answer is safe to leave to the next pass: the chain will show whether it ran, and a
            // second permit reuses the same nonce unless the first one executed.
            Err(RelayError::Unavailable(e)) => Err(format!("submitting the sweep permit for {g}: {e}")),
        }
    }

    async fn sweep_receiver(&self, gf: &GasFree, signer: &Signer) -> Result<String, String> {
        // The float first, while it is below its target and while redemptions are paid from it;
        // custody otherwise. Both are fixed — the float derived, custody configured — so the caller
        // still chooses nothing (spec §4).
        //
        // ponytail: one deposit larger than the target still goes wholly to the float, so the float
        // can overshoot by one deposit. Splitting one sweep between two receivers needs two permits.
        if gf.cfg.payouts {
            let float = gasfree::gasfree_address(gf.cfg.chain, &signer.payout_address()?)?;
            if self.usdt_balance(&float).await? < gf.cfg.payout_float_target_usdt {
                return Ok(float);
            }
        }
        Ok(self.cfg.treasury_address.clone())
    }
```

and add this free function directly after `sign_permit`:

```rust
/// Now plus `secs`, in unix seconds: when a permit signed now stops being valid.
fn deadline_after(secs: u64) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + secs
}
```

In `README.md`, replace the `tron-signer` row of the Crates table with:

```markdown
| `tron-signer` | Signs TRON transactions and GasFree permits: sweeps and payouts | the deposit wallet **mnemonic**, and the GasFree API key |
```

Do not change the tests.

- [ ] **Step 6: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(signer): sweep a GasFree account with a permit

sweep(index) still takes only an index. When GasFree is on it reads the
GasFree account of that index first: it holds back
gasfree::fee_to_hold(activated on chain, the maxima), checks GasFree's
code and the relay's view of the address, waits if a transfer is in
flight, and sends balance - maxFee to the float (below its target) or
custody. Dust at the GasFree account does not block the plain address.
Without GasFree settings nothing changes.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/tron-signer/src/sweep.rs crates/tron-signer/src/sweep/gasfree_rail.rs README.md
git commit -F .superpowers/sdd/2026-09-24-gasfree-signer/commit-msg.txt
```

Controller: run CI. Expected: success; `tests::the_fee_to_hold_includes_activation_only_before_it`, the 15 new `sweep::gasfree_rail::tests::` tests, Task 2's 10 and Task 1's 9 all `ok` by name; no warning located in `crates/tron-signer` or `crates/gasfree`.

---

### Task 4: The GasFree payout, and trace lookup

**Files:**
- Modify: `crates/tron-signer/src/sweep.rs` (`PayoutOutcome` variants, `payout_response`, the dispatch in `payout`)
- Modify: `crates/tron-signer/src/sweep/gasfree_rail.rs` (`payout_gasfree`, `PRE_EXECUTION_REFUSALS`, `gasfree_trace`, `trace_response`)
- Modify: `crates/tron-signer/src/main.rs` (the payout handler's log, the trace route)
- Test: `crates/tron-signer/src/sweep/gasfree_rail/tests.rs`

**Interfaces:**
- Consumes: Task 2's reads and `sign_permit`; Task 3's `deadline_after`; `gasfree::fee_to_hold`; `Relay::trace`, `Trace`, `is_trace_id`.
- Produces:
  - `PayoutOutcome` gains `Submitted { trace_id: String }` and `FloatNotActive { float_address: String }`; `payout_response` gives them `{"status": "submitted", "trace_id"}` and `{"status": "float_not_active", "float_address"}` (Plan 3's `HttpPayoutSigner` parses these)
  - `pub async fn SweepClient::gasfree_trace(&self, trace_id: &str) -> Result<Option<crate::relay::Trace>, String>`
  - `pub fn trace_response(t: &crate::relay::Trace) -> serde_json::Value` → `{"state", "txn_hash", "txn_state", "txn_amount", "txn_total_fee"}`
  - `GET /internal/gasfree/trace/:trace_id` → that JSON; 400 for a malformed id, 404 when GasFree is off, 502 when the relay does not answer

- [ ] **Step 1: The outcomes, as stubs where new**

In `crates/tron-signer/src/sweep.rs`, inside `pub enum PayoutOutcome`, after the `Refused(String),` variant, add:

```rust
    /// A GasFree permit paying `to` is with the relay. Not yet paid: the treasury follows
    /// `trace_id` to the on-chain transaction and confirms it there, as it confirms a TRX payout.
    Submitted { trace_id: String },
    /// The GasFree float has never made a transfer, so its next one would also pay the activation
    /// fee, which a redemption's fee does not cover. Provably nothing was signed. Only the one-time
    /// activation (`/internal/activate-float`) resolves it.
    FloatNotActive { float_address: String },
```

In `payout_response`, add these two arms before the `Refused` arm:

```rust
        PayoutOutcome::Submitted { .. } => todo!("Task 4 Step 5"),
        PayoutOutcome::FloatNotActive { .. } => todo!("Task 4 Step 5"),
```

In `pub async fn payout`, directly below the `if amount_usdt <= 0 { ... }` block, add:

```rust
        // The GasFree float pays when redemptions are on that rail. Same two checks above, first,
        // for both rails.
        if let Some(gf) = self.gasfree.as_ref().filter(|gf| gf.cfg.payouts) {
            return self.payout_gasfree(gf, signer, to, amount_usdt).await;
        }
```

In `crates/tron-signer/src/sweep/gasfree_rail.rs`, change `use super::{abi_address, describe_rejection, SweepClient, SweepOutcome};` to:

```rust
use super::{abi_address, describe_rejection, PayoutOutcome, SweepClient, SweepOutcome};
use crate::relay::Trace;
```

and add, after `sweep_receiver` inside `impl SweepClient`:

```rust
    /// Pay `amount_usdt` to `to` from the GasFree float, `F = gasfree(2/0)`.
    pub(super) async fn payout_gasfree(
        &self,
        gf: &GasFree,
        signer: &Signer,
        to: &str,
        amount_usdt: i64,
    ) -> Result<PayoutOutcome, String> {
        todo!("Task 4 Step 5")
    }

    /// What became of a permit, by trace id. `None` when GasFree is off.
    pub async fn gasfree_trace(&self, trace_id: &str) -> Result<Option<Trace>, String> {
        todo!("Task 4 Step 5")
    }
```

and, after `deadline_after`:

```rust
/// The refusals the GasFree docs list for `submit`. Each is the relay's pre-execution check
/// failing, so for a payout it proves the permit did not pay. Any other answer to a signed payout
/// permit is ambiguous: the permit may still execute before its deadline.
const PRE_EXECUTION_REFUSALS: [&str; 9] = [
    "ProviderAddressNotMatchException",
    "DeadlineExceededException",
    "InvalidSignatureException",
    "UnsupportedTokenException",
    "TooManyPendingTransferException",
    "VersionNotSupportedException",
    "NonceNotMatchException",
    "MaxFeeExceededException",
    "InsufficientBalanceException",
];

/// The wire form of a trace, in this service's own names.
pub fn trace_response(t: &Trace) -> serde_json::Value {
    todo!("Task 4 Step 5")
}
```

and add `trace_response` to the `pub use gasfree_rail::{...}` line in `sweep.rs`:

```rust
pub use gasfree_rail::{load_gasfree_config, trace_response, GasFreeConfig, SelfTest};
```

In `crates/tron-signer/src/main.rs`:

a) In the payout handler's `match &outcome { ... }`, add before the `PayoutOutcome::Refused(reason)` arm:

```rust
                PayoutOutcome::Submitted { trace_id } => tracing::info!(intent_id = %req.intent_id, to = %req.to, amount_usdt = req.amount_usdt, %trace_id, "payout permit submitted"),
                PayoutOutcome::FloatNotActive { float_address } => tracing::warn!(intent_id = %req.intent_id, %float_address, "the GasFree float is not activated yet"),
```

b) Add `trace_response,` to the `use tron_signer::sweep::{ ... }` list.

c) Directly above `/// No request struct, deliberately:`, add:

```rust
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
```

d) Add the route after `.route("/internal/payout", post(payout))`:

```rust
        .route("/internal/gasfree/trace/:trace_id", get(gasfree_trace))
```

- [ ] **Step 2: The payout tests**

Append to `crates/tron-signer/src/sweep/gasfree_rail/tests.rs`:

```rust
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
```

- [ ] **Step 3: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test(signer): the GasFree payout and trace lookup, against stubs

Payout tests against the fake chain and relay: the permit comes from the
float's owner for exactly the amount, an unactivated or short float
signs nothing, the tripwire refuses, only documented relay refusals are
clear answers, and TRX payouts are untouched. payout_gasfree,
gasfree_trace, trace_response and the two new wire forms are todo!() in
this commit.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/tron-signer/src/sweep.rs crates/tron-signer/src/sweep/gasfree_rail.rs crates/tron-signer/src/sweep/gasfree_rail/tests.rs crates/tron-signer/src/main.rs
git commit -F .superpowers/sdd/2026-09-24-gasfree-signer/commit-msg.txt
```

Controller: run CI. Expected: the run fails; these fail by name:

```text
test sweep::gasfree_rail::tests::a_changed_beacon_refuses_the_payout ... FAILED
test sweep::gasfree_rail::tests::a_documented_relay_refusal_is_a_provable_non_payment ... FAILED
test sweep::gasfree_rail::tests::a_float_short_of_amount_plus_fee_is_dry ... FAILED
test sweep::gasfree_rail::tests::a_gasfree_payout_is_a_permit_from_the_float_for_exactly_the_amount ... FAILED
test sweep::gasfree_rail::tests::a_trace_is_reported_in_the_signers_own_words ... FAILED
test sweep::gasfree_rail::tests::an_unactivated_float_answers_float_not_active_and_signs_nothing ... FAILED
test sweep::gasfree_rail::tests::any_other_relay_answer_after_signing_is_ambiguous ... FAILED
test sweep::gasfree_rail::tests::the_new_payout_status_strings_are_pinned ... FAILED
```

`with_trx_payouts_the_gasfree_float_is_never_touched ... ok` is expected (today's path, kept by Step 1's dispatch), and every earlier test stays `ok`.

- [ ] **Step 4: Replace the stubs**

In `crates/tron-signer/src/sweep.rs`, replace the two `todo!()` arms of `payout_response` with:

```rust
        PayoutOutcome::Submitted { trace_id } => serde_json::json!({"status": "submitted", "trace_id": trace_id}),
        PayoutOutcome::FloatNotActive { float_address } => {
            serde_json::json!({"status": "float_not_active", "float_address": float_address})
        }
```

In `crates/tron-signer/src/sweep/gasfree_rail.rs`, replace the two stub methods with:

```rust
    pub(super) async fn payout_gasfree(
        &self,
        gf: &GasFree,
        signer: &Signer,
        to: &str,
        amount_usdt: i64,
    ) -> Result<PayoutOutcome, String> {
        // Until the permit is signed and sent, every failure provably moved nothing, so it is
        // Refused and never Err: the treasury retries a Refused and hands anything else to a human.
        let refused = |what: &str, e: String| -> Result<PayoutOutcome, String> {
            Ok(PayoutOutcome::Refused(format!("{what}: {e}")))
        };

        let owner = match signer.payout_address() {
            Ok(a) => a,
            Err(e) => return refused("deriving the float's owner", e),
        };
        let float = match gasfree::gasfree_address(gf.cfg.chain, &owner) {
            Ok(a) => a,
            Err(e) => return refused("deriving the GasFree float", e),
        };
        if let Err(e) = abi_address(to) {
            return refused("the payout destination", e);
        }
        match self.code_changed(&gf.cfg).await {
            Ok(None) => {}
            Ok(Some(reason)) => return Ok(PayoutOutcome::Refused(reason)),
            Err(e) => return refused("reading GasFree's code", e),
        }
        match self.has_contract(&float).await {
            Ok(true) => {}
            Ok(false) => return Ok(PayoutOutcome::FloatNotActive { float_address: float }),
            Err(e) => return refused("reading whether the GasFree float is activated", e),
        }
        // Activated, so one transfer fee and no activation fee.
        let max_fee = gasfree::fee_to_hold(true, gf.cfg.activate_fee_max_usdt, gf.cfg.transfer_fee_max_usdt);
        let need = amount_usdt + max_fee;
        let have = match self.usdt_balance(&float).await {
            Ok(v) => v,
            Err(e) => return refused("reading the GasFree float's balance", e),
        };
        if have < need {
            return Ok(PayoutOutcome::FloatDry { float_address: float, have_usdt: have, need_usdt: need });
        }
        let account = match gf.relay.account(&owner, &self.cfg.usdt_contract).await {
            Ok(a) => a,
            Err(e) => return refused("reading the GasFree float's account from the relay", format!("{e:?}")),
        };
        if account.gasfree_address != float {
            return Ok(PayoutOutcome::Refused(format!(
                "the relay puts the GasFree float at {}, this signer derives {float}",
                account.gasfree_address
            )));
        }
        let nonce = match self.chain_nonce(gf.cfg.chain, &owner).await {
            Ok(n) => n,
            Err(e) => return refused("reading the float's nonce", e),
        };
        if !account.allow_submit || account.frozen > 0 || account.nonce != nonce {
            return Ok(PayoutOutcome::Refused(
                "a transfer from the GasFree float is still in flight; retry once it lands".into(),
            ));
        }
        let key = match signer.payout_signing_key() {
            Ok(k) => k,
            Err(e) => return refused("deriving the payout signing key", e),
        };
        let value = match u64::try_from(amount_usdt) {
            Ok(v) => v,
            Err(_) => return refused("the payout amount", format!("{amount_usdt} is negative")),
        };
        let permit = gasfree::Permit {
            token: &self.cfg.usdt_contract,
            service_provider: &gf.cfg.service_provider,
            user: &owner,
            receiver: to,
            value,
            max_fee: max_fee as u64,
            deadline: deadline_after(gf.cfg.deadline_secs),
            version: 1,
            nonce,
        };
        let sig = match sign_permit(&key, gf.cfg.chain, &permit) {
            Ok(s) => s,
            Err(e) => return refused("signing the payout permit", e),
        };

        // From here the relay holds a permit that pays `to`. Only a trace id, or a refusal the
        // docs list as a pre-execution check, is a clear answer. Anything else may still execute
        // before the deadline, so it goes to a human as ambiguous and is never retried.
        match gf.relay.submit(&permit, &sig).await {
            Ok(trace_id) => Ok(PayoutOutcome::Submitted { trace_id }),
            Err(RelayError::Refused { reason, message }) if PRE_EXECUTION_REFUSALS.contains(&reason.as_str()) => {
                Ok(PayoutOutcome::Refused(format!("the relay refused the payout permit: {reason} {message}")))
            }
            Err(e) => Err(format!("the relay gave no clear answer to a signed payout permit: {e:?}")),
        }
    }

    pub async fn gasfree_trace(&self, trace_id: &str) -> Result<Option<Trace>, String> {
        let Some(gf) = &self.gasfree else { return Ok(None) };
        gf.relay.trace(trace_id).await.map(Some).map_err(|e| format!("{e:?}"))
    }
```

and replace the body of `trace_response`:

```rust
pub fn trace_response(t: &Trace) -> serde_json::Value {
    serde_json::json!({
        "state": t.state,
        "txn_hash": t.txn_hash,
        "txn_state": t.txn_state,
        "txn_amount": t.txn_amount,
        "txn_total_fee": t.txn_total_fee,
    })
}
```

Do not change the tests.

- [ ] **Step 5: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(signer): pay redemptions from the GasFree float by permit

With APP_TRANSFER_RAIL=gasfree a payout is a permit from
F = gasfree(2/0), for exactly the amount, with one transfer fee on top.
Everything before the permit is sent is a provable refusal; after it,
only a trace id or a refusal the docs list as pre-execution is a clear
answer, and anything else is ambiguous. Adds
GET /internal/gasfree/trace/:trace_id. TRX payouts are unchanged.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/tron-signer/src/sweep.rs crates/tron-signer/src/sweep/gasfree_rail.rs
git commit -F .superpowers/sdd/2026-09-24-gasfree-signer/commit-msg.txt
```

Controller: run CI. Expected: success; the 9 new tests and every earlier one `ok` by name; no warning located in `crates/tron-signer`.

---

### Task 5: Activating the GasFree float

**Files:**
- Modify: `crates/tron-signer/src/sweep/gasfree_rail.rs` (`ActivateFloatOutcome`, `activate_float_response`, `activate_float`, `ACTIVATION_VALUE_USDT`)
- Modify: `crates/tron-signer/src/sweep.rs` (the re-export)
- Modify: `crates/tron-signer/src/main.rs` (the route)
- Test: `crates/tron-signer/src/sweep/gasfree_rail/tests.rs`

**Interfaces:**
- Consumes: Task 2's reads and `sign_permit`; Task 3's `deadline_after`; `gasfree::fee_to_hold`.
- Produces:
  - `pub enum ActivateFloatOutcome { Submitted { trace_id: String }, AlreadyActive { float_address: String }, FloatDry { float_address: String, have_usdt: i64, need_usdt: i64 }, Refused(String) }` (derives `Debug, PartialEq`)
  - `pub fn activate_float_response(o: &ActivateFloatOutcome) -> serde_json::Value` → statuses `submitted`, `already_active`, `float_dry`, `refused`
  - `pub async fn SweepClient::activate_float(&self, signer: &Signer) -> Result<ActivateFloatOutcome, String>`
  - `POST /internal/activate-float`, no body (Plan 4's typed-confirmation workflow calls it)

- [ ] **Step 1: The outcome and the method, as stubs**

In `crates/tron-signer/src/sweep/gasfree_rail.rs`, after `trace_response`, add:

```rust
/// The float's first transfer moves this much to custody: the smallest possible amount, one
/// micro-USDT. The relay does not document a minimum; rollout step 4 (Plan 4) finds out.
const ACTIVATION_VALUE_USDT: i64 = 1;

/// What one activation attempt did. Read by an operator, from a workflow's run log.
#[derive(Debug, PartialEq)]
pub enum ActivateFloatOutcome {
    /// The float's first permit is with the relay. Once it executes the float is activated.
    Submitted { trace_id: String },
    /// The float already has its contract. Nothing to do; nothing was signed.
    AlreadyActive { float_address: String },
    /// The float cannot pay the activation, one transfer fee and the smallest transfer. Nothing
    /// was signed; the float fills from sweeps (spec §4).
    FloatDry { float_address: String, have_usdt: i64, need_usdt: i64 },
    /// Provably nothing was submitted, or the relay refused it.
    Refused(String),
}

pub fn activate_float_response(o: &ActivateFloatOutcome) -> serde_json::Value {
    todo!("Task 5 Step 4")
}
```

and inside `impl SweepClient`, after `gasfree_trace`:

```rust
    /// Make the GasFree float's first transfer, which is what makes the relay deploy its contract.
    ///
    /// Takes nothing, like `fund_float`: the source is the float, the receiver is custody, the
    /// value is the smallest possible, and `maxFee` covers activation plus one transfer. What it
    /// costs — the relay's fee — must come from surplus, which is why the workflow that calls this
    /// refuses unless the reserve leads supply by at least that much (spec §4).
    pub async fn activate_float(&self, signer: &Signer) -> Result<ActivateFloatOutcome, String> {
        todo!("Task 5 Step 4")
    }
```

In `crates/tron-signer/src/sweep.rs`, change the re-export to:

```rust
pub use gasfree_rail::{
    activate_float_response, load_gasfree_config, trace_response, ActivateFloatOutcome, GasFreeConfig, SelfTest,
};
```

In `crates/tron-signer/src/main.rs`:

a) Add `activate_float_response, ActivateFloatOutcome,` to the `use tron_signer::sweep::{ ... }` list.

b) Directly above `#[tokio::main]`, add:

```rust
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
```

c) Add the route after `.route("/internal/fund-float", post(fund_float))`:

```rust
        .route("/internal/activate-float", post(activate_float))
```

- [ ] **Step 2: The activation tests**

Append to `crates/tron-signer/src/sweep/gasfree_rail/tests.rs`:

```rust
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
```

- [ ] **Step 3: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test(signer): activating the GasFree float, against stubs

The float's one-time activation moves one micro-USDT from the float to
custody with maxFee covering activation and one transfer; an activated
float is left alone and a short one signs nothing. activate_float and
its wire form are todo!() in this commit.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/tron-signer/src/sweep.rs crates/tron-signer/src/sweep/gasfree_rail.rs crates/tron-signer/src/sweep/gasfree_rail/tests.rs crates/tron-signer/src/main.rs
git commit -F .superpowers/sdd/2026-09-24-gasfree-signer/commit-msg.txt
```

Controller: run CI. Expected: the run fails; these fail by name:

```text
test sweep::gasfree_rail::tests::a_float_that_cannot_pay_for_its_activation_is_dry ... FAILED
test sweep::gasfree_rail::tests::activation_sends_the_smallest_amount_from_the_float_to_custody ... FAILED
test sweep::gasfree_rail::tests::activation_without_gasfree_is_refused ... FAILED
test sweep::gasfree_rail::tests::an_activated_float_is_left_alone ... FAILED
test sweep::gasfree_rail::tests::every_activation_status_string_is_pinned ... FAILED
```

and every earlier test stays `ok`.

- [ ] **Step 4: Replace the stubs**

In `crates/tron-signer/src/sweep/gasfree_rail.rs`, replace the body of `activate_float_response`:

```rust
pub fn activate_float_response(o: &ActivateFloatOutcome) -> serde_json::Value {
    match o {
        ActivateFloatOutcome::Submitted { trace_id } => serde_json::json!({"status": "submitted", "trace_id": trace_id}),
        ActivateFloatOutcome::AlreadyActive { float_address } => {
            serde_json::json!({"status": "already_active", "float_address": float_address})
        }
        ActivateFloatOutcome::FloatDry { float_address, have_usdt, need_usdt } => serde_json::json!({
            "status": "float_dry",
            "float_address": float_address,
            "have_usdt": have_usdt,
            "need_usdt": need_usdt,
        }),
        ActivateFloatOutcome::Refused(reason) => serde_json::json!({"status": "refused", "reason": reason}),
    }
}
```

and the body of `activate_float`:

```rust
    pub async fn activate_float(&self, signer: &Signer) -> Result<ActivateFloatOutcome, String> {
        let Some(gf) = &self.gasfree else {
            return Ok(ActivateFloatOutcome::Refused("GasFree is not turned on in this signer".into()));
        };
        let refused = |what: &str, e: String| -> Result<ActivateFloatOutcome, String> {
            Ok(ActivateFloatOutcome::Refused(format!("{what}: {e}")))
        };

        let owner = match signer.payout_address() {
            Ok(a) => a,
            Err(e) => return refused("deriving the float's owner", e),
        };
        let float = match gasfree::gasfree_address(gf.cfg.chain, &owner) {
            Ok(a) => a,
            Err(e) => return refused("deriving the GasFree float", e),
        };
        match self.code_changed(&gf.cfg).await {
            Ok(None) => {}
            Ok(Some(reason)) => return Ok(ActivateFloatOutcome::Refused(reason)),
            Err(e) => return refused("reading GasFree's code", e),
        }
        match self.has_contract(&float).await {
            Ok(false) => {}
            Ok(true) => return Ok(ActivateFloatOutcome::AlreadyActive { float_address: float }),
            Err(e) => return refused("reading whether the GasFree float is activated", e),
        }
        let max_fee = gasfree::fee_to_hold(false, gf.cfg.activate_fee_max_usdt, gf.cfg.transfer_fee_max_usdt);
        let need = ACTIVATION_VALUE_USDT + max_fee;
        let have = match self.usdt_balance(&float).await {
            Ok(v) => v,
            Err(e) => return refused("reading the GasFree float's balance", e),
        };
        if have < need {
            return Ok(ActivateFloatOutcome::FloatDry { float_address: float, have_usdt: have, need_usdt: need });
        }
        let account = match gf.relay.account(&owner, &self.cfg.usdt_contract).await {
            Ok(a) => a,
            Err(e) => return refused("reading the GasFree float's account from the relay", format!("{e:?}")),
        };
        if account.gasfree_address != float {
            return Ok(ActivateFloatOutcome::Refused(format!(
                "the relay puts the GasFree float at {}, this signer derives {float}",
                account.gasfree_address
            )));
        }
        let nonce = match self.chain_nonce(gf.cfg.chain, &owner).await {
            Ok(n) => n,
            Err(e) => return refused("reading the float's nonce", e),
        };
        if !account.allow_submit || account.frozen > 0 || account.nonce != nonce {
            return Ok(ActivateFloatOutcome::Refused("a transfer from the GasFree float is already in flight".into()));
        }
        let key = match signer.payout_signing_key() {
            Ok(k) => k,
            Err(e) => return refused("deriving the payout signing key", e),
        };
        let permit = gasfree::Permit {
            token: &self.cfg.usdt_contract,
            service_provider: &gf.cfg.service_provider,
            user: &owner,
            receiver: &self.cfg.treasury_address,
            value: ACTIVATION_VALUE_USDT as u64,
            max_fee: max_fee as u64,
            deadline: deadline_after(gf.cfg.deadline_secs),
            version: 1,
            nonce,
        };
        let sig = match sign_permit(&key, gf.cfg.chain, &permit) {
            Ok(s) => s,
            Err(e) => return refused("signing the activation permit", e),
        };
        // The receiver is custody and the value one micro-USDT, so even an unclear answer cannot
        // send money anywhere but home. It is still Err, so the operator reads the chain.
        match gf.relay.submit(&permit, &sig).await {
            Ok(trace_id) => Ok(ActivateFloatOutcome::Submitted { trace_id }),
            Err(RelayError::Refused { reason, message }) => {
                Ok(ActivateFloatOutcome::Refused(format!("the relay refused the activation permit: {reason} {message}")))
            }
            Err(RelayError::Unavailable(e)) => Err(format!("submitting the activation permit: {e}")),
        }
    }
```

Do not change the tests.

- [ ] **Step 5: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(signer): activate the GasFree float with its first transfer

POST /internal/activate-float takes nothing. It sends one micro-USDT
from the GasFree float to custody with maxFee covering activation plus
one transfer, and leaves an already activated float alone. Plan 4's
workflow calls it once, and only when the reserve's surplus covers the
fee.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/tron-signer/src/sweep/gasfree_rail.rs
git commit -F .superpowers/sdd/2026-09-24-gasfree-signer/commit-msg.txt
```

Controller: run CI. Expected: success; all 5 new tests and every earlier one `ok` by name — 9 `relay::tests::`, 39 `sweep::gasfree_rail::tests::`, 1 `tests::the_fee_to_hold_includes_activation_only_before_it` — and no warning located in `crates/tron-signer` or `crates/gasfree`.

- [ ] **Step 6 (controller): Open the pull request**

Write the body to `.superpowers/sdd/2026-09-24-gasfree-signer/pr-body.md`, filling in the final run id:

```markdown
Plan 2 of 4 for `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md` (plan: `docs/superpowers/plans/2026-09-24-gasfree-signer.md`): `tron-signer` can now sweep a deposit out of a GasFree account, pay a redemption out of the GasFree float, and activate that float, each with a permit it signs and hands to the pinned relay.

**Merging this changes nothing in production.** GasFree is off unless `APP_GASFREE_API_KEY` is set, and no `.env` sets it yet. Two tests pin that: `without_gasfree_the_sweep_never_looks_at_a_gasfree_address` and `with_trx_payouts_the_gasfree_float_is_never_touched`.

## What it does

- `POST /internal/sweep` still takes only an index. With GasFree on it sweeps the GasFree account of that index first: `maxFee = gasfree::fee_to_hold(activated on chain, the maxima)`, `value = balance − maxFee`, to the GasFree float while it is below its target and redemptions use it, to custody otherwise. New statuses: `pending`, `busy`, `rejected`, `halted`, `below_fee`.
- `POST /internal/payout` with `APP_TRANSFER_RAIL=gasfree` pays from `F = gasfree(2/0)` by permit, for exactly the amount. New statuses: `submitted`, `float_not_active`.
- `POST /internal/activate-float` (no body): the float's one-time first transfer, 1 micro-USDT to custody.
- `GET /internal/addresses/:index` and `GET /internal/gasfree/trace/:trace_id`: read-only, for the treasury (Plan 3).
- Before every permit: the beacon's and the controller's `implementation()` must match the reviewed values, the relay must agree about the GasFree address, and the chain's nonce must match the relay's. At boot, the controller's `getGasFreeAddress` must match this signer's derivation.

## Evidence

CI run <id>: all 49 by name — 9 `relay::tests::`, 39 `sweep::gasfree_rail::tests::`, 1 `gasfree` `tests::the_fee_to_hold_includes_activation_only_before_it` — every other test binary green, no warning from `crates/tron-signer` or `crates/gasfree`. Each task was first seen failing by name against stubs.

## Decisions the plan made where the spec is silent

1. The signer enforces the tripwire itself, and also watches the controller, which is an upgradeable proxy too (new setting `APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION`).
2. The nonce comes from the chain; the relay must agree or the signer waits.
3. The relay's GasFree address must match the signer's derivation; a boot check asks the controller.
4. Two read-only endpoints for the treasury.
5. Sweeps fill the float only when payouts use GasFree.
6. A relay refusal of a payout is a clear "not paid" only for the nine refusals the docs list; anything else after signing is ambiguous.
7. Float activation moves 1 micro-USDT.
8. A missing `allowSubmit` counts as allowed.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
```

Then:

```bash
cd /d/source/clutch/clutch-treasury
gh pr create --repo clutchprotocol/clutch-treasury --base main --head feat/gasfree-signer --title "feat(signer): the GasFree rail: sweep, payout and float activation by permit" --body-file .superpowers/sdd/2026-09-24-gasfree-signer/pr-body.md
```

The maintainer merges it. The stage images rebuild on merge, but with no GasFree settings in any `.env` yet the signer behaves exactly as before.

---

## After this plan

- **Plan 3 — treasury and orchestrator.** Treasury: classify each deposit with `GET /internal/addresses/:index`; mint `observed − fee_to_hold(activated on chain, maxima)` and cap the orchestrator's proposal; `MIN_DEPOSIT_USDT` → `needs_manual`; parse the new sweep statuses (page on `rejected` and `halted`, wait on `busy` and `pending`, the chain decides "swept"); follow a GasFree payout's `trace_id` to its transaction and confirm it on chain; "not available yet" while the float is not activated; its own tripwire read each pass, which pages. Orchestrator: store `G` for new users when `TRANSFER_RAIL=gasfree`; refuse to hand out a GasFree address while GasFree's code has changed; return the fee ("up to") and the minimum with the address; the same boot check against `getGasFreeAddress`.
- **Plan 4 — deploy, deposit panel, rollout.** Compose settings for the three services (`.env` names without the `APP_` prefix, mapped in compose), `check-cap-invariants.sh` (`REDEMPTION_FEE_USDT ≥ GASFREE_TRANSFER_FEE_MAX_USDT`, positive maxima and minimum), `PROBE=gasfree` comparing live fees to the maxima, provisioning `PAYOUT_FLOAT_ADDRESS` from `payout_gasfree_address`, the typed-confirmation float activation workflow, the deposit panel showing "fee up to" and the minimum, a zero sweep threshold on this rail, and the five Nile rollout steps in spec §9.
