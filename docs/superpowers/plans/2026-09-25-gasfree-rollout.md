# GasFree Rollout (Plan 4 of 4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put the GasFree rail into service on the Nile testnet: its settings for all three treasury services, the checks and tools the design names in `clutch-deploy`, the fee and the minimum in the deposit panel, the fixes carried from Plans 2 and 3, and the five rollout steps of the design's §9.

**Architecture:** Three pull requests, then a runbook. `clutch-treasury` (Tasks 1-3): the treasury derives the GasFree float from the plain float and counts both, the tripwire also halts minting, two stall ages are published, and two waits get a limit. `clutch-deploy` (Tasks 4-8): one set of `.env` names feeds every service that reads it, `check-cap-invariants.sh` checks the GasFree settings and gates the stage deploy, `PROBE=gasfree` compares the live fees and code with the settings, a typed-confirmation workflow activates the float, and the sweep tool, the alert rules and the on-call page learn the rail. `clutch-hub` (Task 9): the deposit panel shows "fee up to" and "send at least". Task 10 is the Nile rollout, run by the controller and the maintainer together.

**Tech Stack:** Rust (`treasury-service`, `tron-signer`; sqlx, wiremock 0.6, tokio), Bash, GitHub Actions, Docker Compose, Prometheus rules (`clutch-deploy`), React 19 with `node:test` (`clutch-hub/apps/demo`).

**Spec:** `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md` — §6 configuration, §7 invariants, §4 the float and its activation, §5 the tripwire, §9 the rollout. The plans before this one: `docs/superpowers/plans/2026-09-24-gasfree-crate.md` (#49), `2026-09-24-gasfree-signer.md` (#51), `2026-09-25-gasfree-treasury-orchestrator.md` (#53, merged as `239f538`).

## Global Constraints

- **No local builds.** "Do not run `cargo`, `npm`, `docker`, or any build/test/lint command on this Windows host, and forbid it in every subagent prompt. Verify code by dispatching CI and reading the run log."
- A test counts only when the CI log shows it **by name**. A green badge is not evidence. Pick the CI run by head SHA.
- One implementer per checkout at a time; tasks in different repositories may run in parallel. Commit with `git commit -F <file>`; never put backticks in `-m`. Run git inside the repository, never in `D:\source\clutch`.
- "**`tron-signer`'s SWEEP API takes an INDEX and nothing else** — the destination is its own config. Do not add a `to`, `contract`, or `amount` parameter there." (workspace CLAUDE.md). `/internal/activate-float` and `/internal/fund-float` take no parameters at all, and the scripts that call them pass none.
- The payout endpoint "can only spend from the payout float at `2/0` — never a deposit address, never custody"; on the GasFree rail the float is `F = gasfree(the 2/0 address)` (spec §4). `contract` is never a parameter.
- **Merging changes nothing for GasFree.** GasFree stays off until the host `.env` sets `GASFREE_NETWORK`, and `TRANSFER_RAIL` defaults to `trx`. Changes that reach the running TRX rail on purpose, and are named in their tasks: the stage deploy stops when `check-cap-invariants.sh` fails (Task 5); two new alerts can fire, one of them (`TreasuryRedemptionUnpaid`) on either rail (Tasks 2 and 8); the mainnet treasury overlay requires `PAYOUT_FLOAT_ADDRESS` (Task 4 — the mainnet treasury is not running).
- The rule: "CLT minted must never exceed the USDT that will reach custody", and "**Never sign a higher `maxFee` than was held back**" (spec §2).
- "Stage secrets live in the host's `.env`, which the deploy workflow READS and never writes." The maintainer writes `GASFREE_API_KEY` and `GASFREE_API_SECRET` there, unquoted. Nobody pastes them into a chat, a commit, a PR or a log, and no script prints them.
- Never share secrets between `.env` and `.env.mainnet`. **Never `down -v`** against `clutch-main` or `clutch-main-treasury`.
- The mainnet rail stays off in this plan: "Mainnet follows only after that, with its own API key and its own fee reading" (spec §9).
- Line endings: files in `clutch-treasury` are stored CRLF — keep them so. Files in `clutch-deploy` and `clutch-hub` are LF. Edit with the Edit tool; never write Rust source through a bash heredoc (it eats backslashes).
- A fenced code block in a Rust doc comment must be marked `text`, or `cargo test` compiles it as a doctest.
- Commit messages and PR bodies: precise, and the plain word where both words work (workspace CLAUDE.md).

## Facts

Read in the code at `clutch-treasury` `239f538`, `clutch-deploy` `dbd96ce` and `clutch-hub` `9b27014`. Every task relies on these.

1. **Two switches, one set of names.** `gasfree::load_settings` (`crates/gasfree/src/settings.rs:35-67`) serves `treasury-service` and `payment-orchestrator`: GasFree is on when `APP_GASFREE_NETWORK` is set, and then `APP_GASFREE_ACTIVATE_FEE_MAX_USDT`, `APP_GASFREE_TRANSFER_FEE_MAX_USDT`, `APP_MIN_DEPOSIT_USDT`, `APP_GASFREE_EXPECTED_IMPLEMENTATION` and `APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION` are required; `APP_TRANSFER_RAIL` is `trx` (default) or `gasfree`. `tron-signer`'s `load_gasfree_config` (`crates/tron-signer/src/sweep/gasfree_rail.rs:100-168`) turns GasFree on by `APP_GASFREE_API_KEY`, and then needs `APP_GASFREE_NETWORK`, `APP_GASFREE_SERVICE_PROVIDER`, `APP_GASFREE_API_URL`, `APP_GASFREE_API_SECRET`, both maxima, both expected implementations and `APP_PAYOUT_FLOAT_TARGET_USDT`; `APP_GASFREE_DEADLINE_SECS` is optional (default 180). `APP_TRANSFER_RAIL=gasfree` in the signer means payouts by permit. **Both loaders treat a blank value as unset**, so compose can pass `${X:-}`.
2. **The signer's public material.** `GET /internal/xpub` answers `{account_xpub, fee_address, payout_address, payout_gasfree_address}` (`crates/tron-signer/src/main.rs:75-97`): `payout_address` is the plain float at `2/0`, `payout_gasfree_address` its GasFree account (null while GasFree is off). `POST /internal/activate-float` takes no body and answers `submitted{trace_id}`, `already_active{float_address}`, `float_dry{float_address,have_usdt,need_usdt}` or `refused{reason}`; it needs the float to hold at least 1 micro-USDT plus `activate + transfer` at their maxima (`gasfree_rail.rs:600-674`). `POST /internal/sweep` with GasFree on sweeps the index's GasFree account first when it holds USDT, and answers for it (`pending`, `busy`, `rejected`, `halted`); `below_fee` means the plain address was empty and the GasFree account held only dust (`sweep.rs:623-642`).
3. **`PAYOUT_FLOAT_ADDRESS` today.** The compose default is the testnet's plain float, `TF5p9P2UqwzDEtwvMxaXprNPxMPzMy8Uur`, which holds about 1,000 Nile USDT (`docker-compose.treasury.yml:164`, `:295-300`). The mainnet overlay does not override it, and `provision-treasury-secrets.sh` never writes it. In the treasury it is read at five places: `api.rs:311` (the float-activation check), `payout.rs:557` (the ambiguous-payout page), `payout.rs:689` (the confirmed transfer's sender), `payout.rs:746-756` (the signer's float must equal it), `reconciliation.rs:222` (the reserve walk). Plan 3 assumed it would name `F` on the GasFree rail.
4. **The tripwire today.** The signer refuses to sign; the treasury's sweeper skips GasFree sweeps and pages P1 (`sweeper/gasfree_sweep.rs:27-57`); the orchestrator answers 503 for new GasFree addresses. **The verifier keeps minting** — nothing on the mint path reads it.
5. **Metrics.** `clutch_treasury_unswept_deposit_addresses` counts `mint_intents` rows with `swept_at IS NULL` (`metrics.rs:101-115`), not `gasfree_accounts`, so `TreasurySweepingStalled` stays meaningful on the GasFree rail. Nothing publishes an age: a relay `Busy` that never clears, or a payout that never settles, pages nobody.
6. **Two timeouts.** The treasury's payout client times out after 30 s (`treasury-service/src/main.rs:298-301`); a GasFree payout makes two relay calls (20 s limit each) and several TronGrid reads. The signer's boot self-test runs before the port binds, through a `reqwest` client with no timeout (`tron-signer/src/main.rs:305`, `sweep.rs:488`).
7. **The compose files** use the list form of `environment:` (`- KEY=value`), so YAML anchors cannot share one block between services. Every value comes from `${VAR}` interpolation of the file given to `--env-file`. `deploy-stage.sh` already aborts before pulling when `.env` pins a retired USDT contract (`scripts/deploy-stage.sh:94-104`); nothing runs `check-cap-invariants.sh` on a deploy.
8. **CI.** `clutch-deploy`: `check-monitoring-config.yml` (job `promtool`) runs `promtool` and `docker compose ... config` on pull requests touching compose or Prometheus files, and on `workflow_dispatch`; `test-nginx-block.yml` shows the shape of a shell self-check (a `bash -n` step, then `bash scripts/test-*.sh`). A workflow must exist on `main` before `workflow_dispatch` can start it, so a new workflow first runs through a pull request. `clutch-hub`: `docker-publish.yml`'s `test` job runs `npm run test --workspace=clutch-hub-demo-app` (`node --test src/`) on pull requests; its `workflow_dispatch` also pushes a branch-tagged image, so tests on a branch run through a **draft pull request**, never a dispatch. `clutch-treasury`: `test.yml` by `workflow_dispatch` on the branch, as in Plan 3.
9. **The probe.** `PROBE=gasfree` (`scripts/inspect-stage.sh:1129-1181`) reads `GASFREE_API_KEY`/`GASFREE_API_SECRET` from `.env`, signs each GET, and prints the relay's token table, provider list and one account reply for both networks. The script runs under `set -uo pipefail` (no `-e`). TronGrid calls go through the signer: `docker exec clutch-stage-tron-signer-1 sh -c "curl ... \"\$APP_TRONGRID_URL/...\""` (`:423-426`).
10. **Live values.** Nile controller `THQGuFzL87ZqhxkgqYEryRAd7gqFqL5rdc`, beacon `TLtCGmaxH3PbuaF6kbybwteZcHptEdgQGC`; mainnet controller `TFFAMQLZybALaLb4uxHA9RBE7pxhUAjF3U`, beacon `TSP9UW6FQhT76XD2jWA6ipGMx3yGbjDffP` (`crates/gasfree/src/lib.rs:42-55`). Implementations read on 2026-09-24: Nile beacon `b8eda40b467b45af107f198e94cc2fa1378adf50`, Nile controller `2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441`, mainnet beacon `a3b0edffa1b94e93d297dcc9b6860175e9b537ec`, mainnet controller `c8b13e3104f8a2d6e915ac132bdeda7faaf84d7d`. Nile fees 1.00 USDT to activate and 0.30 per transfer. The Nile provider was `TKtWbdzEq5ss9vTS9kwRhBp5mXmBfBns3E`; Task 10 reads it again from the probe before it is written anywhere.
11. **Reconciliation.** `judge` compares `custody_reported` (the whole reserve walk) with `ledger_liability` in one unit (`reconciliation.rs:42-67`); the surplus is `custody_reported - ledger_liability`. Each run is a row of `reconciliation_runs (run_at, status, custody_reported, ledger_liability, ...)`.
12. **The demo app.** `DepositPanel.jsx` reads only `body.address` from `POST /api/v1/deposits` and says "send any amount of Nile USDT". The orchestrator also returns `fee_up_to_usdt` and `min_deposit_usdt` (micro-USDT integers) for a GasFree address (`crates/payment-orchestrator/src/api.rs:190-198`). Demo tests are `node:test` files over pure functions (`src/**/*.test.js`); there are no component tests. `utils/money.js` imports the SDK package, which no demo test loads in Node today. The Nile faucet sends 1,000 test USDT per request.

## Decisions

The spec is the authority; these are where it is silent, or where this plan changes one of its mechanisms, with the reason.

1. **The GasFree float is derived from `PAYOUT_FLOAT_ADDRESS`, and the reserve counts both floats.** `PAYOUT_FLOAT_ADDRESS` keeps meaning the plain float at `2/0` on both rails. While GasFree is on, the treasury derives `F = gasfree(PAYOUT_FLOAT_ADDRESS)` with the `gasfree` crate — key-free, as it already derives each deposit's `G` — and uses `F` wherever Plan 3 used the configured address for GasFree payouts. This changes the spec's §4 mechanism ("provisioning ... writing `PAYOUT_FLOAT_ADDRESS`" as `F`) and keeps its purpose: the treasury and the signer derive `F` from one input with one crate, so they cannot disagree, and a disagreement still pages. Why: provisioning never overwrites, so on the testnet it could not change the existing value; switching the value by hand would drop the plain float's 1,000 Nile USDT out of the reserve and trip the breaker; `fund-float` would keep filling an uncounted address; and switching back to the TRX rail would drop `F`. Counting both is correct in every state. Cost if wrong: one more balance read per reconciliation run while GasFree is on.
2. **Provisioning writes `PAYOUT_FLOAT_ADDRESS` from the signer's `payout_address`, and the mainnet overlay requires it.** Absent: written. Present and equal: left. Present and different: abort, like the xpub check. Today the mainnet treasury would count the testnet's float through the compose default.
3. **The tripwire also halts minting.** The spec's §5 lists three actions; §2's rule adds this one: CLT minted against a GasFree account whose code nobody has reviewed may never reach custody. The sweeper sets the breaker only when it is not already set, so an earlier reason is kept, and a human resumes minting with `resume-minting.yml` after updating the setting. Cost if wrong: deposits to plain addresses also wait for a human after a GasFree upgrade.
4. **Stalls page from two ages, not from counts.** `clutch_treasury_oldest_unswept_gasfree_seconds` (the oldest credited, unswept deposit at a recorded GasFree account; sweeps run every 60 s) and `clutch_treasury_oldest_unpaid_redemption_seconds` (the oldest redemption with its CLT burned and its USDT unpaid, on either rail). Alerts at 1 hour and 2 hours. Plain-address deposits are left out of the first: they wait for the sweep threshold by design.
5. **One `.env` name per setting, repeated in each service's `environment:`**, because list syntax cannot share an anchor. CI sets every name to a marker and asserts each service receives it (Task 4); `check-cap-invariants.sh` asserts the set is complete, so the signer cannot run with GasFree off while the other two run with it on (Task 5).
6. **The invariants gate the stage deploy.** `deploy-stage.sh` runs `check-cap-invariants.sh` before it pulls anything, after the existing retired-contract check, so a broken relationship stops a deploy with the stack as it was.
7. **Float activation reads the surplus from the latest reconciliation run** (status `ok`, under 2 hours old) and the maxima from the running signer's environment — the values that size the permit.
8. **`sweep-address.sh` refuses any index with a recorded GasFree account.** The signer sweeps an index's GasFree account first, and the treasury follows only permits it asked for itself; a permit asked for by hand would move deposits that stay "unswept" on the books.
9. **The float target covers the largest payout.** `PAYOUT_FLOAT_TARGET_USDT >= PER_TX_PAYOUT_CAP_USDT + GASFREE_TRANSFER_FEE_MAX_USDT`, or the largest redemption allowed can find the float too small for ever, since the float stops filling at its target.
10. **Two carried items get no code.** Plan 3's final review M3 (a payout answering `float_not_active` is retried every 2 s) becomes a check in Task 10 step 2 — no redemption may be waiting when payouts switch — because the treasury already refuses new redemptions until the float is activated. M5 (a deposit the fee takes whole pages P1) stays as it is: the deposit panel now shows the minimum before anyone pays.
11. **The deposit panel takes its terms from the POST answer only.** "Send at least" is `fee_up_to_usdt + min_deposit_usdt`: after the fee, that is the minimum. A plain address keeps today's text. The network word comes from `IS_TESTNET`, so the mainnet panel will not say "Nile". The formatter is local to the new module: `money.js` imports the SDK, and no demo test loads the SDK in Node.
12. **The probe's fee comparison is its own script**, `scripts/gasfree-fee-check.sh`, reading the relay's token list on stdin, so CI can test it against a fixture; the probe pipes the live reply into it.
13. **Nile values for Task 10:** maxima 1.50 and 0.50 USDT (spec §2), `MIN_DEPOSIT_USDT` 1.00, `PAYOUT_FLOAT_TARGET_USDT` 30.00 (above the 25 USDT payout cap plus 0.50), deadline left at the signer's default 180 s.
14. **The two waits:** the treasury's payout client allows 60 s while GasFree is on (30 s otherwise); the signer gives its boot self-test 30 s, and a self-test that runs out of time counts as `Unreachable` — boot continues and every permit is still checked before it is signed.

## Order of work

| Repository | Branch | Tasks | Pull request |
|---|---|---|---|
| `clutch-treasury` | `feat/gasfree-rollout` | 1, 2, 3 | one, merged first (builds the images) |
| `clutch-deploy` | `feat/gasfree-deploy` | 4, 5, 6, 7, 8 | one, merged second (its compose change deploys stage) |
| `clutch-hub` | `feat/deposit-panel-gasfree` | 9 | one, merged third (dispatches `deploy-stage`) |
| — | — | 10 | the rollout, after all three merge |

Tasks in different repositories may run in parallel; within one repository, one task at a time. The controller opens the `clutch-deploy` and `clutch-hub` pull requests as **drafts** after their first commit, so CI runs on every push (fact 8), and marks them ready when their last task is complete. The maintainer merges every pull request.

---

### Task 1: The GasFree float, derived and counted (clutch-treasury)

**Files:**
- Modify: `crates/treasury-service/src/configuration.rs` (the `payout_float_address` doc, `AppConfig::gasfree_float`, a boot check in `load`)
- Modify: `crates/treasury-service/src/tron_verifier.rs` (`get_reserve_balance` takes every float)
- Modify: `crates/treasury-service/src/reconciliation.rs` (passes both floats)
- Modify: `crates/treasury-service/src/api.rs` (the activation check asks about `F`)
- Modify: `crates/treasury-service/src/payout.rs` (`F` at three places)
- Test: `crates/treasury-service/tests/db_tron_verifier.rs`, `crates/treasury-service/tests/db_redemption.rs`

**Interfaces:**
- Consumes: `gasfree::gasfree_address(chain: &Chain, user: &str) -> Result<String, String>`; `AppConfig.gasfree: Option<gasfree::Settings>` (Plan 3).
- Produces: `AppConfig::gasfree_float(&self) -> Option<String>`; `TronClient::get_reserve_balance(&self, main_address: &str, unswept_addresses: &[String], float_addresses: &[String], usdt_contract: &str) -> Result<i64, String>`.

Decision 1. `PAYOUT_FLOAT_ADDRESS` stays the plain float; `F` is derived from it.

- [ ] **Step 0: The branch**

```bash
cd /d/source/clutch/clutch-treasury
git checkout main
git pull --ff-only origin main
git checkout -b feat/gasfree-rollout
```

- [ ] **Step 1: The stubs, and the callers of the new signature**

In `crates/treasury-service/src/configuration.rs`, replace the doc comment of `payout_float_address`:

```rust
    /// The payout float address, read off tron-signer's /internal/xpub.
    ///
    /// Configured rather than derived: this service holds no key material and must not be able to
    /// derive spending addresses. It only needs to know where to LOOK, so it is given the address.
    pub payout_float_address: String,
```

with:

```rust
    /// The plain payout float at 2/0, read off tron-signer's /internal/xpub (`payout_address`), on
    /// both rails.
    ///
    /// Configured rather than derived: this service holds no key material and must not be able to
    /// derive spending addresses. It only needs to know where to LOOK, so it is given the address.
    /// Its GasFree account is derived from it, key-free, by `gasfree_float`.
    pub payout_float_address: String,
```

In the same file, inside `impl AppConfig`, right after `effective_mint_threshold`:

```rust
    /// Signatures a Mint needs. 0 and 1 both mean one, matching the node.
    pub fn effective_mint_threshold(&self) -> usize {
        self.mint_threshold.max(1) as usize
    }
```

add:

```rust

    /// The payout float's GasFree account, `F = gasfree(payout_float_address)`, while GasFree is on;
    /// `None` while it is off.
    ///
    /// Derived, key-free, with the crate the signer uses, so the two services cannot name different
    /// floats (GasFree design §4). GasFree payouts are paid from it and sweeps may fill it, so the
    /// reserve counts it beside the plain float, which keeps whatever it held before.
    pub fn gasfree_float(&self) -> Option<String> {
        todo!("Plan 4 Task 1: derive the GasFree float")
    }
```

In `crates/treasury-service/src/tron_verifier.rs`, change only the signature of `get_reserve_balance` and the first lines of its body, so it takes a slice and, for now, reads only its first entry. Replace:

```rust
        unswept_addresses: &[String],
        float_address: &str,
        usdt_contract: &str,
    ) -> Result<i64, String> {
```

with:

```rust
        unswept_addresses: &[String],
        float_addresses: &[String],
        usdt_contract: &str,
    ) -> Result<i64, String> {
        // Plan 4 Task 1 stub: the first float only, as before.
        let float_address = float_addresses.first().map(String::as_str).unwrap_or_default();
```

In `crates/treasury-service/src/reconciliation.rs`, replace:

```rust
            &unswept,
            &config.payout_float_address,
            &config.usdt_contract,
```

with:

```rust
            &unswept,
            &[config.payout_float_address.clone()],
            &config.usdt_contract,
```

- [ ] **Step 2: The tests**

In `crates/treasury-service/tests/db_tron_verifier.rs`:

1. After `const FLOAT: &str = "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY";` add:

```rust
/// A second float, as the GasFree account of the plain one would be. Any valid address: the mocks
/// answer by address.
const GASFREE_FLOAT: &str = "TRhVWK5XEDkQBDevcdCWW7RW51aRncty4W";
```

2. In the five existing calls of `get_reserve_balance` (the tests `reserve_balance_sums_the_main_address_and_every_unswept_deposit_address`, `a_single_unreadable_address_fails_the_whole_reserve_sum`, `the_reserve_includes_the_payout_float` and `a_reserve_walk_that_saw_custody_move_is_refused`), replace the argument `FLOAT` with `&[FLOAT.to_string()]`. Nothing else in those tests changes.

3. After `a_reserve_walk_that_saw_custody_move_is_refused`, add:

```rust
/// While GasFree is on there are two floats: the plain one keeps what it held, and GasFree payouts
/// leave from its GasFree account. Both are the treasury's money, so both are counted.
#[tokio::test]
async fn the_reserve_counts_every_float() {
    let server = MockServer::start().await;
    mount_balance(&server, REAL_MAIN, 700).await;
    mount_balance(&server, FLOAT, 300).await;
    mount_balance(&server, GASFREE_FLOAT, 200).await;

    let client = treasury_service::tron_verifier::TronClient::new(server.uri(), String::new());
    let total = client
        .get_reserve_balance(REAL_MAIN, &[], &[FLOAT.to_string(), GASFREE_FLOAT.to_string()], USDT)
        .await
        .unwrap();

    assert_eq!(total, 1200, "custody + the plain float + the GasFree float");
}

/// A sweep into the GasFree float while the walk reads the deposit addresses would count that USDT
/// twice, once at the deposit address and once in the float: a walk that saw any float move is refused.
#[tokio::test]
async fn a_reserve_walk_that_saw_the_gasfree_float_move_is_refused() {
    let server = MockServer::start().await;
    mount_balance(&server, REAL_MAIN, 700).await;
    mount_balance(&server, FLOAT, 300).await;
    Mock::given(method("POST"))
        .and(path("/wallet/triggerconstantcontract"))
        .and(body_string_contains(GASFREE_FLOAT))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"constant_result": [format!("{:064x}", 200)]})))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    mount_balance(&server, GASFREE_FLOAT, 900).await;

    let client = treasury_service::tron_verifier::TronClient::new(server.uri(), String::new());
    let err = client
        .get_reserve_balance(REAL_MAIN, &[], &[FLOAT.to_string(), GASFREE_FLOAT.to_string()], USDT)
        .await
        .expect_err("a walk that saw a float move is not a sum");

    assert!(err.contains("moved while the reserve was read"), "{err}");
}
```

In `crates/treasury-service/tests/db_redemption.rs`:

1. Replace:

```rust
/// `config().payout_float_address`: on this rail, F = gasfree(2/0).
const FLOAT: &str = "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY";
/// The plain 2/0 address that owns the float, as the signer's /internal/xpub names it.
const FLOAT_OWNER: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";
```

with:

```rust
/// The plain 2/0 address that owns the float, as the signer's /internal/xpub names it. It is
/// `PAYOUT_FLOAT_ADDRESS` on both rails; `gasfree_config` sets it.
const FLOAT_OWNER: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";

/// F = gasfree(2/0), which GasFree payouts leave from: derived here as the treasury and the signer
/// derive it, never written down, so this fixture cannot drift from the derivation.
fn float() -> String {
    gasfree::gasfree_address(&gasfree::NILE, FLOAT_OWNER).unwrap()
}
```

2. In `gasfree_config`, after `cfg.gasfree = Some(nile());`, add the line:

```rust
    cfg.payout_float_address = FLOAT_OWNER.into();
```

3. Replace every other use of `FLOAT` in this file with `float()`. There are nine: in `GasFreeReads::float_owner`, three in `mount_float_history` (bind `let float = float();` at the top of that function, then use `{float}` in the `path(format!(...))` and `float.as_str()` for the two `"from"` values), two in `the_gasfree_payout_answers_are_read_field_by_field`'s `float_not_active` pair, two in its `/internal/xpub` check, and one in `redemptions_wait_for_the_gasfree_floats_activation_and_it_is_said_once`. Also change the doc comment above `struct GasFreeReads` to end "...and FLOAT_OWNER owns `float()`.".

4. In `a_redemption_is_refused_until_the_gasfree_float_is_activated`, pin the second mock to the float, so the test fails if the handler asks about any other address. Replace:

```rust
    let activated = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/wallet/getcontract"))
        .respond_with(
```

with:

```rust
    let activated = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/wallet/getcontract"))
        .and(body_string_contains(float().as_str()))
        .respond_with(
```

5. At the end of the GasFree section (after `a_transaction_that_already_paid_another_redemption_is_not_taken_as_this_ones`), add:

```rust
/// Spec §4: the treasury derives the GasFree float the way the signer does, from the plain float it
/// is given. With GasFree off there is no GasFree float.
#[test]
fn the_gasfree_float_is_the_gasfree_account_of_the_plain_float() {
    assert_eq!(gasfree_config("http://unused".into()).gasfree_float(), Some(float()));
    assert_eq!(config().gasfree_float(), None);
}

/// The signer names a GasFree float other than the one this treasury derives and counts: nothing is
/// settled against it, and it pages.
#[tokio::test]
async fn a_gasfree_float_the_signer_does_not_share_pages_and_settles_nothing() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_float_nonce(&server, 4).await;
    let id = permit_out(&pool, 10_000_000, None, 4, now() - 400).await;

    payout::confirm_gasfree_payouts_once(
        &pool,
        &gasfree_config(server.uri()),
        &nile(),
        &TronClient::new(server.uri(), "k".into()),
        &OtherFloat,
    )
    .await
    .unwrap();

    assert_eq!(state_of(&pool, id).await, ("payout_submitted".into(), None, Some(4)), "not returned, not paid");
    assert_eq!(payout_alerts(&pool, "p1", "derives").await, 1);
}

/// The signer's reads, with a GasFree float that is not `float()`.
struct OtherFloat;

#[async_trait::async_trait]
impl PayoutSigner for OtherFloat {
    async fn pay(&self, _intent_id: Uuid, _to: &str, _amount_usdt: i64) -> PayoutReply {
        panic!("settling a payout must never sign another")
    }
    async fn trace(&self, _trace_id: &str) -> Result<Trace, String> {
        Ok(Trace { state: "SUCCEED".into(), txn_hash: None, txn_amount: None })
    }
    async fn float_owner(&self) -> Result<(String, Option<String>), String> {
        Ok((FLOAT_OWNER.into(), Some(REDEEMER.into())))
    }
}
```

- [ ] **Step 3: Commit, and the controller confirms red**

Write the commit message file `.superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg.txt`:

```text
test(treasury): the GasFree float is derived from the plain float, against stubs

PAYOUT_FLOAT_ADDRESS keeps naming the plain float at 2/0 on both rails,
and the treasury derives its GasFree account from it, as the signer does.
The reserve walk takes every float. gasfree_float() is todo!() and the
walk reads only its first float in this commit, so CI shows each new
test failing by name.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

(An implementer on another model ends with its own `Co-Authored-By` line.)

```bash
cd /d/source/clutch/clutch-treasury
git add crates/treasury-service/src/configuration.rs crates/treasury-service/src/tron_verifier.rs crates/treasury-service/src/reconciliation.rs crates/treasury-service/tests/db_tron_verifier.rs crates/treasury-service/tests/db_redemption.rs
git commit -F .superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg.txt
```

Controller: run CI. Expected: the run fails, and exactly these nine fail by name — the four new tests, and five existing GasFree payout tests whose fixture now names the plain float while the code still compares against it:

```text
test the_reserve_counts_every_float ... FAILED
test a_reserve_walk_that_saw_the_gasfree_float_move_is_refused ... FAILED
test the_gasfree_float_is_the_gasfree_account_of_the_plain_float ... FAILED
test a_gasfree_float_the_signer_does_not_share_pages_and_settles_nothing ... FAILED
test a_gasfree_payout_is_paid_once_its_transfer_from_the_float_is_confirmed ... FAILED
test a_refused_permit_goes_back_to_be_paid_once_its_deadline_passed_and_it_never_ran ... FAILED
test a_permit_whose_nonce_moved_without_a_transfer_is_left_for_a_human ... FAILED
test a_transaction_that_already_paid_another_redemption_is_not_taken_as_this_ones ... FAILED
test a_redemption_is_refused_until_the_gasfree_float_is_activated ... FAILED
```

Every other test is `ok`, including the four existing reserve tests and `a_refused_permit_inside_the_grace_after_its_deadline_is_left_alone`.

- [ ] **Step 4: Replace the stubs**

In `configuration.rs`, the body of `gasfree_float`:

```rust
    pub fn gasfree_float(&self) -> Option<String> {
        let settings = self.gasfree.as_ref()?;
        gasfree::gasfree_address(settings.chain, &self.payout_float_address).ok()
    }
```

In `AppConfig::load`, replace:

```rust
        cfg.gasfree = gasfree::load_settings(|name| std::env::var(name).ok()).unwrap_or_else(|e| panic!("{e}"));
        Ok(cfg)
```

with:

```rust
        cfg.gasfree = gasfree::load_settings(|name| std::env::var(name).ok()).unwrap_or_else(|e| panic!("{e}"));
        // At boot, not at the first payout: with no GasFree float there is nothing to pay from and
        // nothing to count, and every GasFree payout would be refused while the reserve reads low.
        assert!(
            cfg.gasfree.is_none() || cfg.gasfree_float().is_some(),
            "APP_PAYOUT_FLOAT_ADDRESS must be a TRON address while GasFree is on: the GasFree float is derived from it"
        );
        Ok(cfg)
```

In `tron_verifier.rs`, replace the doc lines:

```rust
    /// Custody + every unswept deposit address + the payout float.
    ///
    /// Custody and the float are read before and after the addresses; if either changed, the walk is
    /// refused, because a transfer between two reads would be counted twice.
```

with:

```rust
    /// Custody + every unswept deposit address + every payout float: the plain one, and its GasFree
    /// account while GasFree is on.
    ///
    /// Custody and every float are read before and after the addresses; if any of them changed, the
    /// walk is refused, because a transfer between two reads would be counted twice.
```

and the lines:

```rust
    /// `float_address` is a separate parameter rather than another entry in `unswept_addresses` so a
    /// failure reading it is attributed to the float, not misreported as a deposit problem.
```

with:

```rust
    /// `float_addresses` is a separate parameter rather than more entries in `unswept_addresses` so a
    /// failure reading one is attributed to the float, not misreported as a deposit problem.
```

Then replace the whole body of `get_reserve_balance` — from `// Plan 4 Task 1 stub: the first float only, as before.` down to its final `Ok(total)` — with:

```rust
        // Custody and every float first, and again after the walk. USDT moving between them and an
        // address read in between (a fund-float into the plain float, a sweep into custody or into
        // the GasFree float) would be counted twice, so a walk that saw any of them change is not a
        // sum of one moment.
        let main = self.get_custody_balance(main_address, usdt_contract).await?;
        let floats = self.float_balances(float_addresses, usdt_contract).await?;
        // Saturating: a corrupt balance must not wrap the reserve into something small.
        let mut total = floats.iter().fold(main, |sum, f| sum.saturating_add(*f));
        for addr in unswept_addresses {
            let bal = self
                .get_custody_balance(addr, usdt_contract)
                .await
                .map_err(|e| format!("unswept deposit address {addr}: {e}"))?;
            total = total.saturating_add(bal);
        }
        let main_after = self.get_custody_balance(main_address, usdt_contract).await?;
        let floats_after = self.float_balances(float_addresses, usdt_contract).await?;
        if main_after != main || floats_after != floats {
            return Err(format!(
                "custody or a payout float moved while the reserve was read (custody {main} then {main_after}, \
                 floats {floats:?} then {floats_after:?}); not a sum of one moment"
            ));
        }
        Ok(total)
    }

    /// Each float's balance, in order; an unreadable one fails the whole walk.
    async fn float_balances(&self, float_addresses: &[String], usdt_contract: &str) -> Result<Vec<i64>, String> {
        let mut balances = Vec::with_capacity(float_addresses.len());
        for addr in float_addresses {
            balances.push(
                self.get_custody_balance(addr, usdt_contract)
                    .await
                    .map_err(|e| format!("payout float {addr}: {e}"))?,
            );
        }
        Ok(balances)
```

(The closing `}` of `get_reserve_balance` becomes the closing `}` of `float_balances`.)

In `reconciliation.rs`, replace:

```rust
    let custody_reported = client
        .get_reserve_balance(
            &config.custody_tron_address,
            &unswept,
            &[config.payout_float_address.clone()],
            &config.usdt_contract,
        )
```

with:

```rust
    // The plain float and, while GasFree is on, its GasFree account: GasFree payouts leave from the
    // second and sweeps may fill it, while the first keeps whatever it held (GasFree design §4).
    let mut floats = vec![config.payout_float_address.clone()];
    floats.extend(config.gasfree_float());
    let custody_reported = client
        .get_reserve_balance(&config.custody_tron_address, &unswept, &floats, &config.usdt_contract)
```

In `api.rs`, replace:

```rust
    if state.config.gasfree.as_ref().is_some_and(|s| s.rail) {
        let client = crate::tron_verifier::TronClient::new(
            state.config.trongrid_url.clone(),
            state.config.trongrid_api_key.clone(),
        );
        match client.has_contract(&state.config.payout_float_address).await {
```

with:

```rust
    if state.config.gasfree.as_ref().is_some_and(|s| s.rail) {
        let Some(float) = state.config.gasfree_float() else {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        };
        let client = crate::tron_verifier::TronClient::new(
            state.config.trongrid_url.clone(),
            state.config.trongrid_api_key.clone(),
        );
        match client.has_contract(&float).await {
```

In `payout.rs`, three places.

1. In `drain_once`'s ambiguous page, replace:

```rust
                    float = config.payout_float_address
                )).await;
```

with:

```rust
                    float = if gasfree_payouts {
                        config.gasfree_float().unwrap_or_default()
                    } else {
                        config.payout_float_address.clone()
                    }
                )).await;
```

2. In `confirm_gasfree_payouts_once`, after the `rows` query (right before `let mut paid = 0u32;`), add:

```rust
    // GasFree payouts leave from the float's GasFree account, derived from the plain float.
    let Some(float) = config.gasfree_float() else {
        return Ok(0);
    };
```

and in the same function replace `.confirmed_transfer(&hash, &config.payout_float_address, &to, &config.usdt_contract, amount, since_ms)` with `.confirmed_transfer(&hash, &float, &to, &config.usdt_contract, amount, since_ms)`.

3. In the same function, replace:

```rust
        let owner = match signer.float_owner().await {
            Ok((owner, float)) if float.as_deref() == Some(config.payout_float_address.as_str()) => owner,
            Ok((_, float)) => {
                alert_once(
                    pool,
                    "p1",
                    "payout",
                    &format!(
                        "the signer's GasFree float is {float:?}, but PAYOUT_FLOAT_ADDRESS is {}: the reserve counts a \
                         different float than the one redemptions are paid from, and GasFree payouts cannot be settled \
                         until the two agree",
                        config.payout_float_address
                    ),
```

with:

```rust
        let owner = match signer.float_owner().await {
            Ok((owner, signers)) if signers.as_deref() == Some(float.as_str()) => owner,
            Ok((_, signers)) => {
                alert_once(
                    pool,
                    "p1",
                    "payout",
                    &format!(
                        "the signer's GasFree float is {signers:?}, but this treasury derives {float} from \
                         PAYOUT_FLOAT_ADDRESS {}: the reserve counts a different float than the one redemptions are \
                         paid from, and GasFree payouts cannot be settled until the two agree",
                        config.payout_float_address
                    ),
```

Do not change any test in this step.

- [ ] **Step 5: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(treasury): derive the GasFree float from the plain float, count both

PAYOUT_FLOAT_ADDRESS names the plain float at 2/0 on both rails. While
GasFree is on, the treasury derives F = gasfree(PAYOUT_FLOAT_ADDRESS)
with the crate the signer uses, checks for it at boot, and uses it for
the activation check, the payout's confirmed transfer and the check of
the signer's float. The reserve walk counts both floats and refuses a
walk that saw either move, so the plain float's balance is never dropped
and a sweep into F is never counted twice.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/treasury-service/src/configuration.rs crates/treasury-service/src/tron_verifier.rs crates/treasury-service/src/reconciliation.rs crates/treasury-service/src/api.rs crates/treasury-service/src/payout.rs
git commit -F .superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg.txt
```

Controller: run CI. Expected: success; the nine tests above say `ok` by name, every `test result:` line says `ok`, and there is no new warning in `crates/treasury-service`.

---

### Task 2: Stalls page, and the tripwire halts minting (clutch-treasury)

**Files:**
- Modify: `crates/treasury-service/src/metrics.rs` (two ages)
- Modify: `crates/treasury-service/src/sweeper/gasfree_sweep.rs` (`code_unchanged` halts minting)
- Test: `crates/treasury-service/tests/db_metrics.rs`, `crates/treasury-service/tests/db_sweeper.rs`

**Interfaces:**
- Produces: the gauges `clutch_treasury_oldest_unswept_gasfree_seconds` and `clutch_treasury_oldest_unpaid_redemption_seconds` (integers, seconds, 0 when nothing waits), which Task 8's rules read; a breaker set with a `halt_reason` starting `GasFree tripwire: `.

Decisions 3 and 4. No stubs: the new tests fail until the code exists.

- [ ] **Step 1: The tests**

In `crates/treasury-service/tests/db_metrics.rs`, in `pool()`, replace:

```rust
    sqlx::query("TRUNCATE treasury_events, mint_intents, chain_outbox, reconciliation_runs, alerts RESTART IDENTITY CASCADE")
```

with:

```rust
    sqlx::query("TRUNCATE treasury_events, mint_intents, chain_outbox, reconciliation_runs, alerts, redemption_intents, gasfree_accounts RESTART IDENTITY CASCADE")
```

and add at the end of the file:

```rust
/// Spec §5: a stuck relay "pages after a threshold". A count cannot see one deposit or one
/// redemption stuck for a day; an age can. Both read 0 when nothing waits, and a plain-address
/// deposit, which waits for the sweep threshold by design, does not count toward the GasFree age.
#[tokio::test]
async fn the_stall_ages_follow_the_oldest_waiting_row() {
    let pool = pool().await;
    let body = treasury_service::metrics::render(&pool).await;
    assert!(body.contains("clutch_treasury_oldest_unswept_gasfree_seconds 0\n"), "{body}");
    assert!(body.contains("clutch_treasury_oldest_unpaid_redemption_seconds 0\n"), "{body}");

    sqlx::query(
        "INSERT INTO gasfree_accounts (derivation_index, gasfree_address, owner_address)
         VALUES (3, 'TGasFreeAccountOf3', 'TOwnerOf3')",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (address, index, hours) in [("TGasFreeAccountOf3", 3_i64, 2_i32), ("TPlainAddressOf4", 4, 30)] {
        sqlx::query(
            "INSERT INTO mint_intents
                (id, beneficiary, amount_clt, credit_ref, created_by, approved_by, client_ref,
                 expected_amount_usdt, deposit_address, derivation_index, status, verified_at)
             VALUES ($1, 'TBene', 1000000, $2, 'orchestrator', 'tron-verifier', $3, 1000000, $4, $5,
                     'credited', now() - make_interval(hours => $6))",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(format!("ref-{address}"))
        .bind(format!("client-{address}"))
        .bind(address)
        .bind(index)
        .bind(hours)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO redemption_intents (id, redeemer_address, payout_address, amount_clt, status, redemption_ref, created_at)
         VALUES ($1, '0xaaaa000000000000000000000000000000000009', 'TRedeemer', 5000000, 'payout_pending', $2,
                 now() - interval '3 hours')",
    )
    .bind(uuid::Uuid::new_v4())
    .bind("a".repeat(64))
    .execute(&pool)
    .await
    .unwrap();

    let body = treasury_service::metrics::render(&pool).await;
    let gauge = |name: &str| -> i64 {
        body.lines()
            .find_map(|l| l.strip_prefix(name).and_then(|rest| rest.strip_prefix(' ')))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("{name} missing:\n{body}"))
    };
    let gasfree = gauge("clutch_treasury_oldest_unswept_gasfree_seconds");
    assert!((7_100..=7_300).contains(&gasfree), "the GasFree deposit, two hours old, not the older plain one: {gasfree}");
    let unpaid = gauge("clutch_treasury_oldest_unpaid_redemption_seconds");
    assert!((10_700..=10_900).contains(&unpaid), "three hours: {unpaid}");
}
```

In `crates/treasury-service/tests/db_sweeper.rs`, in `pool()`, after the `TRUNCATE ...` statement (its `.unwrap();`), add:

```rust
    // The tripwire sets the breaker; no test may inherit it from another.
    sqlx::query("UPDATE breaker_state SET minting_halted = FALSE, halt_reason = NULL")
        .execute(&pool)
        .await
        .unwrap();
```

and after `a_changed_gasfree_implementation_stops_gasfree_sweeps_and_pages_once`, add:

```rust
/// The GasFree design's §2 rule: CLT is minted only against USDT that will reach custody. Once
/// GasFree's code changes that is no longer known, so minting halts until a human looks.
#[tokio::test]
async fn a_changed_gasfree_implementation_halts_minting() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 0, "00000000000000000000000000000000000000ff").await;
    seed_account(&pool, 7, OWNER_7, false).await;
    seed_gasfree_deposit(&pool, 7, &account, 2_000_000).await;
    let signer = FakeSigner::new(pending(8_000_000, 2_000_000, 0));

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    let (halted, reason): (bool, Option<String>) =
        sqlx::query_as("SELECT minting_halted, halt_reason FROM breaker_state").fetch_one(&pool).await.unwrap();
    assert!(halted, "minting halts while GasFree's code is not the reviewed code");
    assert!(reason.unwrap_or_default().starts_with("GasFree tripwire: "), "the reason names the tripwire");
}

/// A breaker already set, by a person or by a mismatch, keeps its own reason: the tripwire does not
/// overwrite what a human is already reading.
#[tokio::test]
async fn the_tripwire_keeps_an_earlier_halt_reason() {
    let pool = pool().await;
    sqlx::query("UPDATE breaker_state SET minting_halted = TRUE, halt_reason = 'halted by hand'")
        .execute(&pool)
        .await
        .unwrap();
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 0, "00000000000000000000000000000000000000ff").await;
    seed_account(&pool, 7, OWNER_7, false).await;
    seed_gasfree_deposit(&pool, 7, &account, 2_000_000).await;

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &FakeSigner::new(SignerReply::Busy)).await;

    let reason: Option<String> = sqlx::query_scalar("SELECT halt_reason FROM breaker_state").fetch_one(&pool).await.unwrap();
    assert_eq!(reason.as_deref(), Some("halted by hand"));
}
```

- [ ] **Step 2: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test(treasury): stall ages and the tripwire's halt

Two ages a count cannot give: the oldest credited deposit at a GasFree
account not yet swept, and the oldest burned redemption not yet paid.
And after GasFree's code changes, minting halts, keeping an earlier
reason if the breaker is already set. Nothing here exists yet, so CI
shows the new tests failing by name.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/treasury-service/tests/db_metrics.rs crates/treasury-service/tests/db_sweeper.rs
git commit -F .superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg.txt
```

Controller: run CI. Expected: the run fails; exactly these two fail by name, and every other test is `ok` (`the_tripwire_keeps_an_earlier_halt_reason` passes already — nothing writes the breaker yet — and it is the guard for Step 3's `WHERE NOT minting_halted`):

```text
test the_stall_ages_follow_the_oldest_waiting_row ... FAILED
test a_changed_gasfree_implementation_halts_minting ... FAILED
```

- [ ] **Step 3: The code**

In `crates/treasury-service/src/metrics.rs`, right after the line:

```rust
    out.push_str(&format!("clutch_treasury_unswept_deposit_addresses {unswept}\n"));
```

add:

```rust

    // Two ages, because a count cannot see a stall: one deposit stuck for a day moves no count.
    //
    // A GasFree account is swept on the next one-minute pass after its deposit is credited, so its
    // oldest credited, unswept deposit is minutes old unless something stopped it: the relay refusing
    // or busy for good, GasFree's code changing, a live fee above the maximum, or the signer's
    // settings disagreeing. Plain addresses wait for the sweep threshold by design and are left out.
    let unswept_gasfree_age: i64 = sqlx::query_scalar(
        "SELECT COALESCE(EXTRACT(EPOCH FROM now() - MIN(m.verified_at)), 0)::BIGINT
           FROM mint_intents m JOIN gasfree_accounts g ON g.gasfree_address = m.deposit_address
          WHERE m.swept_at IS NULL AND m.status IN ('credited', 'submitted')",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    header(
        &mut out,
        "clutch_treasury_oldest_unswept_gasfree_seconds",
        "Age of the oldest credited deposit at a GasFree account that is not yet swept. Sweeps run every minute; an hour means sweeping has stopped.",
        "gauge",
    );
    out.push_str(&format!("clutch_treasury_oldest_unswept_gasfree_seconds {unswept_gasfree_age}\n"));

    // A redemption whose CLT is burned and whose USDT is not yet paid, on either rail.
    let unpaid_redemption_age: i64 = sqlx::query_scalar(
        "SELECT COALESCE(EXTRACT(EPOCH FROM now() - MIN(created_at)), 0)::BIGINT
           FROM redemption_intents WHERE status IN ('burn_confirmed', 'payout_pending', 'payout_submitted')",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    header(
        &mut out,
        "clutch_treasury_oldest_unpaid_redemption_seconds",
        "Age of the oldest redemption whose CLT is burned and whose USDT is not yet paid.",
        "gauge",
    );
    out.push_str(&format!("clutch_treasury_oldest_unpaid_redemption_seconds {unpaid_redemption_age}\n"));
```

In `crates/treasury-service/src/sweeper/gasfree_sweep.rs`, in `code_unchanged`, replace the `Ok(Some(reason))` arm:

```rust
        Ok(Some(reason)) => {
            alert_once(
                pool,
                "p1",
                "sweeper",
                &format!(
                    "{reason}. GasFree sweeps have stopped until someone reviews the new code and updates the \
                     setting. Money already in GasFree accounts is exposed either way; no more should go in."
                ),
                hourly(),
            )
            .await;
            false
        }
```

with:

```rust
        Ok(Some(reason)) => {
            // Minting stops too: CLT minted against a GasFree account whose code nobody has reviewed
            // may never reach custody (the GasFree design's §2 rule). Only a breaker that is not set
            // already is set, so an earlier reason stays for whoever is reading it.
            if let Err(e) = sqlx::query(
                "UPDATE breaker_state SET minting_halted = TRUE, halt_reason = $1, updated_at = now()
                  WHERE NOT minting_halted",
            )
            .bind(format!("GasFree tripwire: {reason}"))
            .execute(pool)
            .await
            {
                tracing::error!("sweeper: could not halt minting after GasFree's code changed: {e}");
            }
            alert_once(
                pool,
                "p1",
                "sweeper",
                &format!(
                    "{reason}. GasFree sweeps and all minting have stopped until someone reviews the new code, \
                     updates the setting, and resumes minting (resume-minting.yml). Money already in GasFree \
                     accounts is exposed either way; no more should go in."
                ),
                hourly(),
            )
            .await;
            false
        }
```

Do not change any test in this step.

- [ ] **Step 4: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(treasury): publish two stall ages, and halt minting on the tripwire

clutch_treasury_oldest_unswept_gasfree_seconds and
clutch_treasury_oldest_unpaid_redemption_seconds let a rule see one stuck
deposit or one burned, unpaid redemption, which no count can. After
GasFree's code changes, the sweeper now also sets the breaker, unless it
is set already: CLT minted against unreviewed code may never reach
custody.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/treasury-service/src/metrics.rs crates/treasury-service/src/sweeper/gasfree_sweep.rs
git commit -F .superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg.txt
```

Controller: run CI. Expected: success; the three tests of this task say `ok` by name, `a_changed_gasfree_implementation_stops_gasfree_sweeps_and_pages_once` still says `ok`, every `test result:` line says `ok`, and there is no new warning.

---

### Task 3: Two waits get a limit (clutch-treasury)

**Files:**
- Modify: `crates/tron-signer/src/sweep/gasfree_rail.rs` (`gasfree_self_test_within`)
- Modify: `crates/tron-signer/src/main.rs` (boot calls it)
- Modify: `crates/treasury-service/src/main.rs` (the payout client's timeout)
- Test: `crates/tron-signer/src/sweep/gasfree_rail/tests.rs`

**Interfaces:**
- Produces: `SweepClient::gasfree_self_test_within(&self, signer: &Signer, limit: std::time::Duration) -> SelfTest`.

Decision 14.

- [ ] **Step 1: The stub and the test**

In `crates/tron-signer/src/sweep/gasfree_rail.rs`, right after the end of `pub async fn gasfree_self_test(&self, signer: &Signer) -> SelfTest { ... }`, add:

```rust

    /// `gasfree_self_test`, given up after `limit`. It runs before the port binds, and a TronGrid
    /// that accepts the connection and never answers would otherwise keep the signer from starting
    /// at all. Given up is `Unreachable`, not `Failed`: every permit is still checked before it is
    /// signed.
    pub async fn gasfree_self_test_within(&self, signer: &Signer, limit: std::time::Duration) -> SelfTest {
        todo!("Plan 4 Task 3: bound the self-test by {limit:?} for {}", signer.address_at(0).is_ok())
    }
```

In `crates/tron-signer/src/sweep/gasfree_rail/tests.rs`, after `the_self_test_is_not_fatal_when_trongrid_is_down`, add:

```rust
#[tokio::test]
async fn the_self_test_gives_up_on_a_trongrid_that_never_answers() {
    // Bound and never accepted: the connection is made, and no answer ever comes.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", silent.local_addr().unwrap());
    let result = client(&url)
        .gasfree_self_test_within(&signer(), std::time::Duration::from_millis(300))
        .await;
    assert!(matches!(result, SelfTest::Unreachable(_)), "got {result:?}");
}
```

- [ ] **Step 2: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test(signer): the boot self-test gives up on a silent TronGrid, against a stub

The self-test runs before the port binds. A TronGrid that accepts a
connection and never answers would keep the signer from starting.
gasfree_self_test_within is todo!() in this commit so CI shows the new
test failing by name.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/tron-signer/src/sweep/gasfree_rail.rs crates/tron-signer/src/sweep/gasfree_rail/tests.rs
git commit -F .superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg.txt
```

Controller: run CI. Expected: the run fails; exactly `the_self_test_gives_up_on_a_trongrid_that_never_answers ... FAILED`, and every other test is `ok`.

- [ ] **Step 3: The code**

In `gasfree_rail.rs`, the body of `gasfree_self_test_within`:

```rust
    pub async fn gasfree_self_test_within(&self, signer: &Signer, limit: std::time::Duration) -> SelfTest {
        tokio::time::timeout(limit, self.gasfree_self_test(signer))
            .await
            .unwrap_or_else(|_| SelfTest::Unreachable(format!("TronGrid gave no answer within {} s", limit.as_secs())))
    }
```

In `crates/tron-signer/src/main.rs`, replace:

```rust
    match sweeper.gasfree_self_test(&signer).await {
```

with:

```rust
    match sweeper.gasfree_self_test_within(&signer, std::time::Duration::from_secs(30)).await {
```

In `crates/treasury-service/src/main.rs`, replace:

```rust
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
```

with:

```rust
            // A GasFree payout makes two relay calls, each bounded at 20 s in the signer, and several
            // TronGrid reads before it answers; at 30 s a slow success would read as Ambiguous and
            // page, holding the float for its longest deadline.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(if config.gasfree.is_some() { 60 } else { 30 }))
```

Do not change any test in this step.

- [ ] **Step 4: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
fix: bound the signer's boot self-test and widen the GasFree payout wait

tron-signer gives its GasFree self-test 30 s before boot goes on without
it (Unreachable, as for a TronGrid that is down); every permit is still
checked before it is signed. treasury-service waits 60 s for a payout
answer while GasFree is on, 30 s otherwise, so a slow but real GasFree
payout does not read as Ambiguous.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/tron-signer/src/sweep/gasfree_rail.rs crates/tron-signer/src/main.rs crates/treasury-service/src/main.rs
git commit -F .superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg.txt
```

Controller: run CI. Expected: success; `the_self_test_gives_up_on_a_trongrid_that_never_answers` and the four existing self-test tests say `ok` by name, every `test result:` line says `ok`, and there is no new warning in `crates/tron-signer` or `crates/treasury-service`.

- [ ] **Step 5 (controller): Open the clutch-treasury pull request**

Write the body to `.superpowers/sdd/2026-09-25-gasfree-rollout/pr-body-treasury.md`, filling in the final run id:

```markdown
Plan 4 of 4 for `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md`, Tasks 1-3 (plan: `docs/superpowers/plans/2026-09-25-gasfree-rollout.md`). The treasury-side changes the rollout needs.

**GasFree is still off after this merges**; nothing sets `GASFREE_NETWORK` yet. One change reaches the TRX rail: a new metric, `clutch_treasury_oldest_unpaid_redemption_seconds`, which `clutch-deploy`'s next PR alerts on.

- **The GasFree float is derived from the plain float, and the reserve counts both.** `PAYOUT_FLOAT_ADDRESS` names the plain float at 2/0 on both rails; the treasury derives `F = gasfree(PAYOUT_FLOAT_ADDRESS)` as the signer does. This replaces the spec's "provisioning writes F": on the testnet that would have dropped the plain float's 1,000 Nile USDT out of the reserve.
- **The tripwire also halts minting**, keeping an earlier breaker reason.
- **Two stall ages**: the oldest unswept GasFree deposit and the oldest burned, unpaid redemption.
- **Two waits get a limit**: the signer's boot self-test (30 s) and the treasury's GasFree payout call (60 s).

## Evidence

CI run <id>: the 8 new tests and the re-pinned activation test by name, every test binary green, no new warning. Each task was first seen failing by name.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
```

```bash
cd /d/source/clutch/clutch-treasury
gh pr create --repo clutchprotocol/clutch-treasury --base main --head feat/gasfree-rollout --title "feat: the treasury side of the GasFree rollout" --body-file .superpowers/sdd/2026-09-25-gasfree-rollout/pr-body-treasury.md
```

---

### Task 4: The GasFree settings reach all three services (clutch-deploy)

**Files:**
- Modify: `docker-compose.treasury.yml` (the three services' `environment:`)
- Modify: `docker-compose.mainnet.treasury.yml` (`PAYOUT_FLOAT_ADDRESS` required)
- Modify: `.env.example`, `.env.mainnet.example` (the GasFree block, documented and commented out)
- Modify: `.github/workflows/check-monitoring-config.yml` (assert every service receives its settings)

**Interfaces:**
- Consumes: the `APP_*` names of fact 1.
- Produces: the `.env` names every later task and the rollout use — `TRANSFER_RAIL`, `GASFREE_NETWORK`, `GASFREE_API_URL`, `GASFREE_API_KEY`, `GASFREE_API_SECRET`, `GASFREE_SERVICE_PROVIDER`, `GASFREE_ACTIVATE_FEE_MAX_USDT`, `GASFREE_TRANSFER_FEE_MAX_USDT`, `MIN_DEPOSIT_USDT`, `GASFREE_EXPECTED_IMPLEMENTATION`, `GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION`, `PAYOUT_FLOAT_TARGET_USDT`. Each maps to `APP_<same name>`.

Decisions 2 and 5.

- [ ] **Step 0: The branch**

```bash
cd /d/source/clutch/clutch-deploy
git checkout main
git pull --ff-only origin main
git checkout -b feat/gasfree-deploy
```

- [ ] **Step 1: The assertion, before the mappings exist**

In `.github/workflows/check-monitoring-config.yml`, in the step `Compose files must parse, including both mainnet projects`, add `PAYOUT_FLOAT_ADDRESS` to the dummy values the mainnet merge gets. Replace:

```yaml
                   ORCHESTRATOR_POSTGRES_PASSWORD JWT_SECRET; do
```

with:

```yaml
                   ORCHESTRATOR_POSTGRES_PASSWORD JWT_SECRET PAYOUT_FLOAT_ADDRESS; do
```

Then add a new step at the end of the job, after that step's final `echo "all compose files parse"`:

```yaml

      - name: GasFree settings reach every service that reads them
        run: |
          set -euo pipefail
          # One .env name per setting feeds all three treasury services, so they cannot read
          # different values. What can go wrong is a service missing a line: it then runs with
          # GasFree off while the others run with it on. Set every name to a marker and read back
          # what each service receives. (.env exists: the step above wrote it.)
          {
            for v in JWT_SECRET GRAFANA_ADMIN_PASSWORD TREASURY_POSTGRES_PASSWORD ORCHESTRATOR_POSTGRES_PASSWORD \
                     MINT_AUTHORITY_SECRET TREASURY_INITIATOR_TOKEN TREASURY_APPROVER_TOKEN TREASURY_READONLY_TOKEN \
                     SIGNER_TOKEN DEPOSIT_MNEMONIC CUSTODY_TRON_ADDRESS DEPOSIT_ACCOUNT_XPUB; do
              echo "$v=ci"
            done
            for v in TRANSFER_RAIL GASFREE_NETWORK GASFREE_API_URL GASFREE_API_KEY GASFREE_API_SECRET \
                     GASFREE_SERVICE_PROVIDER GASFREE_ACTIVATE_FEE_MAX_USDT GASFREE_TRANSFER_FEE_MAX_USDT \
                     MIN_DEPOSIT_USDT GASFREE_EXPECTED_IMPLEMENTATION GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION \
                     PAYOUT_FLOAT_TARGET_USDT; do
              echo "$v=marker-$v"
            done
          } > /tmp/g.env
          merged=$(docker compose --env-file /tmp/g.env -f docker-compose.yml -f docker-compose.treasury.yml config --format json)
          receives() {  # receives <service> <.env name>...: each must arrive as APP_<name>, unchanged
            local svc="$1" v got
            shift
            for v in "$@"; do
              got=$(printf '%s' "$merged" | jq -r --arg s "$svc" --arg k "APP_$v" '.services[$s].environment[$k] // "MISSING"')
              if [ "$got" != "marker-$v" ]; then
                echo "::error::$svc receives APP_$v=$got, expected marker-$v"
                exit 1
              fi
            done
            echo "$svc: all $# GasFree settings"
          }
          SHARED="TRANSFER_RAIL GASFREE_NETWORK GASFREE_ACTIVATE_FEE_MAX_USDT GASFREE_TRANSFER_FEE_MAX_USDT GASFREE_EXPECTED_IMPLEMENTATION GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION"
          receives treasury-service $SHARED MIN_DEPOSIT_USDT
          receives payment-orchestrator $SHARED MIN_DEPOSIT_USDT
          receives tron-signer $SHARED GASFREE_API_URL GASFREE_API_KEY GASFREE_API_SECRET GASFREE_SERVICE_PROVIDER PAYOUT_FLOAT_TARGET_USDT
```

- [ ] **Step 2: Commit, open the draft pull request, and the controller confirms red**

Write the commit message file `D:\source\clutch\clutch-treasury\.superpowers\sdd\2026-09-25-gasfree-rollout\commit-msg-deploy.txt`:

```text
ci: assert every treasury service receives its GasFree settings

Each GasFree setting is one .env name mapped into every service that
reads it. The new step sets each name to a marker and checks what each
service receives. The mappings are not in the compose file yet, so this
commit's run fails at that step.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-deploy
git add .github/workflows/check-monitoring-config.yml
git commit -F /d/source/clutch/clutch-treasury/.superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg-deploy.txt
```

The commit message files for all three repositories live in this plan's workspace in `clutch-treasury`, so nothing is written into `clutch-deploy` or `clutch-hub` except the task's own files.

Controller: push the branch and open the pull request as a draft: `gh pr create --repo clutchprotocol/clutch-deploy --base main --head feat/gasfree-deploy --draft --title "feat: deploy the GasFree rail (settings, invariants, probe, float activation)" --body "Plan 4 of the GasFree rail, Tasks 4-8. Draft until the plan's Task 8 is complete."`. Expected in `Check monitoring config` / `promtool`: every step before the new one passes (including the mainnet merge, which does not require `PAYOUT_FLOAT_ADDRESS` yet), and the new step fails with `::error::treasury-service receives APP_TRANSFER_RAIL=MISSING, expected marker-TRANSFER_RAIL`.

- [ ] **Step 3: The mappings**

In `docker-compose.treasury.yml`, in `treasury-service`, after the line:

```yaml
      - APP_REDEMPTION_FEE_USDT=${REDEMPTION_FEE_USDT:-1000000}
```

add:

```yaml
      # The GasFree rail (clutch-treasury docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md,
      # §6). OFF while GASFREE_NETWORK is blank, which is the default: every line below is then empty,
      # and this service runs the TRX rail exactly as before.
      #
      # The SAME .env names feed treasury-service, payment-orchestrator and tron-signer, so the three
      # cannot read different values. check-monitoring-config.yml asserts each service maps them, and
      # check-cap-invariants.sh, run before every stage deploy, asserts the set is complete. Once any
      # user has a GasFree address, keep these set for as long as that address exists, even with
      # TRANSFER_RAIL=trx: without them this service refuses deposits there, and the orchestrator will
      # not show the address.
      - APP_TRANSFER_RAIL=${TRANSFER_RAIL:-trx}
      - APP_GASFREE_NETWORK=${GASFREE_NETWORK:-}
      - APP_GASFREE_ACTIVATE_FEE_MAX_USDT=${GASFREE_ACTIVATE_FEE_MAX_USDT:-}
      - APP_GASFREE_TRANSFER_FEE_MAX_USDT=${GASFREE_TRANSFER_FEE_MAX_USDT:-}
      - APP_MIN_DEPOSIT_USDT=${MIN_DEPOSIT_USDT:-}
      - APP_GASFREE_EXPECTED_IMPLEMENTATION=${GASFREE_EXPECTED_IMPLEMENTATION:-}
      - APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION=${GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION:-}
```

In `tron-signer`, between:

```yaml
      - APP_PER_TX_PAYOUT_CAP_USDT=${PER_TX_PAYOUT_CAP_USDT:-25000000}
      - APP_HTTP_ADDR=0.0.0.0:8093
```

insert:

```yaml
      # The GasFree rail, from the same .env names as treasury-service's (see there). This service
      # turns it on by GASFREE_API_KEY, the other two by GASFREE_NETWORK; check-cap-invariants.sh
      # refuses a deploy with one set and not the other. TRANSFER_RAIL=gasfree here means payouts by
      # permit from the GasFree float. The key and secret are relay credentials: they cannot move money
      # (only a permit signed here can), and they live only in the host's .env.
      - APP_TRANSFER_RAIL=${TRANSFER_RAIL:-trx}
      - APP_GASFREE_NETWORK=${GASFREE_NETWORK:-}
      - APP_GASFREE_API_URL=${GASFREE_API_URL:-}
      - APP_GASFREE_API_KEY=${GASFREE_API_KEY:-}
      - APP_GASFREE_API_SECRET=${GASFREE_API_SECRET:-}
      - APP_GASFREE_SERVICE_PROVIDER=${GASFREE_SERVICE_PROVIDER:-}
      - APP_GASFREE_ACTIVATE_FEE_MAX_USDT=${GASFREE_ACTIVATE_FEE_MAX_USDT:-}
      - APP_GASFREE_TRANSFER_FEE_MAX_USDT=${GASFREE_TRANSFER_FEE_MAX_USDT:-}
      - APP_GASFREE_EXPECTED_IMPLEMENTATION=${GASFREE_EXPECTED_IMPLEMENTATION:-}
      - APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION=${GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION:-}
      # Sweeps pay into the GasFree float until it holds this much, then into custody (design §4).
      - APP_PAYOUT_FLOAT_TARGET_USDT=${PAYOUT_FLOAT_TARGET_USDT:-}
```

In `payment-orchestrator`, after the line:

```yaml
      - APP_PERMANENT_DEPOSIT_ADDRESSES_ENABLED=${PERMANENT_DEPOSIT_ADDRESSES_ENABLED:-true}
```

add:

```yaml
      # The GasFree rail, from the same .env names as treasury-service's (see there). With
      # TRANSFER_RAIL=gasfree a new user is given a GasFree address, shown with the fee "up to" and
      # the minimum; a user keeps the kind of address they were given.
      - APP_TRANSFER_RAIL=${TRANSFER_RAIL:-trx}
      - APP_GASFREE_NETWORK=${GASFREE_NETWORK:-}
      - APP_GASFREE_ACTIVATE_FEE_MAX_USDT=${GASFREE_ACTIVATE_FEE_MAX_USDT:-}
      - APP_GASFREE_TRANSFER_FEE_MAX_USDT=${GASFREE_TRANSFER_FEE_MAX_USDT:-}
      - APP_MIN_DEPOSIT_USDT=${MIN_DEPOSIT_USDT:-}
      - APP_GASFREE_EXPECTED_IMPLEMENTATION=${GASFREE_EXPECTED_IMPLEMENTATION:-}
      - APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION=${GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION:-}
```

In `docker-compose.mainnet.treasury.yml`, in `treasury-service`'s `environment:`, after the line:

```yaml
      - "APP_USDT_CONTRACT=${USDT_CONTRACT:?set USDT_CONTRACT in .env.mainnet — verify it on tronscan.org first}"
```

add:

```yaml
      # The plain payout float at 2/0 of THIS deployment's wallet, which provision-treasury-secrets.sh
      # writes from the signer's own /internal/xpub. Required here: the base file's default is the
      # TESTNET float, and a mainnet reserve counting it would miss the real float entirely. The
      # GasFree float is derived from this one (treasury-service's AppConfig::gasfree_float).
      - "APP_PAYOUT_FLOAT_ADDRESS=${PAYOUT_FLOAT_ADDRESS:?set PAYOUT_FLOAT_ADDRESS in .env.mainnet — provision-treasury-secrets.sh writes it}"
```

At the end of `.env.example`, add:

```text

# The GasFree rail: USDT sweeps and payouts with no TRX, the relay's fee paid in USDT. OFF while
# GASFREE_NETWORK is unset. See clutch-treasury's docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md
# §6, and docs/ON-CALL.md "The GasFree rail". Values below are the testnet's (Nile).
#
# Keep every line set for as long as any user has a GasFree address, even after going back to
# TRANSFER_RAIL=trx. Put the key and the secret in unquoted, and never into a chat, a commit or a log.
# Keep each comment on its own line: check-cap-invariants.sh reads a value to the end of its line.
#
# TRANSFER_RAIL=gasfree
# GASFREE_NETWORK=nile
# GASFREE_API_URL=https://open-test.gasfree.io/nile
# GASFREE_API_KEY=
# GASFREE_API_SECRET=
# The relay's own address; PROBE=gasfree prints the provider list.
# GASFREE_SERVICE_PROVIDER=
# Above the live fees (1.00 and 0.30 on Nile). Users are charged these, "up to".
# GASFREE_ACTIVATE_FEE_MAX_USDT=1500000
# GASFREE_TRANSFER_FEE_MAX_USDT=500000
# After the fee, a deposit below this mints nothing and waits for a human.
# MIN_DEPOSIT_USDT=1000000
# GasFree's reviewed code; PROBE=gasfree prints the live values.
# GASFREE_EXPECTED_IMPLEMENTATION=b8eda40b467b45af107f198e94cc2fa1378adf50
# GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION=2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441
# At least the largest payout plus its relay fee.
# PAYOUT_FLOAT_TARGET_USDT=30000000
```

At the end of `.env.mainnet.example`, add:

```text

# ---------------------------------------------------------------------------------------------
# The GasFree rail. NOT before mainnet has its own API key and its own fee reading (design §9):
# the Nile key answers "Apikey not found." here. Same names and rules as .env.example's block.
# ---------------------------------------------------------------------------------------------
# TRANSFER_RAIL=gasfree
# GASFREE_NETWORK=mainnet
# GASFREE_API_URL=https://open.gasfree.io/tron
# GASFREE_API_KEY=
# GASFREE_API_SECRET=
# GASFREE_SERVICE_PROVIDER=
# GASFREE_ACTIVATE_FEE_MAX_USDT=
# GASFREE_TRANSFER_FEE_MAX_USDT=
# MIN_DEPOSIT_USDT=
# Read on 2026-09-24; read again with PROBE=gasfree before switching on.
# GASFREE_EXPECTED_IMPLEMENTATION=a3b0edffa1b94e93d297dcc9b6860175e9b537ec
# GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION=c8b13e3104f8a2d6e915ac132bdeda7faaf84d7d
# PAYOUT_FLOAT_TARGET_USDT=
```

and in its `GENERATED` list, after the line `# DEPOSIT_ACCOUNT_XPUB=    <- derived from it and checked against it on every run`, add:

```text
# PAYOUT_FLOAT_ADDRESS=    <- the plain float at 2/0, read off the signer; the GasFree float derives from it
```

- [ ] **Step 4: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(compose): map the GasFree settings into the three treasury services

One .env name per setting, mapped into every service that reads it,
blank by default, so GasFree stays off until GASFREE_NETWORK is set.
The mainnet treasury overlay now requires PAYOUT_FLOAT_ADDRESS: the base
default is the testnet's float. Both env examples document the block,
commented out.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-deploy
git add docker-compose.treasury.yml docker-compose.mainnet.treasury.yml .env.example .env.mainnet.example
git commit -F /d/source/clutch/clutch-treasury/.superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg-deploy.txt
```

Controller: push the branch, then read CI on the draft pull request. Expected: `Check monitoring config` / `promtool` succeeds, and its log shows `treasury-service: all 7 GasFree settings`, `payment-orchestrator: all 7 GasFree settings`, `tron-signer: all 11 GasFree settings` and `merge asserted: KMS signer, empty plaintext secret, mainnet node URL`.

---

### Task 5: The invariants check the GasFree settings, and gate the deploy (clutch-deploy)

**Files:**
- Modify: `scripts/check-cap-invariants.sh` (invariants 6-10)
- Modify: `scripts/deploy-stage.sh` (run it before pulling)
- Create: `scripts/test-cap-invariants.sh`
- Create: `.github/workflows/test-treasury-scripts.yml`

**Interfaces:**
- Consumes: the `.env` names of Task 4.
- Produces: `check-cap-invariants.sh` exits 1 on a broken GasFree setting; `test-treasury-scripts.yml` (job `test`), which Tasks 6 and 7 extend.

Decisions 5, 6 and 9, and the spec's §7.

- [ ] **Step 1: The self-check and its workflow**

Create `scripts/test-cap-invariants.sh`:

```bash
#!/usr/bin/env bash
# Self-check for check-cap-invariants.sh: each GasFree relationship it guards, by its exit code and by
# the line it prints. CI runs this (test-treasury-scripts.yml) with no .env, no docker, no network.
#
# The checker reads .env from its own repository root, so every case runs a copy of it from a temp
# directory that has none, and passes the values in the environment, which the checker reads first.
set -euo pipefail
cd "$(dirname "$0")/.."

T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/scripts"
cp scripts/check-cap-invariants.sh "$T/scripts/"

passed=0
failed=0

# check <name> <expected exit code> <text the output must contain> [NAME=value ...]
check() {
  local name="$1" want="$2" text="$3" out code=0
  shift 3
  out=$(env -i PATH="$PATH" "$@" bash "$T/scripts/check-cap-invariants.sh" 2>&1) || code=$?
  if [ "$code" -eq "$want" ] && printf '%s' "$out" | grep -qF -- "$text"; then
    passed=$((passed + 1))
    echo "ok    $name"
  else
    failed=$((failed + 1))
    echo "FAIL  $name: exit $code (wanted $want), wanted the text: $text"
    printf '%s\n' "$out" | sed 's/^/        /'
  fi
}

# A complete Nile set, as the rollout plan's Task 10 writes it.
NILE=(
  GASFREE_NETWORK=nile
  GASFREE_API_URL=https://open-test.gasfree.io/nile
  GASFREE_API_KEY=key-marker-7f3a
  GASFREE_API_SECRET=secret-marker-9c1d
  GASFREE_SERVICE_PROVIDER=TKtWbdzEq5ss9vTS9kwRhBp5mXmBfBns3E
  GASFREE_ACTIVATE_FEE_MAX_USDT=1500000
  GASFREE_TRANSFER_FEE_MAX_USDT=500000
  MIN_DEPOSIT_USDT=1000000
  GASFREE_EXPECTED_IMPLEMENTATION=b8eda40b467b45af107f198e94cc2fa1378adf50
  GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION=2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441
  PAYOUT_FLOAT_TARGET_USDT=30000000
)

check "GasFree off: the caps alone, as before" 0 "All invariants hold"
check "a complete Nile set holds" 0 "the redemption fee covers a GasFree payout's relay fee" "${NILE[@]}" TRANSFER_RAIL=gasfree
check "the float target covers the largest payout" 0 "the payout float fills far enough for the largest payout" "${NILE[@]}"
check "a trailing slash on the relay URL is the same URL" 0 "All invariants hold" "${NILE[@]}" GASFREE_API_URL=https://open-test.gasfree.io/nile/
check "the redemption fee below the transfer maximum" 1 "is below GASFREE_TRANSFER_FEE_MAX_USDT" "${NILE[@]}" REDEMPTION_FEE_USDT=400000
check "a zero maximum" 1 "GASFREE_TRANSFER_FEE_MAX_USDT is zero" "${NILE[@]}" GASFREE_TRANSFER_FEE_MAX_USDT=0
check "a maximum that is not a whole number" 1 "GASFREE_ACTIVATE_FEE_MAX_USDT must be a positive integer" "${NILE[@]}" GASFREE_ACTIVATE_FEE_MAX_USDT=1.5
check "the signer's API key missing" 1 "GASFREE_API_KEY is not set while GASFREE_NETWORK is" "${NILE[@]}" GASFREE_API_KEY=
check "the service provider missing" 1 "GASFREE_SERVICE_PROVIDER is not set while GASFREE_NETWORK is" "${NILE[@]}" GASFREE_SERVICE_PROVIDER=
check "the other network's relay URL" 1 "needs https://open-test.gasfree.io/nile" "${NILE[@]}" GASFREE_API_URL=https://open.gasfree.io/tron
check "an unknown network" 1 "GASFREE_NETWORK must be nile or mainnet" "${NILE[@]}" GASFREE_NETWORK=shasta
check "an implementation that is not 40 hex" 1 "GASFREE_EXPECTED_IMPLEMENTATION must be 40 hex characters" "${NILE[@]}" GASFREE_EXPECTED_IMPLEMENTATION=b8eda40b
check "the float target below the largest payout" 1 "PAYOUT_FLOAT_TARGET_USDT is below the largest payout plus its relay fee" "${NILE[@]}" PAYOUT_FLOAT_TARGET_USDT=20000000
check "TRANSFER_RAIL=gasfree without GasFree settings" 1 "TRANSFER_RAIL=gasfree needs GASFREE_NETWORK" TRANSFER_RAIL=gasfree
check "an unknown TRANSFER_RAIL" 1 "TRANSFER_RAIL must be trx or gasfree" TRANSFER_RAIL=gasfee

# The key and the secret are never printed, whatever else happens.
out=$(env -i PATH="$PATH" "${NILE[@]}" bash "$T/scripts/check-cap-invariants.sh" 2>&1 || true)
if printf '%s' "$out" | grep -qE 'key-marker-7f3a|secret-marker-9c1d'; then
  failed=$((failed + 1))
  echo "FAIL  the API key and secret never appear in the output"
else
  passed=$((passed + 1))
  echo "ok    the API key and secret never appear in the output"
fi

echo ""
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
```

Create `.github/workflows/test-treasury-scripts.yml`:

```yaml
# Self-checks for the scripts that guard the treasury's settings and move its money.
#
# check-cap-invariants.sh gates every stage deploy (deploy-stage.sh): a bug in it either lets a
# broken limit through or stops every deploy. No secrets, no host access, no docker — copies of the
# scripts, fixtures, and a temp directory. Same shape as test-nginx-block.yml.

name: Test treasury scripts

on:
  pull_request:
    paths:
      - "scripts/check-cap-invariants.sh"
      - "scripts/test-cap-invariants.sh"
      - ".github/workflows/test-treasury-scripts.yml"
  push:
    branches: [main]
    paths:
      - "scripts/check-cap-invariants.sh"
      - "scripts/test-cap-invariants.sh"
  workflow_dispatch:

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Syntax
        run: |
          bash -n scripts/check-cap-invariants.sh
          bash -n scripts/test-cap-invariants.sh
      - name: Cap invariants
        run: bash scripts/test-cap-invariants.sh
```

- [ ] **Step 2: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test: a self-check for check-cap-invariants.sh's GasFree relationships

Fifteen cases, each by exit code and printed line, plus a check that the
relay key and secret are never printed. The checker knows nothing of
GasFree yet, so this commit's run fails case by case.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-deploy
git add scripts/test-cap-invariants.sh .github/workflows/test-treasury-scripts.yml
git commit -F /d/source/clutch/clutch-treasury/.superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg-deploy.txt
```

Controller: push the branch, then read CI on the draft pull request. Expected: `Test treasury scripts` / `test` fails at the step `Cap invariants`; its log shows `ok` for exactly three cases — `GasFree off: the caps alone, as before`, `a trailing slash on the relay URL is the same URL`, `the API key and secret never appear in the output` — and `FAIL` for the other thirteen, then `3 passed, 13 failed`.

- [ ] **Step 3: The invariants, and the deploy gate**

In `scripts/check-cap-invariants.sh`, between the `fi` that closes invariant 5 and the final summary — that is, right before these lines:

```bash
echo ""
if [ "$fail" -eq 0 ]; then
  echo "All invariants hold. This says nothing about whether the VALUES are right —"
```

insert:

```bash
# 6-10. The GasFree rail (clutch-treasury's GasFree design, §6 and §7). Checked while GASFREE_NETWORK is
# set, which is what turns GasFree on in treasury-service and payment-orchestrator. The relay's API key
# and secret are reported as set or missing, never printed.
RAIL=$(val TRANSFER_RAIL trx)
GF_NETWORK=$(val GASFREE_NETWORK "")
case "$RAIL" in
  trx|gasfree) ;;
  *) bad "TRANSFER_RAIL must be trx or gasfree, got '$RAIL' — every treasury service refuses to start" ;;
esac
if [ "$RAIL" = "gasfree" ] && [ -z "$GF_NETWORK" ]; then
  bad "TRANSFER_RAIL=gasfree needs GASFREE_NETWORK and the other GasFree settings — every treasury service refuses to start"
fi
if [ -n "$GF_NETWORK" ]; then
  echo ""
  echo "=== the GasFree rail (GASFREE_NETWORK=$GF_NETWORK, TRANSFER_RAIL=$RAIL) ==="
  gf_ok=1
  # 6. Whole, positive micro-USDT. A zero maximum signs permits the relay refuses.
  for name in GASFREE_ACTIVATE_FEE_MAX_USDT GASFREE_TRANSFER_FEE_MAX_USDT MIN_DEPOSIT_USDT PAYOUT_FLOAT_TARGET_USDT; do
    v=$(val "$name" "")
    case "$v" in
      ''|*[!0-9]*) bad "$name must be a positive integer of micro-USDT, got '$v'"; gf_ok=0; continue ;;
    esac
    if [ "$v" -eq 0 ]; then
      bad "$name is zero — a zero maximum signs permits the relay refuses, and every sweep stops quietly"
      gf_ok=0
    fi
  done
  # 7. The signer turns GasFree on by its API key, the other two services by the network. One without
  #    the other runs two services with GasFree and one without it.
  for name in GASFREE_API_KEY GASFREE_API_SECRET GASFREE_SERVICE_PROVIDER; do
    if [ -z "$(val "$name" "")" ]; then
      bad "$name is not set while GASFREE_NETWORK is — tron-signer would run without GasFree while the other two run with it"
    fi
  done
  for name in GASFREE_EXPECTED_IMPLEMENTATION GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION; do
    v=$(val "$name" "")
    v="${v#0x}"
    if ! printf '%s' "$v" | grep -qE '^[0-9a-fA-F]{40}$'; then
      bad "$name must be 40 hex characters (PROBE=gasfree prints the live value), got '$v'"
    fi
  done
  # 8. The relay URL names a network too, and the other network's relay refuses every permit.
  case "$GF_NETWORK" in
    nile)    want_url="https://open-test.gasfree.io/nile" ;;
    mainnet) want_url="https://open.gasfree.io/tron" ;;
    *)       want_url=""; bad "GASFREE_NETWORK must be nile or mainnet, got '$GF_NETWORK'" ;;
  esac
  GF_URL=$(val GASFREE_API_URL "")
  if [ -n "$want_url" ] && [ "${GF_URL%/}" != "$want_url" ]; then
    bad "GASFREE_API_URL is '$GF_URL', but GASFREE_NETWORK=$GF_NETWORK needs $want_url"
  fi
  if [ "$gf_ok" -eq 1 ]; then
    ACT_MAX=$(val GASFREE_ACTIVATE_FEE_MAX_USDT "")
    XFER_MAX=$(val GASFREE_TRANSFER_FEE_MAX_USDT "")
    FLOAT_TARGET=$(val PAYOUT_FLOAT_TARGET_USDT "")
    note "fee held back, first deposit   up to $(usd $((ACT_MAX + XFER_MAX)))"
    note "fee held back, later deposits  up to $(usd "$XFER_MAX")"
    note "minimum after the fee          $(usd "$(val MIN_DEPOSIT_USDT "")")"
    note "payout float target            $(usd "$FLOAT_TARGET")"
    # 9. The design's §7 invariant 1: a GasFree payout's relay fee comes out of the float, and only the
    #    redemption fee pays it back. Below it, every redemption lowers the reserve below supply.
    if [ "$FEE" -lt "$XFER_MAX" ]; then
      bad "the redemption fee ($(usd "$FEE")) is below GASFREE_TRANSFER_FEE_MAX_USDT ($(usd "$XFER_MAX"))"
      note "A GasFree payout may cost the float the whole maximum, so every redemption would leave the reserve short."
    else
      ok "the redemption fee covers a GasFree payout's relay fee"
    fi
    # 10. Sweeps fill the float only while it is below its target. Below the largest payout plus its
    #     relay fee, the largest redemption allowed can wait for ever on a float that stopped filling.
    if [ "$FLOAT_TARGET" -lt $((PAYOUT_CAP + XFER_MAX)) ]; then
      bad "PAYOUT_FLOAT_TARGET_USDT is below the largest payout plus its relay fee ($(usd $((PAYOUT_CAP + XFER_MAX))))"
    else
      ok "the payout float fills far enough for the largest payout"
    fi
  fi
fi

```

In `scripts/deploy-stage.sh`, right after the `fi` that ends the retired-contract check (the block that prints `DEPLOY ABORTED — .env pins a retired USDT contract:`), add:

```bash

# The treasury's limits and its GasFree settings must agree with each other before anything is pulled
# or recreated. A broken relationship fails quietly in production — a limit that refuses everything,
# one that protects nothing, or three services reading GasFree differently — so a deploy that would
# run one stops here, with the stack as it was.
if [ "$TREASURY" = "true" ] && ! bash scripts/check-cap-invariants.sh; then
  echo ""
  echo "DEPLOY ABORTED — check-cap-invariants.sh found a broken relationship (above). Nothing was changed."
  exit 1
fi
```

- [ ] **Step 4: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat: check the GasFree settings before every stage deploy

check-cap-invariants.sh gains the design's §7 invariants: the redemption
fee covers a GasFree payout's relay fee, the maxima and the minimum are
positive, the float target covers the largest payout, the signer gets
the settings the other two services get, and the relay URL matches the
network. deploy-stage.sh now runs it before pulling anything, so a broken
relationship stops the deploy with the stack as it was.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-deploy
git add scripts/check-cap-invariants.sh scripts/deploy-stage.sh
git commit -F /d/source/clutch/clutch-treasury/.superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg-deploy.txt
```

Controller: push the branch, then read CI on the draft pull request. Expected: `Test treasury scripts` / `test` succeeds; its log shows `ok` for all sixteen cases by name and `16 passed, 0 failed`.

---

### Task 6: PROBE=gasfree compares the live network with the settings (clutch-deploy)

**Files:**
- Create: `scripts/gasfree-fee-check.sh`
- Create: `scripts/test-gasfree-fee-check.sh`
- Modify: `scripts/inspect-stage.sh` (the `gasfree` probe)
- Modify: `.github/workflows/test-treasury-scripts.yml`

**Interfaces:**
- Consumes: the `.env` names of Task 4; the controller and beacon addresses of fact 10.
- Produces: `bash scripts/gasfree-fee-check.sh <token> <activate max> <transfer max> < token-list` (exit 0: both fees at or below their maxima); the probe's three new sections, which Task 10 reads.

Decision 12, and the spec's §7 ("`PROBE=gasfree` reports whether the **live** fee is at or below each configured maximum").

- [ ] **Step 1: A stub, the self-check, and the workflow steps**

Create `scripts/gasfree-fee-check.sh`:

```bash
#!/usr/bin/env bash
# Plan 4 Task 6 stub: written in Step 3.
echo "not written yet"
exit 2
```

Create `scripts/test-gasfree-fee-check.sh`:

```bash
#!/usr/bin/env bash
# Self-check for gasfree-fee-check.sh, against fixtures shaped like the relay's reply to
# GET /api/v1/config/token/all. CI runs it (test-treasury-scripts.yml): no network, no .env.
set -euo pipefail
cd "$(dirname "$0")/.."

USDT=TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf

# reply <activateFee> <transferFee>: the token list as the relay sends it, compact, with another token
# listed first whose fees must never be read as ours.
reply() {
  printf '{"code":200,"reason":null,"message":null,"data":{"tokens":[{"tokenAddress":"TXLAQ63Xg1NAzckPwKHvzw7CSEmLMEqcdj","activateFee":9000000,"transferFee":9000000,"supported":true,"symbol":"USDT","decimal":6},{"tokenAddress":"%s","createdAt":"2024-06-01T00:00:00Z","activateFee":%s,"transferFee":%s,"supported":true,"symbol":"USDT","decimal":6}]}}' "$USDT" "$1" "$2"
}

passed=0
failed=0

# check <name> <expected exit code> <text the output must contain> <stdin> <token> <activate max> <transfer max>
check() {
  local name="$1" want="$2" text="$3" input="$4" out code=0
  shift 4
  out=$(printf '%s' "$input" | bash scripts/gasfree-fee-check.sh "$@" 2>&1) || code=$?
  if [ "$code" -eq "$want" ] && printf '%s' "$out" | grep -qF -- "$text"; then
    passed=$((passed + 1))
    echo "ok    $name"
  else
    failed=$((failed + 1))
    echo "FAIL  $name: exit $code (wanted $want), wanted the text: $text"
    printf '%s\n' "$out" | sed 's/^/        /'
  fi
}

check "Nile's live fees under the plan's maxima" 0 "transferFee: live 300000, at or below GASFREE_TRANSFER_FEE_MAX_USDT 500000" "$(reply 1000000 300000)" "$USDT" 1500000 500000
check "a fee exactly at its maximum is allowed" 0 "activateFee: live 1500000, at or below GASFREE_ACTIVATE_FEE_MAX_USDT 1500000" "$(reply 1500000 500000)" "$USDT" 1500000 500000
check "a transfer fee above its maximum" 1 "transferFee: live 600000, ABOVE GASFREE_TRANSFER_FEE_MAX_USDT 500000" "$(reply 1000000 600000)" "$USDT" 1500000 500000
check "an activation fee above its maximum" 1 "activateFee: live 2000000, ABOVE GASFREE_ACTIVATE_FEE_MAX_USDT 1500000" "$(reply 2000000 300000)" "$USDT" 1500000 500000
check "another token's fees are not ours" 0 "activateFee: live 1000000, at or below" "$(reply 1000000 300000)" "$USDT" 1500000 500000
check "a token the relay does not list" 1 "does not include TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t" "$(reply 1000000 300000)" TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t 1500000 500000
check "spaces after the colons" 0 "transferFee: live 300000, at or below" '{"data":{"tokens":[{"tokenAddress": "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf", "activateFee": 1000000, "transferFee": 300000}]}}' "$USDT" 1500000 500000
check "an empty reply" 1 "does not include TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf" "" "$USDT" 1500000 500000

echo ""
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
```

In `.github/workflows/test-treasury-scripts.yml`, add to both `paths:` lists (the `pull_request` one and the `push` one):

```yaml
      - "scripts/gasfree-fee-check.sh"
      - "scripts/test-gasfree-fee-check.sh"
      - "scripts/inspect-stage.sh"
```

add to the `Syntax` step's `run:`:

```bash
          bash -n scripts/gasfree-fee-check.sh
          bash -n scripts/test-gasfree-fee-check.sh
          bash -n scripts/inspect-stage.sh
```

and add a step at the end of the job:

```yaml
      - name: GasFree fee check
        run: bash scripts/test-gasfree-fee-check.sh
```

- [ ] **Step 2: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test: a self-check for the GasFree fee comparison, against a stub

gasfree-fee-check.sh will read the relay's token list and compare one
token's live fees with the configured maxima. Eight cases against
fixtures; the script is a stub in this commit, so the run fails case by
case.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-deploy
git add scripts/gasfree-fee-check.sh scripts/test-gasfree-fee-check.sh .github/workflows/test-treasury-scripts.yml
git commit -F /d/source/clutch/clutch-treasury/.superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg-deploy.txt
```

Controller: push the branch, then read CI on the draft pull request. Expected: `Test treasury scripts` / `test` passes `Syntax` and `Cap invariants` (16 passed), and fails at `GasFree fee check` with `FAIL` for all eight cases and `0 passed, 8 failed`.

- [ ] **Step 3: The comparison, and the probe**

Replace the whole of `scripts/gasfree-fee-check.sh` with:

```bash
#!/usr/bin/env bash
#
# Compare the GasFree relay's live fees for one token with the configured maxima. The GasFree
# design's §7: "PROBE=gasfree reports whether the live fee is at or below each configured maximum."
#
#   bash scripts/gasfree-fee-check.sh <token contract> <activate max> <transfer max>  < token list
#
# stdin is the relay's reply to GET /api/v1/config/token/all. Prints one line per fee, and exits 0
# when both are at or below their maximum, 1 when either is above it or cannot be read. No network
# and no .env: PROBE=gasfree pipes the live reply in, and test-gasfree-fee-check.sh a fixture.
set -uo pipefail

TOKEN="$1" ACT_MAX="$2" XFER_MAX="$3"

# One JSON object per line; the token's own object carries its fees.
entry=$(tr '{' '\n' | grep -E "\"tokenAddress\" *: *\"$TOKEN\"" | head -1)
if [ -z "$entry" ]; then
  echo "the relay's token list does not include $TOKEN"
  exit 1
fi

status=0
for check in "activateFee $ACT_MAX GASFREE_ACTIVATE_FEE_MAX_USDT" "transferFee $XFER_MAX GASFREE_TRANSFER_FEE_MAX_USDT"; do
  set -- $check
  live=$(printf '%s' "$entry" | sed -n "s/.*\"$1\" *: *\([0-9][0-9]*\).*/\1/p")
  if [ -z "$live" ]; then
    echo "$1: not in the relay's reply"
    status=1
  elif [ "$live" -le "$2" ]; then
    echo "$1: live $live, at or below $3 $2 -- OK"
  else
    echo "$1: live $live, ABOVE $3 $2 -- the relay refuses permits at this maximum; read docs/ON-CALL.md before raising it"
    status=1
  fi
done
exit "$status"
```

In `scripts/inspect-stage.sh`, in the `gasfree` probe, right before these two lines (the end of the branch that has an API key):

```bash
    echo ""
    echo "    Fees are in the token's smallest unit. For USDT, 1000000 = 1 USDT."
```

insert:

```bash
    # What this host is set to, against the live network (the design's §7). The network is the one
    # in .env; with GASFREE_NETWORK unset GasFree is off here and there is nothing to compare.
    GF_NET=$(sed -n 's/^GASFREE_NETWORK=//p' .env 2>/dev/null | head -1)
    case "$GF_NET" in
      nile)    GF_HOST=https://open-test.gasfree.io GF_PREFIX=/nile
               GF_BEACON=TLtCGmaxH3PbuaF6kbybwteZcHptEdgQGC GF_CONTROLLER=THQGuFzL87ZqhxkgqYEryRAd7gqFqL5rdc ;;
      mainnet) GF_HOST=https://open.gasfree.io GF_PREFIX=/tron
               GF_BEACON=TSP9UW6FQhT76XD2jWA6ipGMx3yGbjDffP GF_CONTROLLER=TFFAMQLZybALaLb4uxHA9RBE7pxhUAjF3U ;;
      *)       GF_HOST="" ;;
    esac
    echo ""
    echo "=== this host's GasFree settings against the live network (GASFREE_NETWORK=${GF_NET:-unset}) ==="
    if [ -z "$GF_HOST" ]; then
      echo "    GASFREE_NETWORK is not nile or mainnet in .env: GasFree is off here, nothing to compare."
    else
      GF_TOKEN=$(sed -n 's/^USDT_CONTRACT=//p' .env 2>/dev/null | head -1)
      [ -n "$GF_TOKEN" ] || GF_TOKEN=TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf
      GF_ACT=$(sed -n 's/^GASFREE_ACTIVATE_FEE_MAX_USDT=//p' .env 2>/dev/null | head -1)
      GF_XFER=$(sed -n 's/^GASFREE_TRANSFER_FEE_MAX_USDT=//p' .env 2>/dev/null | head -1)
      echo "--- live fees against the maxima ---"
      gf_get "$GF_HOST" "$GF_PREFIX/api/v1/config/token/all" \
        | bash scripts/gasfree-fee-check.sh "$GF_TOKEN" "${GF_ACT:-0}" "${GF_XFER:-0}" | sed 's/^/    /'
      # The tripwire's own reads, from inside the signer, so they use the TronGrid the sweeps use.
      # An expected value that is unset prints as unset: copy the live one in at switch-on.
      echo "--- GasFree's code against the expected implementations ---"
      for pair in "beacon $GF_BEACON GASFREE_EXPECTED_IMPLEMENTATION" \
                  "controller $GF_CONTROLLER GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION"; do
        set -- $pair
        word=$(docker exec clutch-stage-tron-signer-1 sh -c \
          "curl -fsS -X POST \"\$APP_TRONGRID_URL/wallet/triggerconstantcontract\" \
             -H 'Content-Type: application/json' \
             -d '{\"owner_address\":\"$2\",\"contract_address\":\"$2\",\"function_selector\":\"implementation()\",\"visible\":true}'" \
          2>/dev/null | sed -n 's/.*"constant_result"[ ]*:[ ]*\["\([0-9a-fA-F]*\)".*/\1/p')
        live="${word:24:40}"
        want=$(sed -n "s/^$3=//p" .env 2>/dev/null | head -1)
        want="${want#0x}"
        if [ -z "$live" ]; then
          echo "    $1 $2: implementation() unreadable (is tron-signer up?)"
        elif [ "$live" = "$want" ]; then
          echo "    $1 $2 runs $live -- matches $3"
        else
          echo "    $1 $2 runs $live -- $3 is '${want:-unset}'"
        fi
      done
      # The float GasFree payouts leave from, and whether its one-time activation has run.
      echo "--- the GasFree payout float ---"
      GF_FLOAT=$(docker exec clutch-stage-tron-signer-1 sh -c \
        'curl -fsS -H "Authorization: Bearer $APP_SIGNER_TOKEN" http://localhost:8093/internal/xpub' 2>/dev/null \
        | sed -n 's/.*"payout_gasfree_address"[ ]*:[ ]*"\([^"]*\)".*/\1/p')
      if [ -z "$GF_FLOAT" ]; then
        echo "    tron-signer names no GasFree float: GasFree is off in the signer (is GASFREE_API_KEY set?)"
      else
        GF_CONTRACT=$(docker exec clutch-stage-tron-signer-1 sh -c \
          "curl -fsS -X POST \"\$APP_TRONGRID_URL/wallet/getcontract\" \
             -H 'Content-Type: application/json' \
             -d '{\"value\":\"$GF_FLOAT\",\"visible\":true}'" 2>/dev/null)
        case "$GF_CONTRACT" in
          *'"contract_address"'*) echo "    $GF_FLOAT: activated" ;;
          '{}')                   echo "    $GF_FLOAT: NOT activated -- redemptions answer 'not available yet' until activate-float.yml runs" ;;
          *)                      echo "    $GF_FLOAT: activation unreadable" ;;
        esac
      fi
    fi
```

- [ ] **Step 4: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(probe): PROBE=gasfree compares the live network with this host's settings

gasfree-fee-check.sh reads the relay's token list and says, for the
configured USDT, whether each live fee is at or below its maximum (the
design's §7). PROBE=gasfree now runs it for the network in .env, reads
the beacon's and the controller's implementation() through the signer
against the expected values, and says whether the GasFree float is
activated.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-deploy
git add scripts/gasfree-fee-check.sh scripts/inspect-stage.sh
git commit -F /d/source/clutch/clutch-treasury/.superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg-deploy.txt
```

Controller: push the branch, then read CI on the draft pull request. Expected: `Test treasury scripts` / `test` succeeds; its log shows `ok` for all eight fee-check cases by name, `8 passed, 0 failed`, and still `16 passed, 0 failed` for the cap invariants. The probe's new sections are read for real in Task 10, step 2.

---

### Task 7: Activating the float, and provisioning its address (clutch-deploy)

**Files:**
- Create: `scripts/activate-float.sh`
- Create: `scripts/test-activate-float.sh`
- Create: `.github/workflows/activate-float.yml`
- Modify: `scripts/provision-treasury-secrets.sh` (writes `PAYOUT_FLOAT_ADDRESS`)
- Modify: `.github/workflows/test-treasury-scripts.yml`

**Interfaces:**
- Consumes: `POST /internal/activate-float` and `GET /internal/xpub` (fact 2); `reconciliation_runs` (fact 11); the running signer's `APP_GASFREE_ACTIVATE_FEE_MAX_USDT` and `APP_GASFREE_TRANSFER_FEE_MAX_USDT`.
- Produces: `can_activate <status> <age seconds> <custody_reported> <ledger_liability> <activate max> <transfer max>` (exit 0: activation may go ahead), defined by `activate-float.sh` when it is sourced; the workflow `activate-float.yml` (typed confirmation `activate`), which Task 10 runs once.

Decisions 2 and 7, and the spec's §4 ("a typed-confirmation workflow in `clutch-deploy`, like `fund-float.yml` — which refuses unless `reserve − supply ≥ ACTIVATE_MAX + TRANSFER_MAX`").

- [ ] **Step 1: A stub, the self-check, and the workflow step**

Create `scripts/activate-float.sh`:

```bash
#!/usr/bin/env bash
# Plan 4 Task 7 stub: written in Step 3.
can_activate() {
  echo "not written yet"
  return 2
}
```

Create `scripts/test-activate-float.sh`:

```bash
#!/usr/bin/env bash
# Self-check for activate-float.sh's decision, can_activate: whether the reserve's surplus pays for the
# GasFree float's one-time activation. CI runs it (test-treasury-scripts.yml). Nothing is activated:
# sourced, the script only defines its functions.
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=activate-float.sh
source scripts/activate-float.sh

passed=0
failed=0

# check <name> <expected exit code> <text> <status> <age s> <custody_reported> <ledger_liability> <activate max> <transfer max>
check() {
  local name="$1" want="$2" text="$3" out code=0
  shift 3
  out=$(can_activate "$@" 2>&1) || code=$?
  if [ "$code" -eq "$want" ] && printf '%s' "$out" | grep -qF -- "$text"; then
    passed=$((passed + 1))
    echo "ok    $name"
  else
    failed=$((failed + 1))
    echo "FAIL  $name: exit $code (wanted $want), wanted the text: $text"
    printf '%s\n' "$out" | sed 's/^/        /'
  fi
}

check "a fresh ok run with enough surplus" 0 "the surplus is 3000000 micro-USDT; activation may cost up to 2000000" ok 600 10000000 7000000 1500000 500000
check "a surplus exactly the most it may cost" 0 "the surplus is 2000000 micro-USDT" ok 600 9000000 7000000 1500000 500000
check "a surplus one micro-USDT short" 1 "the surplus is 1999999 micro-USDT; activation may cost up to 2000000" ok 600 8999999 7000000 1500000 500000
check "a reserve below liability" 1 "the surplus is -1000000 micro-USDT" ok 600 6000000 7000000 1500000 500000
check "a mismatch run" 1 "the latest reconciliation run is 'mismatch', not ok" mismatch 600 10000000 7000000 1500000 500000
check "no run at all" 1 "the latest reconciliation run is 'none', not ok" none "" "" "" 1500000 500000
check "a run over two hours old" 1 "is 7201s old" ok 7201 10000000 7000000 1500000 500000
check "a signer without GasFree settings" 1 "the running signer has no GasFree maxima" ok 600 10000000 7000000 "" ""

echo ""
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
```

In `.github/workflows/test-treasury-scripts.yml`, add to both `paths:` lists:

```yaml
      - "scripts/activate-float.sh"
      - "scripts/test-activate-float.sh"
      - "scripts/provision-treasury-secrets.sh"
```

add to the `Syntax` step's `run:`:

```bash
          bash -n scripts/activate-float.sh
          bash -n scripts/test-activate-float.sh
          bash -n scripts/provision-treasury-secrets.sh
```

and add a step at the end of the job:

```yaml
      - name: Float activation decision
        run: bash scripts/test-activate-float.sh
```

- [ ] **Step 2: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test: a self-check for the float activation's surplus decision, against a stub

activate-float.sh will activate the GasFree float only when the latest
reconciliation run is ok, under two hours old, and shows a surplus of at
least the two fee maxima. Eight cases; can_activate is a stub in this
commit, so the run fails case by case.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-deploy
git add scripts/activate-float.sh scripts/test-activate-float.sh .github/workflows/test-treasury-scripts.yml
git commit -F /d/source/clutch/clutch-treasury/.superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg-deploy.txt
```

Controller: push the branch, then read CI on the draft pull request. Expected: `Test treasury scripts` / `test` passes its earlier steps (16 and 8 passed) and fails at `Float activation decision` with `FAIL` for all eight cases and `0 passed, 8 failed`.

- [ ] **Step 3: The script, its workflow, and provisioning**

Replace the whole of `scripts/activate-float.sh` with:

```bash
#!/usr/bin/env bash
#
# Activate the GasFree payout float, once, from the reserve's surplus (GasFree design §4).
#
#   bash scripts/activate-float.sh
#
# The float's first outgoing transfer also pays for deploying its contract, up to ACTIVATE + TRANSFER.
# A redemption's fee covers only TRANSFER, so a redemption making that first transfer would leave the
# reserve below supply. So the first transfer is this one: the smallest amount the relay accepts, from
# the float to custody, paid for by the surplus the treasury already holds. Users are charged the
# configured maxima, and the difference stays behind as backing — that is what pays for this.
#
# It refuses unless the latest reconciliation run is `ok`, under two hours old, and shows
#
#   custody_reported - ledger_liability >= activate max + transfer max
#
# with the maxima read from the RUNNING signer: the values that size the permit's maxFee. Custody gains
# the amount moved; the reserve loses only the relay's fee, which the surplus covers.
#
# The endpoint takes no parameters — the float, custody, the amount and the fee cap are all the
# signer's own — and this script passes none. It must never grow any.

set -euo pipefail

SIGNER=clutch-stage-tron-signer-1
PG=clutch-stage-treasury-postgres-1

# Whether the surplus pays for the activation. Pure, so test-activate-float.sh can run it.
#
#   can_activate <status> <age seconds> <custody_reported> <ledger_liability> <activate max> <transfer max>
can_activate() {
  local status="$1" age="$2" reserve="$3" liability="$4" act="$5" xfer="$6" v
  for v in "$act" "$xfer"; do
    case "$v" in
      ''|*[!0-9]*)
        echo "the running signer has no GasFree maxima (APP_GASFREE_*_FEE_MAX_USDT): GasFree is off in tron-signer"
        return 1 ;;
    esac
  done
  if [ "$status" != "ok" ]; then
    echo "the latest reconciliation run is '$status', not ok"
    return 1
  fi
  for v in "$age" "$reserve" "$liability"; do
    case "$v" in
      ''|*[!0-9-]*)
        echo "the latest reconciliation run is unreadable: '$v'"
        return 1 ;;
    esac
  done
  if [ "$age" -gt 7200 ]; then
    echo "the latest reconciliation run is ${age}s old; wait for one under two hours old"
    return 1
  fi
  local surplus=$((reserve - liability)) need=$((act + xfer))
  echo "the surplus is $surplus micro-USDT; activation may cost up to $need"
  [ "$surplus" -ge "$need" ]
}

main() {
  local c run status age reserve liability act xfer msg resp
  for c in "$PG" "$SIGNER"; do
    if ! docker ps --format '{{.Names}}' | grep -qx "$c"; then
      echo "ABORT: container $c is not running."
      exit 1
    fi
  done

  echo "=== the float, from the signer itself ==="
  docker exec "$SIGNER" sh -c \
    "curl -fsS -H \"Authorization: Bearer \$APP_SIGNER_TOKEN\" http://localhost:8093/internal/xpub" \
    2>/dev/null | sed 's/,/,\n    /g' | sed 's/^/    /' || echo "    (could not read /internal/xpub)"

  echo ""
  echo "=== does the surplus pay for the activation? ==="
  run=$(docker exec "$PG" psql -U treasury -d treasury -tA -F ' ' -c \
    "select status, extract(epoch from now() - run_at)::bigint, custody_reported, ledger_liability
       from reconciliation_runs order by run_at desc limit 1;" 2>/dev/null || true)
  read -r status age reserve liability <<< "$run" || true
  act=$(docker exec "$SIGNER" printenv APP_GASFREE_ACTIVATE_FEE_MAX_USDT 2>/dev/null || true)
  xfer=$(docker exec "$SIGNER" printenv APP_GASFREE_TRANSFER_FEE_MAX_USDT 2>/dev/null || true)
  if msg=$(can_activate "${status:-none}" "${age:-}" "${reserve:-}" "${liability:-}" "$act" "$xfer"); then
    echo "    $msg"
  else
    echo "ABORT: $msg. Nothing was signed."
    exit 1
  fi

  echo ""
  echo "=== activating the GasFree float ==="
  echo "    (no parameters: the float, custody, the amount and the fee cap are the signer's own)"
  resp=$(docker exec "$SIGNER" sh -c \
    "curl -fsS -X POST -H \"Authorization: Bearer \$APP_SIGNER_TOKEN\" -H 'Content-Type: application/json' \
          http://localhost:8093/internal/activate-float") || {
    echo "ABORT: the signer refused or was unreachable."
    echo "  A 500 may follow a real submission: read the float's outbound transfers on chain before"
    echo "  running this again."
    exit 1
  }
  echo "    $resp"
  echo ""
  case "$resp" in
    *'"status":"submitted"'*)
      echo "submitted. The float is activated once this permit runs, usually within a minute; PROBE=gasfree"
      echo "then shows it activated, and redemptions stop answering 'not available yet'. The next"
      echo "reconciliation run shows the surplus lower by the relay's fee, and nothing else."
      ;;
    *'"status":"already_active"'*)
      echo "already activated. Nothing was signed."
      ;;
    *'"status":"float_dry"'*)
      echo "the float cannot pay for its own activation yet. It fills from GasFree sweeps while it holds"
      echo "less than PAYOUT_FLOAT_TARGET_USDT: run this again after a deposit is swept. Nothing was signed."
      exit 1
      ;;
    *'"status":"refused"'*)
      echo "refused before anything was signed -- see the reason above."
      exit 1
      ;;
    *)
      echo "unrecognised response. Nothing is assumed: read tron-signer's logs and the float's transfers"
      echo "on chain before running this again."
      exit 1
      ;;
  esac
}

# Sourced by test-activate-float.sh for can_activate; run, it activates.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
```

Create `.github/workflows/activate-float.yml`:

```yaml
# Activate the GasFree payout float, once (clutch-treasury's GasFree design §4).
#
# The float's first outgoing transfer also pays for deploying its contract, which a redemption's fee
# does not cover. This makes that first transfer — the smallest amount, from the float to custody —
# paid for by the reserve's surplus, and refuses unless the latest reconciliation run shows the
# surplus covers the most it may cost. Until it has run, redemptions answer "not available yet".
#
# It moves money, so it is manual and attributed, like fund-float. It takes no address, no amount and
# no destination, because the endpoint behind it takes none either.

name: Activate the GasFree payout float (stage)

on:
  workflow_dispatch:
    inputs:
      reason:
        description: "Why the float is being activated now. Recorded in this log and nowhere else."
        required: true
      confirm:
        description: 'Type "activate" to confirm moving money'
        required: true
        default: ""

concurrency:
  group: activate-float
  cancel-in-progress: false

jobs:
  activate:
    runs-on: ubuntu-latest
    steps:
      - name: Check the confirmation
        run: |
          if [ "${{ inputs.confirm }}" != "activate" ]; then
            echo "Refusing: confirm was '${{ inputs.confirm }}', expected 'activate'."
            exit 1
          fi

      # Through env, never interpolated into the script body: a reason containing backticks or
      # $(...) would otherwise execute as shell.
      - name: Record who and why
        env:
          ACTOR: ${{ github.actor }}
          REASON: ${{ inputs.reason }}
        run: |
          echo "dispatched by $ACTOR"
          echo "reason: $REASON"

      - name: Activate via SSH
        env:
          SSHPASS: ${{ secrets.STAGE_SSH_PASSWORD }}
          CLUTCH_SSH_HOST: ${{ secrets.STAGE_HOST }}
          CLUTCH_SSH_USER: ${{ secrets.STAGE_USER }}
        run: |
          # The same transport as fund-float.yml: plain ssh, and the script travels as a file.
          set -euo pipefail
          command -v sshpass >/dev/null || { sudo apt-get update -qq && sudo apt-get install -y -qq sshpass; }
          R="/tmp/clutch-ci-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"
          SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=30 -o ServerAliveInterval=30"
          cat > "$RUNNER_TEMP/clutch-remote.sh" <<'CLUTCH_REMOTE_EOF'
          set -euo pipefail
          cd "${{ secrets.STAGE_DEPLOY_PATH }}"
          git pull --ff-only origin main
          bash scripts/activate-float.sh
          CLUTCH_REMOTE_EOF
          sshpass -e ssh $SSH_OPTS "$CLUTCH_SSH_USER@$CLUTCH_SSH_HOST" "cat > $R.sh" < "$RUNNER_TEMP/clutch-remote.sh"
          timeout 10m sshpass -e ssh $SSH_OPTS "$CLUTCH_SSH_USER@$CLUTCH_SSH_HOST" "find /tmp -maxdepth 1 -name 'clutch-ci-*' -mtime +1 -delete 2>/dev/null; bash $R.sh"
```

In `scripts/provision-treasury-secrets.sh`, three edits.

1. Replace:

```bash
XPUB=$(printf '%s' "$JSON" | sed -n 's/.*"account_xpub"[ ]*:[ ]*"\([^"]*\)".*/\1/p')
FEE=$(printf '%s' "$JSON" | sed -n 's/.*"fee_address"[ ]*:[ ]*"\([^"]*\)".*/\1/p')

if [ -z "$XPUB" ] || [ -z "$FEE" ]; then
```

with:

```bash
XPUB=$(printf '%s' "$JSON" | sed -n 's/.*"account_xpub"[ ]*:[ ]*"\([^"]*\)".*/\1/p')
FEE=$(printf '%s' "$JSON" | sed -n 's/.*"fee_address"[ ]*:[ ]*"\([^"]*\)".*/\1/p')
FLOAT=$(printf '%s' "$JSON" | sed -n 's/.*"payout_address"[ ]*:[ ]*"\([^"]*\)".*/\1/p')

if [ -z "$XPUB" ] || [ -z "$FEE" ] || [ -z "$FLOAT" ]; then
```

2. Right after the block that ends:

```bash
else
  echo "DEPOSIT_ACCOUNT_XPUB=$XPUB" >> "$ENV_FILE"
  echo "    DEPOSIT_ACCOUNT_XPUB: written"
fi
```

add:

```bash

# The plain payout float at 2/0: the treasury counts it in the reserve, and derives the GasFree float
# from it (GasFree design §4). Written from the signer's own answer, like the xpub, so the treasury
# cannot count a float the signer does not pay from.
if has PAYOUT_FLOAT_ADDRESS; then
  EXISTING=$(val PAYOUT_FLOAT_ADDRESS)
  if [ "$EXISTING" = "$FLOAT" ]; then
    echo "    PAYOUT_FLOAT_ADDRESS: already set and matches the mnemonic"
  else
    echo "ABORT: PAYOUT_FLOAT_ADDRESS in $ENV_FILE is $EXISTING, but this mnemonic's float at 2/0 is $FLOAT."
    echo "  The treasury would count a float the signer does not pay from. Find which one is wrong first."
    exit 1
  fi
else
  echo "PAYOUT_FLOAT_ADDRESS=$FLOAT" >> "$ENV_FILE"
  echo "    PAYOUT_FLOAT_ADDRESS: written"
fi
```

3. Replace:

```bash
echo "    account_xpub = $XPUB"
echo "    fee_address  = $FEE"
```

with:

```bash
echo "    account_xpub = $XPUB"
echo "    fee_address  = $FEE"
echo "    payout_float = $FLOAT"
```

- [ ] **Step 4: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat: activate the GasFree float from the surplus, and provision its address

activate-float.yml (typed "activate") runs activate-float.sh, which asks
the signer for the float's one-time activation only when the latest
reconciliation run is ok, under two hours old, and shows a surplus of at
least the running signer's two fee maxima (the design's §4). The endpoint
takes no parameters and the script passes none. Provisioning now writes
PAYOUT_FLOAT_ADDRESS from the signer's payout_address, and aborts when an
existing value names another float.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-deploy
git add scripts/activate-float.sh .github/workflows/activate-float.yml scripts/provision-treasury-secrets.sh
git commit -F /d/source/clutch/clutch-treasury/.superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg-deploy.txt
```

Controller: push the branch, then read CI on the draft pull request. Expected: `Test treasury scripts` / `test` succeeds; its log shows `ok` for all eight activation cases by name and `8 passed, 0 failed`, with the earlier self-checks still at `16 passed` and `8 passed`. The workflow itself runs for real in Task 10, step 4.

---

### Task 8: The sweep tool, the alerts and the on-call page learn the rail (clutch-deploy)

**Files:**
- Modify: `scripts/sweep-address.sh`
- Modify: `config/monitoring/prometheus/rules/treasury.yml`
- Modify: `docs/ON-CALL.md`
- Modify: `.github/workflows/test-treasury-scripts.yml` (`bash -n` on `sweep-address.sh`)

**Interfaces:**
- Consumes: the gauges of Task 2; the page texts of Plans 3 and 4.
- Produces: the alerts `TreasuryGasFreeSweepStalled` and `TreasuryRedemptionUnpaid`.

Decisions 4 and 8. No new self-check: `sweep-address.sh` runs only against the live containers, so CI checks its syntax and the task reviewer reads its new branches; `promtool` (the `Check monitoring config` workflow) validates the rules.

- [ ] **Step 1: The sweep tool**

In `scripts/sweep-address.sh`, right before the line:

```bash
echo ""
echo "=== sweeping derivation index $INDEX ==="
```

add:

```bash
# An index with a GasFree account is the sweeper's alone. The signer sweeps an index's GasFree account
# first whenever it holds USDT, and the treasury follows to the chain only the permits it asked for
# itself (sweeper/gasfree_sweep.rs): deposits moved by a permit asked for here would stay "unswept" on
# the books for good. The table exists from migration 0014; before it, the count reads 0.
GASFREE=$(docker exec "$PG" psql -U treasury -d treasury -tAc \
  "select count(*) from gasfree_accounts where derivation_index = $INDEX;" 2>/dev/null | tr -d '[:space:]' || true)
if [ "${GASFREE:-0}" != "0" ]; then
  echo ""
  echo "ABORT: derivation index $INDEX has a GasFree account. The sweeper sweeps its credited deposits"
  echo "  every minute and follows each permit to the chain; a permit asked for here would not be followed."
  echo "  PROBE=sweeper shows what it is waiting on. See docs/ON-CALL.md, \"The GasFree rail\"."
  exit 1
fi
```

and in the final `case "$RESP" in`, right before the last arm (`  *)`), add:

```bash
  *'"status":"below_fee"'*)
    # The plain address is empty, and this index's GasFree account holds no more than the relay's fee:
    # for this address the same as nothing_to_sweep, so the same backfill.
    docker exec "$PG" psql -U treasury -d treasury -c       "update mint_intents set swept_at = now()
        where deposit_address = '$ADDRESS' and swept_at is null and status = 'credited';" 2>&1 | sed 's/^/    /'
    echo "nothing to sweep here: the address is empty, and its GasFree account holds only what the"
    echo "relay's fee would take. No USDT moved."
    ;;
  *'"status":"pending"'*|*'"status":"busy"'*|*'"status":"rejected"'*|*'"status":"halted"'*)
    echo "the signer answered for this index's GasFree account, which it sweeps before the plain"
    echo "address. Nothing was recorded for $ADDRESS. A 'pending' answer means a permit the treasury"
    echo "did not ask for is with the relay: once it runs, the USDT is in custody or the float and"
    echo "still counted, but the treasury will not mark it swept. Read docs/ON-CALL.md before retrying."
    exit 1
    ;;
```

In `.github/workflows/test-treasury-scripts.yml`, add `- "scripts/sweep-address.sh"` to both `paths:` lists, and the line `bash -n scripts/sweep-address.sh` to the `Syntax` step's `run:`.

- [ ] **Step 2: The alerts**

In `config/monitoring/prometheus/rules/treasury.yml`, right after the `TreasurySweepingStalled` rule (after its description, which ends `"TRX float (fee account)" section.`), add:

```yaml

      # A GasFree account is swept on the next one-minute pass after its deposit is credited, so an
      # hour-old unswept GasFree deposit means something stopped it: the relay refusing, or busy for
      # good, GasFree's code changing (the tripwire), a live fee above the maximum, or the signer and
      # the treasury disagreeing about the rail. The deposit is still counted in the reserve.
      - alert: TreasuryGasFreeSweepStalled
        expr: clutch_treasury_oldest_unswept_gasfree_seconds > 3600
        for: 10m
        labels:
          severity: warning
        annotations:
          summary: "A GasFree deposit has not been swept for over an hour"
          description: >-
            The deposit is credited and still counted in the reserve; only the sweep has stopped. Read
            the P1 alerts first (a relay refusal, the tripwire, a maxFee above the hold), then
            PROBE=gasfree for the live fees and GasFree's code.

      # CLT burned, USDT not paid, on either rail. The user has already given up the CLT, so this is the
      # one stall a user feels directly.
      - alert: TreasuryRedemptionUnpaid
        expr: clutch_treasury_oldest_unpaid_redemption_seconds > 7200
        for: 10m
        labels:
          severity: critical
        annotations:
          summary: "A burned redemption has not been paid for over two hours"
          description: >-
            Its CLT is burned and its USDT not yet sent: a dry float, a cap, a payout the treasury
            could not confirm, or the GasFree float not yet activated. Read the P1 alerts and the
            treasury probe. Never return a GasFree payout to payout_pending before the time its page
            names.
```

- [ ] **Step 3: The on-call page**

In `docs/ON-CALL.md`, in the table under `## Alert by alert`, right after the row that starts `` | `TreasurySweepingStalled` ``, add:

```markdown
| `TreasuryGasFreeSweepStalled` | A deposit at a GasFree account has waited over an hour to be swept. Sweeps run every minute. | Read the P1 alerts first: a relay refusal, the tripwire, a maxFee above the hold, or the services' settings disagreeing — see "The GasFree rail" below. The deposit is still counted; nothing is lost while it waits. |
| `TreasuryRedemptionUnpaid` | A redemption's CLT is burned and its USDT has not been paid for over two hours. | Read the P1 alerts and the `treasury` probe. A dry float fills from deposits on the GasFree rail, or from `fund-float.yml` on the TRX rail. **Never** return a GasFree payout to `payout_pending` before the time its page names: the permit may still run. |
```

Then, right before the line `## Things only two people can do`, add:

```markdown
## The GasFree rail

Off unless `.env` sets `GASFREE_NETWORK` (clutch-treasury's `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md`). Its pages, and what to do:

| The page starts with | What it means | What to do |
|---|---|---|
| `the GasFree beacon … now runs 0x…, not the reviewed 0x…` (or `controller`) | GasFree changed the code that holds users' money. GasFree sweeps and **all minting** have stopped, and new users get no GasFree address. | Run `PROBE=gasfree`: it prints the live and the expected implementations. Do not resume minting until someone has read the new code. Then set `GASFREE_EXPECTED_IMPLEMENTATION` (or `GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION`) in `.env` to the live value, deploy, and run `resume-minting.yml`. |
| `the relay refused the sweep of GasFree account …` | The relay would not take the permit, most often because the live fee is above the maximum. The deposit stays at the account, still counted. | `PROBE=gasfree` compares the live fees with the maxima. Raising a maximum covers only deposits minted after the change; nothing is signed for deposits that held less. Change a maximum only in `.env` (all three services read it), and deploy — the deploy runs `check-cap-invariants.sh` first. |
| `a sweep of GasFree account … may now cost up to …, but its deposits held back …` | A maximum was raised after these deposits were minted, so nothing is signed for them. | They wait, still counted. Lower the maximum again when the live fee allows. |
| `the signer set maxFee … above the … held back` | The signer and the treasury read different maxima. The permit is already signed. | This should be impossible with one `.env`: compare the running containers' environments. |
| `the signer answers sweeps with GasFree statuses, but this treasury has GasFree off` / `the signer pays redemptions by GasFree permit, but this treasury has GasFree off` | The services' settings disagree. | Give all three the same settings; `check-cap-invariants.sh` names what is missing. Never return those redemptions to `payout_pending`: the float may already have paid them. |
| `redemption …: payout outcome UNKNOWN … do not return this intent to payout_pending before …` | A GasFree permit may still run until that time. | Wait until the time has passed, then follow the page. |
| `redemptions are not available yet: the GasFree float … has never made a transfer` | The float's one-time activation has not run. | Run `activate-float.yml` once. It refuses unless the reserve's surplus covers the most the activation may cost. |

Rules that do not change:

- **Never remove the `GASFREE_*` settings while any user has a GasFree address**, even after setting `TRANSFER_RAIL=trx`: without them the treasury refuses deposits there, and the orchestrator will not show the address.
- `sweep-address.yml` refuses an index that has a GasFree account. The sweeper sweeps those every minute and follows each permit to the chain.
- `fund-float.yml` fills the plain float at 2/0. GasFree payouts do not use that float, but it stays in the reserve count, so running it is still safe.
```

- [ ] **Step 4: Commit, and the controller confirms**

Overwrite the commit message file with:

```text
feat: the sweep tool, the alerts and the on-call page learn the GasFree rail

sweep-address.sh refuses an index with a GasFree account (the sweeper
follows only the permits it asked for) and reads the new sweep statuses.
TreasuryGasFreeSweepStalled and TreasuryRedemptionUnpaid alert on the
two stall ages the treasury now publishes. ON-CALL.md explains every
GasFree page and what to do.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-deploy
git add scripts/sweep-address.sh config/monitoring/prometheus/rules/treasury.yml docs/ON-CALL.md .github/workflows/test-treasury-scripts.yml
git commit -F /d/source/clutch/clutch-treasury/.superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg-deploy.txt
```

Controller: push the branch, then read CI on the draft pull request. Expected: `Check monitoring config` / `promtool` succeeds, and its `=== alert expressions ===` list shows `- alert: TreasuryGasFreeSweepStalled` with `expr: clutch_treasury_oldest_unswept_gasfree_seconds > 3600` and `- alert: TreasuryRedemptionUnpaid` with `expr: clutch_treasury_oldest_unpaid_redemption_seconds > 7200`; `Test treasury scripts` / `test` succeeds, its `Syntax` step now covering `sweep-address.sh`.

- [ ] **Step 5 (controller): Mark the clutch-deploy pull request ready**

Replace the draft's body with one written to this plan's workspace in clutch-treasury, `.superpowers/sdd/2026-09-25-gasfree-rollout/pr-body-deploy.md`:

```markdown
Plan 4 of 4 for clutch-treasury's `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md`, Tasks 4-8 (plan: clutch-treasury `docs/superpowers/plans/2026-09-25-gasfree-rollout.md`).

**GasFree stays off after this merges**: every new setting is blank by default, and no `.env` sets `GASFREE_NETWORK` yet. Merging deploys stage (this PR touches compose files). What reaches it on purpose:
- **The stage deploy now runs `check-cap-invariants.sh` first** and stops, with the stack as it was, if a relationship is broken.
- **Two new alerts**: `TreasuryGasFreeSweepStalled`, and `TreasuryRedemptionUnpaid`, which also watches the TRX rail.
- The mainnet treasury overlay now requires `PAYOUT_FLOAT_ADDRESS` (the mainnet treasury is not running).

## What it adds

- The GasFree settings mapped into all three treasury services from one set of `.env` names; CI asserts each service receives them.
- `check-cap-invariants.sh`: the redemption fee covers a GasFree payout's relay fee, positive maxima and minimum, a float target above the largest payout, the signer's settings complete, the relay URL matching the network. A self-check with 16 cases.
- `PROBE=gasfree`: the live fees against the maxima, GasFree's code against the expected implementations, and whether the float is activated.
- `activate-float.yml`: the float's one-time activation, refused unless the latest reconciliation run shows the surplus covers it.
- Provisioning writes `PAYOUT_FLOAT_ADDRESS` from the signer. `sweep-address.sh` refuses GasFree indexes. `ON-CALL.md` explains every GasFree page.

## Evidence

`Check monitoring config` (run <id>) and `Test treasury scripts` (run <id>, 16 + 8 + 8 cases by name) on the final head.

Needs clutch-treasury's rollout PR (Tasks 1-3) merged first, so the images this deploy pulls publish the stall ages.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
```

```bash
cd /d/source/clutch/clutch-deploy
gh pr edit feat/gasfree-deploy --repo clutchprotocol/clutch-deploy --body-file /d/source/clutch/clutch-treasury/.superpowers/sdd/2026-09-25-gasfree-rollout/pr-body-deploy.md
gh pr ready feat/gasfree-deploy --repo clutchprotocol/clutch-deploy
```

---

### Task 9: The deposit panel shows the fee and the minimum (clutch-hub)

**Files:**
- Create: `apps/demo/src/utils/depositTerms.js`
- Create: `apps/demo/src/utils/depositTerms.test.js`
- Modify: `apps/demo/src/components/DepositPanel.jsx`

**Interfaces:**
- Consumes: `POST /api/v1/deposits` → `{address, fee_up_to_usdt?, min_deposit_usdt?}` (fact 12).
- Produces: `depositTerms(body) -> {feeUpTo, minimum, sendAtLeast} | null` (USDT strings such as `"2.00"`).

Decision 11, and the spec's §2: "The deposit panel shows the minimum and the fee **before** the user pays", "presented to users as 'fee up to', never as a fixed fee".

- [ ] **Step 0: The branch**

```bash
cd /d/source/clutch/clutch-hub
git checkout main
git pull --ff-only origin main
git checkout -b feat/deposit-panel-gasfree
```

- [ ] **Step 1: A stub and the tests**

Create `apps/demo/src/utils/depositTerms.js`:

```js
// Plan 4 Task 9 stub: written in Step 3.
export function depositTerms() {
  throw new Error('not written yet');
}
```

Create `apps/demo/src/utils/depositTerms.test.js`:

```js
import test from 'node:test';
import assert from 'node:assert/strict';
import { depositTerms } from './depositTerms.js';

test('a plain address has no terms', () => {
  assert.equal(depositTerms({ address: 'TPlainAddress' }), null);
  assert.equal(depositTerms(undefined), null);
});

test('a first deposit to a GasFree address: activation and transfer, then the minimum', () => {
  assert.deepEqual(
    depositTerms({ address: 'TGasFreeAddress', fee_up_to_usdt: 2_000_000, min_deposit_usdt: 1_000_000 }),
    { feeUpTo: '2.00', minimum: '1.00', sendAtLeast: '3.00' },
  );
});

test('a later deposit pays the transfer fee only', () => {
  assert.deepEqual(
    depositTerms({ address: 'TGasFreeAddress', fee_up_to_usdt: 500_000, min_deposit_usdt: 1_000_000 }),
    { feeUpTo: '0.50', minimum: '1.00', sendAtLeast: '1.50' },
  );
});

test('every micro-USDT is kept', () => {
  assert.deepEqual(
    depositTerms({ fee_up_to_usdt: '1234567', min_deposit_usdt: '1' }),
    { feeUpTo: '1.234567', minimum: '0.000001', sendAtLeast: '1.234568' },
  );
});

test('one of the two fields alone is not terms', () => {
  assert.equal(depositTerms({ fee_up_to_usdt: 500_000 }), null);
  assert.equal(depositTerms({ min_deposit_usdt: 1_000_000 }), null);
});
```

- [ ] **Step 2: Commit, open the draft pull request, and the controller confirms red**

Write the commit message file `D:\source\clutch\clutch-treasury\.superpowers\sdd\2026-09-25-gasfree-rollout\commit-msg-hub.txt`:

```text
test(demo): the deposit terms of a GasFree address, against a stub

A GasFree address comes with the fee "up to" and the minimum. The panel
will tell the user to send at least the two together. depositTerms is a
stub in this commit, so the new tests fail by name.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-hub
git add apps/demo/src/utils/depositTerms.js apps/demo/src/utils/depositTerms.test.js
git commit -F /d/source/clutch/clutch-treasury/.superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg-hub.txt
```

Controller: push the branch and open a **draft** pull request (fact 8: never dispatch `docker-publish.yml` on a branch): `gh pr create --repo clutchprotocol/clutch-hub --base main --head feat/deposit-panel-gasfree --draft --title "feat(demo): show the GasFree fee and minimum in the deposit panel" --body "Plan 4 of the GasFree rail, Task 9. Draft until the task is complete."`. Expected in `docker-publish.yml` / `test`: `npm run test --workspace=clutch-hub-demo-app` fails, and its `node:test` output marks these four as failed by name — `a first deposit to a GasFree address: activation and transfer, then the minimum`, `a later deposit pays the transfer fee only`, `every micro-USDT is kept`, `one of the two fields alone is not terms` — and `a plain address has no terms` also fails (the stub throws for every input); the existing demo tests pass.

- [ ] **Step 3: The terms, and the panel**

Replace the whole of `apps/demo/src/utils/depositTerms.js` with:

```js
/**
 * What a user must know before paying a GasFree deposit address (clutch-treasury's GasFree design,
 * §2). The relay's fee is taken from each deposit, and the orchestrator reports the most it can be:
 * "up to" — the configured maximum, never the live fee. After the fee, a deposit must still reach the
 * minimum, or nothing is minted and it waits for a human. So a user must send at least the two
 * together.
 *
 * `body` is the orchestrator's `POST /api/v1/deposits` answer. A plain address carries neither field
 * and gets `null`: nothing is taken from it.
 *
 * Formats here rather than through `money.js`, which imports the SDK: `node --test` loads this module,
 * and no demo test loads the SDK in Node.
 *
 * @param {{ fee_up_to_usdt?: number|string, min_deposit_usdt?: number|string } | undefined} body
 * @returns {{ feeUpTo: string, minimum: string, sendAtLeast: string } | null} amounts in USDT, e.g. "2.00"
 */
export function depositTerms(body) {
  if (body?.fee_up_to_usdt == null || body?.min_deposit_usdt == null) return null;
  const fee = BigInt(body.fee_up_to_usdt);
  const minimum = BigInt(body.min_deposit_usdt);
  return { feeUpTo: usdt(fee), minimum: usdt(minimum), sendAtLeast: usdt(fee + minimum) };
}

/** Micro-USDT as USDT, with trailing zeros trimmed down to two decimals: "2.00", "1.234567". */
function usdt(micro) {
  const whole = micro / 1_000_000n;
  const fraction = (micro % 1_000_000n).toString().padStart(6, '0').replace(/0+$/, '').padEnd(2, '0');
  return `${whole}.${fraction}`;
}
```

In `apps/demo/src/components/DepositPanel.jsx`, four edits.

1. After `import { formatExactUsdt } from '../utils/money';` add:

```js
import { depositTerms } from '../utils/depositTerms';
```

2. After `  const [deposits, setDeposits] = useState([]);` add:

```js
  const [terms, setTerms] = useState(null);
```

3. Replace:

```js
        if (!cancelled) setAddress(body.address);
```

with:

```js
        if (!cancelled) {
          setAddress(body.address);
          setTerms(depositTerms(body));
        }
```

4. Replace the paragraph under the address:

```jsx
          <p style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
            This is your permanent deposit address — send any amount of Nile USDT (TRC-20) to it and
            it is credited automatically, appearing in your balance. Any other token or network sent
            here cannot be recovered.
          </p>
```

with:

```jsx
          {terms ? (
            // A GasFree address: the relay's fee comes out of each deposit (GasFree design §2), so the
            // user is told the most it can be, and what to send for anything to be credited.
            <p style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              This is your permanent deposit address. Send at least{' '}
              <strong>
                {terms.sendAtLeast} {IS_TESTNET ? 'Nile USDT' : 'USDT'} (TRC-20)
              </strong>
              . A network fee of up to {terms.feeUpTo} USDT is taken from each deposit, and what is left
              must be at least {terms.minimum} USDT to be credited. Any other token or network sent here
              cannot be recovered.
            </p>
          ) : (
            <p style={{ fontSize: '0.8rem', color: 'var(--text-secondary)' }}>
              This is your permanent deposit address — send any amount of {IS_TESTNET ? 'Nile USDT' : 'USDT'}{' '}
              (TRC-20) to it and it is credited automatically, appearing in your balance. Any other token or
              network sent here cannot be recovered.
            </p>
          )}
```

Do not change the test file in this step.

- [ ] **Step 4: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(demo): show the GasFree fee and minimum before a user pays

For a GasFree address the orchestrator returns the fee "up to" and the
minimum. The deposit panel now says to send at least the two together,
names the fee as "up to", and says what is left must reach the minimum.
A plain address keeps its text. The network word follows IS_TESTNET, so
the mainnet panel will not say "Nile".

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-hub
git add apps/demo/src/utils/depositTerms.js apps/demo/src/components/DepositPanel.jsx
git commit -F /d/source/clutch/clutch-treasury/.superpowers/sdd/2026-09-25-gasfree-rollout/commit-msg-hub.txt
```

Controller: push the branch, then read CI on the draft pull request. Expected: `docker-publish.yml` / `test` succeeds; the five `depositTerms` tests pass by name and the existing demo and SDK tests pass. `build-and-push` builds the image without pushing it (a pull request). The panel itself has no test (fact 12): the task reviewer reads the JSX, and Task 10 step 3 looks at it on stage. Then mark the pull request ready: `gh pr ready feat/deposit-panel-gasfree --repo clutchprotocol/clutch-hub`, with a body naming the plan, the spec §2 lines above and the CI run.

---

### Task 10: The Nile rollout (controller and maintainer)

No code. The design's §9, "In order, with reconciliation reading OK after **every** step". The controller runs the read-only probes (`inspect-stage.yml`) and reports what they print. **Every step that changes the host's `.env`, deploys, or moves money is the maintainer's, or needs the maintainer's yes in chat for that step.** Record each step's result in the ledger.

After each step, the gate is the same: `PROBE=treasury` shows the latest reconciliation run `ok`, the breaker clear, and no new P1 alert. A failed gate stops the rollout; nothing later is attempted until it is understood.

**Execution notes (2026-09-25, from the final reviews). Where they differ from the steps below, they win.**

- **The gate is `PROBE=sweeper`, not `PROBE=treasury`.** `sweeper` prints the open alerts, the breaker, the last reconciliation runs, the mint intents and (from #104) the redemptions not yet paid. `treasury` prints the settings, the deposit intents and the fee account. Read every `PROBE=treasury` below that looks for a reconciliation run, the breaker, an alert or a redemption as `PROBE=sweeper`.

- **A merge does not move the treasury images.** Since clutch-deploy #103 every image is pinned to `sha-<7>`, and the treasury's move only by hand. Step 1 is, in this order:
  1. Merge the clutch-treasury PR (#55). Its `docker-build-push.yml` publishes the three images as `sha-<first 7 of the merge commit>`.
  2. Run "Deploy stage (VPS)" with `set_images` = `clutch-treasury=sha-X clutch-orchestrator=sha-X clutch-tron-signer=sha-X`. The compose on main does not map the GasFree key into tron-signer yet, so this deploy is safe with the key still in `.env`.
  3. Before merging the clutch-deploy PR (#104), on the host: comment out `GASFREE_API_KEY` and `GASFREE_API_SECRET` in `.env` (#104 maps the key into tron-signer, which turns GasFree on by the key alone and then refuses to start without `GASFREE_NETWORK`); run `bash scripts/check-cap-invariants.sh`, which must end with `All invariants hold`; and check that no redemption waits in `burn_confirmed`, `payout_pending` or `payout_submitted` (#104's `TreasuryRedemptionUnpaid` fires at once on one over 2 hours old). No probe on main shows redemptions yet, so this is read on the host: `docker exec clutch-stage-treasury-postgres-1 psql -U treasury -d treasury -c "select status, count(*) from redemption_intents group by status;"`.
  4. Merge #104. Its deploy runs `check-cap-invariants.sh` first, and its health check now includes tron-signer.
  5. Merge the clutch-hub PR (#20). Its image dispatch carries its own pin.
- **Step 2.2:** the key and the secret are commented out since step 1. Uncomment them in the same edit as the rest of the block, never alone.
- **Stage reconciles every hour** (`APP_RECONCILIATION_INTERVAL_SECS=3600`, from #104), not once a day. A gate that needs a run after its step waits for the next one, at most an hour; `PROBE=treasury` prints each run's time.
- **Step 4.1:** `activate-float.sh` runs its own reconciliation before it decides, and counts what burned, unpaid redemptions are owed as liability (there should be none, per step 2.1). If the surplus is short, the maintainer sends the difference to `CUSTODY_TRON_ADDRESS` and dispatches `activate-float.yml` again; there is no run to wait for.

- [ ] **Step 1: Merge everything, GasFree still off**

1. The maintainer merges the clutch-treasury pull request (Tasks 1-3). Its `docker-build-push.yml` publishes the three images; it does not deploy.
2. The maintainer merges the clutch-deploy pull request (Tasks 4-8). Its compose change starts `deploy-stage.yml`, which pulls the new images too. In the deploy log, `check-cap-invariants.sh` prints `All invariants hold` before the pull. If it stops the deploy, the stack is as it was and the output names the broken relationship.
3. The maintainer merges the clutch-hub pull request (Task 9). Its image publishes and dispatches `deploy-stage`.
4. Controller: `PROBE=containers` (every treasury service up, running the new images), `PROBE=treasury` (the gate), `PROBE=gasfree` (prints `GASFREE_NETWORK is not nile or mainnet in .env: GasFree is off here, nothing to compare.`).

- [ ] **Step 2: Switch the testnet to GasFree**

1. Controller: `PROBE=treasury` — **no redemption may be waiting** (`burn_confirmed`, `payout_pending` or `payout_submitted`). With payouts on GasFree and the float not yet activated, a waiting redemption is retried every 2 seconds (decision 10). If one is waiting, wait until it is paid.
2. The maintainer edits the host's `.env` (SSH): adds the GasFree block of `.env.example`, uncommented, with its Nile values. `GASFREE_API_KEY` and `GASFREE_API_SECRET` are already there (since 2026-09-24). `GASFREE_SERVICE_PROVIDER` is the Nile provider in `PROBE=gasfree`'s provider list (it was `TKtWbdzEq5ss9vTS9kwRhBp5mXmBfBns3E`). `PAYOUT_FLOAT_ADDRESS` is left as it is: it names the plain float, on both rails. Nothing is deployed yet: the services read `.env` only when they are recreated, while the probe reads it now.
3. Controller: `PROBE=gasfree`. Expected: `activateFee: live 1000000, at or below GASFREE_ACTIVATE_FEE_MAX_USDT 1500000 -- OK` and `transferFee: live 300000, at or below GASFREE_TRANSFER_FEE_MAX_USDT 500000 -- OK` (if the live fees moved, the maxima are decided again before going on); `beacon TLtCGmaxH3PbuaF6kbybwteZcHptEdgQGC runs b8eda40b467b45af107f198e94cc2fa1378adf50 -- matches GASFREE_EXPECTED_IMPLEMENTATION` and the same for the controller (this is open question 2, resolved on the live network); `tron-signer names no GasFree float` (not deployed yet).
4. The maintainer dispatches `deploy-stage.yml`. Its log shows `=== the GasFree rail (GASFREE_NETWORK=nile, TRANSFER_RAIL=gasfree) ===` and `All invariants hold`.
5. Controller: `PROBE=containers` (all three up; the signer's log says `GasFree rail on` and no self-test failure), `PROBE=gasfree` (the float line now reads `<F>: NOT activated -- ...`), and the gate: the reserve now counts both floats, so the reconciliation run stays `ok`.

- [ ] **Step 3: One real deposit to a new GasFree address**

1. The maintainer opens the stage app with a **new** wallet — users who already have a deposit address keep their plain one — and opens "Top up with USDT". Expected: an address, and the text `Send at least 3.00 Nile USDT (TRC-20). A network fee of up to 2.00 USDT is taken from each deposit, and what is left must be at least 1.00 USDT to be credited.`
2. The maintainer sends **40 Nile USDT** to it from a Nile wallet they control (funded from the faucet first). Not from the faucet straight to the deposit address: the faucet sends 1,000 USDT, above the 50 USDT per-transaction mint cap, and that deposit would wait for a human.
3. Controller, within about five minutes: `PROBE=treasury` / `PROBE=sweeper` show the deposit credited with **38.00 CLT** minted (2.00 held: the account has not yet transferred), then swept: its row has `swept_at`, into the GasFree float, which is below its 30 USDT target. No P1 page `the relay says … reached the receiver; the permit said …` — so the relay's fee came on top of `value` (open question 1). The fee account's TRX balance (`PROBE=treasury`, "TRX float (fee account)") is unchanged: no TRX was spent.
4. The gate. The surplus (`custody_reported - ledger_liability`) rose by about 0.70 USDT: 2.00 held, 1.30 paid to the relay.

- [ ] **Step 4: Activate the float**

1. Controller: `PROBE=treasury` — the latest reconciliation run is `ok`, under two hours old, and the surplus is at least 2.00 USDT. If it is less, the maintainer sends the difference in Nile USDT to `CUSTODY_TRON_ADDRESS` (a wallet they control) and waits for the next reconciliation run.
2. The maintainer dispatches `activate-float.yml` (reason, and `activate`). Expected: `the surplus is … micro-USDT; activation may cost up to 2000000`, then `submitted.`
3. Controller, a few minutes later: `PROBE=gasfree` shows `<F>: activated`; the gate, with the surplus lower by the relay's fee only (about 1.30 at Nile's fees).

- [ ] **Step 5: One real redemption**

1. The maintainer withdraws **6 CLT** in the stage app, with the same new wallet, to a Nile address they control. With the 1.00 USDT redemption fee, 5.00 USDT is paid.
2. Controller: `PROBE=treasury` — the redemption moves to `payout_submitted` (a GasFree permit from the float) and then `paid`, once the float's transfer of exactly 5.00 USDT to that address is confirmed on chain (this also proves TronGrid reports `from` on the float's transfers). The float fell by 5.00 plus the relay's fee (at most 0.50).
3. The gate.

- [ ] **Step 6: The done check**

The design's §9: "a deposit swept to custody with **zero TRX** in the fee account, custody rising by exactly the minted amount, a redemption paid from the GasFree float, and reconciliation OK throughout." Steps 3 and 5 filled and paid from the float; the float now holds about 31 USDT, above its target, so the next sweep goes to custody.

1. The maintainer notes the custody wallet's USDT balance (nile.tronscan.org), then opens the stage app with **a second new wallet** and sends **10 Nile USDT** to its deposit address. A second new address, because an address swept before keeps the relay's unused margin (its `maxFee` less the real fee, about 0.70 here), and its next sweep moves that margin too — correct, but then custody rises by more than that deposit's mint.
2. Controller: credited with **8.00 CLT** (2.00 held), swept to custody, since the float is above its target; custody's balance rose by exactly **8.00 USDT**; the fee account's TRX balance is unchanged since step 1; the gate.

Record, from these steps, the answers to Plan 3's Nile questions: a never-used account got a relay reply (step 3); the permit's 130-hex signature was accepted (steps 3-6); 1 micro-USDT was accepted for the activation (step 4); `implementation()` reads work on the controller's proxy (step 2); the fee is charged on top of `value` (step 3); TronGrid reports `from` on the float's transfers (step 5). Two can only be seen if they happen, and are recorded if they do: the relay's nonce returning to the chain's after an expired permit, and a documented refusal arriving as body code 400.

---

## After this plan

- **Mainnet.** Its own GasFree API key (developer.gasfree.io), a `PROBE=gasfree` fee reading against mainnet, maxima chosen from that reading, and then the same steps against the mainnet treasury, which has its own blockers first (custody wallet, TronGrid key, funding). `inspect-stage.sh` reads `.env`; a mainnet reading needs it to read `.env.mainnet`.
- **Parked, with their reasons in the ledgers:** a deposit the relay's fee takes whole still pages P1 (Plan 3 M5; the panel now shows the minimum); a `needs_manual` GasFree deposit moved by another deposit's sweep stays unswept on the books; the pre-signing hold read has no status filter (safe: it can only raise the hold); the orchestrator never re-derives a stored GasFree address against the current settings; the missing-`derivation_index` alert pages again when its set of addresses changes; Plan 3's weaker test mocks (the chain stub answering any `getGasFreeAddress`, no failing test with a wrong controller implementation in the orchestrator).
- **The design's open question 4** — who controls the beacon's upgrade (`owner()` reverts) — stays open. The tripwire does not depend on it.
- **Out of scope, as in the spec:** moving users off GasFree addresses; several relays; batching addresses into one permit; any change to how custody is spent.
