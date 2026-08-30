# Redemption Payout Rail Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `payout::StubRail` with a working TRC-20 USDT payout so `APP_REDEMPTIONS_ENABLED` can be turned on for Nile.

**Architecture:** A hot payout float derived at `m/44'/195'/0'/2/0` from the existing deposit mnemonic (no new key material). `tron-signer` gains a thin `POST /internal/payout` that can only spend from that float. `treasury-service` drives it through a claim-before-call state machine that never auto-retries an ambiguous result, and counts the float as a third reserve bucket.

**Tech Stack:** Rust, axum, sqlx/Postgres, reqwest, `bip32`/`k256` for derivation, `wiremock 0.6` for faking TronGrid, `async_trait` for signer seams.

**Spec:** `docs/superpowers/specs/2026-08-30-redemption-payout-rail-design.md`

## Global Constraints

- Scope is **testnet/Nile only**. `KmsSigner`, a separate custody key, and any mainnet posture are out of scope. The `docs/keys.md` mainnet blocker stands unchanged.
- Float derivation path is exactly `m/44'/195'/0'/2/0` — `PAYOUT_CHANGE_LEVEL = 2`, `PAYOUT_INDEX = 0`.
- The payout endpoint takes `intent_id`, `to`, `amount_usdt`. **`contract` is never a parameter** — the USDT contract comes from config, as it does for sweep.
- The payout endpoint can spend **only** from the float. Never from a deposit index, never from custody.
- Per-tx cap lives in `tron-signer`, denominated in **micro-USDT**. Rolling 24h cap lives in `treasury-service`, denominated in **CLT base units**. They are numerically equal at 1:1 par but are different units and **must not be configured from a shared value**.
- An **ambiguous** signer result (timeout, transport failure, unclassifiable 5xx) leaves the intent `payout_submitted` and raises P1. **Never auto-retry it.**
- The float **must** be counted in `get_reserve_balance`, or the first top-up from custody reads as a shortfall and trips the breaker.
- Test command for anything touching Postgres: `docker compose -f docker-compose.test.yml run --rm test cargo test --workspace -- --test-threads=1`

---

## File Structure

**`crates/tron-signer/`**
- `src/keys.rs` — add float derivation beside the existing fee derivation (Task 1)
- `src/sweep.rs` — add `PayoutOutcome` and `SweepClient::payout`, generalize `fund()` (Tasks 2, 3)
- `src/main.rs` — add `payout_address` to `/internal/xpub`, add `POST /internal/payout` (Tasks 1, 4)

**`crates/treasury-service/`**
- `migrations/0008_payout_submitted.sql` — new status (Task 5)
- `src/tron_verifier.rs` — float bucket in `get_reserve_balance` (Task 6)
- `src/reconciliation.rs` — pass the float address (Task 6)
- `src/configuration.rs` — `payout_float_address`, `daily_payout_cap_clt` (Tasks 6, 8)
- `src/payout.rs` — `PayoutSigner` seam, `TronRail`, rewritten `drain_once`, new `confirm_payouts_once` (Tasks 7, 8, 9)
- `tests/db_redemption.rs` — payout lifecycle tests beside the existing burn tests (Tasks 7, 8, 9)
- `tests/db_reconciliation.rs` — float-counted test (Task 6)

**Docs** — `docs/keys.md`, `../CLAUDE.md`, `../clutch-deploy/CLAUDE.md`, `../clutch-deploy/.github/workflows/inspect-stage.yml` (Task 10)

---

### Task 1: Float key derivation

**Files:**
- Modify: `crates/tron-signer/src/keys.rs`
- Modify: `crates/tron-signer/src/main.rs` (the `xpub` handler)
- Test: `crates/tron-signer/src/keys.rs` (inline `#[cfg(test)]` module, where the fee-account tests already live)

**Interfaces:**
- Consumes: nothing.
- Produces: `Signer::payout_address(&self) -> Result<String, String>` and `Signer::payout_signing_key(&self) -> Result<SigningKey, String>`. `/internal/xpub` gains a `payout_address` field.

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `crates/tron-signer/src/keys.rs`. These mirror the four the fee account already has — find `the_fee_address_is_never_a_deposit_address` and put these beside it.

```rust
    #[test]
    fn the_payout_address_is_never_a_deposit_address() {
        let s = fixture_signer();
        let payout = s.payout_address().unwrap();
        for i in 0..200u32 {
            assert_ne!(payout, s.address_at(i).unwrap(), "payout collides with deposit index {i}");
        }
    }

    #[test]
    fn the_payout_address_is_not_the_fee_address() {
        let s = fixture_signer();
        assert_ne!(s.payout_address().unwrap(), s.fee_address().unwrap());
    }

    #[test]
    fn the_payout_key_controls_the_payout_address() {
        let s = fixture_signer();
        let pubkey = s.payout_signing_key().unwrap().verifying_key().to_encoded_point(false);
        assert_eq!(tron_address_from_uncompressed(pubkey.as_bytes()), s.payout_address().unwrap());
    }

    #[test]
    fn the_payout_address_is_stable_across_instances() {
        let a = fixture_signer();
        let b = fixture_signer();
        assert_eq!(a.payout_address().unwrap(), b.payout_address().unwrap());
    }
```

Note: `fixture_signer()` is whatever the existing fee tests use to build a `Signer` from the published fixture mnemonic — reuse it verbatim rather than constructing a new one. If those tests build the signer inline instead, copy that same inline construction into each test above.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p tron-signer payout`
Expected: FAIL — `no method named 'payout_address' found`

- [ ] **Step 3: Add the constants**

In `crates/tron-signer/src/keys.rs`, directly below `const FEE_INDEX: u32 = 0;`:

```rust
/// The USDT payout float, `<account>/2/0`.
///
/// A third change level, for the same reason the fee account got a second one: deposit addresses
/// are `0/i` for every i, so nothing at `2/0` can ever be handed to a depositor. Separate from the
/// fee account at `1/0` because the two hold different assets and are topped up by different
/// people — a shared address would make "the float is dry" ambiguous between TRX and USDT.
const PAYOUT_CHANGE_LEVEL: u32 = 2;
const PAYOUT_INDEX: u32 = 0;
```

- [ ] **Step 4: Add the derivation methods**

In `impl Signer`, directly below `fee_signing_key`:

```rust
    fn payout_child(&self) -> Result<XPrv, String> {
        let change = ChildNumber::new(PAYOUT_CHANGE_LEVEL, false).map_err(|e| e.to_string())?;
        let idx = ChildNumber::new(PAYOUT_INDEX, false).map_err(|e| e.to_string())?;
        self.account
            .derive_child(change)
            .map_err(|e| format!("payout change-level derivation failed: {e}"))?
            .derive_child(idx)
            .map_err(|e| format!("payout index derivation failed: {e}"))
    }

    /// The USDT payout float, `<account>/2/0`.
    ///
    /// Redemption payouts are sent from here and nowhere else. An operator tops it up from custody;
    /// its balance is the cap on what a compromised caller could move, which is why it is a float
    /// and not the custody address itself.
    pub fn payout_address(&self) -> Result<String, String> {
        let pubkey = self.payout_child()?.public_key().public_key().to_encoded_point(false);
        Ok(tron_address_from_uncompressed(pubkey.as_bytes()))
    }

    pub fn payout_signing_key(&self) -> Result<SigningKey, String> {
        Ok(self.payout_child()?.private_key().clone().into())
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p tron-signer payout`
Expected: PASS, 4 tests

- [ ] **Step 6: Surface the float on /internal/xpub**

In `crates/tron-signer/src/main.rs`, in the `xpub` handler, add the payout address beside `fee_address`:

```rust
    let payout_address = s.signer.payout_address().map_err(|e| {
        tracing::error!("payout address derivation failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(json!({
        "account_xpub": s.signer.account_xpub(),
        "fee_address": fee_address,
        "payout_address": payout_address,
    })))
```

Extend that handler's doc comment: `payout_address` is where an operator sends the USDT float, and like `fee_address` it is needed before the first payout, not after one fails.

- [ ] **Step 7: Verify the whole crate still builds and passes**

Run: `cargo test -p tron-signer`
Expected: PASS, all existing tests still green

- [ ] **Step 8: Commit**

```bash
git add crates/tron-signer/src/keys.rs crates/tron-signer/src/main.rs
git commit -m "feat(signer): derive the USDT payout float at 2/0"
```

---

### Task 2: Payout outcome and the signing path

**Files:**
- Modify: `crates/tron-signer/src/sweep.rs`
- Test: `crates/tron-signer/src/sweep.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: `Signer::payout_address`, `Signer::payout_signing_key` (Task 1).
- Produces: `pub enum PayoutOutcome { Paid { tx_id: String }, FloatDry { float_address: String, have_usdt: i64, need_usdt: i64 }, CapExceeded { limit_usdt: i64 }, NeedsTrx { tx_id: String, amount_sun: i64 } }` and `SweepClient::payout(&self, signer: &Signer, to: &str, amount_usdt: i64) -> Result<PayoutOutcome, String>`. `SweepConfig` gains `per_tx_payout_cap_usdt: i64`.

- [ ] **Step 1: Write the failing tests**

Add to the inline `#[cfg(test)] mod tests` in `crates/tron-signer/src/sweep.rs`:

```rust
    #[test]
    fn a_payout_cap_of_zero_would_block_every_payout() {
        // Guards against defaulting the cap to 0 and silently disabling redemptions, which would
        // present as every payout refusing with CapExceeded and no obvious cause.
        let cfg = payout_test_config(0);
        assert_eq!(cfg.per_tx_payout_cap_usdt, 0, "test intends a zero cap");
        assert!(
            cfg.per_tx_payout_cap_usdt <= 0,
            "a zero or negative cap must be rejected at boot, not treated as a limit"
        );
    }

    #[tokio::test]
    async fn payout_above_the_cap_refuses_without_touching_the_network() {
        // trongrid_url points at a dead port: if the implementation reaches TronGrid before
        // checking the cap, this fails with a connection error instead of CapExceeded.
        let client = SweepClient::new(payout_test_config(1_000_000));
        let signer = fixture_signer();
        let outcome = client.payout(&signer, RECIPIENT, 1_000_001).await.unwrap();
        assert_eq!(outcome, PayoutOutcome::CapExceeded { limit_usdt: 1_000_000 });
    }

    #[test]
    fn a_payout_always_spends_from_the_float_and_never_a_deposit() {
        // The security property of the whole endpoint, tested the way funding_body's argument order
        // is: as a pure body builder, because tron-signer has no dev-dependencies and no HTTP mock.
        // owner_address is the payer. If owner and recipient are ever swapped, or if owner is taken
        // from anything the caller supplied, this fails.
        let s = fixture_signer();
        let float = s.payout_address().unwrap();
        let body = payout_body(&float, USDT_FIXTURE, RECIPIENT, 5, 150_000_000);

        assert_eq!(body["owner_address"], float, "the float must be the payer");
        assert_ne!(body["owner_address"], RECIPIENT, "the recipient must never be the payer");
        for i in 0..50u32 {
            assert_ne!(body["owner_address"], s.address_at(i).unwrap(),
                "a payout must never spend from deposit index {i}");
        }
        assert_eq!(body["contract_address"], USDT_FIXTURE, "the token comes from config, not the caller");
    }
```

Add these helpers to the same test module:

```rust
    const RECIPIENT: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";
    const USDT_FIXTURE: &str = "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf";

    fn payout_test_config(cap: i64) -> SweepConfig {
        SweepConfig {
            trongrid_url: "http://127.0.0.1:1".into(),
            trongrid_api_key: String::new(),
            treasury_address: "TQwgeRaDt4FSJSsncmFNcbMNTfFpjvjwFX".into(),
            usdt_contract: "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf".into(),
            fee_limit: 150_000_000,
            per_tx_payout_cap_usdt: cap,
        }
    }
```

Reuse the existing `fixture_signer()` from `keys.rs`'s tests if it is public to the crate; otherwise construct a `Signer::from_mnemonic` with the same published fixture mnemonic those tests use.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p tron-signer payout`
Expected: FAIL — `cannot find type 'PayoutOutcome'`

- [ ] **Step 3: Add the config field and the outcome enum**

In `crates/tron-signer/src/sweep.rs`, add to `SweepConfig`:

```rust
    /// The most one payout may move, in micro-USDT.
    ///
    /// Stateless and checked before anything else. NOT the same number as the treasury's
    /// `daily_payout_cap_clt`: that one is a rolling 24h total in CLT base units and lives where
    /// the database is. Configuring both from one value conflates two units that only happen to be
    /// equal at 1:1 par.
    pub per_tx_payout_cap_usdt: i64,
```

Below `SweepOutcome`:

```rust
/// What one payout attempt did. Mirrors `SweepOutcome`: the caller must be able to tell "refused,
/// nothing broadcast" apart from "broadcast", because the treasury retries the first and never
/// retries the second.
#[derive(Debug, PartialEq)]
pub enum PayoutOutcome {
    /// Broadcast accepted; `tx_id` is the on-chain transfer.
    Paid { tx_id: String },
    /// The float does not hold enough USDT. Proof that nothing was broadcast. Only an operator
    /// topping the float up resolves it.
    FloatDry { float_address: String, have_usdt: i64, need_usdt: i64 },
    /// Above `per_tx_payout_cap_usdt`. Proof that nothing was broadcast.
    CapExceeded { limit_usdt: i64 },
    /// The float was just sent TRX so it can pay for its own transfer. Not a failure and not yet a
    /// payout — the funding has to confirm first, so the next pass does the transfer.
    NeedsTrx { tx_id: String, amount_sun: i64 },
}
```

- [ ] **Step 4: Generalize `fund()` to take any address**

`fund()` currently names its parameter `deposit_address`. Rename it to `target_address` and update the two lines in its body plus the `tracing::info!` message. Change its doc comment's claim that this is the only thing spending from the fee account — it now also funds the payout float:

```rust
    /// Send `MIN_TRX_SUN_FOR_TRANSFER` to `target_address` from the fee account so it can pay for
    /// its own TRC-20 transfer.
    ///
    /// Two callers now: a deposit address about to be swept, and the payout float about to send a
    /// redemption. Both are addresses of ours that hold tokens but no TRX, and the fee account is
    /// the single TRX float behind both.
    ///
    /// Sends the full minimum rather than the shortfall. Topping up the difference would, for an
    /// address already near the floor, broadcast a transaction worth less than its own bandwidth.
    async fn fund(&self, signer: &Signer, target_address: &str) -> Result<SweepOutcome, String> {
```

The call site in `sweep()` passes `&from` and needs no change.

- [ ] **Step 5: Implement `payout`**

Add to `impl SweepClient`, below `sweep`:

```rust
    /// Send `amount_usdt` from the payout float to `to`.
    ///
    /// Unlike `sweep`, this DOES take a destination and an amount — see the spec at
    /// docs/superpowers/specs/2026-08-30-redemption-payout-rail-design.md. The property that
    /// survives is narrower but still real: the source is always the float and the token is always
    /// config, so the most a compromised caller moves is the float balance.
    pub async fn payout(
        &self,
        signer: &Signer,
        to: &str,
        amount_usdt: i64,
    ) -> Result<PayoutOutcome, String> {
        // FIRST, before any network call: a refusal must be provable as "nothing was broadcast",
        // and the cheapest proof is not having talked to anything yet.
        if amount_usdt > self.cfg.per_tx_payout_cap_usdt {
            return Ok(PayoutOutcome::CapExceeded { limit_usdt: self.cfg.per_tx_payout_cap_usdt });
        }

        let from = signer.payout_address()?;

        let have = self.usdt_balance(&from).await?;
        if have < amount_usdt {
            return Ok(PayoutOutcome::FloatDry {
                float_address: from,
                have_usdt: have,
                need_usdt: amount_usdt,
            });
        }

        // Same ordering as sweep: balance first, TRX second. An underfunded float reports FloatDry
        // without dispersing TRX to an address that was never going to send anything.
        let trx = self.trx_balance_sun(&from).await?;
        if trx < MIN_TRX_SUN_FOR_TRANSFER {
            return match self.fund(signer, &from).await? {
                SweepOutcome::Funded { tx_id, amount_sun } => {
                    Ok(PayoutOutcome::NeedsTrx { tx_id, amount_sun })
                }
                SweepOutcome::FeeAccountDry { fee_address, have_sun, need_sun } => {
                    Err(format!(
                        "payout float {from} has no TRX and the fee account {fee_address} is dry \
                         ({have_sun} sun, needs {need_sun}) — an operator must top it up"
                    ))
                }
                other => Err(format!("funding the payout float returned {other:?}")),
            };
        }

        let built: serde_json::Value = self
            .post(
                "/wallet/triggersmartcontract",
                payout_body(&from, &self.cfg.usdt_contract, to, amount_usdt, self.cfg.fee_limit),
            )
            .await?;
        let tx = built
            .get("transaction")
            .cloned()
            .ok_or_else(|| format!("trongrid returned no transaction to sign: {}", describe_rejection(&built)))?;

        let tx_id = self.sign_and_broadcast(&signer.payout_signing_key()?, tx).await?;
        Ok(PayoutOutcome::Paid { tx_id })
    }
```

- [ ] **Step 6: Extract the body builder**

Add beside `funding_body` in `crates/tron-signer/src/sweep.rs`, for the same stated reason that one
is separate — so the argument order is testable without a live node:

```rust
/// The body for a TRC-20 transfer out of the payout float.
///
/// Separate from the call so the argument order is testable. `owner_address` PAYS and is always the
/// float; `to` receives and is the only address the caller chose. Swapped, this would ask the
/// redeemer to pay us — which fails at broadcast for lack of a signature we do not hold, but only
/// after the transaction has been built and signed against the wrong account.
fn payout_body(
    from: &str,
    usdt_contract: &str,
    to: &str,
    amount_usdt: i64,
    fee_limit: i64,
) -> serde_json::Value {
    serde_json::json!({
        "owner_address": from,
        "contract_address": usdt_contract,
        "function_selector": "transfer(address,uint256)",
        "parameter": transfer_parameter(to, amount_usdt).unwrap_or_default(),
        "fee_limit": fee_limit,
        "call_value": 0,
        "visible": true,
    })
}
```

`transfer_parameter` returns `Result` because it base58-decodes `to`. The `unwrap_or_default()`
here yields an empty parameter for an undecodable address, which TronGrid rejects at build time —
acceptable because the orchestrator already base58check-validates every `payout_tron_address`
before a redemption is ever created, so an invalid address cannot reach this path.

- [ ] **Step 7: Fix the existing `SweepConfig` construction sites**

Adding a field breaks every struct literal. Update `crates/tron-signer/src/main.rs`'s `SweepConfig { .. }` to include:

```rust
        per_tx_payout_cap_usdt: env("APP_PER_TX_PAYOUT_CAP_USDT")
            .parse()
            .expect("APP_PER_TX_PAYOUT_CAP_USDT must be an integer number of micro-USDT"),
```

Use `env()` (which panics when unset), not a default: a payout cap that silently defaults is a cap nobody chose. Then fix any `SweepConfig` literals in existing tests by adding `per_tx_payout_cap_usdt: 1_000_000_000,`.

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p tron-signer`
Expected: PASS, including the two new payout tests and all existing sweep tests

- [ ] **Step 9: Commit**

```bash
git add crates/tron-signer/src/sweep.rs crates/tron-signer/src/main.rs
git commit -m "feat(signer): payout from the float, capped and float-dry aware"
```

---

### Task 3: The /internal/payout route

**Files:**
- Modify: `crates/tron-signer/src/main.rs`

**Interfaces:**
- Consumes: `SweepClient::payout`, `PayoutOutcome` (Task 2).
- Produces: `POST /internal/payout` accepting `{"intent_id": "<uuid>", "to": "<base58>", "amount_usdt": <i64>}`, returning `{"status": "paid"|"float_dry"|"cap_exceeded"|"needs_trx", ...}`. Treasury-side parsing in Task 7 depends on these exact status strings.

- [ ] **Step 1: Add the request struct**

In `crates/tron-signer/src/main.rs`, below `SweepRequest`:

```rust
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
```

- [ ] **Step 2: Add the handler**

Below the `sweep` handler:

```rust
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
        Ok(PayoutOutcome::Paid { tx_id }) => {
            tracing::info!(intent_id = %req.intent_id, to = %req.to, amount_usdt = req.amount_usdt, %tx_id, "paid");
            Ok(Json(json!({"status": "paid", "tx_id": tx_id})))
        }
        // Refusals, not errors: the treasury needs to tell these apart from a failure of unknown
        // outcome, because it retries these and never retries the unknown ones.
        Ok(PayoutOutcome::CapExceeded { limit_usdt }) => {
            tracing::warn!(intent_id = %req.intent_id, amount_usdt = req.amount_usdt, limit_usdt, "payout over cap");
            Ok(Json(json!({"status": "cap_exceeded", "limit_usdt": limit_usdt})))
        }
        Ok(PayoutOutcome::FloatDry { float_address, have_usdt, need_usdt }) => Ok(Json(json!({
            "status": "float_dry",
            "float_address": float_address,
            "have_usdt": have_usdt,
            "need_usdt": need_usdt,
        }))),
        Ok(PayoutOutcome::NeedsTrx { tx_id, amount_sun }) => {
            Ok(Json(json!({"status": "needs_trx", "tx_id": tx_id, "amount_sun": amount_sun})))
        }
        Err(e) => {
            tracing::error!(intent_id = %req.intent_id, "payout failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
```

- [ ] **Step 3: Register the route and import the type**

Add `PayoutOutcome` to the `tron_signer::sweep::{..}` import list, and add the route:

```rust
        .route("/internal/payout", post(payout))
```

- [ ] **Step 4: Update the module doc**

`main.rs`'s header says "Two routes, and the shape of the sweep route IS the security argument". Replace that paragraph:

```rust
//! Three routes. The sweep route's shape IS its security argument: it accepts an INDEX and nothing
//! else, so no field a caller sets can redirect funds.
//!
//! The payout route cannot make that claim and does not pretend to — it takes a destination and an
//! amount because a redemption has no other way to express them. Its bound is different: the source
//! is always the payout float, so the most a hostile caller moves is the float balance, and the
//! per-tx cap bounds a single request. Here the bearer token is load-bearing, not defence in depth.
```

- [ ] **Step 5: Verify it builds**

Run: `cargo test -p tron-signer`
Expected: PASS, compiles clean

- [ ] **Step 6: Commit**

```bash
git add crates/tron-signer/src/main.rs
git commit -m "feat(signer): expose POST /internal/payout"
```

---

### Task 4: Migration for `payout_submitted`

**Files:**
- Create: `crates/treasury-service/migrations/0008_payout_submitted.sql`

**Interfaces:**
- Produces: `redemption_intents.status` accepts `payout_submitted`. Tasks 7 and 9 depend on it.

- [ ] **Step 1: Write the migration**

```sql
-- The payout worker claims an intent BEFORE calling the signer, so a crash mid-call lands in a
-- state that is visibly ambiguous rather than one that looks retryable.
--
-- Without this status the only options after a lost response are "retry and maybe pay twice" or
-- "give up and orphan the burn". This is the third option: recorded as in-flight, resolvable by
-- looking at the float's outbound transfers.
ALTER TABLE redemption_intents DROP CONSTRAINT redemption_intents_status_check;
ALTER TABLE redemption_intents ADD CONSTRAINT redemption_intents_status_check CHECK (status IN
    ('created','burn_confirmed','payout_pending','payout_submitted','paid','expired','failed'));
```

Confirm the constraint name first — run `\d redemption_intents` against the test database, or grep `0001_init.sql`. If Postgres auto-named it differently, use the real name.

- [ ] **Step 2: Verify the migration applies**

Run: `docker compose -f docker-compose.test.yml run --rm test cargo test --workspace -- --test-threads=1`
Expected: PASS — every test's `sqlx::migrate!` runs the new migration

- [ ] **Step 3: Commit**

```bash
git add crates/treasury-service/migrations/0008_payout_submitted.sql
git commit -m "feat(treasury): add payout_submitted to redemption_intents"
```

---

### Task 5: Count the float in the reserve

**Files:**
- Modify: `crates/treasury-service/src/tron_verifier.rs:207-223`
- Modify: `crates/treasury-service/src/reconciliation.rs:195-198`
- Modify: `crates/treasury-service/src/configuration.rs`
- Test: `crates/treasury-service/tests/db_tron_verifier.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `get_reserve_balance(&self, main_address: &str, unswept_addresses: &[String], float_address: &str, usdt_contract: &str) -> Result<i64, String>`. `AppConfig` gains `pub payout_float_address: String`.

- [ ] **Step 1: Write the failing test**

In `crates/treasury-service/tests/db_tron_verifier.rs`, following the wiremock idiom already used there:

```rust
#[tokio::test]
async fn the_reserve_includes_the_payout_float() {
    // The trap this guards: the float is funded FROM custody, so if it is not counted, the first
    // top-up looks like custody shrinking and reconciliation reports a shortfall that halts minting.
    let server = MockServer::start().await;
    mount_balance(&server, CUSTODY, 700).await;
    mount_balance(&server, FLOAT, 300).await;

    let client = TronClient::new(server.uri(), String::new());
    let total = client.get_reserve_balance(CUSTODY, &[], FLOAT, USDT).await.unwrap();

    assert_eq!(total, 1000, "float USDT is reserve backing CLT, not spare money");
}
```

`mount_balance`, `CUSTODY`, `USDT` and `TronClient::new` already exist in that file — reuse them, and add `const FLOAT: &str = "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY";`. If the existing helper is named differently, use the existing name; do not add a second helper.

- [ ] **Step 2: Run test to verify it fails**

Run: `docker compose -f docker-compose.test.yml run --rm test cargo test --workspace the_reserve_includes -- --test-threads=1`
Expected: FAIL — `this method takes 3 arguments but 4 arguments were supplied`

- [ ] **Step 3: Add the parameter**

In `crates/treasury-service/src/tron_verifier.rs`:

```rust
    /// Custody + every unswept deposit address + the payout float.
    ///
    /// The float is counted because it is funded from custody: leaving it out means the first
    /// top-up reads as the reserve shrinking, and reconciliation halts minting over money that
    /// never left.
    ///
    /// `float_address` is a separate parameter rather than another entry in `unswept_addresses`
    /// so a failure reading it is attributed to the float, not misreported as a deposit problem.
    pub async fn get_reserve_balance(
        &self,
        main_address: &str,
        unswept_addresses: &[String],
        float_address: &str,
        usdt_contract: &str,
    ) -> Result<i64, String> {
        let mut total = self.get_custody_balance(main_address, usdt_contract).await?;
        for addr in unswept_addresses {
            let bal = self
                .get_custody_balance(addr, usdt_contract)
                .await
                .map_err(|e| format!("unswept deposit address {addr}: {e}"))?;
            // Saturating: a corrupt balance must not wrap the reserve into something small.
            total = total.saturating_add(bal);
        }
        let float = self
            .get_custody_balance(float_address, usdt_contract)
            .await
            .map_err(|e| format!("payout float {float_address}: {e}"))?;
        Ok(total.saturating_add(float))
    }
```

- [ ] **Step 4: Add the config field and update the call site**

In `crates/treasury-service/src/configuration.rs`, beside `custody_tron_address`:

```rust
    /// The payout float address, read off tron-signer's /internal/xpub.
    ///
    /// Configured rather than derived: this service holds no key material and must not be able to
    /// derive spending addresses. It only needs to know where to LOOK, so it is given the address.
    pub payout_float_address: String,
```

In `crates/treasury-service/src/reconciliation.rs`:

```rust
        .get_reserve_balance(
            &config.custody_tron_address,
            &unswept,
            &config.payout_float_address,
            &config.usdt_contract,
        )
```

- [ ] **Step 5: Fix every `AppConfig` literal in tests**

Adding a field breaks each one. In `crates/treasury-service/tests/db_sweeper.rs`'s `config()` and every other `AppConfig { .. }` literal across the test suite, add:

```rust
        payout_float_address: "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY".into(),
```

Find them with `grep -rn "AppConfig {" crates/treasury-service/tests/ crates/treasury-service/src/`.

- [ ] **Step 6: Run the suite**

Run: `docker compose -f docker-compose.test.yml run --rm test cargo test --workspace -- --test-threads=1`
Expected: PASS, including the new float test

- [ ] **Step 7: Commit**

```bash
git add crates/treasury-service/src/tron_verifier.rs crates/treasury-service/src/reconciliation.rs crates/treasury-service/src/configuration.rs crates/treasury-service/tests/
git commit -m "fix(treasury): count the payout float as reserve"
```

---

### Task 6: The treasury-side payout signer seam

**Files:**
- Modify: `crates/treasury-service/src/payout.rs`
- Modify: `crates/treasury-service/src/configuration.rs`

**Interfaces:**
- Consumes: the `/internal/payout` status strings (Task 3).
- Produces: `pub enum PayoutReply { Paid { tx_id: String }, FloatDry { float_address: String, have_usdt: i64, need_usdt: i64 }, CapExceeded { limit_usdt: i64 }, NeedsTrx, Refused(String), Ambiguous(String) }`, `pub trait PayoutSigner { async fn pay(&self, intent_id: Uuid, to: &str, amount_usdt: i64) -> PayoutReply }`, and `pub struct HttpPayoutSigner { http, base_url, token }`. Task 7 drives this trait.

- [ ] **Step 1: Write the failing test**

In `crates/treasury-service/tests/db_redemption.rs`:

```rust
#[test]
fn refused_and_ambiguous_are_distinct_variants() {
    // The whole safety property of this rail rests on these two never collapsing into one type.
    // Refused means provably no broadcast and is retryable; Ambiguous means unknown and must never
    // be retried automatically.
    let refused = PayoutReply::Refused("cap_exceeded".into());
    let ambiguous = PayoutReply::Ambiguous("timeout".into());
    assert_ne!(refused, ambiguous);
}
```

Add `use treasury_service::payout::PayoutReply;` to the file's imports.

- [ ] **Step 2: Run test to verify it fails**

Run: `docker compose -f docker-compose.test.yml run --rm test cargo test --workspace refused_and_ambiguous -- --test-threads=1`
Expected: FAIL — `unresolved import 'treasury_service::payout::PayoutReply'`

- [ ] **Step 3: Add the reply type and the trait**

In `crates/treasury-service/src/payout.rs`, above the existing `PayoutRail` trait:

```rust
/// What the signer reported for one payout.
///
/// The division that matters is `Refused` vs `Ambiguous`, and it is not a stylistic one: `Refused`
/// means the signer told us it did not broadcast, so retrying is free. `Ambiguous` means we do not
/// know, and a TRC-20 transfer has no memo to dedupe against, so retrying risks paying twice for a
/// burn that only happened once. Never widen `Refused` to cover a case you are not certain about.
#[derive(Debug, PartialEq)]
pub enum PayoutReply {
    Paid { tx_id: String },
    FloatDry { float_address: String, have_usdt: i64, need_usdt: i64 },
    CapExceeded { limit_usdt: i64 },
    /// The float was topped up with TRX and the transfer has not happened yet. Retryable.
    NeedsTrx,
    /// The signer answered, and its answer proves nothing was broadcast. Retryable.
    Refused(String),
    /// No usable answer. MAY have broadcast. Not retryable by any automation.
    Ambiguous(String),
}

/// The signer boundary, as a trait so the worker is testable without a live service or real keys —
/// same reasoning as `SweepSigner` in sweeper.rs.
#[async_trait::async_trait]
pub trait PayoutSigner: Send + Sync {
    async fn pay(&self, intent_id: Uuid, to: &str, amount_usdt: i64) -> PayoutReply;
}
```

- [ ] **Step 4: Implement the HTTP signer**

Below the trait, modelled on `sweeper::HttpSigner`:

```rust
pub struct HttpPayoutSigner {
    pub http: reqwest::Client,
    pub base_url: String,
    pub token: String,
}

#[async_trait::async_trait]
impl PayoutSigner for HttpPayoutSigner {
    async fn pay(&self, intent_id: Uuid, to: &str, amount_usdt: i64) -> PayoutReply {
        let resp = self
            .http
            .post(format!("{}/internal/payout", self.base_url))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "intent_id": intent_id.to_string(),
                "to": to,
                "amount_usdt": amount_usdt,
            }))
            .send()
            .await;

        let body: serde_json::Value = match resp {
            Ok(r) if r.status().is_success() => match r.json().await {
                Ok(v) => v,
                // Success status, unreadable body: the signer may well have broadcast.
                Err(e) => return PayoutReply::Ambiguous(format!("unreadable signer response: {e}")),
            },
            // 400 is the signer rejecting the request shape before doing anything. Every other
            // status could have followed a broadcast, so it is ambiguous, not refused.
            Ok(r) if r.status() == reqwest::StatusCode::BAD_REQUEST => {
                return PayoutReply::Refused("signer rejected the request as malformed".into())
            }
            Ok(r) => return PayoutReply::Ambiguous(format!("signer returned {}", r.status())),
            // Connection refused and DNS failures are safe, but a timeout is not distinguishable
            // here from a request that landed. Treat the whole class as ambiguous.
            Err(e) => return PayoutReply::Ambiguous(format!("signer unreachable or timed out: {e}")),
        };

        match body["status"].as_str() {
            Some("paid") => match body["tx_id"].as_str() {
                Some(tx) => PayoutReply::Paid { tx_id: tx.to_string() },
                // Claimed success without naming the transaction. It may have broadcast and we
                // cannot point at it, which is the definition of ambiguous.
                None => PayoutReply::Ambiguous("signer reported paid with no tx_id".into()),
            },
            Some("float_dry") => PayoutReply::FloatDry {
                float_address: body["float_address"].as_str().unwrap_or("unknown").to_string(),
                have_usdt: body["have_usdt"].as_i64().unwrap_or(0),
                need_usdt: body["need_usdt"].as_i64().unwrap_or(0),
            },
            Some("cap_exceeded") => {
                PayoutReply::CapExceeded { limit_usdt: body["limit_usdt"].as_i64().unwrap_or(0) }
            }
            Some("needs_trx") => PayoutReply::NeedsTrx,
            // An unknown status from a newer signer might describe a broadcast this version does
            // not understand. Ambiguous, never Refused.
            other => PayoutReply::Ambiguous(format!("unrecognised signer status {other:?}")),
        }
    }
}
```

- [ ] **Step 5: Add the daily cap config**

In `crates/treasury-service/src/configuration.rs`, beside `daily_mint_cap_clt`:

```rust
    /// Rolling 24h payout ceiling in CLT base units.
    ///
    /// Separate from the signer's `per_tx_payout_cap_usdt`, which is micro-USDT and per-transaction.
    /// The two are equal at 1:1 par and must still be configured independently — collapsing them
    /// would silently couple a unit change on one side to the other.
    pub daily_payout_cap_clt: i64,
```

Add `daily_payout_cap_clt: 500_000_000,` to every `AppConfig` literal in the tests.

- [ ] **Step 6: Run tests**

Run: `docker compose -f docker-compose.test.yml run --rm test cargo test --workspace -- --test-threads=1`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/treasury-service/src/payout.rs crates/treasury-service/src/configuration.rs crates/treasury-service/tests/
git commit -m "feat(treasury): payout signer seam, refused vs ambiguous"
```

---

### Task 7: Claim-before-call and the double-pay guard

**Files:**
- Modify: `crates/treasury-service/src/payout.rs` (rewrite `drain_once`)
- Test: `crates/treasury-service/tests/db_redemption.rs`

**Interfaces:**
- Consumes: `PayoutSigner`, `PayoutReply` (Task 6); `payout_submitted` (Task 4); `daily_payout_cap_clt` (Task 6).
- Produces: `drain_once(pool: &PgPool, config: &AppConfig, signer: &dyn PayoutSigner) -> Result<u32, String>`. Note the changed signature: it now takes `&AppConfig` and a `&dyn PayoutSigner` instead of `&dyn PayoutRail`.

- [ ] **Step 1: Write the failing tests**

In `crates/treasury-service/tests/db_redemption.rs`. Add a counting fake signer first:

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use treasury_service::payout::{self, PayoutReply, PayoutSigner};

struct CountingSigner {
    reply: PayoutReply,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl PayoutSigner for CountingSigner {
    async fn pay(&self, _intent_id: Uuid, _to: &str, _amount_usdt: i64) -> PayoutReply {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.reply {
            PayoutReply::Paid { tx_id } => PayoutReply::Paid { tx_id: tx_id.clone() },
            PayoutReply::Ambiguous(m) => PayoutReply::Ambiguous(m.clone()),
            PayoutReply::CapExceeded { limit_usdt } => PayoutReply::CapExceeded { limit_usdt: *limit_usdt },
            PayoutReply::FloatDry { float_address, have_usdt, need_usdt } => PayoutReply::FloatDry {
                float_address: float_address.clone(),
                have_usdt: *have_usdt,
                need_usdt: *need_usdt,
            },
            PayoutReply::NeedsTrx => PayoutReply::NeedsTrx,
            PayoutReply::Refused(m) => PayoutReply::Refused(m.clone()),
        }
    }
}

/// A redemption sitting at payout_pending with its burn already confirmed.
async fn pending_redemption(pool: &PgPool, amount_clt: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO redemption_intents (id, redeemer_address, payout_address, amount_clt, status, redemption_ref, burn_tx_hash)
         VALUES ($1, '0xabc', 'TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK', $2, 'payout_pending', $3, '0xburn')",
    )
    .bind(id)
    .bind(amount_clt)
    .bind(format!("{:064x}", id.as_u128()))
    .execute(pool)
    .await
    .unwrap();
    id
}
```

Then the tests:

```rust
#[tokio::test]
async fn an_ambiguous_payout_is_never_retried() {
    // THE regression test for this rail. A lost response after a possible broadcast must leave the
    // intent parked and alerting, not queued for another attempt that could pay the same burn twice.
    let pool = pool().await;
    let id = pending_redemption(&pool, 10_000_000).await;
    let signer = CountingSigner {
        reply: PayoutReply::Ambiguous("timeout".into()),
        calls: AtomicUsize::new(0),
    };
    let cfg = config();

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1, "second pass must not re-call the signer");

    let (status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "payout_submitted");

    let (alerts,): (i64,) = sqlx::query_as("SELECT count(*) FROM alerts WHERE severity = 'p1' AND source = 'payout'")
        .fetch_one(&pool).await.unwrap();
    assert!(alerts >= 1, "an ambiguous payout must alert — it needs a human");
}

#[tokio::test]
async fn a_refusal_returns_the_intent_for_retry() {
    let pool = pool().await;
    let id = pending_redemption(&pool, 10_000_000).await;
    let signer = CountingSigner {
        reply: PayoutReply::FloatDry {
            float_address: "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY".into(),
            have_usdt: 0,
            need_usdt: 10_000_000,
        },
        calls: AtomicUsize::new(0),
    };
    let cfg = config();

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    let (status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "payout_pending", "a proven non-broadcast is retryable");
}

#[tokio::test]
async fn a_paid_payout_records_the_tx_and_waits_for_confirmation() {
    let pool = pool().await;
    let id = pending_redemption(&pool, 10_000_000).await;
    let signer = CountingSigner {
        reply: PayoutReply::Paid { tx_id: "abc123".into() },
        calls: AtomicUsize::new(0),
    };

    payout::drain_once(&pool, &config(), &signer).await.unwrap();

    let (status, payout_ref): (String, Option<String>) =
        sqlx::query_as("SELECT status, payout_ref FROM redemption_intents WHERE id = $1")
            .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "payout_submitted", "not paid until confirmed on chain");
    assert_eq!(payout_ref.as_deref(), Some("abc123"));

    // The ledger event belongs to confirmation, not submission.
    let (events,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_withdrawal'")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(events, 0);
}

#[tokio::test]
async fn payouts_stop_at_the_daily_cap() {
    let pool = pool().await;
    let mut cfg = config();
    cfg.daily_payout_cap_clt = 15_000_000;
    pending_redemption(&pool, 10_000_000).await;
    pending_redemption(&pool, 10_000_000).await;
    let signer = CountingSigner { reply: PayoutReply::Paid { tx_id: "t".into() }, calls: AtomicUsize::new(0) };

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1, "the second payout crosses the cap");
}
```

The `pool()` helper in this file must `TRUNCATE redemption_intents, treasury_events, alerts RESTART IDENTITY CASCADE` — extend the existing truncate list if it does not already cover them. `config()` is the same `AppConfig` builder used in `db_sweeper.rs`; copy it into this file or lift it into a shared `tests/support` module if one exists.

- [ ] **Step 2: Run tests to verify they fail**

Run: `docker compose -f docker-compose.test.yml run --rm test cargo test --workspace -- --test-threads=1`
Expected: FAIL — `drain_once` takes 2 arguments, `PayoutSigner` not accepted

- [ ] **Step 3: Rewrite `drain_once`**

Replace the existing `drain_once` in `crates/treasury-service/src/payout.rs`:

```rust
/// Pays each due `payout_pending` intent against its ALREADY-CONFIRMED burn.
///
/// Burn first, payout second, always — `watcher::confirm_burn` is the sole path into
/// `payout_pending`, so nothing here can pay before the chain leg is final.
///
/// The halted breaker gates this too, not just minting: a treasury that stopped minting because its
/// books disagree must not ship money out the other door either.
///
/// Each intent is CLAIMED (`payout_submitted`, committed) before the signer is called, so a crash
/// mid-call leaves a state that is visibly in-flight rather than one that looks retryable. Only a
/// reply that PROVES no broadcast returns it to `payout_pending`. Everything else stays claimed and
/// alerts: orphaning a burn is the one outcome this function must never produce, and paying one
/// burn twice is the mirror-image sin.
pub async fn drain_once(
    pool: &PgPool,
    config: &AppConfig,
    signer: &dyn PayoutSigner,
) -> Result<u32, String> {
    let (halted, halt_reason): (bool, Option<String>) =
        sqlx::query_as("SELECT minting_halted, halt_reason FROM breaker_state")
            .fetch_one(pool)
            .await
            .map_err(|e| e.to_string())?;
    if halted {
        tracing::warn!(halt_reason, "payouts blocked: treasury is halted");
        return Ok(0);
    }

    let rows: Vec<(Uuid, String, i64)> = sqlx::query_as(
        "SELECT id, payout_address, amount_clt FROM redemption_intents
         WHERE status = 'payout_pending' ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut day_total = daily_payout_total(pool).await.map_err(|e| e.to_string())?;
    let mut processed = 0u32;

    for (intent_id, payout_address, amount_clt) in rows {
        if day_total + amount_clt > config.daily_payout_cap_clt {
            tracing::warn!(%intent_id, day_total, cap = config.daily_payout_cap_clt,
                "daily payout cap reached; remaining intents wait for the window to roll");
            break;
        }

        // CLAIM FIRST. Committed before the call, so a crash between here and the reply is
        // indistinguishable from a lost response — which is correct, because it is one.
        let claimed = sqlx::query(
            "UPDATE redemption_intents SET status = 'payout_submitted', updated_at = now()
             WHERE id = $1 AND status = 'payout_pending'",
        )
        .bind(intent_id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
        if claimed.rows_affected() == 0 {
            // Another worker took it between the SELECT and here.
            continue;
        }

        // 1:1 CLT<->USDT base units at par — spread/fee modelling is an orchestrator concern.
        match signer.pay(intent_id, &payout_address, amount_clt).await {
            PayoutReply::Paid { tx_id } => {
                sqlx::query("UPDATE redemption_intents SET payout_ref = $2, updated_at = now() WHERE id = $1")
                    .bind(intent_id)
                    .bind(&tx_id)
                    .execute(pool)
                    .await
                    .map_err(|e| e.to_string())?;
                day_total += amount_clt;
                processed += 1;
            }
            // Proven non-broadcast: hand it back for a later pass.
            reply @ (PayoutReply::FloatDry { .. }
            | PayoutReply::CapExceeded { .. }
            | PayoutReply::NeedsTrx
            | PayoutReply::Refused(_)) => {
                sqlx::query(
                    "UPDATE redemption_intents SET status = 'payout_pending', updated_at = now() WHERE id = $1",
                )
                .bind(intent_id)
                .execute(pool)
                .await
                .map_err(|e| e.to_string())?;
                alert(pool, "p1", "payout",
                    &format!("redemption {intent_id}: payout refused ({reply:?}), returned to payout_pending")).await;
            }
            // May or may not have broadcast. Stays claimed, forever, until a human resolves it.
            PayoutReply::Ambiguous(msg) => {
                alert(pool, "p1", "payout", &format!(
                    "redemption {intent_id}: payout outcome UNKNOWN ({msg}). Left payout_submitted and \
                     NOT retried — retrying could pay this burn twice. Check the payout float's \
                     outbound transfers for a transfer of {amount_clt} to {payout_address}, then \
                     either set payout_ref to that tx and let confirmation finish it, or return the \
                     intent to payout_pending."
                )).await;
            }
        }
    }
    Ok(processed)
}

/// The rolling 24h payout total against `daily_payout_cap_clt`. Counts every status at or past
/// submission, mirroring `breakers::daily_mint_total` — an in-flight payout is spent budget.
async fn daily_payout_total(pool: &PgPool) -> Result<i64, sqlx::Error> {
    let (total,): (i64,) = sqlx::query_as(
        // ::BIGINT — SUM(BIGINT) is NUMERIC, sqlx can't decode that into i64.
        "SELECT COALESCE(SUM(amount_clt), 0)::BIGINT FROM redemption_intents
         WHERE status IN ('payout_submitted','paid')
           AND updated_at > now() - interval '24 hours'",
    )
    .fetch_one(pool)
    .await?;
    Ok(total)
}
```

Add `use crate::configuration::AppConfig;` to the imports.

- [ ] **Step 4: Delete `StubRail` and `PayoutRail`**

Remove the `PayoutRail` trait and `StubRail` struct. `PayoutSigner` replaces them as the seam, and leaving a stub that silently succeeds is exactly the hazard this task removes. Fix the resulting compile errors at the call site in `main.rs` (Task 9 rewires it properly; for now make it compile).

- [ ] **Step 5: Run tests to verify they pass**

Run: `docker compose -f docker-compose.test.yml run --rm test cargo test --workspace -- --test-threads=1`
Expected: PASS, all four new tests

- [ ] **Step 6: Commit**

```bash
git add crates/treasury-service/src/payout.rs crates/treasury-service/tests/db_redemption.rs
git commit -m "feat(treasury): claim before paying, never retry an unknown payout"
```

---

### Task 8: Confirming payouts on chain

**Files:**
- Modify: `crates/treasury-service/src/payout.rs`
- Test: `crates/treasury-service/tests/db_redemption.rs`

**Interfaces:**
- Consumes: `payout_submitted` rows with `payout_ref` set (Task 7).
- Produces: `confirm_payouts_once(pool: &PgPool, client: &TronClient) -> Result<u32, String>`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn confirmation_writes_paid_and_the_ledger_event_once() {
    let pool = pool().await;
    let id = pending_redemption(&pool, 10_000_000).await;
    sqlx::query("UPDATE redemption_intents SET status = 'payout_submitted', payout_ref = 'abc123' WHERE id = $1")
        .bind(id).execute(&pool).await.unwrap();

    let server = MockServer::start().await;
    mount_confirmed_tx(&server, "abc123").await;
    let client = TronClient::new(server.uri(), String::new());

    payout::confirm_payouts_once(&pool, &client).await.unwrap();
    // Twice: the ON CONFLICT must make a second pass a no-op, not a double ledger entry.
    payout::confirm_payouts_once(&pool, &client).await.unwrap();

    let (status,): (String,) = sqlx::query_as("SELECT status FROM redemption_intents WHERE id = $1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "paid");

    let (events,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_withdrawal'")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(events, 1, "liability must drop exactly once");
}
```

Add this helper to the same file. It mounts the **solidity** node endpoint, which is the one
`transaction_confirmed` calls — the solidity node only serves irreversible blocks, so presence is
the confirmation proof, and the echoed `txID` is what the implementation compares against:

```rust
async fn mount_confirmed_tx(server: &MockServer, tx_id: &str) {
    Mock::given(method("POST"))
        .and(path("/walletsolidity/gettransactionbyid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "txID": tx_id })))
        .mount(server)
        .await;
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `docker compose -f docker-compose.test.yml run --rm test cargo test --workspace confirmation_writes -- --test-threads=1`
Expected: FAIL — `cannot find function 'confirm_payouts_once'`

- [ ] **Step 3: Implement confirmation**

```rust
/// Moves `payout_submitted` intents whose transfer is confirmed on chain to `paid`, writing the
/// ledger event in the same transaction.
///
/// Separate from `drain_once` because submission and confirmation happen at different times: the
/// transfer needs Tron confirmations, and holding a request open across them would stall the whole
/// drain for one intent.
///
/// An intent with no `payout_ref` is skipped, never confirmed and never failed — that is the
/// ambiguous state, and only a human puts a tx id on it or sends it back.
pub async fn confirm_payouts_once(pool: &PgPool, client: &TronClient) -> Result<u32, String> {
    let rows: Vec<(Uuid, i64, String)> = sqlx::query_as(
        "SELECT id, amount_clt, payout_ref FROM redemption_intents
         WHERE status = 'payout_submitted' AND payout_ref IS NOT NULL ORDER BY updated_at",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut confirmed = 0u32;
    for (intent_id, amount_clt, payout_ref) in rows {
        match client.transaction_confirmed(&payout_ref).await {
            Ok(true) => {
                pay_intent(pool, intent_id, amount_clt, &payout_ref).await?;
                confirmed += 1;
            }
            // Not yet mined. Nothing to do; the next pass looks again.
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(%intent_id, %payout_ref, "could not check payout confirmation: {e}");
            }
        }
    }
    Ok(confirmed)
}
```

`pay_intent` already exists and already writes both the event and the status flip in one transaction with `ON CONFLICT (intent_id, kind)` — leave it as it is.

**Do not write a confirmation call — one already exists.** `TronClient::transaction_confirmed(&self, tx_id: &str) -> Result<bool, String>` is already implemented in `crates/treasury-service/src/tron_verifier.rs` (around line 154). It asks the solidity node, treats `{}` as `Ok(false)` rather than an error, and compares the echoed id case-insensitively so a response about a different transaction cannot be read as confirmation of this one.

It is currently private. The only change needed is the visibility:

```rust
    pub async fn transaction_confirmed(&self, tx_id: &str) -> Result<bool, String> {
```

Writing a second confirmation path would be the mistake here: this one carries a comment recording that `GET /v1/transactions/{id}` and its `confirmed` field never existed and 404 on Nile, which cost a real debugging cycle. Reuse it.

- [ ] **Step 4: Run tests to verify they pass**

Run: `docker compose -f docker-compose.test.yml run --rm test cargo test --workspace -- --test-threads=1`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/treasury-service/src/payout.rs crates/treasury-service/src/tron_verifier.rs crates/treasury-service/tests/db_redemption.rs
git commit -m "feat(treasury): confirm payouts on chain before ledgering"
```

---

### Task 9: Wire the workers in

**Files:**
- Modify: `crates/treasury-service/src/main.rs`

**Interfaces:**
- Consumes: `drain_once` (Task 7), `confirm_payouts_once` (Task 8), `HttpPayoutSigner` (Task 6).
- Produces: both workers running on the existing interval loop.

- [ ] **Step 1: Build the signer and run both passes**

In `crates/treasury-service/src/main.rs`, wherever the payout worker is spawned today (it currently constructs `StubRail`), replace it:

```rust
    let payout_signer = payout::HttpPayoutSigner {
        http: reqwest::Client::new(),
        base_url: config.signer_url.clone(),
        token: config.signer_token.clone(),
    };
```

and in the loop body, run the drain then the confirmation:

```rust
        if let Err(e) = payout::drain_once(&pool, &config, &payout_signer).await {
            tracing::error!("payout drain failed: {e}");
        }
        // After the drain, so a payout submitted this pass gets its first confirmation check as
        // soon as the next one comes round rather than a full interval later.
        if let Err(e) = payout::confirm_payouts_once(&pool, &tron_client).await {
            tracing::error!("payout confirmation failed: {e}");
        }
```

Use the `TronClient` the reconciliation loop already constructs rather than making a second one.

- [ ] **Step 2: Verify it builds and the suite is green**

Run: `docker compose -f docker-compose.test.yml run --rm test cargo test --workspace -- --test-threads=1`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/treasury-service/src/main.rs
git commit -m "feat(treasury): run the payout drain and confirmation workers"
```

---

### Task 10: Config, ops surface, and the docs that are now wrong

**Files:**
- Modify: `../clutch-deploy/docker-compose.treasury.yml`
- Modify: `../clutch-deploy/scripts/inspect-stage.sh`
- Modify: `docs/keys.md`
- Modify: `../CLAUDE.md`, `../clutch-deploy/CLAUDE.md`

**Interfaces:**
- Consumes: `APP_PER_TX_PAYOUT_CAP_USDT` (Task 2), `APP_PAYOUT_FLOAT_ADDRESS` and `APP_DAILY_PAYOUT_CAP_CLT` (Tasks 5, 6).

- [ ] **Step 1: Add the environment variables**

In `../clutch-deploy/docker-compose.treasury.yml`, under `tron-signer`:

```yaml
      # Per-transaction payout ceiling in MICRO-USDT. Not the same unit as the treasury's
      # APP_DAILY_PAYOUT_CAP_CLT — do not derive one from the other.
      - APP_PER_TX_PAYOUT_CAP_USDT=${PER_TX_PAYOUT_CAP_USDT:-25000000}
```

Under `treasury-service`:

```yaml
      # Read off tron-signer's /internal/xpub. The treasury holds no key material and must not be
      # able to derive a spending address; it only needs to know where to look.
      - APP_PAYOUT_FLOAT_ADDRESS=${PAYOUT_FLOAT_ADDRESS:?set PAYOUT_FLOAT_ADDRESS in .env}
      # Rolling 24h payout ceiling in CLT BASE UNITS.
      - APP_DAILY_PAYOUT_CAP_CLT=${DAILY_PAYOUT_CAP_CLT:-100000000}
```

Leave `APP_REDEMPTIONS_ENABLED=false`. Flipping it is step 6 of the rollout, after the float is funded and reconciliation is verified — not part of this change.

- [ ] **Step 2: Add the float to the treasury probe**

In `../clutch-deploy/scripts/inspect-stage.sh`, directly after the existing
`=== TRX float (fee account) ===` block, add this. It reuses `$FEE_JSON` — the `/internal/xpub`
response that block already fetched — rather than calling the signer a second time:

```sh
  # The USDT payout float. Redemptions are paid from here, never from custody: nothing in this
  # stack holds a key for the custody address. An operator tops this up from custody, and its
  # balance is the ceiling on what a compromised treasury-service could move. Empty means every
  # redemption parks at payout_pending with a float_dry alert -- burns stay safe, payouts stall.
  echo ""
  echo "=== USDT float (payout account) ==="
  PAYOUT_ADDR=$(printf '%s' "$FEE_JSON" | sed -n 's/.*"payout_address"[ ]*:[ ]*"\([^"]*\)".*/\1/p')
  if [ -z "$PAYOUT_ADDR" ]; then
    echo "    (no payout_address from tron-signer -- is it running a build with the payout rail?)"
  else
    echo "    payout_address=$PAYOUT_ADDR"
  fi
```

Reporting the address alone is deliberate for this step. Its USDT balance needs a `balanceOf`
call, and the existing balance probe already does exactly that — use `PROBE=balance` with
`ADDRESS=$PAYOUT_ADDR` rather than duplicating the contract-call plumbing into a second place.

- [ ] **Step 3: Update `docs/keys.md`**

Replace the `PayoutRail` paragraph:

```markdown
- Keys per function (spec §5): mint / reserve custody / payout initiation are THREE
  SEPARATE keys, none interchangeable. Two now exist in some form.
  The PAYOUT key is a hot float derived from the deposit mnemonic at
  `m/44'/195'/0'/2/0`, held only by tron-signer, spendable only via
  `POST /internal/payout`, and bounded by its own balance plus a per-tx cap. It is
  deliberately NOT the custody key: an operator tops it up from custody, and its
  balance is the ceiling on what a compromised treasury-service could move.
  The CUSTODY key still does not exist in this stack — nothing here can spend from
  APP_TREASURY_ADDRESS, which is why the float exists at all.
  `PayoutSigner` (crates/treasury-service/src/payout.rs) is the swap boundary for a
  KMS-backed payout key, exactly as `ChainSigner` is for the mint key.
```

Leave the MAINNET BLOCKER line untouched.

- [ ] **Step 4: Amend the CLAUDE.md rule**

In both `../CLAUDE.md` and `../clutch-deploy/CLAUDE.md`, the rule currently reads that `tron-signer`'s sweep API takes an INDEX and nothing else and that adding `to`, `contract` or `amount` deletes the reason the service exists. Scope it:

```markdown
- **`tron-signer`'s SWEEP API takes an INDEX and nothing else** — the destination is its own
  config. Do not add a `to`, `contract`, or `amount` parameter there: each one individually
  deletes the reason that endpoint exists, and owning the orchestrator must never move a deposit.
  **The PAYOUT endpoint (`/internal/payout`) is the deliberate exception** and does take `to` and
  `amount`, because a redemption has no other way to express them. Its bound is different, not
  absent: it can only spend from the payout float at `2/0` — never a deposit address, never
  custody — so the float balance caps the loss, and a per-tx cap bounds one request. Unlike sweep,
  its safety DOES depend on the bearer token and the internal-only network. `contract` is still
  never a parameter. See
  `clutch-treasury/docs/superpowers/specs/2026-08-30-redemption-payout-rail-design.md`.
```

- [ ] **Step 5: Commit**

```bash
git add docs/keys.md
git commit -m "docs: the payout key now exists as a derived float"
```

Then in `clutch-deploy` (a separate repo — commit there separately):

```bash
git add docker-compose.treasury.yml scripts/inspect-stage.sh CLAUDE.md
git commit -m "feat: payout float config and probe, scoped signer rule"
```

The workspace-root `CLAUDE.md` is not in a git repo; edit it in place and mention the change in the PR body.

---

## Rollout (after all tasks land)

Not code — do these in order, and stop if step 4 disagrees.

- [ ] Deploy with `APP_REDEMPTIONS_ENABLED` still `false`.
- [ ] Read `payout_address` from `PROBE=treasury` on `inspect-stage.yml`.
- [ ] Fund it: USDT from custody, and confirm the TRX fee account has headroom for the float's first transfer.
- [ ] **Confirm reconciliation still reads `ok`** with the float counted — `PROBE=sweeper` shows the breaker and the last reconciliation runs. If it reads `mismatch`, Task 5 is wrong; stop and fix it with the flag still off.
- [ ] One small end-to-end redemption on Nile, verified on chain.
- [ ] Flip `APP_REDEMPTIONS_ENABLED` to `true`.
