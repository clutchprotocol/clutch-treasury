# GasFree Treasury and Orchestrator Implementation Plan (Plan 3 of 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Teach `treasury-service` and `payment-orchestrator` the GasFree rail: hand new users a GasFree address, mint a GasFree deposit less the fee a sweep may pay, sweep it by permit and decide from the chain when that ran, and pay redemptions from the GasFree float.

**Architecture:** Both services read one shared settings type from the `gasfree` crate, and GasFree stays off unless `APP_GASFREE_NETWORK` is set. The treasury decides every number that matters from its own settings and records: it asks the signer which address a deposit went to, sizes the hold from its own record of the account's first sweep, settles sweeps by the owner's nonce on the GasFree controller, and settles payouts by their transfer from the float on chain. The orchestrator computes `G = gasfree(D)` itself and hands it out only while GasFree's code is the reviewed code.

**Tech Stack:** Rust 2021, axum 0.7, sqlx 0.8 (Postgres), reqwest 0.12, the workspace `gasfree` crate (Plans 1 and 2), wiremock 0.6 in tests.

**Spec:** `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md`. This plan implements the treasury's and the orchestrator's halves of §1 (addresses), §2 (minting and the reserve rule), §3 (sweeping), §4 (payouts and the float) and §5 (failures and the tripwire). Plan 2 (`docs/superpowers/plans/2026-09-24-gasfree-signer.md`, merged as #51) built the signer these call. Configuration in `clutch-deploy`, the invariants, the deposit panel and the Nile rollout are Plan 4.

## Global Constraints

- **No local builds.** "Do not run `cargo`, `npm`, `docker`, or any build/test/lint command on this Windows host, and forbid it in every subagent prompt. Verify code by dispatching CI and reading the run log."
- A test counts only when the CI log shows it **by name**. A green badge is not evidence. Pick the CI run by head SHA.
- One implementer per checkout at a time. Commit with `git commit -F <file>`; never put backticks in `-m`.
- "**`tron-signer`'s SWEEP API takes an INDEX and nothing else** — the destination is its own config. Do not add a `to`, `contract`, or `amount` parameter there." (workspace CLAUDE.md) `SweepSigner::sweep(index)` keeps that shape. The new signer reads (`addresses`, `trace`, `float_owner`) move nothing.
- The payout endpoint "can only spend from the payout float at `2/0` — never a deposit address, never custody"; on this rail the float is `F = gasfree(the 2/0 address)` (spec §4). `contract` is never a parameter.
- **Merging changes nothing in production.** Without `APP_GASFREE_NETWORK` the treasury and the orchestrator behave as today, and `APP_TRANSFER_RAIL` defaults to `trx` (spec: "defaulting to `trx`"). Two changes reach the TRX rail on purpose, and their tasks name them: CI runs with `--no-fail-fast` (Task 1), and the sweeper asks once per index per pass and marks every row of a swept index (Task 3).
- The rule: "CLT minted must never exceed the USDT that will reach custody", and "The treasury decides the fee, not the orchestrator" (spec §2).
- "**Never sign a higher `maxFee` than was held back**" (spec §2). The treasury holds `gasfree::fee_to_hold(its own record of the account's first sweep, maxima)`. That is never below the `maxFee` the signer sizes from the chain.
- "The chain is the truth, not the relay." (spec §5) A sweep is done when the owner's nonce on the controller moves past the permit's nonce. A payout is paid when its transfer from the float is confirmed on chain.
- The orchestrator holds no key and no signer token. It computes GasFree addresses with the `gasfree` crate.
- The `gasfree` crate stays "Pure functions over public constants: no key, no network, no async" (its `Cargo.toml`).
- In treasury and orchestrator tests, wiremock stands in for TronGrid and for the signer. Both crates already have it as a dev-dependency.
- A fenced code block in a doc comment must be marked `text`, or `cargo test` compiles it as a doctest.
- Commit messages and the PR body: precise, and the plain word where both words work (workspace CLAUDE.md).

## Facts this plan relies on, checked 2026-09-25

1. **The signer's API, merged in #51 (`92d357e`).**
   - `GET /internal/addresses/:index` answers `{"index", "plain", "gasfree"}`; `gasfree` is null when GasFree is off in the signer.
   - `POST /internal/sweep` adds these statuses: `pending {trace_id, gasfree_address, receiver, value_usdt, max_fee_usdt, nonce, deadline}`, `busy {gasfree_address}`, `rejected {reason, message}`, `halted {reason}`, `below_fee {gasfree_address, balance_usdt, max_fee_usdt}`.
   - `POST /internal/payout` adds `submitted {trace_id, nonce, deadline}`, `float_not_active {float_address}`, and `refused {reason, nonce, deadline}` for a relay refusal after the permit was sent. `refused {reason}` without them is still a refusal before anything was signed.
   - `GET /internal/gasfree/trace/:trace_id` answers `{state, txn_hash, txn_state, txn_amount, txn_total_fee}`: 404 when GasFree is off, 502 when the relay cannot be read.
   - `GET /internal/xpub` adds `payout_gasfree_address` next to `payout_address`.
2. **The signer sizes `maxFee` from the chain** (`fee_to_hold(getcontract shows a contract, maxima)`), sends `value = balance − maxFee`, and signs only when `balance > maxFee`. For one index it sweeps the GasFree account first, and goes to the plain address only when the GasFree account holds 0 or no more than the fee.
3. **The fee is on top of `value`** (Plan 2, fact 1): the receiver gets `value`, and the relay takes at most `maxFee` more. Rollout step 3 (Plan 4) confirms this with test money; this plan also compares the relay's `txn_amount` with `value` after every sweep.
4. **An activated GasFree account has an empty `bytecode`.** Only `{}` from `getcontract` means no contract (Plan 2, fact 2).
5. **Both GasFree proxies are upgradeable.** Nile beacon implementation `b8eda40b467b45af107f198e94cc2fa1378adf50`, Nile controller implementation `2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441` (Plan 2, facts 3 and 4). The tests use these values.
6. **The reserve walk** (`reconciliation.rs`) counts custody, every distinct unswept deposit address and the float, all read on chain, and `judge` compares that with liability. The backing-ratio breaker (`breakers.rs`) uses the ledger's `custody_usdt`, which records what arrived.
7. **One index can have many unswept rows** (permanent addresses). Today `sweep_once` asks the signer once per row and marks one row per sweep. A second row of a swept address then reads a zero balance, is never swept, and stays in the reserve walk.
8. **TronGrid's TRC-20 history has a `from` field**, which nothing in this repo reads yet. The payout check in Task 4 depends on it. If it were missing, every GasFree payout would go to a human after its deadline, which is the safe direction. Rollout step 5 (Plan 4) sees one real payout confirm.
9. **Without `--no-fail-fast`, `cargo test --workspace` stops at the first test binary that fails**, so a red run cannot show failures in the binaries after it.

## Decisions this plan makes where the spec is silent

1. **The hold follows the treasury's own record, not the chain's.** Spec §2 names "whether `G` has contract code on-chain" as the one source of truth, so that the hold and the signer's `maxFee` agree. A sweep asked for some other way can activate an account before a deposit it moved was credited; a hold read from the chain would then keep back one transfer fee for a deposit that also paid activation, and the reserve would be short (Plan 2's final review, I2: 0.80 USDT on Nile's numbers). So an account counts as activated only once the treasury's own sweeper has seen its own first sweep of it run. That record implies contract code, so the hold is never below the signer's `maxFee`.
2. **Every GasFree account the treasury has recorded stays in the reserve count**, also after all its deposits are swept. Spec §2: "the relay's unused margin stays at `G`, still counted". Ceiling: one balance read per account per reconciliation run.
3. **Below the minimum, the amount is lowered and the intent waits for a human** (`needs_manual`, spec §2), so an approval can never mint more than arrived less the fee. A deposit the fee takes whole is rejected: nothing can ever be minted for it. Both stay counted in the reserve through their GasFree account.
4. **A deposit address its own index does not lead to is rejected**, because no sweep of that index could move it. With GasFree on, an intent with no index waits (the 24-hour stuck check pages a human).
5. **A GasFree account is swept as soon as a credited deposit sits there and it holds more than the fee** — no threshold, no age (spec §3, "as soon as it confirms"). The sweeper runs every 60 seconds while GasFree is on.
6. **One signer call per index per pass, on both rails**, and a TRX sweep marks every row of that index (fact 7).
7. **A permit whose `maxFee` is above every hold of the deposits it moves pages a human.** It is already signed; paging is what is left.
8. **GasFree payouts: one permit alive at a time.** A relay refusal is settled from the chain after the permit's deadline. An answer that may have sent a permit, with its nonce unknown, holds the float for 600 seconds, the longest deadline the signer signs.
9. **A payout is paid when its transaction's transfer from the float to the redeemer, of exactly the quoted amount, is confirmed on chain.** The relay's trace only names the transaction.
10. **A new redemption answers 503 while GasFree payouts are on and the float is not activated** (spec §4, "not available yet"), before anything exists to burn against. The orchestrator shows that as "not yet available".
11. **The orchestrator shows "fee up to" from the chain's activation record**, and the larger fee when that cannot be read. In one rare case the treasury holds more than was shown: a sweep ran that the treasury's own sweeper did not see (decision 1). The user then pays at most activation plus one transfer, the larger maximum.
12. **Both implementations are watched** — the beacon's and the controller's — in the treasury's tripwire and in the orchestrator's, as in the signer (Plan 2, decision 1).
13. **Only the orchestrator runs a boot check** (`getGasFreeAddress`), because it is the service that shows users an address it computed. The treasury never computes one; its tripwire, read every pass, fails loudly on a network mismatch because the proxies are not contracts there.
14. **One settings parser for the treasury and the orchestrator**, in the `gasfree` crate, reading the same variable names as the signer. The signer keeps its own parser from #51.
15. **CI runs `cargo test` with `--no-fail-fast`** (fact 9).

---

## File Structure

- `.github/workflows/test.yml` — `--no-fail-fast` (Task 1)
- `crates/gasfree/src/settings.rs` — `Settings` and `load_settings`, shared by the treasury and the orchestrator (Task 1)
- `crates/gasfree/src/lib.rs` — the module, and a `Debug` for `Chain` that leaves out the creation code (Task 1)
- `Cargo.lock` — `gasfree` in the dependency lists of `treasury-service` (Task 2) and `payment-orchestrator` (Task 5)
- `crates/treasury-service/Cargo.toml` — the `gasfree` dependency (Task 2)
- `crates/treasury-service/src/configuration.rs` — `AppConfig.gasfree` (Task 2)
- `crates/treasury-service/src/gasfree_rail.rs` — the treasury's pure GasFree rules: what a deposit may mint (Task 2), the relay's trace and the tripwire (Task 3)
- `crates/treasury-service/src/lib.rs` — `pub mod gasfree_rail;` (Task 2)
- `crates/treasury-service/migrations/0014_gasfree_holds.sql` — `mint_intents.fee_held_usdt`, `gasfree_accounts` (Task 2)
- `crates/treasury-service/migrations/0015_gasfree_sweeps.sql` — the permit in flight, on `gasfree_accounts` (Task 3)
- `crates/treasury-service/migrations/0016_gasfree_payouts.sql` — a payout's permit, on `redemption_intents` (Task 4)
- `crates/treasury-service/src/tron_verifier.rs` — classification and the hold at verification (Task 2); chain reads for the sweeper (Task 3) and for payouts (Task 4)
- `crates/treasury-service/src/sweeper.rs` — the signer's new answers and reads (Tasks 2 and 3), one request per index, the GasFree dispatch (Task 3)
- `crates/treasury-service/src/sweeper/gasfree_sweep.rs` — settling permits, the tripwire read, sweeping one GasFree account (Task 3)
- `crates/treasury-service/src/reconciliation.rs` — GasFree accounts in the reserve walk (Task 3)
- `crates/treasury-service/src/payout.rs` — GasFree payout answers, one permit at a time, settlement (Task 4)
- `crates/treasury-service/src/api.rs` — refuse a redemption while the GasFree float is not activated (Task 4)
- `crates/treasury-service/src/main.rs` — the 60-second sweep pass (Task 3) and GasFree payout settlement (Task 4)
- `crates/payment-orchestrator/Cargo.toml`, `src/configuration.rs`, `src/lib.rs`, `src/main.rs` — the dependency, `OrchConfig.gasfree`, the boot check (Task 5)
- `crates/payment-orchestrator/migrations/0014_gasfree_addresses.sql` — `deposit_addresses.gasfree` (Task 5)
- `crates/payment-orchestrator/src/gasfree_chain.rs` — TronGrid reads, the tripwire, the boot check (Task 5)
- `crates/payment-orchestrator/src/addresses.rs`, `src/api.rs` — issuing `G`, the tripwire in front of it, the fee and the minimum (Task 5)
- `crates/payment-orchestrator/src/redemptions.rs`, `src/treasury_bridge.rs` — "not yet available", and the `needs_manual` page's wording (Task 5)
- Tests: `crates/treasury-service/tests/db_tron_verifier.rs`, `db_sweeper.rs`, `db_reconciliation.rs`, `db_redemption.rs`, plus `db_breakers.rs` and `db_outbox.rs` for the config field; `crates/payment-orchestrator/tests/db_addresses.rs`, `db_deposit_api.rs`, `db_redemptions.rs`, `db_bridge.rs`

Commit message files go in `.superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/`, which git ignores.

## How CI is run in this plan (controller steps)

Every "run CI" step is the controller's, never the implementer's:

```bash
cd /d/source/clutch/clutch-treasury
git push -u origin feat/gasfree-treasury
gh workflow run test.yml --repo clutchprotocol/clutch-treasury --ref feat/gasfree-treasury
SHA=$(git rev-parse HEAD)
gh run list --repo clutchprotocol/clutch-treasury --workflow test.yml --branch feat/gasfree-treasury --event workflow_dispatch --json databaseId,headSha --jq ".[] | select(.headSha==\"$SHA\") | .databaseId"
```

Repeat the last command until it prints an id, then:

```bash
RUN=<the id>
gh run watch "$RUN" --repo clutchprotocol/clutch-treasury --exit-status
gh run view "$RUN" --repo clutchprotocol/clutch-treasury --log \
  | sed -e 's/\x1b\[[0-9;]*m//g' -e 's/\^\[\[[0-9;]*m//g' -e 's/^[^\t]*\t[^\t]*\t[^ ]* //' > "run-$RUN.log"
grep -E "^test .* \.\.\. (ok|FAILED)$|^test result:|^error|could not compile" "run-$RUN.log"
grep -B3 -E -- "--> crates/(gasfree|treasury-service|payment-orchestrator)" "run-$RUN.log" | grep -E "^warning|--> crates/"
```

`gh` prints cargo's colour codes as the literal characters `^[`, and the `sed` removes both forms along with the job, step and time prefix. Unit tests print with their module path (`test gasfree_rail::tests::…`); tests in a `tests/*.rs` file print with their bare name (`test a_first_gasfree_deposit_…`). Warnings are expected in red runs only.

---

### Task 1: One settings type for the treasury and the orchestrator

**Files:**
- Create: `crates/gasfree/src/settings.rs`
- Modify: `crates/gasfree/src/lib.rs`
- Modify: `.github/workflows/test.yml`
- Test: `crates/gasfree/src/settings.rs` (inline `mod tests`), `crates/gasfree/src/lib.rs` (its `mod tests`)

**Interfaces:**
- Consumes: `gasfree::{Chain, NILE, MAINNET}` (Plan 1).
- Produces:
  - `pub struct gasfree::Settings { pub chain: &'static Chain, pub rail: bool, pub activate_fee_max_usdt: i64, pub transfer_fee_max_usdt: i64, pub min_deposit_usdt: i64, pub expected_beacon_implementation: String, pub expected_controller_implementation: String }` (derives `Debug, Clone`)
  - `pub fn gasfree::load_settings(var: impl Fn(&str) -> Option<String>) -> Result<Option<Settings>, String>`
  - `impl std::fmt::Debug for gasfree::Chain` (chain id, controller and beacon; never the creation code)

- [ ] **Step 1: Branch**

```bash
cd /d/source/clutch/clutch-treasury
git checkout main
git pull --ff-only origin main
git checkout -b feat/gasfree-treasury
```

- [ ] **Step 2: Let a red run show every failing test**

In `.github/workflows/test.yml`, replace:

```yaml
      # --test-threads=1: the DB tests share one Postgres and TRUNCATE each other's tables, so
      # parallel threads inside a binary race. Serial is fine at this size, and running them
      # in parallel produces failures that look like logic bugs but are not.
      - name: cargo test
        run: cargo test --workspace -- --test-threads=1
```

with:

```yaml
      # --test-threads=1: the DB tests share one Postgres and TRUNCATE each other's tables, so
      # parallel threads inside a binary race. Serial is fine at this size, and running them
      # in parallel produces failures that look like logic bugs but are not.
      #
      # --no-fail-fast: without it cargo stops at the first test binary that fails, and the
      # binaries after it never run, so a red run could not show which of their tests fail.
      - name: cargo test
        run: cargo test --workspace --no-fail-fast -- --test-threads=1
```

- [ ] **Step 3: Write the settings module and its tests, with a stub body**

Create `crates/gasfree/src/settings.rs`. `load_settings` is `todo!()` on purpose, so CI shows every test failing by name; Step 5 replaces it.

```rust
//! The GasFree settings the treasury and the orchestrator read: the same variables, from the same
//! env file, as the signer.
//!
//! Parsing only, with no network, like the rest of this crate. The signer reads these variables
//! too, plus its relay credentials, in its own loader (`tron-signer`'s `load_gasfree_config`).

use crate::{Chain, MAINNET, NILE};

/// GasFree as the treasury and the orchestrator see it.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Which GasFree deployment: `NILE` or `MAINNET`.
    pub chain: &'static Chain,
    /// `APP_TRANSFER_RAIL=gasfree`: new users are given GasFree addresses, and redemptions are paid
    /// from the GasFree float. False is the TRX rail. The settings still apply then, to every user
    /// who already has a GasFree address, because deposit addresses are permanent (spec §5).
    pub rail: bool,
    /// The most a first transfer may pay for activation, in micro-USDT. Above the live fee.
    pub activate_fee_max_usdt: i64,
    /// The most any transfer may pay the relay, in micro-USDT. Above the live fee.
    pub transfer_fee_max_usdt: i64,
    /// Below this after the fee, a deposit mints nothing and waits for a human (spec §2), in micro-USDT.
    pub min_deposit_usdt: i64,
    /// The beacon's `implementation()` when it was reviewed: 40 lowercase hex, no `0x`.
    pub expected_beacon_implementation: String,
    /// The controller's `implementation()` when it was reviewed: 40 lowercase hex, no `0x`.
    pub expected_controller_implementation: String,
}

/// Read the settings. `Ok(None)` means GasFree is off, which is the default.
///
/// GasFree is on when `APP_GASFREE_NETWORK` is set, and then every other setting is required: a
/// missing one stops the service at boot, not at the first deposit. A blank value counts as unset,
/// because the deploy repo passes an unset optional value as an empty string (`${X:-}`).
pub fn load_settings(var: impl Fn(&str) -> Option<String>) -> Result<Option<Settings>, String> {
    todo!("Task 1 Step 5")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn nile_vars() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("APP_TRANSFER_RAIL", "gasfree"),
            ("APP_GASFREE_NETWORK", "nile"),
            ("APP_GASFREE_ACTIVATE_FEE_MAX_USDT", "1500000"),
            ("APP_GASFREE_TRANSFER_FEE_MAX_USDT", "500000"),
            ("APP_MIN_DEPOSIT_USDT", "1000000"),
            ("APP_GASFREE_EXPECTED_IMPLEMENTATION", "0xB8EDA40B467B45AF107F198E94CC2FA1378ADF50"),
            ("APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION", "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441"),
        ])
    }

    fn load(vars: &HashMap<&'static str, &'static str>) -> Result<Option<Settings>, String> {
        load_settings(|name| vars.get(name).map(|v| v.to_string()))
    }

    #[test]
    fn nothing_set_is_gasfree_off() {
        assert!(load(&HashMap::new()).unwrap().is_none());
    }

    #[test]
    fn the_full_nile_set_is_read_field_by_field() {
        let s = load(&nile_vars()).unwrap().expect("GasFree is on");
        assert_eq!(s.chain.chain_id, NILE.chain_id);
        assert!(s.rail);
        assert_eq!(
            (s.activate_fee_max_usdt, s.transfer_fee_max_usdt, s.min_deposit_usdt),
            (1_500_000, 500_000, 1_000_000)
        );
        assert_eq!(
            s.expected_beacon_implementation, "b8eda40b467b45af107f198e94cc2fa1378adf50",
            "0x and upper case are accepted and normalised"
        );
        assert_eq!(s.expected_controller_implementation, "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441");
    }

    /// Switching back to trx keeps the settings: users who were given a GasFree address keep it.
    #[test]
    fn the_trx_rail_with_gasfree_settings_keeps_them() {
        let mut vars = nile_vars();
        vars.insert("APP_TRANSFER_RAIL", "trx");
        let s = load(&vars).unwrap().expect("the settings stay on");
        assert!(!s.rail);
    }

    #[test]
    fn the_gasfree_rail_without_a_network_is_refused() {
        let vars = HashMap::from([("APP_TRANSFER_RAIL", "gasfree")]);
        assert!(load(&vars).unwrap_err().contains("APP_GASFREE_NETWORK"));
    }

    #[test]
    fn a_missing_zero_or_malformed_setting_is_refused_by_name() {
        for (name, bad) in [
            ("APP_GASFREE_TRANSFER_FEE_MAX_USDT", "0"),
            ("APP_MIN_DEPOSIT_USDT", "1.5"),
            ("APP_GASFREE_ACTIVATE_FEE_MAX_USDT", ""),
            ("APP_GASFREE_EXPECTED_IMPLEMENTATION", "0xb8eda40b"),
            ("APP_GASFREE_NETWORK", "shasta"),
            ("APP_TRANSFER_RAIL", "TRX"),
        ] {
            let mut vars = nile_vars();
            vars.insert(name, bad);
            let err = load(&vars).expect_err(name);
            assert!(err.contains(name), "{name}={bad:?} gave {err:?}");
        }
    }
}
```

In `crates/gasfree/src/lib.rs`, replace:

```rust
mod address;
mod permit;

pub use address::gasfree_address;
pub use permit::{permit_hash, Permit};
```

with:

```rust
mod address;
mod permit;
mod settings;

pub use address::gasfree_address;
pub use permit::{permit_hash, Permit};
pub use settings::{load_settings, Settings};
```

Directly after the closing brace of `impl Chain { … }` (the block holding `fn creation_code`), add:

```rust
impl std::fmt::Debug for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        todo!("Task 1 Step 5")
    }
}
```

In the same file's `#[cfg(test)] mod tests`, after `the_fee_to_hold_includes_activation_only_before_it`, add:

```rust
    /// Services print their config; 997 bytes of creation code in every such line helps nobody.
    #[test]
    fn a_chains_debug_form_names_it_without_its_creation_code() {
        let shown = format!("{:?}", super::NILE);
        assert!(shown.contains("THQGuFzL87ZqhxkgqYEryRAd7gqFqL5rdc"), "{shown}");
        assert!(shown.len() < 300, "the creation code must not be printed: {shown}");
    }
```

- [ ] **Step 4: Commit, and the controller confirms red**

Create `.superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/commit-msg.txt`:

```text
test(gasfree): shared GasFree settings tests, against stubs

The treasury and the orchestrator will read one settings type from the
gasfree crate, under the variable names the signer already uses.
load_settings and Chain's Debug are todo!() in this commit so CI shows
every test failing by name. CI now runs with --no-fail-fast, so a red
run reaches every test binary.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add .github/workflows/test.yml crates/gasfree/src/settings.rs crates/gasfree/src/lib.rs
git commit -F .superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/commit-msg.txt
```

Controller: run CI as described in "How CI is run". Expected: the run fails, and all six fail by name, while every other test binary still runs:

```text
test settings::tests::a_missing_zero_or_malformed_setting_is_refused_by_name ... FAILED
test settings::tests::nothing_set_is_gasfree_off ... FAILED
test settings::tests::the_full_nile_set_is_read_field_by_field ... FAILED
test settings::tests::the_gasfree_rail_without_a_network_is_refused ... FAILED
test settings::tests::the_trx_rail_with_gasfree_settings_keeps_them ... FAILED
test tests::a_chains_debug_form_names_it_without_its_creation_code ... FAILED
```

- [ ] **Step 5: Replace the stubs**

In `crates/gasfree/src/settings.rs`, replace the `load_settings` function with the following, and add the two helpers below it:

```rust
pub fn load_settings(var: impl Fn(&str) -> Option<String>) -> Result<Option<Settings>, String> {
    let optional = |name: &str| var(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let rail = match optional("APP_TRANSFER_RAIL").as_deref() {
        None | Some("trx") => false,
        Some("gasfree") => true,
        Some(other) => return Err(format!("APP_TRANSFER_RAIL must be trx or gasfree, got {other:?}")),
    };
    let Some(network) = optional("APP_GASFREE_NETWORK") else {
        return if rail {
            Err("APP_TRANSFER_RAIL=gasfree needs APP_GASFREE_NETWORK and the other GasFree settings".into())
        } else {
            Ok(None)
        };
    };
    let chain: &'static Chain = match network.as_str() {
        "nile" => &NILE,
        "mainnet" => &MAINNET,
        other => return Err(format!("APP_GASFREE_NETWORK must be nile or mainnet, got {other:?}")),
    };
    let required =
        |name: &str| optional(name).ok_or_else(|| format!("{name} must be set when APP_GASFREE_NETWORK is"));
    let micro_usdt = |name: &str| required(name).and_then(|raw| positive_micro_usdt(name, &raw));
    let implementation = |name: &str| required(name).and_then(|raw| implementation_hex(name, &raw));
    Ok(Some(Settings {
        chain,
        rail,
        activate_fee_max_usdt: micro_usdt("APP_GASFREE_ACTIVATE_FEE_MAX_USDT")?,
        transfer_fee_max_usdt: micro_usdt("APP_GASFREE_TRANSFER_FEE_MAX_USDT")?,
        min_deposit_usdt: micro_usdt("APP_MIN_DEPOSIT_USDT")?,
        expected_beacon_implementation: implementation("APP_GASFREE_EXPECTED_IMPLEMENTATION")?,
        expected_controller_implementation: implementation("APP_GASFREE_EXPECTED_CONTROLLER_IMPLEMENTATION")?,
    }))
}

/// A zero maximum sizes every hold and every permit at nothing: the relay refuses such permits, and
/// every sweep stops while the service looks set up.
fn positive_micro_usdt(name: &str, raw: &str) -> Result<i64, String> {
    match raw.parse::<i64>() {
        Ok(v) if v > 0 => Ok(v),
        _ => Err(format!("{name} must be a positive whole number of micro-USDT, got {raw:?}")),
    }
}

/// `0xA3B0…` or `a3b0…` in, 40 lowercase hex characters out.
fn implementation_hex(name: &str, raw: &str) -> Result<String, String> {
    let hex = raw.trim_start_matches("0x").trim_start_matches("0X").to_ascii_lowercase();
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{name} must be a 20-byte hex address like 0xa3b0edff…, got {raw:?}"));
    }
    Ok(hex)
}
```

In `crates/gasfree/src/lib.rs`, replace the body of `impl std::fmt::Debug for Chain` so it reads:

```rust
impl std::fmt::Debug for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the creation code: it is 997 bytes of hex, and it is tested byte for byte elsewhere.
        f.debug_struct("Chain")
            .field("chain_id", &self.chain_id)
            .field("controller", &self.controller)
            .field("beacon", &self.beacon)
            .finish_non_exhaustive()
    }
}
```

Do not change the tests.

- [ ] **Step 6: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(gasfree): one settings type for the treasury and the orchestrator

GasFree is on when APP_GASFREE_NETWORK is set, and then the maxima, the
minimum deposit and both reviewed implementations are required. The rail
flag is APP_TRANSFER_RAIL=gasfree; switching it back to trx keeps the
settings, because users keep the GasFree address they were given. A
Chain now prints its id and addresses, never its creation code.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/gasfree/src/settings.rs crates/gasfree/src/lib.rs
git commit -F .superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/commit-msg.txt
```

Controller: run CI. Expected: success; the six tests above say `ok` by name, every `test result:` line says `ok`, and there is no warning located in `crates/gasfree`.

---

### Task 2: What a GasFree deposit mints

**Files:**
- Modify: `crates/treasury-service/Cargo.toml`, `Cargo.lock`
- Modify: `crates/treasury-service/src/configuration.rs`
- Modify: `crates/treasury-service/src/lib.rs`
- Create: `crates/treasury-service/src/gasfree_rail.rs`
- Create: `crates/treasury-service/migrations/0014_gasfree_holds.sql`
- Modify: `crates/treasury-service/src/sweeper.rs` (the signer's address read)
- Modify: `crates/treasury-service/src/tron_verifier.rs`
- Modify: `crates/treasury-service/tests/db_tron_verifier.rs` (new tests), and the config literal in `db_breakers.rs`, `db_outbox.rs`, `db_redemption.rs`, `db_sweeper.rs`
- Test: `crates/treasury-service/src/gasfree_rail.rs` (inline `mod tests`), `crates/treasury-service/tests/db_tron_verifier.rs`

**Interfaces:**
- Consumes: `gasfree::{Settings, load_settings, fee_to_hold, gasfree_address, NILE}` (Task 1, Plans 1-2); the signer's `GET /internal/addresses/:index` (fact 1).
- Produces:
  - `AppConfig.gasfree: Option<gasfree::Settings>` (`#[serde(skip)]`, set in `AppConfig::load`)
  - `treasury_service::gasfree_rail::{deposit_mint, DepositMint}`: `pub fn deposit_mint(observed_usdt: i64, fee_usdt: i64, min_deposit_usdt: i64) -> DepositMint`; `pub enum DepositMint { Mint { cap: i64 }, BelowMinimum { cap: i64 }, NothingToMint }` (derives `Debug, PartialEq`)
  - `treasury_service::sweeper::IndexAddresses { pub plain: String, pub gasfree: Option<String> }` (derives `Debug, PartialEq`) and `HttpSigner::addresses(&self, index: i64) -> Result<IndexAddresses, String>`
  - table `gasfree_accounts (derivation_index BIGINT PRIMARY KEY, gasfree_address TEXT NOT NULL UNIQUE, owner_address TEXT NOT NULL, first_transfer_at TIMESTAMPTZ, created_at TIMESTAMPTZ NOT NULL DEFAULT now())`
  - column `mint_intents.fee_held_usdt BIGINT` — set for a deposit to a GasFree account, NULL otherwise

- [ ] **Step 1: The dependency, the setting, the module and the migration**

In `crates/treasury-service/Cargo.toml`, add after the `bs58` dependency (its comment block and its line):

```toml
# The GasFree rail (gasfree_rail.rs): the shared settings, the fee rule and the network constants.
# Pure and key-free: the one copy the signer and the orchestrator use too.
gasfree = { path = "../gasfree" }
```

In `Cargo.lock`, in the `treasury-service` package's `dependencies` list, add ` "gasfree",` between ` "dotenv",` and ` "hex",`.

In `crates/treasury-service/src/configuration.rs`, add as the last field of `AppConfig`, after `pub signer_token: String,`:

```rust
    /// GasFree (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md), read by `load`
    /// from the environment with `gasfree::load_settings`, never from TOML. `None`, the default, is
    /// the TRX rail exactly as before.
    #[serde(skip)]
    pub gasfree: Option<gasfree::Settings>,
```

In `AppConfig::load`, change `let cfg: Self = Config::builder()` to `let mut cfg: Self = Config::builder()`, and replace:

```rust
        assert!(cfg.backing_halt_bps <= cfg.backing_target_bps, "halt bps above target bps");
        Ok(cfg)
```

with:

```rust
        assert!(cfg.backing_halt_bps <= cfg.backing_target_bps, "halt bps above target bps");
        // From the environment only, like the secrets above: the three services read the same
        // variables from one env file (spec §6), and a half-set rail stops the service here.
        cfg.gasfree = gasfree::load_settings(|name| std::env::var(name).ok()).unwrap_or_else(|e| panic!("{e}"));
        Ok(cfg)
```

In `crates/treasury-service/src/lib.rs`, add `pub mod gasfree_rail;` after `pub mod configuration;`.

Create `crates/treasury-service/migrations/0014_gasfree_holds.sql`:

```sql
-- GasFree deposits (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md §2).
--
-- A GasFree sweep pays the relay out of the USDT it moves, so a deposit to a GasFree account mints
-- less than arrived. This is what was held back. NULL for a deposit to a plain address, which pays
-- no fee out of the USDT; the sweeper tells the two kinds of deposit apart by it.
ALTER TABLE mint_intents
    ADD COLUMN fee_held_usdt BIGINT CHECK (fee_held_usdt IS NULL OR fee_held_usdt >= 0);

-- One row per GasFree account the treasury has verified a deposit at.
CREATE TABLE gasfree_accounts (
    derivation_index  BIGINT PRIMARY KEY,
    -- G, where the user pays. Counted in the reserve for as long as the row exists: a sweep leaves
    -- the relay's unused margin there.
    gasfree_address   TEXT NOT NULL UNIQUE,
    -- D, the plain address of the same index: the permit's `user`. Its nonce on the GasFree
    -- controller says whether a permit ran.
    owner_address     TEXT NOT NULL,
    -- When the treasury saw ITS OWN first sweep of this account run. Until then every deposit here
    -- holds back the activation fee as well: a sweep asked for some other way can activate the
    -- account before a deposit it moved was credited.
    first_transfer_at TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

In each of `crates/treasury-service/tests/db_breakers.rs`, `db_outbox.rs`, `db_redemption.rs`, `db_sweeper.rs` and `db_tron_verifier.rs`, the `AppConfig { … }` literal ends with `signer_token: "s".into(),`. Add after that line, in all five:

```rust
        gasfree: None,
```

- [ ] **Step 2: The mint rule, with a stub body, and its tests**

Create `crates/treasury-service/src/gasfree_rail.rs`. `deposit_mint` is `todo!()`; Step 7 replaces it.

```rust
//! The treasury's half of the GasFree rail (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md).
//!
//! A GasFree sweep pays the relay out of the USDT it moves, so a deposit to a GasFree account can
//! mint only what will still be there after that fee (spec §2). The treasury decides the fee from
//! its own settings and its own records, and caps what the orchestrator proposed: the
//! orchestrator is public-facing, and a fee it chose could be zero.

/// What a verified deposit to a GasFree account may mint.
#[derive(Debug, PartialEq)]
pub enum DepositMint {
    /// Mint the intent's amount, lowered to `cap` if it proposed more.
    Mint { cap: i64 },
    /// Below the minimum after the fee: mint nothing, and hold the deposit for a human. `cap` is
    /// still the most that may ever be minted for it.
    BelowMinimum { cap: i64 },
    /// The fee takes all of it: nothing can ever be minted for this deposit.
    NothingToMint,
}

/// `observed − fee` is the most a deposit may mint, and it is judged against the minimum (spec §2).
///
/// `fee` is `gasfree::fee_to_hold` for what the treasury has recorded about the account — never the
/// relay's `active` field, and never the chain's contract record alone (`record_account` in
/// tron_verifier.rs says why).
pub fn deposit_mint(observed_usdt: i64, fee_usdt: i64, min_deposit_usdt: i64) -> DepositMint {
    todo!("Task 2 Step 7")
}

#[cfg(test)]
mod tests {
    use super::{deposit_mint, DepositMint};

    // The maxima the design sizes against Nile's live fees, 1.00 and 0.30 (spec §2).
    const ACTIVATE_MAX: i64 = 1_500_000;
    const TRANSFER_MAX: i64 = 500_000;
    const MIN_DEPOSIT: i64 = 1_000_000;
    const TEN: i64 = 10_000_000;

    #[test]
    fn the_cap_is_what_arrived_less_the_fee() {
        assert_eq!(deposit_mint(TEN, 2_000_000, MIN_DEPOSIT), DepositMint::Mint { cap: 8_000_000 });
    }

    #[test]
    fn below_the_minimum_after_the_fee_is_held_not_minted() {
        assert_eq!(deposit_mint(2_500_000, 2_000_000, MIN_DEPOSIT), DepositMint::BelowMinimum { cap: 500_000 });
        assert_eq!(deposit_mint(3_000_000, 2_000_000, MIN_DEPOSIT), DepositMint::Mint { cap: 1_000_000 }, "exactly the minimum mints");
    }

    #[test]
    fn a_deposit_the_fee_takes_whole_mints_nothing_ever() {
        assert_eq!(deposit_mint(2_000_000, 2_000_000, MIN_DEPOSIT), DepositMint::NothingToMint);
        assert_eq!(deposit_mint(1, 2_000_000, MIN_DEPOSIT), DepositMint::NothingToMint);
    }

    /// Spec §8, "the reserve rule, directly": one GasFree account, custody and the float as the
    /// reserve counts them, and the CLT in circulation. The treasury's side runs the real functions;
    /// the signer and the relay are modelled on what the signer code and a permit allow.
    struct World {
        /// The GasFree account's balance. Counted in the reserve for as long as it holds anything.
        g: i64,
        custody: i64,
        float: i64,
        supply: i64,
        /// What the chain says, which is what the signer sizes `maxFee` from.
        activated_on_chain: bool,
        /// What the treasury recorded, which is what it sizes its hold from.
        treasury_saw_first_transfer: bool,
        activate_live: i64,
        transfer_live: i64,
    }

    impl World {
        /// Nile's live fees on 2026-09-24.
        fn nile() -> Self {
            World {
                g: 0,
                custody: 0,
                float: 0,
                supply: 0,
                activated_on_chain: false,
                treasury_saw_first_transfer: false,
                activate_live: 1_000_000,
                transfer_live: 300_000,
            }
        }

        fn surplus(&self) -> i64 {
            self.g + self.custody + self.float - self.supply
        }

        fn backed(&self, step: &str) {
            assert!(self.surplus() >= 0, "{step}: the reserve is {} below the supply", -self.surplus());
        }

        /// The treasury verifies a deposit of `observed`, the orchestrator having proposed all of it.
        fn verify(&mut self, observed: i64) -> DepositMint {
            let fee = gasfree::fee_to_hold(self.treasury_saw_first_transfer, ACTIVATE_MAX, TRANSFER_MAX);
            let decision = deposit_mint(observed, fee, MIN_DEPOSIT);
            if let DepositMint::Mint { cap } = decision {
                self.supply += cap;
            }
            decision
        }

        /// The signer sweeps the account: `maxFee` from the chain, `value = balance − maxFee`, and
        /// the relay takes its live fee on top of `value`. `by_treasury`: the treasury's own sweeper
        /// asked, so it records the first transfer.
        fn sweep(&mut self, by_treasury: bool, to_float: bool) -> bool {
            let max_fee = gasfree::fee_to_hold(self.activated_on_chain, ACTIVATE_MAX, TRANSFER_MAX);
            let live = if self.activated_on_chain { self.transfer_live } else { self.activate_live + self.transfer_live };
            // The signer's below_fee, and the relay refusing a permit whose maxFee is under its fee.
            if self.g <= max_fee || live > max_fee {
                return false;
            }
            let value = self.g - max_fee;
            if to_float {
                self.float += value;
            } else {
                self.custody += value;
            }
            self.g -= value + live;
            self.activated_on_chain = true;
            if by_treasury {
                self.treasury_saw_first_transfer = true;
            }
            true
        }
    }

    #[test]
    fn the_reserve_covers_supply_through_a_first_and_a_later_deposit() {
        let mut w = World::nile();
        w.g += TEN;
        assert_eq!(w.verify(TEN), DepositMint::Mint { cap: 8_000_000 }, "a first deposit holds activation and one transfer");
        w.backed("first deposit verified");
        assert!(w.sweep(true, false));
        w.backed("first deposit swept");
        assert_eq!(w.g, 700_000, "the relay's unused margin stays in the account, still counted");

        w.g += TEN;
        assert_eq!(w.verify(TEN), DepositMint::Mint { cap: 9_500_000 }, "after its own first sweep ran, one transfer fee");
        w.backed("later deposit verified");
        assert!(w.sweep(true, false));
        w.backed("later deposit swept");
    }

    #[test]
    fn two_deposits_swept_together_over_reserve_by_one_fee() {
        let mut w = World::nile();
        w.g += TEN;
        w.verify(TEN);
        w.g += TEN;
        w.verify(TEN);
        assert!(w.sweep(true, false));
        w.backed("two deposits, one sweep");
        assert_eq!(w.surplus(), 2_000_000 + 700_000, "one extra hold, plus the sweep's unused margin");
    }

    /// Plan 2's final review (I2): a sweep that runs before the deposit it moves is credited
    /// activates the account first. A treasury that trusted the chain's record would then hold one
    /// transfer fee for a deposit that paid activation too.
    #[test]
    fn a_sweep_before_the_credit_is_covered_because_the_hold_follows_the_treasurys_own_record() {
        let mut w = World::nile();
        w.g += TEN;
        assert!(w.sweep(false, false), "someone reached the signer before the credit");
        assert_eq!(w.verify(TEN), DepositMint::Mint { cap: 8_000_000 }, "the treasury did not see that sweep, so it holds both fees");
        w.backed("early sweep, then the credit");

        let from_the_chain = deposit_mint(TEN, gasfree::fee_to_hold(true, ACTIVATE_MAX, TRANSFER_MAX), MIN_DEPOSIT);
        assert_eq!(from_the_chain, DepositMint::Mint { cap: 9_500_000 });
        assert!(w.g + w.custody < 9_500_000, "sized from the chain, the same deposit would be under-reserved");
    }

    #[test]
    fn a_live_fee_above_the_maximum_moves_nothing_and_leaves_the_deposit_counted() {
        let mut w = World::nile();
        w.activate_live = 2_000_000;
        w.g += TEN;
        w.verify(TEN);
        assert!(!w.sweep(true, false), "the relay refuses a permit whose maxFee is below its fee");
        w.backed("refused sweep");
        assert_eq!(w.g, TEN, "the deposit stays where it is, still counted");
    }

    #[test]
    fn below_the_minimum_nothing_is_minted_and_the_deposit_still_counts() {
        let mut w = World::nile();
        w.g += 2_500_000;
        assert_eq!(w.verify(2_500_000), DepositMint::BelowMinimum { cap: 500_000 });
        assert_eq!(w.supply, 0);
        w.backed("below the minimum");
    }

    /// Spec §4 and invariant 1: a redemption keeps back REDEMPTION_FEE_USDT, and the relay's fee for
    /// the payout comes out of the (activated) float on top of what the redeemer gets.
    #[test]
    fn a_redemption_paid_from_the_gasfree_float_keeps_the_reserve_whole() {
        let mut w = World::nile();
        w.g += TEN;
        w.verify(TEN);
        assert!(w.sweep(true, true), "below its target, the float takes the sweep");
        w.backed("float filled");
        let redemption_fee = TRANSFER_MAX; // the smallest the invariant allows
        let burned = 5_000_000;
        w.supply -= burned;
        w.float -= (burned - redemption_fee) + w.transfer_live;
        w.backed("redemption paid");
    }

    /// Spec §4: the float's activation is paid from surplus, and the workflow that asks for it
    /// refuses unless the reserve leads supply by activation plus one transfer, at their maxima.
    #[test]
    fn the_floats_activation_is_covered_by_the_surplus_the_workflow_requires() {
        let mut w = World::nile();
        w.g += TEN;
        w.verify(TEN);
        assert!(w.sweep(true, true));
        let gate = ACTIVATE_MAX + TRANSFER_MAX;
        assert!(w.surplus() < gate, "one first deposit's margin is not enough, so the workflow refuses");
        w.custody += gate - w.surplus(); // later deposits' margins, or a top-up
        // One micro-USDT from the float to custody, and the relay's live fees for a first transfer.
        w.float -= 1 + w.activate_live + w.transfer_live;
        w.custody += 1;
        w.backed("float activated from surplus");
    }
}
```

- [ ] **Step 3: The signer's address read, as a stub**

In `crates/treasury-service/src/sweeper.rs`, add after the closing brace of `impl SweepSigner for HttpSigner { … }`:

```rust
/// Both deposit addresses of an index, as the signer derives them.
#[derive(Debug, PartialEq)]
pub struct IndexAddresses {
    pub plain: String,
    /// `None` when GasFree is off in the signer.
    pub gasfree: Option<String>,
}

impl HttpSigner {
    /// `GET /internal/addresses/:index`: both addresses of an index. Moves nothing. The treasury
    /// asks here, where the addresses are derived, instead of trusting the address the
    /// orchestrator reported, because whether a deposit pays a relay fee must not be the
    /// orchestrator's choice.
    pub async fn addresses(&self, index: i64) -> Result<IndexAddresses, String> {
        todo!("Task 2 Step 7")
    }
}
```

- [ ] **Step 4: The verifier's decision, with a stub where GasFree decides**

In `crates/treasury-service/src/tron_verifier.rs`:

a) In `struct DepositBackedIntent`, add after the `deposit_tx_id` field:

```rust
    /// The BIP32 index the orchestrator reported for `deposit_address`. With GasFree on, the signer
    /// is asked which of that index's two addresses the deposit was paid to.
    derivation_index: Option<i64>,
```

b) In `verify_once`, replace:

```rust
    let client = TronClient::new(config.trongrid_url.clone(), config.trongrid_api_key.clone());
    let rows: Vec<(Uuid, Option<i64>, Option<String>, Option<String>, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(
            "SELECT id, expected_amount_usdt, deposit_address, deposit_tx_id, created_at FROM mint_intents
             WHERE status = 'created' AND client_ref IS NOT NULL
             ORDER BY created_at",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;

    let intents: Vec<DepositBackedIntent> = rows
        .into_iter()
        .map(|(id, expected_amount_usdt, deposit_address, deposit_tx_id, created_at)| DepositBackedIntent {
            id,
            expected_amount_usdt,
            deposit_address,
            deposit_tx_id,
            created_at,
        })
        .collect();
```

with:

```rust
    let client = TronClient::new(config.trongrid_url.clone(), config.trongrid_api_key.clone());
    // With GasFree on, the signer is asked which of an index's two addresses a deposit went to.
    let signer = crate::sweeper::HttpSigner {
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?,
        base_url: config.signer_url.clone(),
        token: config.signer_token.clone(),
    };
    let rows: Vec<(Uuid, Option<i64>, Option<String>, Option<String>, chrono::DateTime<chrono::Utc>, Option<i64>)> =
        sqlx::query_as(
            "SELECT id, expected_amount_usdt, deposit_address, deposit_tx_id, created_at, derivation_index
             FROM mint_intents
             WHERE status = 'created' AND client_ref IS NOT NULL
             ORDER BY created_at",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;

    let intents: Vec<DepositBackedIntent> = rows
        .into_iter()
        .map(|(id, expected_amount_usdt, deposit_address, deposit_tx_id, created_at, derivation_index)| {
            DepositBackedIntent { id, expected_amount_usdt, deposit_address, deposit_tx_id, created_at, derivation_index }
        })
        .collect();
```

c) In the `Evidence::Pass` arm of `verify_once`, replace:

```rust
                if !may_approve {
                    continue;
                }
                match approve_and_ledger(pool, intent.id, observed_amount_usdt, &tx_id).await {
                    Ok(true) => approved += 1,
                    Ok(false) => {} // already approved by a prior run — rerun-safe no-op
                    Err(e) => {
                        alert(pool, "p1", "tron_verifier", &format!("intent {}: approval write failed: {e}", intent.id)).await;
                    }
                }
```

with:

```rust
                if !may_approve {
                    continue;
                }
                let verdict = match &config.gasfree {
                    // The TRX rail: the whole deposit reaches custody, so the intent mints what it says.
                    None => Verdict::Approve { cap: None },
                    Some(settings) => gasfree_verdict(pool, settings, &signer, intent, observed_amount_usdt).await,
                };
                match verdict {
                    Verdict::Approve { cap } => {
                        match approve_and_ledger(pool, intent.id, observed_amount_usdt, &tx_id, "approved", cap).await {
                            Ok(true) => approved += 1,
                            Ok(false) => {} // already approved by a prior run — rerun-safe no-op
                            Err(e) => {
                                alert(pool, "p1", "tron_verifier", &format!("intent {}: approval write failed: {e}", intent.id)).await;
                            }
                        }
                    }
                    Verdict::Hold { cap, reason } => {
                        match approve_and_ledger(pool, intent.id, observed_amount_usdt, &tx_id, "needs_manual", Some(cap)).await {
                            Ok(true) => {
                                alert(pool, "p1", "tron_verifier", &format!("mint intent {} needs manual review: {reason}", intent.id)).await;
                            }
                            Ok(false) => {}
                            Err(e) => {
                                alert(pool, "p1", "tron_verifier", &format!("intent {}: recording it for manual review failed: {e}", intent.id)).await;
                            }
                        }
                    }
                    Verdict::Reject(reason) => {
                        if let Err(e) = reject_and_alert(pool, intent.id, &reason).await {
                            alert(pool, "p1", "tron_verifier", &format!("intent {}: reject write failed: {e}", intent.id)).await;
                        }
                    }
                    // Nothing decided: the intent stays `created`, the next tick retries it, and the
                    // stuck-intent sweep pages a human if it stays that way.
                    Verdict::Wait(reason) => {
                        tracing::debug!(intent_id = %intent.id, reason, "tron_verifier: verified, not yet approvable; retrying next tick");
                    }
                }
```

d) Replace the whole `approve_and_ledger` function (its doc comment and body) with:

```rust
/// Approve (or hold for a human) + `verified_at` + outbox row + `ledger::append_event("custody_deposit", ...)`
/// in ONE transaction (brief's exactly-once requirement). `WHERE status = 'created'` on the UPDATE
/// is what makes a rerun after a crash safe: if a prior run already moved this intent on, this
/// UPDATE affects zero rows and the function returns Ok(false) — no second `approved_by` write, no
/// second outbox row, no second ledger event. If the crash was BEFORE commit, the whole transaction
/// never happened and this rerun performs the one real attempt.
///
/// `status` is `approved`, or `needs_manual` for a GasFree deposit below the minimum: recorded and
/// ledgered the same way, with no outbox row, so nothing mints until a human approves it.
///
/// `cap` is set for a deposit to a GasFree account (spec §2): the mint drops to it when the
/// orchestrator proposed more, and what was held back is stored beside it.
async fn approve_and_ledger(
    pool: &PgPool,
    intent_id: Uuid,
    observed_amount_usdt: i64,
    tx_id: &str,
    status: &str,
    cap: Option<i64>,
) -> Result<bool, String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

    // Postgres evaluates every SET expression against the row as it was, so both CASE lines read the
    // amount the orchestrator proposed.
    let updated = sqlx::query(
        "UPDATE mint_intents
         SET status = $2, approved_by = 'tron-verifier', verified_at = now(), updated_at = now(),
             amount_clt = CASE WHEN $3::BIGINT IS NULL THEN amount_clt ELSE LEAST(amount_clt, $3::BIGINT) END,
             fee_held_usdt = CASE WHEN $3::BIGINT IS NULL THEN NULL
                                  ELSE $4::BIGINT - LEAST(amount_clt, $3::BIGINT) END
         WHERE id = $1 AND status = 'created'",
    )
    .bind(intent_id)
    .bind(status)
    .bind(cap)
    .bind(observed_amount_usdt)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    if updated.rows_affected() == 0 {
        // Already moved on by a prior run — rerun-safe no-op. Roll back rather than commit an empty
        // transaction; either is harmless here, but rollback makes "nothing happened" true of the DB
        // log too.
        tx.rollback().await.map_err(|e| e.to_string())?;
        return Ok(false);
    }

    if status == "approved" {
        sqlx::query("INSERT INTO chain_outbox (intent_id) VALUES ($1)")
            .bind(intent_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }

    // Custody enters the ledger ONLY here — independent verification is what makes the
    // backing-ratio breaker meaningful (brief). OBSERVED amount, not amount_clt: everything that
    // arrived, so the ledger agrees with real custody. A GasFree deposit's fee is spent only when it
    // is swept, and reconciliation judges the reserve on chain.
    sqlx::query(
        "INSERT INTO treasury_events (kind, amount_clt, amount_usdt, intent_id, chain_tx_hash, description)
         VALUES ('custody_deposit', 0, $1, $2, $3, 'TronGrid-verified deposit')
         ON CONFLICT (intent_id, kind) WHERE intent_id IS NOT NULL DO NOTHING",
    )
    .bind(observed_amount_usdt)
    .bind(intent_id)
    .bind(tx_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(true)
}
```

e) Add after `approve_and_ledger`:

```rust
/// What a verified deposit may do.
enum Verdict {
    /// Approve and mint. `cap`, for a GasFree account, is the most it may mint.
    Approve { cap: Option<i64> },
    /// Record the deposit, mint nothing, and page a human.
    Hold { cap: i64, reason: String },
    Reject(String),
    /// Nothing is decided this pass; the intent stays `created`.
    Wait(String),
}

/// With GasFree on: a plain address mints as before, a GasFree account holds back the relay's fee
/// (spec §2), and an address its own index does not lead to is one no sweep could ever move.
async fn gasfree_verdict(
    pool: &PgPool,
    settings: &gasfree::Settings,
    signer: &crate::sweeper::HttpSigner,
    intent: &DepositBackedIntent,
    observed_amount_usdt: i64,
) -> Verdict {
    todo!("Task 2 Step 7")
}
```

- [ ] **Step 5: The verifier tests**

In `crates/treasury-service/tests/db_tron_verifier.rs`, in `pool()`, change the TRUNCATE to:

```rust
        "TRUNCATE treasury_events, mint_intents, chain_outbox, reconciliation_runs, alerts, gasfree_accounts RESTART IDENTITY CASCADE",
```

Add at the end of the file:

```rust
// --- GasFree (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md §2) ---
//
// With GasFree on, a deposit at a user's GasFree account mints what arrived less the most a sweep
// may pay the relay. The signer, which derives both addresses of an index, says which one the
// deposit was paid to; the same wiremock server stands in for it and for TronGrid.

/// The plain address of index 7 in these tests. The treasury never derives it.
const PLAIN_7: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";
/// A valid address that is neither of index 7's.
const NOT_OF_7: &str = "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx";

fn gasfree_7() -> String {
    gasfree::gasfree_address(&gasfree::NILE, PLAIN_7).unwrap()
}

/// Nile, with the maxima the design sizes against Nile's live fees (1.00 and 0.30).
fn nile() -> gasfree::Settings {
    gasfree::Settings {
        chain: &gasfree::NILE,
        rail: true,
        activate_fee_max_usdt: 1_500_000,
        transfer_fee_max_usdt: 500_000,
        min_deposit_usdt: 1_000_000,
        expected_beacon_implementation: "b8eda40b467b45af107f198e94cc2fa1378adf50".into(),
        expected_controller_implementation: "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441".into(),
    }
}

fn gasfree_config(server: &MockServer) -> treasury_service::configuration::AppConfig {
    let mut config = test_config(server.uri());
    config.signer_url = server.uri();
    config.gasfree = Some(nile());
    config
}

/// The signer's `GET /internal/addresses/7`.
async fn mount_addresses_of_7(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/internal/addresses/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"index": 7, "plain": PLAIN_7, "gasfree": gasfree_7()})))
        .mount(server)
        .await;
}

/// A confirmed deposit of `observed` at `address`, as TronGrid shows it.
async fn mount_confirmed_deposit(server: &MockServer, address: &str, tx_id: &str, observed: i64) {
    mount_trc20_list_for(server, address, vec![trc20_transfer_json(tx_id, address, USDT, &observed.to_string())]).await;
    mount_transaction_confirmed(server, tx_id, true).await;
}

/// A `created` deposit-backed intent at `address`, with the index the orchestrator reported.
async fn seed_indexed_intent(pool: &PgPool, amount_clt: i64, expected: i64, tx_id: &str, address: &str, index: Option<i64>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents
            (id, beneficiary, amount_clt, credit_ref, created_by, client_ref, deposit_tx_id, expected_amount_usdt,
             deposit_address, derivation_index)
         VALUES ($1, 'TBeneficiary1111111111111111111111', $2, $3, 'orchestrator', $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(amount_clt)
    .bind(format!("ref-{id}"))
    .bind(format!("client-{id}"))
    .bind(tx_id)
    .bind(expected)
    .bind(address)
    .bind(index)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// (amount_clt, fee_held_usdt) of an intent.
async fn minted_and_held(pool: &PgPool, id: Uuid) -> (i64, Option<i64>) {
    sqlx::query_as("SELECT amount_clt, fee_held_usdt FROM mint_intents WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn outbox_rows(pool: &PgPool, id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM chain_outbox WHERE intent_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn gasfree_accounts(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM gasfree_accounts").fetch_one(pool).await.unwrap()
}

/// A first deposit holds back activation and one transfer: 10.00 in, 8.00 minted, 2.00 held.
#[tokio::test]
async fn a_first_gasfree_deposit_mints_what_arrived_less_activation_and_one_transfer() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_addresses_of_7(&server).await;
    mount_confirmed_deposit(&server, &gasfree_7(), "tx-gf-first", 10_000_000).await;
    let id = seed_indexed_intent(&pool, 10_000_000, 10_000_000, "tx-gf-first", &gasfree_7(), Some(7)).await;

    let approved = treasury_service::tron_verifier::verify_once(&pool, &gasfree_config(&server)).await.unwrap();

    assert_eq!(approved, 1);
    assert_eq!(status_of(&pool, id).await, "approved");
    assert_eq!(minted_and_held(&pool, id).await, (8_000_000, Some(2_000_000)));
    assert_eq!(outbox_rows(&pool, id).await, 1);
    let custody: i64 = sqlx::query_scalar(
        "SELECT amount_usdt FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_deposit'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(custody, 10_000_000, "the ledger records everything that arrived");
    let (owner, seen): (String, bool) = sqlx::query_as(
        "SELECT owner_address, first_transfer_at IS NOT NULL FROM gasfree_accounts WHERE derivation_index = 7",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(owner, PLAIN_7, "the permit's owner is recorded for the sweeper's nonce check");
    assert!(!seen, "no sweep of this account has run yet");
}

/// Once the treasury's own sweeper saw a sweep of this account run, one transfer fee is enough.
#[tokio::test]
async fn once_the_treasury_saw_its_own_first_sweep_one_transfer_fee_is_held() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_addresses_of_7(&server).await;
    mount_confirmed_deposit(&server, &gasfree_7(), "tx-gf-later", 10_000_000).await;
    sqlx::query(
        "INSERT INTO gasfree_accounts (derivation_index, gasfree_address, owner_address, first_transfer_at)
         VALUES (7, $1, $2, now())",
    )
    .bind(gasfree_7())
    .bind(PLAIN_7)
    .execute(&pool)
    .await
    .unwrap();
    let id = seed_indexed_intent(&pool, 10_000_000, 10_000_000, "tx-gf-later", &gasfree_7(), Some(7)).await;

    treasury_service::tron_verifier::verify_once(&pool, &gasfree_config(&server)).await.unwrap();

    assert_eq!(minted_and_held(&pool, id).await, (9_500_000, Some(500_000)));
}

/// The orchestrator proposes the mint and cannot raise it: anything above what arrived less the fee is capped.
#[tokio::test]
async fn the_orchestrator_cannot_mint_more_than_arrived_less_the_fee() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_addresses_of_7(&server).await;
    mount_confirmed_deposit(&server, &gasfree_7(), "tx-gf-greedy", 10_000_000).await;
    let id = seed_indexed_intent(&pool, 25_000_000, 10_000_000, "tx-gf-greedy", &gasfree_7(), Some(7)).await;

    treasury_service::tron_verifier::verify_once(&pool, &gasfree_config(&server)).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "approved");
    assert_eq!(minted_and_held(&pool, id).await, (8_000_000, Some(2_000_000)));
}

/// Spec §2: below the minimum after the fee, nothing is minted and a human decides. The amount is
/// lowered first, so an approval can never mint more than arrived less the fee.
#[tokio::test]
async fn a_gasfree_deposit_below_the_minimum_is_held_for_a_human_and_mints_nothing() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_addresses_of_7(&server).await;
    mount_confirmed_deposit(&server, &gasfree_7(), "tx-gf-small", 2_500_000).await;
    let id = seed_indexed_intent(&pool, 2_500_000, 2_500_000, "tx-gf-small", &gasfree_7(), Some(7)).await;

    let approved = treasury_service::tron_verifier::verify_once(&pool, &gasfree_config(&server)).await.unwrap();

    assert_eq!(approved, 0);
    assert_eq!(status_of(&pool, id).await, "needs_manual");
    assert_eq!(minted_and_held(&pool, id).await, (500_000, Some(2_000_000)));
    assert_eq!(outbox_rows(&pool, id).await, 0, "nothing mints until a human approves it");
    let custody: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_deposit'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(custody, 1, "the deposit arrived, so the ledger records it");
    let pages: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM alerts WHERE severity = 'p1' AND source = 'tron_verifier' AND message LIKE '%below the minimum%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pages, 1);
}

#[tokio::test]
async fn a_gasfree_deposit_the_fee_takes_whole_is_rejected() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_addresses_of_7(&server).await;
    mount_confirmed_deposit(&server, &gasfree_7(), "tx-gf-dust", 2_000_000).await;
    let id = seed_indexed_intent(&pool, 2_000_000, 2_000_000, "tx-gf-dust", &gasfree_7(), Some(7)).await;

    treasury_service::tron_verifier::verify_once(&pool, &gasfree_config(&server)).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "rejected");
    assert_eq!(outbox_rows(&pool, id).await, 0);
    assert_eq!(gasfree_accounts(&pool).await, 1, "the account is recorded, so its USDT is still counted in the reserve");
}

/// With GasFree on, a user who still has a plain address is minted in full, as before.
#[tokio::test]
async fn a_deposit_to_the_plain_address_mints_in_full_with_gasfree_on() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_addresses_of_7(&server).await;
    mount_confirmed_deposit(&server, PLAIN_7, "tx-plain", 10_000_000).await;
    let id = seed_indexed_intent(&pool, 10_000_000, 10_000_000, "tx-plain", PLAIN_7, Some(7)).await;

    treasury_service::tron_verifier::verify_once(&pool, &gasfree_config(&server)).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "approved");
    assert_eq!(minted_and_held(&pool, id).await, (10_000_000, None));
    assert_eq!(gasfree_accounts(&pool).await, 0);
}

#[tokio::test]
async fn an_address_its_index_does_not_lead_to_is_rejected() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_addresses_of_7(&server).await;
    mount_confirmed_deposit(&server, NOT_OF_7, "tx-stray", 10_000_000).await;
    let id = seed_indexed_intent(&pool, 10_000_000, 10_000_000, "tx-stray", NOT_OF_7, Some(7)).await;

    treasury_service::tron_verifier::verify_once(&pool, &gasfree_config(&server)).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "rejected");
    assert_eq!(outbox_rows(&pool, id).await, 0);
}

#[tokio::test]
async fn while_the_signer_cannot_classify_it_the_deposit_waits() {
    let pool = pool().await;
    let server = MockServer::start().await; // no /internal/addresses mock: the signer answers 404
    mount_confirmed_deposit(&server, &gasfree_7(), "tx-gf-wait", 10_000_000).await;
    let id = seed_indexed_intent(&pool, 10_000_000, 10_000_000, "tx-gf-wait", &gasfree_7(), Some(7)).await;

    treasury_service::tron_verifier::verify_once(&pool, &gasfree_config(&server)).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "created", "retried on the next tick, never approved blind");
    assert_eq!(outbox_rows(&pool, id).await, 0);
}

#[tokio::test]
async fn with_gasfree_on_a_deposit_without_an_index_waits() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_addresses_of_7(&server).await;
    mount_confirmed_deposit(&server, &gasfree_7(), "tx-gf-noindex", 10_000_000).await;
    let id = seed_indexed_intent(&pool, 10_000_000, 10_000_000, "tx-gf-noindex", &gasfree_7(), None).await;

    treasury_service::tron_verifier::verify_once(&pool, &gasfree_config(&server)).await.unwrap();

    assert_eq!(status_of(&pool, id).await, "created");
    let asked = server.received_requests().await.unwrap_or_default();
    assert!(
        asked.iter().all(|r| !r.url.path().starts_with("/internal/addresses")),
        "without an index there is nothing to ask the signer"
    );
}
```

- [ ] **Step 6: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test(treasury): what a GasFree deposit mints, against stubs

With GasFree on, the verifier asks the signer which of an index's two
addresses a deposit went to, and a deposit to a GasFree account mints
what arrived less the most a sweep may pay the relay. The rule, the
signer read and the verdict are todo!() in this commit so CI shows each
new test failing by name. The TRX path already runs through the new
approve_and_ledger and must stay green.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add Cargo.lock crates/treasury-service/Cargo.toml crates/treasury-service/src/configuration.rs crates/treasury-service/src/lib.rs crates/treasury-service/src/gasfree_rail.rs crates/treasury-service/migrations/0014_gasfree_holds.sql crates/treasury-service/src/sweeper.rs crates/treasury-service/src/tron_verifier.rs crates/treasury-service/tests
git commit -F .superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/commit-msg.txt
```

Controller: run CI. Expected: the run fails; these 19 fail by name, and every test that existed before this task is `ok`:

```text
test gasfree_rail::tests::a_deposit_the_fee_takes_whole_mints_nothing_ever ... FAILED
test gasfree_rail::tests::a_live_fee_above_the_maximum_moves_nothing_and_leaves_the_deposit_counted ... FAILED
test gasfree_rail::tests::a_redemption_paid_from_the_gasfree_float_keeps_the_reserve_whole ... FAILED
test gasfree_rail::tests::a_sweep_before_the_credit_is_covered_because_the_hold_follows_the_treasurys_own_record ... FAILED
test gasfree_rail::tests::below_the_minimum_after_the_fee_is_held_not_minted ... FAILED
test gasfree_rail::tests::below_the_minimum_nothing_is_minted_and_the_deposit_still_counts ... FAILED
test gasfree_rail::tests::the_cap_is_what_arrived_less_the_fee ... FAILED
test gasfree_rail::tests::the_floats_activation_is_covered_by_the_surplus_the_workflow_requires ... FAILED
test gasfree_rail::tests::the_reserve_covers_supply_through_a_first_and_a_later_deposit ... FAILED
test gasfree_rail::tests::two_deposits_swept_together_over_reserve_by_one_fee ... FAILED
test a_deposit_to_the_plain_address_mints_in_full_with_gasfree_on ... FAILED
test a_first_gasfree_deposit_mints_what_arrived_less_activation_and_one_transfer ... FAILED
test a_gasfree_deposit_below_the_minimum_is_held_for_a_human_and_mints_nothing ... FAILED
test a_gasfree_deposit_the_fee_takes_whole_is_rejected ... FAILED
test an_address_its_index_does_not_lead_to_is_rejected ... FAILED
test once_the_treasury_saw_its_own_first_sweep_one_transfer_fee_is_held ... FAILED
test the_orchestrator_cannot_mint_more_than_arrived_less_the_fee ... FAILED
test while_the_signer_cannot_classify_it_the_deposit_waits ... FAILED
test with_gasfree_on_a_deposit_without_an_index_waits ... FAILED
```

- [ ] **Step 7: Replace the stubs**

In `crates/treasury-service/src/gasfree_rail.rs`, replace the body of `deposit_mint`:

```rust
pub fn deposit_mint(observed_usdt: i64, fee_usdt: i64, min_deposit_usdt: i64) -> DepositMint {
    let cap = observed_usdt.saturating_sub(fee_usdt);
    if cap < 1 {
        DepositMint::NothingToMint
    } else if cap < min_deposit_usdt {
        DepositMint::BelowMinimum { cap }
    } else {
        DepositMint::Mint { cap }
    }
}
```

In `crates/treasury-service/src/sweeper.rs`, replace the body of `HttpSigner::addresses`:

```rust
    pub async fn addresses(&self, index: i64) -> Result<IndexAddresses, String> {
        let resp = self
            .http
            .get(format!("{}/internal/addresses/{index}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("signer unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("signer returned {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| format!("unreadable signer response: {e}"))?;
        if body["index"].as_i64() != Some(index) {
            return Err(format!("asked for index {index}, the signer answered for {}", body["index"]));
        }
        let plain = body["plain"]
            .as_str()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| format!("the signer named no plain address: {body}"))?;
        let gasfree = match &body["gasfree"] {
            serde_json::Value::Null => None,
            serde_json::Value::String(a) if !a.is_empty() => Some(a.clone()),
            other => return Err(format!("the signer gave an unreadable GasFree address: {other}")),
        };
        Ok(IndexAddresses { plain: plain.to_string(), gasfree })
    }
```

In `crates/treasury-service/src/tron_verifier.rs`, add `use crate::gasfree_rail::{deposit_mint, DepositMint};` after `use crate::configuration::AppConfig;`, replace the body of `gasfree_verdict`, and add `record_account` after it:

```rust
async fn gasfree_verdict(
    pool: &PgPool,
    settings: &gasfree::Settings,
    signer: &crate::sweeper::HttpSigner,
    intent: &DepositBackedIntent,
    observed_amount_usdt: i64,
) -> Verdict {
    let Some(index) = intent.derivation_index else {
        return Verdict::Wait(format!(
            "intent {} has no derivation_index, so its address cannot be classified; with GasFree on it is never approved without one",
            intent.id
        ));
    };
    // `evaluate` already refused an intent with no address, so a Pass always has one.
    let deposit_address = intent.deposit_address.as_deref().unwrap_or_default();
    let addresses = match signer.addresses(index).await {
        Ok(a) => a,
        Err(e) => return Verdict::Wait(format!("asking the signer for the addresses of index {index}: {e}")),
    };
    if deposit_address == addresses.plain {
        return Verdict::Approve { cap: None };
    }
    if addresses.gasfree.as_deref() != Some(deposit_address) {
        return Verdict::Reject(format!(
            "deposit address {deposit_address} is neither the plain address {} nor the GasFree account {:?} of index \
             {index}: no sweep of that index could move it",
            addresses.plain, addresses.gasfree
        ));
    }
    let seen_first_transfer = match record_account(pool, index, deposit_address, &addresses.plain).await {
        Ok(seen) => seen,
        Err(e) => return Verdict::Wait(e),
    };
    let fee = gasfree::fee_to_hold(seen_first_transfer, settings.activate_fee_max_usdt, settings.transfer_fee_max_usdt);
    match deposit_mint(observed_amount_usdt, fee, settings.min_deposit_usdt) {
        DepositMint::Mint { cap } => Verdict::Approve { cap: Some(cap) },
        DepositMint::BelowMinimum { cap } => Verdict::Hold {
            cap,
            reason: format!(
                "{observed_amount_usdt} micro-USDT arrived at GasFree account {deposit_address}. After the {fee} held \
                 for the relay's fee, {cap} is below the minimum deposit of {}, so nothing was minted. The USDT stays \
                 at the account, counted in the reserve, and is swept with the user's next deposit. Approving this \
                 intent (mint-intent-approve) mints at most {cap}.",
                settings.min_deposit_usdt
            ),
        },
        DepositMint::NothingToMint => Verdict::Reject(format!(
            "{observed_amount_usdt} micro-USDT arrived at GasFree account {deposit_address}, no more than the {fee} a \
             sweep may pay the relay, so nothing can ever be minted for it. The USDT stays at the account, counted in \
             the reserve, and is swept with the user's next deposit."
        )),
    }
}

/// Record the GasFree account of `index`, and say whether the treasury has seen its own first sweep
/// of it run.
///
/// That record, not the chain's contract record, decides the hold (spec §2; Plan 2's final review,
/// I2). A sweep asked for some other way can activate the account before a deposit it moved was
/// credited. That permit paid activation, and a hold sized from the chain would have kept back only
/// one transfer fee. The record implies contract code, so a hold sized from it is never below the
/// `maxFee` the signer sizes from the chain.
async fn record_account(pool: &PgPool, index: i64, gasfree_address: &str, owner: &str) -> Result<bool, String> {
    sqlx::query(
        "INSERT INTO gasfree_accounts (derivation_index, gasfree_address, owner_address)
         VALUES ($1, $2, $3) ON CONFLICT (derivation_index) DO NOTHING",
    )
    .bind(index)
    .bind(gasfree_address)
    .bind(owner)
    .execute(pool)
    .await
    .map_err(|e| format!("recording GasFree account {gasfree_address}: {e}"))?;
    let seen: Option<bool> = sqlx::query_scalar(
        "SELECT first_transfer_at IS NOT NULL FROM gasfree_accounts
         WHERE derivation_index = $1 AND gasfree_address = $2",
    )
    .bind(index)
    .bind(gasfree_address)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("reading GasFree account {gasfree_address}: {e}"))?;
    seen.ok_or_else(|| format!("index {index} is recorded with a different GasFree account than {gasfree_address}"))
}
```

Do not change the tests.

- [ ] **Step 8: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(treasury): a GasFree deposit mints what arrived less the fee

With GasFree on, the verifier asks the signer which address of the
deposit's index was paid. A plain address mints as before. A GasFree
account mints observed minus fee_to_hold, where activation counts as
paid only after the treasury's own sweeper has seen its first sweep of
that account run; the orchestrator's proposal is capped, and what was
held back is stored. Below the minimum the intent waits for a human
with its amount already lowered; a deposit the fee takes whole, or an
address its index does not lead to, is rejected.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/treasury-service/src/gasfree_rail.rs crates/treasury-service/src/sweeper.rs crates/treasury-service/src/tron_verifier.rs
git commit -F .superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/commit-msg.txt
```

Controller: run CI. Expected: success; the 19 tests above say `ok` by name, every `test result:` line says `ok`, and there is no warning located in `crates/treasury-service` or `crates/gasfree`.

---

### Task 3: The GasFree sweep in the treasury

**Files:**
- Create: `crates/treasury-service/migrations/0015_gasfree_sweeps.sql`
- Modify: `crates/treasury-service/src/gasfree_rail.rs` (the trace, the tripwire)
- Modify: `crates/treasury-service/src/tron_verifier.rs` (three chain reads)
- Modify: `crates/treasury-service/src/sweeper.rs`
- Create: `crates/treasury-service/src/sweeper/gasfree_sweep.rs`
- Modify: `crates/treasury-service/src/reconciliation.rs`
- Modify: `crates/treasury-service/src/main.rs`
- Test: `crates/treasury-service/tests/db_sweeper.rs`, `crates/treasury-service/tests/db_reconciliation.rs`

**Interfaces:**
- Consumes: `AppConfig.gasfree`, `gasfree_accounts`, `mint_intents.fee_held_usdt` (Task 2); the signer's sweep statuses and trace route (fact 1).
- Produces:
  - `SignerReply` derives `Clone`, and gains `Pending { trace_id: String, gasfree_address: String, value_usdt: i64, max_fee_usdt: i64, nonce: u64, deadline: u64 }`, `Busy`, `Rejected { reason: String }`, `Halted { reason: String }`, `BelowFee`
  - `SweepSigner::trace(&self, trace_id: &str) -> Result<Trace, String>`, a default method answering `Err`; `HttpSigner` implements it
  - `treasury_service::gasfree_rail::{Trace, parse_trace, fetch_trace, code_changed}`: `pub struct Trace { pub state: String, pub txn_hash: Option<String>, pub txn_amount: Option<i64> }` (derives `Debug, Clone, PartialEq`); `pub fn parse_trace(body: &serde_json::Value) -> Result<Trace, String>`; `pub async fn fetch_trace(http: &reqwest::Client, base_url: &str, token: &str, trace_id: &str) -> Result<Trace, String>`; `pub async fn code_changed(client: &TronClient, settings: &gasfree::Settings) -> Result<Option<String>, String>`
  - `TronClient::{view_word, gasfree_nonce, implementation}`: `pub async fn view_word(&self, contract: &str, selector: &str, parameter: Option<&str>) -> Result<String, String>`, `pub async fn gasfree_nonce(&self, controller: &str, owner: &str) -> Result<u64, String>`, `pub async fn implementation(&self, proxy: &str) -> Result<String, String>`
  - columns `gasfree_accounts.pending_trace_id, pending_nonce, pending_deadline, pending_value_usdt, pending_requested_at` — all NULL, or all set
  - `reconciliation::unswept_addresses` also returns every `gasfree_accounts.gasfree_address`

- [ ] **Step 1: The migration**

Create `crates/treasury-service/migrations/0015_gasfree_sweeps.sql`:

```sql
-- A GasFree sweep in flight (spec §3, §5). A permit returns a trace id, not a finished transfer. The
-- sweep is done when the controller's nonces(owner) moves past the permit's nonce — the chain, not
-- the relay — and after its deadline it can no longer run. All NULL, or all set.
ALTER TABLE gasfree_accounts
    ADD COLUMN pending_trace_id     TEXT,
    ADD COLUMN pending_nonce        BIGINT,
    ADD COLUMN pending_deadline     BIGINT,
    ADD COLUMN pending_value_usdt   BIGINT,
    ADD COLUMN pending_requested_at TIMESTAMPTZ,
    ADD CONSTRAINT gasfree_pending_all_or_nothing CHECK (
        (pending_nonce IS NULL) = (pending_trace_id IS NULL)
        AND (pending_nonce IS NULL) = (pending_deadline IS NULL)
        AND (pending_nonce IS NULL) = (pending_value_usdt IS NULL)
        AND (pending_nonce IS NULL) = (pending_requested_at IS NULL));
```

- [ ] **Step 2: The new answers, the trace and the reads, as stubs**

In `crates/treasury-service/src/gasfree_rail.rs`, add before `#[cfg(test)]`:

```rust
/// The relay's record of a permit, as the signer passes it on (`GET /internal/gasfree/trace/:id`).
/// Only ever a cross-check, or a pointer to a transaction: the chain decides what happened.
#[derive(Debug, Clone, PartialEq)]
pub struct Trace {
    /// `WAITING`, `INPROGRESS`, `CONFIRMING`, `SUCCEED` or `FAILED`.
    pub state: String,
    pub txn_hash: Option<String>,
    /// What reached the receiver.
    pub txn_amount: Option<i64>,
}

/// The signer's trace reply, read field by field.
pub fn parse_trace(body: &serde_json::Value) -> Result<Trace, String> {
    todo!("Task 3 Step 5")
}

/// `GET /internal/gasfree/trace/:trace_id` on the signer. The signer holds the relay's API key;
/// this service does not.
pub async fn fetch_trace(http: &reqwest::Client, base_url: &str, token: &str, trace_id: &str) -> Result<Trace, String> {
    todo!("Task 3 Step 5")
}

/// Why GasFree must stop, when its code is not the reviewed code; `None` when it is (spec §5). Both
/// proxies: the beacon behind every GasFree account, and the controller that moves money out of
/// them, which is upgradeable too.
pub async fn code_changed(
    client: &crate::tron_verifier::TronClient,
    settings: &gasfree::Settings,
) -> Result<Option<String>, String> {
    todo!("Task 3 Step 5")
}
```

In `crates/treasury-service/src/tron_verifier.rs`, add inside `impl TronClient`, after `get_custody_balance`:

```rust
    /// The first 32-byte word a view function returns, as 64 lowercase hex characters.
    pub async fn view_word(&self, contract: &str, selector: &str, parameter: Option<&str>) -> Result<String, String> {
        todo!("Task 3 Step 5")
    }

    /// The next nonce the GasFree controller accepts from `owner`: the chain's count of the permits
    /// it has run for them. Moving past a permit's nonce is how a GasFree sweep is known to have run.
    pub async fn gasfree_nonce(&self, controller: &str, owner: &str) -> Result<u64, String> {
        todo!("Task 3 Step 5")
    }

    /// An upgradeable proxy's `implementation()`, as 40 lowercase hex characters.
    pub async fn implementation(&self, proxy: &str) -> Result<String, String> {
        todo!("Task 3 Step 5")
    }
```

In `crates/treasury-service/src/sweeper.rs`:

a) Replace `use crate::ledger::alert;` with:

```rust
use crate::gasfree_rail::Trace;
use crate::ledger::{alert, alert_once};

mod gasfree_sweep;
```

b) Change `#[derive(Debug, PartialEq)]` on `pub enum SignerReply` to `#[derive(Debug, Clone, PartialEq)]`, and add these variants after `Failed(String),`:

```rust
    /// A GasFree permit is with the relay (spec §3). NOT swept yet: the chain decides that, when the
    /// controller's `nonces(owner)` moves past `nonce`. After `deadline` it can no longer run.
    Pending {
        trace_id: String,
        gasfree_address: String,
        value_usdt: i64,
        max_fee_usdt: i64,
        nonce: u64,
        deadline: u64,
    },
    /// A transfer from this GasFree account is already in flight. Nothing was signed.
    Busy,
    /// The relay refused the permit. The deposit stays where it is, still counted, and a human
    /// decides — never with a higher maxFee than was held back.
    Rejected { reason: String },
    /// The signer signs nothing for GasFree until a human acts: GasFree's code changed, or the relay
    /// and the signer disagree about an address.
    Halted { reason: String },
    /// The GasFree account holds no more than a sweep may pay the relay.
    BelowFee,
```

c) Replace the `SweepSigner` trait with:

```rust
/// The signer boundary, as a trait so the worker is testable without a live service or real keys.
#[async_trait::async_trait]
pub trait SweepSigner: Send + Sync {
    /// Sweep the address at `index`. Deliberately takes ONLY an index: the destination is the
    /// signer's own config, so nothing here can redirect funds. Do not widen this signature.
    async fn sweep(&self, index: i64) -> SignerReply;

    /// The relay's record of a GasFree permit. Moves nothing.
    async fn trace(&self, _trace_id: &str) -> Result<Trace, String> {
        Err("this signer cannot read GasFree traces".into())
    }
}
```

d) In `impl SweepSigner for HttpSigner`, add after the `sweep` method:

```rust
    async fn trace(&self, trace_id: &str) -> Result<Trace, String> {
        crate::gasfree_rail::fetch_trace(&self.http, &self.base_url, &self.token, trace_id).await
    }
```

e) In `sweep_once`, the `match signer.sweep(index).await` ends with the `SignerReply::Failed(e) => { … }` arm. Add after that arm, as the last one:

```rust
            other => tracing::warn!("sweeper: {address} (index {index}) got {other:?}"),
```

Create `crates/treasury-service/src/sweeper/gasfree_sweep.rs`:

```rust
//! Sweeping deposits out of GasFree accounts (spec §3), and deciding from the chain when a sweep ran
//! (spec §5).
//!
//! A GasFree sweep ends as a permit with the relay, not a finished transfer. The account is swept
//! when the controller's `nonces(owner)` moves past the permit's nonce. The account's balance never
//! says it: the relay's unused margin stays there, so it never falls to zero.

use sqlx::PgPool;

use super::SweepSigner;
use crate::configuration::AppConfig;
use crate::tron_verifier::TronClient;

/// The tripwire, read once per pass (spec §5). `false`: no GasFree sweep is asked for this pass.
pub(super) async fn code_unchanged(pool: &PgPool, settings: &gasfree::Settings, client: &TronClient) -> bool {
    todo!("Task 3 Step 5")
}

/// Every permit in flight: did it run, and can it still run?
pub(super) async fn settle(pool: &PgPool, settings: &gasfree::Settings, client: &TronClient, signer: &dyn SweepSigner) {
    todo!("Task 3 Step 5")
}

/// One GasFree account holding credited deposits: ask for a permit if one could move anything.
pub(super) async fn sweep_account(
    pool: &PgPool,
    config: &AppConfig,
    settings: &gasfree::Settings,
    client: &TronClient,
    signer: &dyn SweepSigner,
    index: i64,
    address: &str,
) {
    todo!("Task 3 Step 5")
}
```

- [ ] **Step 3: The sweeper tests**

In `crates/treasury-service/tests/db_sweeper.rs`:

a) Change the imports at the top so they read:

```rust
use async_trait::async_trait;
use sqlx::PgPool;
use treasury_service::gasfree_rail::Trace;
use treasury_service::sweeper::{self, SignerReply, SweepSigner};
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
```

b) In `pool()`, change the TRUNCATE to:

```rust
    sqlx::query("TRUNCATE treasury_events, mint_intents, chain_outbox, alerts, gasfree_accounts RESTART IDENTITY CASCADE")
```

c) Replace `struct FakeSigner`, its `impl FakeSigner` and its `impl SweepSigner for FakeSigner` with:

```rust
/// Records which indices it was asked to sweep, so a test can assert the worker did NOT reach for
/// an address it had no business touching.
struct FakeSigner {
    reply: SignerReply,
    asked: std::sync::Mutex<Vec<i64>>,
    /// What the relay's record of any permit says reached the receiver. `None`: not known yet.
    trace_amount: Option<i64>,
}

impl FakeSigner {
    fn new(reply: SignerReply) -> Self {
        Self { reply, asked: std::sync::Mutex::new(Vec::new()), trace_amount: None }
    }
    fn asked(&self) -> Vec<i64> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl SweepSigner for FakeSigner {
    async fn sweep(&self, index: i64) -> SignerReply {
        self.asked.lock().unwrap().push(index);
        self.reply.clone()
    }

    async fn trace(&self, _trace_id: &str) -> Result<Trace, String> {
        Ok(Trace { state: "SUCCEED".into(), txn_hash: Some("tx-permit".into()), txn_amount: self.trace_amount })
    }
}
```

d) Add at the end of the file:

```rust
/// Several deposits at one address are one sweep of its whole balance: the signer is asked once,
/// and every row is marked. Asking per row swept the address, then read it empty for the next row,
/// which then stayed unswept for good.
#[tokio::test]
async fn every_deposit_at_one_index_is_asked_for_once_and_marked_together() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, 500_000_000).await;
    let first = seed(&pool, "credited", 7, 2).await;
    let second = seed(&pool, "credited", 7, 1).await;

    let signer = FakeSigner::new(SignerReply::Swept { tx_id: "tx-both".into() });
    sweeper::sweep_once(&pool, &config(server.uri(), 100_000_000), &tron(&server), &signer).await;

    assert_eq!(signer.asked(), vec![7], "one request for the index, not one per deposit");
    assert!(swept_at(&pool, first).await.is_some());
    assert!(swept_at(&pool, second).await.is_some());
}

// --- GasFree (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md §3, §5) ---

/// Plain addresses of indexes 7 and 8: the permits' owners, as the verifier recorded them.
const OWNER_7: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";
const OWNER_8: &str = "TRhVWK5XEDkQBDevcdCWW7RW51aRncty4W";
/// Nile's reviewed implementations (Plan 2, facts 3 and 4).
const BEACON_OK: &str = "b8eda40b467b45af107f198e94cc2fa1378adf50";
const CONTROLLER_OK: &str = "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441";
const TRACE: &str = "6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60";

fn account_of(owner: &str) -> String {
    gasfree::gasfree_address(&gasfree::NILE, owner).unwrap()
}

fn nile() -> gasfree::Settings {
    gasfree::Settings {
        chain: &gasfree::NILE,
        rail: true,
        activate_fee_max_usdt: 1_500_000,
        transfer_fee_max_usdt: 500_000,
        min_deposit_usdt: 1_000_000,
        expected_beacon_implementation: BEACON_OK.into(),
        expected_controller_implementation: CONTROLLER_OK.into(),
    }
}

/// GasFree on, with the TRX threshold left at $100: a GasFree account ignores it.
fn gasfree_config(server: &MockServer) -> treasury_service::configuration::AppConfig {
    let mut config = config(server.uri(), 100_000_000);
    config.gasfree = Some(nile());
    config
}

fn in_three_minutes() -> u64 {
    (chrono::Utc::now().timestamp() + 180) as u64
}

fn pending(value_usdt: i64, max_fee_usdt: i64, nonce: u64) -> SignerReply {
    SignerReply::Pending {
        trace_id: TRACE.into(),
        gasfree_address: account_of(OWNER_7),
        value_usdt,
        max_fee_usdt,
        nonce,
        deadline: in_three_minutes(),
    }
}

/// `balanceOf` for one address only.
async fn mount_usdt_balance(server: &MockServer, address: &str, micro_usdt: i64) {
    Mock::given(method("POST"))
        .and(path("/wallet/triggerconstantcontract"))
        .and(body_string_contains("balanceOf(address)"))
        .and(body_string_contains(address))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"constant_result": [format!("{micro_usdt:064x}")]})),
        )
        .mount(server)
        .await;
}

/// Both GasFree proxies' `implementation()`: the beacon as given, the controller as reviewed.
async fn mount_implementations(server: &MockServer, beacon: &str) {
    for (proxy, implementation) in [(gasfree::NILE.beacon, beacon), (gasfree::NILE.controller, CONTROLLER_OK)] {
        Mock::given(method("POST"))
            .and(path("/wallet/triggerconstantcontract"))
            .and(body_string_contains("implementation()"))
            .and(body_string_contains(proxy))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"constant_result": [format!("{implementation:0>64}")]})),
            )
            .mount(server)
            .await;
    }
}

/// TronGrid as a GasFree pass reads it: one account's USDT balance, the owner's nonce, and both
/// proxies. Each mock answers its own call only.
async fn mount_gasfree_chain(server: &MockServer, account: &str, balance: i64, nonce: u64, beacon: &str) {
    mount_usdt_balance(server, account, balance).await;
    Mock::given(method("POST"))
        .and(path("/wallet/triggerconstantcontract"))
        .and(body_string_contains("nonces(address)"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"constant_result": [format!("{nonce:064x}")]})),
        )
        .mount(server)
        .await;
    mount_implementations(server, beacon).await;
}

/// A credited deposit at a GasFree account, as the verifier leaves one: its fee held back, verified a minute ago.
async fn seed_gasfree_deposit(pool: &PgPool, index: i64, account: &str, fee_held_usdt: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents
            (id, beneficiary, amount_clt, credit_ref, created_by, approved_by, client_ref,
             expected_amount_usdt, deposit_address, derivation_index, status, verified_at, fee_held_usdt)
         VALUES ($1, 'TBene', $2, $3, 'orchestrator', 'tron-verifier', $4, 10000000, $5, $6,
                 'credited', now() - interval '1 minute', $7)",
    )
    .bind(id)
    .bind(10_000_000 - fee_held_usdt)
    .bind(format!("ref-{id}"))
    .bind(format!("client-{id}"))
    .bind(account)
    .bind(index)
    .bind(fee_held_usdt)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn seed_account(pool: &PgPool, index: i64, owner: &str, seen_first_transfer: bool) {
    sqlx::query(
        "INSERT INTO gasfree_accounts (derivation_index, gasfree_address, owner_address, first_transfer_at)
         VALUES ($1, $2, $3, CASE WHEN $4 THEN now() END)",
    )
    .bind(index)
    .bind(account_of(owner))
    .bind(owner)
    .bind(seen_first_transfer)
    .execute(pool)
    .await
    .unwrap();
}

/// A permit already in flight for index 7.
async fn seed_pending_7(pool: &PgPool, trace_id: &str, nonce: i64, deadline: i64, value_usdt: i64) {
    sqlx::query(
        "UPDATE gasfree_accounts
            SET pending_trace_id = $1, pending_nonce = $2, pending_deadline = $3,
                pending_value_usdt = $4, pending_requested_at = now()
          WHERE derivation_index = 7",
    )
    .bind(trace_id)
    .bind(nonce)
    .bind(deadline)
    .bind(value_usdt)
    .execute(pool)
    .await
    .unwrap();
}

/// (trace id in flight, its nonce, first transfer seen) for an index.
async fn account_state(pool: &PgPool, index: i64) -> (Option<String>, Option<i64>, bool) {
    sqlx::query_as(
        "SELECT pending_trace_id, pending_nonce, first_transfer_at IS NOT NULL FROM gasfree_accounts WHERE derivation_index = $1",
    )
    .bind(index)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn sweeper_alerts(pool: &PgPool, severity: &str, containing: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM alerts WHERE source = 'sweeper' AND severity = $1 AND message LIKE '%' || $2 || '%'",
    )
    .bind(severity)
    .bind(containing)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Spec §3 and §5: a sweep by permit is not a sweep until the chain says so. The account keeps the
/// relay's unused margin, so its balance never says it; the owner's nonce moving past the permit's does.
#[tokio::test]
async fn a_gasfree_deposit_is_swept_by_permit_and_counted_swept_only_once_the_nonce_moves() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 0, BEACON_OK).await;
    seed_account(&pool, 7, OWNER_7, false).await;
    let id = seed_gasfree_deposit(&pool, 7, &account, 2_000_000).await;
    let signer = FakeSigner::new(pending(8_000_000, 2_000_000, 0));
    let cfg = gasfree_config(&server);

    sweeper::sweep_once(&pool, &cfg, &tron(&server), &signer).await;
    assert_eq!(signer.asked(), vec![7], "a credited deposit above the fee is swept at once, whatever the threshold");
    assert!(swept_at(&pool, id).await.is_none(), "a permit with the relay is not a sweep");
    assert_eq!(account_state(&pool, 7).await, (Some(TRACE.into()), Some(0), false));

    sweeper::sweep_once(&pool, &cfg, &tron(&server), &signer).await;
    assert_eq!(signer.asked(), vec![7], "one permit at a time: none while one may still run");

    server.reset().await;
    mount_gasfree_chain(&server, &account, 700_000, 1, BEACON_OK).await;
    sweeper::sweep_once(&pool, &cfg, &tron(&server), &signer).await;
    assert!(swept_at(&pool, id).await.is_some(), "the nonce moved past the permit's, so it ran");
    assert_eq!(account_state(&pool, 7).await, (None, None, true), "the treasury saw its own first sweep run");
    assert_eq!(ledger_events(&pool).await, 0, "a sweep must NOT touch the ledger");
    assert_eq!(signer.asked(), vec![7], "nothing is left to ask for");
}

/// Past its deadline and a grace minute, a permit that did not run never can: a fresh one replaces it.
#[tokio::test]
async fn an_expired_permit_that_did_not_run_is_replaced() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 3, BEACON_OK).await;
    seed_account(&pool, 7, OWNER_7, true).await;
    seed_pending_7(&pool, "11111111-1111-4111-8111-111111111111", 3, chrono::Utc::now().timestamp() - 120, 9_500_000).await;
    let id = seed_gasfree_deposit(&pool, 7, &account, 500_000).await;
    let signer = FakeSigner::new(pending(9_500_000, 500_000, 3));

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert_eq!(signer.asked(), vec![7], "the dead permit is dropped and a fresh one asked for in the same pass");
    assert_eq!(account_state(&pool, 7).await, (Some(TRACE.into()), Some(3), true));
    assert!(swept_at(&pool, id).await.is_none());
}

/// Spec §5: after GasFree's code changes, no more money goes in or moves by permit until a human looks.
#[tokio::test]
async fn a_changed_gasfree_implementation_stops_gasfree_sweeps_and_pages_once() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 0, "00000000000000000000000000000000000000ff").await;
    seed_account(&pool, 7, OWNER_7, false).await;
    let id = seed_gasfree_deposit(&pool, 7, &account, 2_000_000).await;
    let signer = FakeSigner::new(pending(8_000_000, 2_000_000, 0));
    let cfg = gasfree_config(&server);

    sweeper::sweep_once(&pool, &cfg, &tron(&server), &signer).await;
    sweeper::sweep_once(&pool, &cfg, &tron(&server), &signer).await;

    assert!(signer.asked().is_empty(), "no permit is asked for while GasFree's code is not the reviewed code");
    assert_eq!(sweeper_alerts(&pool, "p1", "not the reviewed").await, 1, "paged once, not every pass");
    assert!(swept_at(&pool, id).await.is_none());
}

#[tokio::test]
async fn a_relay_refusal_pages_and_leaves_the_deposit_where_it_is() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 0, BEACON_OK).await;
    seed_account(&pool, 7, OWNER_7, false).await;
    let id = seed_gasfree_deposit(&pool, 7, &account, 2_000_000).await;
    let signer = FakeSigner::new(SignerReply::Rejected { reason: "MaxFeeExceededException max fee exceeded".into() });

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert_eq!(signer.asked(), vec![7]);
    assert!(swept_at(&pool, id).await.is_none());
    assert_eq!(account_state(&pool, 7).await, (None, None, false), "nothing is in flight");
    assert_eq!(sweeper_alerts(&pool, "p1", "refused").await, 1);
}

/// Spec §2: "Never sign a higher `maxFee` than was held back". The permit is signed by then, so paging is what is left.
#[tokio::test]
async fn a_permit_whose_max_fee_is_above_what_was_held_pages() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 10_000_000, 4, BEACON_OK).await;
    seed_account(&pool, 7, OWNER_7, true).await;
    seed_gasfree_deposit(&pool, 7, &account, 500_000).await;
    let signer = FakeSigner::new(pending(8_000_000, 2_000_000, 4));

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert_eq!(sweeper_alerts(&pool, "p1", "held back").await, 1);
    assert_eq!(account_state(&pool, 7).await, (Some(TRACE.into()), Some(4), true), "the permit exists, so it is followed");
}

/// A balance no larger than what the relay may take would move nothing (spec §3). The fee is sized on
/// the treasury's own record, so activation counts until its own first sweep of the account ran.
#[tokio::test]
async fn a_gasfree_account_is_swept_only_when_it_holds_more_than_the_fee() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_gasfree_chain(&server, &account_of(OWNER_7), 2_000_000, 0, BEACON_OK).await;
    mount_usdt_balance(&server, &account_of(OWNER_8), 2_000_001).await;
    seed_account(&pool, 7, OWNER_7, false).await;
    seed_account(&pool, 8, OWNER_8, false).await;
    seed_gasfree_deposit(&pool, 7, &account_of(OWNER_7), 2_000_000).await;
    seed_gasfree_deposit(&pool, 8, &account_of(OWNER_8), 2_000_000).await;
    let signer = FakeSigner::new(SignerReply::Busy);

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert_eq!(signer.asked(), vec![8], "exactly the fee is not enough; one micro-USDT more is");
}

/// Open question 1, checked on every sweep: the receiver must get exactly the permit's `value`, with
/// the relay's fee on top of it.
#[tokio::test]
async fn the_relays_record_of_what_reached_the_receiver_is_checked() {
    let pool = pool().await;
    let server = MockServer::start().await;
    let account = account_of(OWNER_7);
    mount_gasfree_chain(&server, &account, 700_000, 1, BEACON_OK).await;
    seed_account(&pool, 7, OWNER_7, false).await;
    let id = seed_gasfree_deposit(&pool, 7, &account, 2_000_000).await;
    seed_pending_7(&pool, TRACE, 0, chrono::Utc::now().timestamp() + 180, 8_000_000).await;
    let mut signer = FakeSigner::new(SignerReply::Busy);
    signer.trace_amount = Some(7_700_000);

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert!(swept_at(&pool, id).await.is_some());
    assert_eq!(sweeper_alerts(&pool, "p1", "reached the receiver").await, 1);
}

/// The fee account is empty by design on this rail. Plain addresses still waiting on it must not stop
/// the GasFree accounts, which need no TRX.
#[tokio::test]
async fn with_gasfree_on_a_dry_fee_account_does_not_stop_the_pass() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_implementations(&server, BEACON_OK).await;
    mount_usdt_balance(&server, ADDRS[0], 500_000_000).await; // index 10
    mount_usdt_balance(&server, ADDRS[1], 500_000_000).await; // index 11
    seed(&pool, "credited", 10, 5).await;
    seed(&pool, "credited", 11, 1).await;
    let signer = FakeSigner::new(SignerReply::FeeAccountDry {
        fee_address: "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH".into(),
        have_sun: 0,
        need_sun: 31_000_000,
    });

    sweeper::sweep_once(&pool, &gasfree_config(&server), &tron(&server), &signer).await;

    assert_eq!(signer.asked(), vec![10, 11], "the pass goes on");
    let alerts: i64 = sqlx::query_scalar("SELECT count(*) FROM alerts WHERE source = 'sweeper'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alerts, 1, "still one alert per pass, naming the account to top up");
}

#[tokio::test]
async fn the_gasfree_sweep_answers_are_read_field_by_field() {
    let cases = [
        (
            serde_json::json!({"status": "pending", "trace_id": TRACE, "gasfree_address": "TGasFree", "receiver": "TFloat",
                               "value_usdt": 8_000_000, "max_fee_usdt": 2_000_000, "nonce": 4, "deadline": 1_790_000_000u64}),
            SignerReply::Pending {
                trace_id: TRACE.into(),
                gasfree_address: "TGasFree".into(),
                value_usdt: 8_000_000,
                max_fee_usdt: 2_000_000,
                nonce: 4,
                deadline: 1_790_000_000,
            },
        ),
        (serde_json::json!({"status": "busy", "gasfree_address": "TGasFree"}), SignerReply::Busy),
        (
            serde_json::json!({"status": "rejected", "reason": "MaxFeeExceededException", "message": "max fee exceeded"}),
            SignerReply::Rejected { reason: "MaxFeeExceededException max fee exceeded".into() },
        ),
        (
            serde_json::json!({"status": "halted", "reason": "GasFree's code changed"}),
            SignerReply::Halted { reason: "GasFree's code changed".into() },
        ),
        (
            serde_json::json!({"status": "below_fee", "gasfree_address": "TGasFree", "balance_usdt": 1, "max_fee_usdt": 2_000_000}),
            SignerReply::BelowFee,
        ),
    ];
    for (body, want) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/sweep"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .mount(&server)
            .await;
        let signer = sweeper::HttpSigner { http: reqwest::Client::new(), base_url: server.uri(), token: "t".into() };
        assert_eq!(signer.sweep(7).await, want, "{body}");
    }

    // A pending answer without its permit's fields cannot be followed, so it is not taken as one.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/internal/sweep"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"status": "pending", "trace_id": TRACE})))
        .mount(&server)
        .await;
    let signer = sweeper::HttpSigner { http: reqwest::Client::new(), base_url: server.uri(), token: "t".into() };
    assert!(matches!(signer.sweep(7).await, SignerReply::Failed(_)));
}

#[tokio::test]
async fn a_trace_is_read_from_the_signer() {
    for (body, want) in [
        (
            serde_json::json!({"state": "SUCCEED", "txn_hash": "abc", "txn_state": "ON_CHAIN", "txn_amount": 8_000_000, "txn_total_fee": 1_300_000}),
            Trace { state: "SUCCEED".into(), txn_hash: Some("abc".into()), txn_amount: Some(8_000_000) },
        ),
        (
            serde_json::json!({"state": "WAITING", "txn_hash": null, "txn_state": null, "txn_amount": null, "txn_total_fee": null}),
            Trace { state: "WAITING".into(), txn_hash: None, txn_amount: None },
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/internal/gasfree/trace/{TRACE}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .mount(&server)
            .await;
        let signer = sweeper::HttpSigner { http: reqwest::Client::new(), base_url: server.uri(), token: "t".into() };
        assert_eq!(signer.trace(TRACE).await, Ok(want), "{body}");
    }
}
```

In `crates/treasury-service/tests/db_reconciliation.rs`, in `pool()`, change the TRUNCATE to:

```rust
    sqlx::query("TRUNCATE treasury_events, mint_intents, reconciliation_runs, alerts, gasfree_accounts RESTART IDENTITY CASCADE")
```

and add at the end of the file:

```rust
/// Spec §2: after a GasFree sweep the relay's unused margin stays at the account, and it still backs
/// CLT. So a recorded account is counted when none of its deposits is unswept — and only once while
/// one is.
#[tokio::test]
async fn a_gasfree_account_is_counted_after_its_deposits_are_swept_and_only_once() {
    let pool = pool().await;
    sqlx::query(
        "INSERT INTO gasfree_accounts (derivation_index, gasfree_address, owner_address) VALUES (7, $1, 'TOwner7')",
    )
    .bind(SHARED_ADDR)
    .execute(&pool)
    .await
    .unwrap();

    let addrs = treasury_service::reconciliation::unswept_addresses(&pool).await.unwrap();
    assert_eq!(addrs, vec![SHARED_ADDR.to_string()], "no deposit is unswept, and the account still counts");

    seed_unswept_mint(&pool, SHARED_ADDR, "tx-gf", Some(7)).await;
    let addrs = treasury_service::reconciliation::unswept_addresses(&pool).await.unwrap();
    assert_eq!(addrs, vec![SHARED_ADDR.to_string()], "an unswept deposit at the same account does not count it twice");
}
```

- [ ] **Step 4: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test(treasury): the GasFree sweep, against stubs

The sweeper tests for a GasFree account: swept by permit, marked swept
only when the owner's nonce moves past the permit's, one permit at a
time, a fresh permit after an expired one, the tripwire, relay
refusals, a maxFee above what was held, the relay's record of the
amount, and a dry fee account that no longer stops the pass. Also one
signer call per index on the TRX rail, and GasFree accounts in the
reserve walk. The new reads and the GasFree pass are todo!() here.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/treasury-service/migrations/0015_gasfree_sweeps.sql crates/treasury-service/src crates/treasury-service/tests
git commit -F .superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/commit-msg.txt
```

Controller: run CI. Expected: the run fails; these 12 fail by name, and every test that existed before this task is `ok`:

```text
test a_changed_gasfree_implementation_stops_gasfree_sweeps_and_pages_once ... FAILED
test a_gasfree_account_is_swept_only_when_it_holds_more_than_the_fee ... FAILED
test a_gasfree_deposit_is_swept_by_permit_and_counted_swept_only_once_the_nonce_moves ... FAILED
test a_permit_whose_max_fee_is_above_what_was_held_pages ... FAILED
test a_relay_refusal_pages_and_leaves_the_deposit_where_it_is ... FAILED
test a_trace_is_read_from_the_signer ... FAILED
test an_expired_permit_that_did_not_run_is_replaced ... FAILED
test every_deposit_at_one_index_is_asked_for_once_and_marked_together ... FAILED
test the_gasfree_sweep_answers_are_read_field_by_field ... FAILED
test the_relays_record_of_what_reached_the_receiver_is_checked ... FAILED
test with_gasfree_on_a_dry_fee_account_does_not_stop_the_pass ... FAILED
test a_gasfree_account_is_counted_after_its_deposits_are_swept_and_only_once ... FAILED
```

- [ ] **Step 5: Replace the stubs**

In `crates/treasury-service/src/gasfree_rail.rs`, replace the bodies of `parse_trace`, `fetch_trace` and `code_changed`:

```rust
pub fn parse_trace(body: &serde_json::Value) -> Result<Trace, String> {
    let state = body["state"].as_str().ok_or_else(|| format!("a trace with no state: {body}"))?;
    Ok(Trace {
        state: state.to_string(),
        txn_hash: body["txn_hash"].as_str().filter(|h| !h.is_empty()).map(str::to_string),
        txn_amount: body["txn_amount"].as_i64(),
    })
}

pub async fn fetch_trace(http: &reqwest::Client, base_url: &str, token: &str, trace_id: &str) -> Result<Trace, String> {
    let resp = http
        .get(format!("{base_url}/internal/gasfree/trace/{trace_id}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("signer unreachable: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("signer returned {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("unreadable trace: {e}"))?;
    parse_trace(&body)
}

pub async fn code_changed(
    client: &crate::tron_verifier::TronClient,
    settings: &gasfree::Settings,
) -> Result<Option<String>, String> {
    let checks = [
        ("beacon", settings.chain.beacon, &settings.expected_beacon_implementation),
        ("controller", settings.chain.controller, &settings.expected_controller_implementation),
    ];
    for (what, proxy, expected) in checks {
        let now = client.implementation(proxy).await?;
        if now != *expected {
            return Ok(Some(format!("the GasFree {what} {proxy} now runs 0x{now}, not the reviewed 0x{expected}")));
        }
    }
    Ok(None)
}
```

In `crates/treasury-service/src/tron_verifier.rs`, replace the bodies of the three reads:

```rust
    pub async fn view_word(&self, contract: &str, selector: &str, parameter: Option<&str>) -> Result<String, String> {
        // A constant call runs nothing and costs nothing. TronGrid wants an `owner_address`, and a
        // view does not care who asks, so the contract is named as its own caller.
        let mut body = serde_json::json!({
            "owner_address": contract,
            "contract_address": contract,
            "function_selector": selector,
            "visible": true,
        });
        if let Some(p) = parameter {
            body["parameter"] = serde_json::Value::from(p);
        }
        let resp = self
            .http
            .post(format!("{}/wallet/triggerconstantcontract", self.base_url))
            .header("TRON-PRO-API-KEY", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("trongrid {selector} on {contract} failed: {status} {text}"));
        }
        let parsed: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let word = parsed["constant_result"][0]
            .as_str()
            .ok_or_else(|| format!("{selector} on {contract} returned nothing: {parsed}"))?;
        if word.len() != 64 || !word.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{selector} on {contract} returned {word:?}, not one 32-byte word"));
        }
        Ok(word.to_ascii_lowercase())
    }

    pub async fn gasfree_nonce(&self, controller: &str, owner: &str) -> Result<u64, String> {
        let word = self.view_word(controller, "nonces(address)", Some(&abi_encode_address(owner)?)).await?;
        let (high, low) = word.split_at(48);
        if high.bytes().any(|b| b != b'0') {
            return Err(format!("nonces({owner}) returned 0x{word}, more than a u64"));
        }
        u64::from_str_radix(low, 16).map_err(|e| format!("nonces({owner}) returned 0x{word}: {e}"))
    }

    pub async fn implementation(&self, proxy: &str) -> Result<String, String> {
        Ok(self.view_word(proxy, "implementation()", None).await?[24..].to_string())
    }
```

In `crates/treasury-service/src/sweeper.rs`:

a) In `HttpSigner::sweep`, replace:

```rust
            // An unknown status must never be treated as benign: it could mean a newer signer swept
            // in a way this version does not understand.
            other => SignerReply::Failed(format!("unrecognised signer status {other:?}")),
```

with:

```rust
            Some("pending") => match (
                body["trace_id"].as_str(),
                body["gasfree_address"].as_str(),
                body["value_usdt"].as_i64(),
                body["max_fee_usdt"].as_i64(),
                body["nonce"].as_u64(),
                body["deadline"].as_u64(),
            ) {
                (Some(trace_id), Some(gasfree_address), Some(value_usdt), Some(max_fee_usdt), Some(nonce), Some(deadline)) => {
                    SignerReply::Pending {
                        trace_id: trace_id.to_string(),
                        gasfree_address: gasfree_address.to_string(),
                        value_usdt,
                        max_fee_usdt,
                        nonce,
                        deadline,
                    }
                }
                // A permit is with the relay and nothing here could follow it. It can only pay the
                // float or custody, so the deposits stay unswept on the books, still counted.
                _ => SignerReply::Failed(format!("signer reported pending without the permit's fields: {body}")),
            },
            Some("busy") => SignerReply::Busy,
            Some("rejected") => SignerReply::Rejected {
                reason: format!(
                    "{} {}",
                    body["reason"].as_str().unwrap_or("no reason given"),
                    body["message"].as_str().unwrap_or("")
                )
                .trim_end()
                .to_string(),
            },
            Some("halted") => SignerReply::Halted {
                reason: body["reason"].as_str().unwrap_or("no reason given").to_string(),
            },
            Some("below_fee") => SignerReply::BelowFee,
            // An unknown status must never be treated as benign: it could mean a newer signer swept
            // in a way this version does not understand.
            other => SignerReply::Failed(format!("unrecognised signer status {other:?}")),
```

b) Replace the whole `sweep_once` function (its doc comment through its closing brace) and the whole `mark_swept` function with:

```rust
/// One pass: settle GasFree permits in flight, then for every index with unswept deposits decide,
/// sweep, record. Returns the number of `approved`/`submitted`/`credited` rows that currently have
/// NO `derivation_index` — see the warning below for why that count matters.
pub async fn sweep_once(pool: &PgPool, config: &AppConfig, client: &TronClient, signer: &dyn SweepSigner) -> usize {
    // GasFree first, before the rows are read: a permit that ran marks its deposits swept here, so
    // they are not asked for again below. The tripwire is read once for the whole pass.
    let gasfree_ready = match &config.gasfree {
        Some(settings) => {
            gasfree_sweep::settle(pool, settings, client, signer).await;
            gasfree_sweep::code_unchanged(pool, settings, client).await
        }
        None => false,
    };

    let rows: Vec<(Uuid, String, i64, f64, bool)> = match sqlx::query_as(
        // `credited` and later only. Sweeping an address whose deposit has not yet been credited
        // would move the evidence out from under the verifier before it has finished with it.
        "SELECT id, deposit_address, derivation_index,
                (EXTRACT(EPOCH FROM (now() - created_at)) / 3600.0)::double precision,
                fee_held_usdt IS NOT NULL
         FROM mint_intents
         WHERE deposit_address IS NOT NULL
           AND derivation_index IS NOT NULL
           AND swept_at IS NULL
           AND status IN ('credited', 'submitted')
         ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("sweeper: could not list unswept addresses: {e}");
            return 0;
        }
    };

    // One line per pass, always -- including when there is nothing to do.
    //
    // Without it this worker is completely silent while idle, which is byte-for-byte what a worker
    // that died at startup looks like. There is no way to tell them apart from outside, and the
    // difference is "no deposits to consolidate" versus "money is accumulating at addresses nothing
    // will ever sweep".
    tracing::info!("sweeper: pass over {} unswept address(es)", rows.len());

    // A credited deposit with no derivation_index can never satisfy the query above (it requires
    // `derivation_index IS NOT NULL`), so it would sit unswept forever while this pass keeps logging
    // the same "N unswept address(es)" line a healthy pass would show. Checked independently of the
    // loop below, every pass, so a row stuck like this cannot hide behind an otherwise-quiet worker.
    let missing_index: Vec<String> = match sqlx::query_scalar(
        "SELECT deposit_address FROM mint_intents
         WHERE deposit_address IS NOT NULL
           AND derivation_index IS NULL
           AND swept_at IS NULL
           AND status IN ('approved', 'submitted', 'credited', 'needs_manual')",
    )
    .fetch_all(pool)
    .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("sweeper: could not check for deposits missing a derivation_index: {e}");
            Vec::new()
        }
    };
    let missing_count = missing_index.len();
    if missing_count > 0 {
        let mut distinct_addresses = missing_index;
        distinct_addresses.sort_unstable();
        distinct_addresses.dedup();
        // An 'approved' row here is not yet stuck — the sweep query above only ever selects
        // ('credited', 'submitted'), so 'approved' simply isn't sweep-eligible yet regardless of
        // this column. It is 'submitted'/'credited' rows missing the index that are truly stuck:
        // those statuses ARE what the sweep query selects on, so a missing index is the only thing
        // excluding them, and derivation_index never gets set after the row is created.
        let message = format!(
            "sweeper: {missing_count} deposit(s) have no derivation_index — an 'approved' row is not \
             yet eligible for sweeping, but any already 'submitted' or 'credited' can never be swept \
             without one: {distinct_addresses:?}"
        );
        tracing::warn!("{message}");
        // Beside the log line: a plain warn! is invisible to whatever watches the alerts table, and
        // this condition is exactly as actionable as every other sweeper alert below.
        alert(pool, "warn", "sweeper", &message).await;
    }

    let mut dry_alerted = false;
    for group in by_index(rows) {
        let (index, address) = (group.index, group.address.as_str());

        if group.gasfree {
            match &config.gasfree {
                Some(settings) if gasfree_ready => {
                    gasfree_sweep::sweep_account(pool, config, settings, client, signer, index, address).await
                }
                // The tripwire stopped GasFree sweeps or could not be read, and has already alerted.
                Some(_) => {}
                None => {
                    alert_once(
                        pool,
                        "warn",
                        "sweeper",
                        &format!(
                            "credited deposits wait at GasFree account {address} (index {index}), but GasFree is \
                             off in this service. They stay there, counted in the reserve, until the GasFree \
                             settings are back."
                        ),
                        chrono::Duration::hours(1),
                    )
                    .await;
                }
            }
            continue;
        }

        let balance = match client.get_custody_balance(address, &config.usdt_contract).await {
            Ok(b) => b,
            Err(e) => {
                // Transient. Never mark swept on an unread balance: that would abandon real funds
                // at an address nothing looks at again.
                tracing::warn!("sweeper: balance read failed for {address}: {e}");
                continue;
            }
        };

        if !should_sweep(
            balance,
            config.sweep_threshold_usdt,
            group.age_hours as i64,
            config.sweep_max_age_hours,
            config.sweep_min_usdt,
        ) {
            continue;
        }

        match signer.sweep(index).await {
            SignerReply::Swept { tx_id } => {
                // swept_at ONLY. No ledger event — see the module docs: the reserve did not change,
                // and recording one here would double-count the deposit. Every row of the index: the
                // sweep moved the address's whole balance.
                if let Err(e) = mark_swept(pool, &group.ids).await {
                    // The funds moved but we failed to record it. Loud, because the next pass will
                    // find a now-empty address and resolve it as NothingToSweep — correct, but only
                    // by luck, and a human should know a write was lost.
                    alert(
                        pool,
                        "p1",
                        "sweeper",
                        &format!("swept {address} in {tx_id} but failed to record swept_at for intents {:?}: {e}", group.ids),
                    )
                    .await;
                } else {
                    tracing::info!("swept {balance} micro-USDT from {address} (index {index}) in {tx_id}");
                }
            }

            // Already empty: nothing to move, and the address is done. Recorded so it stops being
            // polled and stops inflating the reserve walk.
            SignerReply::NothingToSweep => {
                if let Err(e) = mark_swept(pool, &group.ids).await {
                    tracing::error!("sweeper: failed to mark empty address {address} swept: {e}");
                }
            }

            // Expected, not exceptional — every fresh address starts here, because receiving
            // tokens does not create a TRX balance. Left unswept deliberately: the funding transfer
            // has to confirm before the sweep can spend it, so the next pass finishes the job.
            SignerReply::Funded { tx_id, amount_sun } => {
                tracing::info!(
                    "sweeper: funded {address} (index {index}) with {amount_sun} sun in {tx_id}; \
                     sweeping {balance} micro-USDT on a later pass"
                );
            }

            // The only outcome no retry resolves, and every remaining plain address gets the same
            // answer, so one alert would be buried under a pass-sized burst of duplicates. On the TRX
            // rail nothing else can move this pass, so it stops. With GasFree on, the fee account is
            // empty by design and passes come every minute: the fact is said once an hour, and the
            // pass goes on to the GasFree accounts, which need no TRX.
            SignerReply::FeeAccountDry { fee_address, have_sun, need_sun } => {
                let message = format!(
                    "TRX float exhausted: {fee_address} holds {have_sun} sun, needs {need_sun}. \
                     No deposit at a plain address can be swept until it is topped up."
                );
                if config.gasfree.is_none() {
                    alert(pool, "warn", "sweeper", &message).await;
                    return missing_count;
                }
                if !dry_alerted {
                    alert_once(pool, "warn", "sweeper", &message, chrono::Duration::hours(1)).await;
                    dry_alerted = true;
                }
            }

            SignerReply::Failed(e) => sweep_failed(pool, config, address, index, &e).await,

            // The signer answered for this index's GasFree account: it found USDT there and asked
            // for a permit. No deposit on the books is at that account, so nothing is recorded; the
            // plain address is looked at again on the next pass.
            other => tracing::warn!("sweeper: index {index} (plain address {address}) was answered for its GasFree account: {other:?}"),
        }
    }

    missing_count
}

/// A sweep that failed. On the TRX rail passes are an hour apart and each failure is alerted. With
/// GasFree on they are a minute apart, so one unchanged failure is said once an hour, with the
/// reason in the log (spec §5: retried every pass, paged if it persists).
async fn sweep_failed(pool: &PgPool, config: &AppConfig, address: &str, index: i64, e: &str) {
    if config.gasfree.is_none() {
        alert(pool, "warn", "sweeper", &format!("sweep of {address} (index {index}) failed: {e}")).await;
        return;
    }
    tracing::warn!("sweeper: sweep of {address} (index {index}) failed: {e}");
    alert_once(
        pool,
        "warn",
        "sweeper",
        &format!("sweep of {address} (index {index}) keeps failing; the log has the reason. It is retried every pass."),
        chrono::Duration::hours(1),
    )
    .await;
}

/// The unswept deposits of one index. A user's permanent address carries every deposit they have
/// made, so one index can have many rows, and each pass asks the signer about it once.
struct IndexGroup {
    index: i64,
    address: String,
    /// The oldest row's age: the age valve in `should_sweep` is about the oldest money waiting.
    age_hours: f64,
    /// A deposit to a GasFree account (`fee_held_usdt` is set).
    gasfree: bool,
    ids: Vec<Uuid>,
}

/// Rows grouped by index, in the order of each index's oldest row.
fn by_index(rows: Vec<(Uuid, String, i64, f64, bool)>) -> Vec<IndexGroup> {
    let mut groups: Vec<IndexGroup> = Vec::new();
    let mut at: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for (id, address, index, age_hours, gasfree) in rows {
        match at.get(&index) {
            Some(&i) => groups[i].ids.push(id),
            None => {
                at.insert(index, groups.len());
                groups.push(IndexGroup { index, address, age_hours, gasfree, ids: vec![id] });
            }
        }
    }
    groups
}

async fn mark_swept(pool: &PgPool, ids: &[Uuid]) -> Result<(), sqlx::Error> {
    // Guarded on IS NULL so a concurrent or repeated pass cannot overwrite the original timestamp.
    sqlx::query("UPDATE mint_intents SET swept_at = now() WHERE id = ANY($1) AND swept_at IS NULL")
        .bind(ids)
        .execute(pool)
        .await
        .map(|_| ())
}
```

Replace the whole of `crates/treasury-service/src/sweeper/gasfree_sweep.rs` with:

```rust
//! Sweeping deposits out of GasFree accounts (spec §3), and deciding from the chain when a sweep ran
//! (spec §5).
//!
//! A GasFree sweep ends as a permit with the relay, not a finished transfer. The account is swept
//! when the controller's `nonces(owner)` moves past the permit's nonce. The account's balance never
//! says it: the relay's unused margin stays there, so it never falls to zero.

use sqlx::PgPool;

use super::{SignerReply, SweepSigner};
use crate::configuration::AppConfig;
use crate::ledger::{alert, alert_once};
use crate::tron_verifier::TronClient;

/// After a permit's deadline the controller refuses it, but a block at the deadline may take this
/// long to show in what TronGrid answers.
const DEADLINE_GRACE_SECS: i64 = 60;

/// How often one unchanged condition pages again.
fn hourly() -> chrono::Duration {
    chrono::Duration::hours(1)
}

/// The tripwire, read once per pass (spec §5). `false`: no GasFree sweep is asked for this pass.
///
/// The signer refuses to sign after a change anyway. What this adds is a human being told.
pub(super) async fn code_unchanged(pool: &PgPool, settings: &gasfree::Settings, client: &TronClient) -> bool {
    match crate::gasfree_rail::code_changed(client, settings).await {
        Ok(None) => true,
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
        Err(e) => {
            tracing::warn!("sweeper: GasFree's implementation() is unreadable: {e}");
            alert_once(
                pool,
                "warn",
                "sweeper",
                "GasFree's implementation() could not be read; GasFree sweeps wait until it can be",
                hourly(),
            )
            .await;
            false
        }
    }
}

/// Every permit in flight: did it run, and can it still run?
pub(super) async fn settle(pool: &PgPool, settings: &gasfree::Settings, client: &TronClient, signer: &dyn SweepSigner) {
    let pending: Vec<(i64, String, String, String, i64, i64, i64, chrono::DateTime<chrono::Utc>)> = match sqlx::query_as(
        "SELECT derivation_index, gasfree_address, owner_address, pending_trace_id, pending_nonce,
                pending_deadline, pending_value_usdt, pending_requested_at
         FROM gasfree_accounts WHERE pending_nonce IS NOT NULL
         ORDER BY derivation_index",
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("sweeper: could not list GasFree permits in flight: {e}");
            return;
        }
    };

    for (index, account, owner, trace_id, nonce, deadline, value_usdt, requested_at) in pending {
        let chain_nonce = match client.gasfree_nonce(settings.chain.controller, &owner).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("sweeper: nonces({owner}) is unreadable, so the permit for {account} waits: {e}");
                continue;
            }
        };

        if chain_nonce > nonce as u64 {
            // It ran: the controller moves past a nonce only by running a permit that carries it.
            // Every deposit verified before the permit was asked for was in the balance it moved.
            let swept: Vec<i64> = match sqlx::query_scalar(
                "UPDATE mint_intents SET swept_at = now()
                 WHERE derivation_index = $1 AND deposit_address = $2 AND swept_at IS NULL
                   AND verified_at <= $3
                   AND status IN ('approved', 'submitted', 'credited', 'needs_manual')
                 RETURNING amount_clt",
            )
            .bind(index)
            .bind(&account)
            .bind(requested_at)
            .fetch_all(pool)
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    alert(pool, "p1", "sweeper", &format!("the permit for {account} ran, but recording its deposits as swept failed: {e}. The next pass retries.")).await;
                    continue;
                }
            };
            // From here on a deposit at this account holds back one transfer fee (spec §2). The record
            // is the treasury's own: this service asked for the permit that ran.
            if let Err(e) = sqlx::query(
                "UPDATE gasfree_accounts
                    SET first_transfer_at = COALESCE(first_transfer_at, now()),
                        pending_trace_id = NULL, pending_nonce = NULL, pending_deadline = NULL,
                        pending_value_usdt = NULL, pending_requested_at = NULL
                  WHERE derivation_index = $1 AND pending_nonce = $2",
            )
            .bind(index)
            .bind(nonce)
            .execute(pool)
            .await
            {
                alert(pool, "p1", "sweeper", &format!("the permit for {account} ran, but clearing it failed: {e}. The next pass retries.")).await;
                continue;
            }
            tracing::info!(
                "sweeper: GasFree account {account} (index {index}) swept by permit {trace_id}; {} deposit(s) recorded",
                swept.len()
            );

            // Open question 1 of the design, checked on every sweep: the receiver must get exactly
            // `value`, with the relay's fee on top of it.
            match signer.trace(&trace_id).await {
                Ok(t) => {
                    if let Some(got) = t.txn_amount.filter(|got| *got != value_usdt) {
                        alert(
                            pool,
                            "p1",
                            "sweeper",
                            &format!(
                                "the relay says {got} micro-USDT of the sweep of {account} reached the receiver; the \
                                 permit said {value_usdt}. If the fee came out of the value, every GasFree mint is too \
                                 large by the fee: stop GasFree deposits and read open question 1 of the GasFree design."
                            ),
                        )
                        .await;
                    }
                }
                Err(e) => tracing::warn!("sweeper: the relay's record of permit {trace_id} is unreadable: {e}"),
            }
        } else if chrono::Utc::now().timestamp() > deadline + DEADLINE_GRACE_SECS {
            // It can no longer run, and it did not: a fresh permit on this pass or a later one.
            match sqlx::query(
                "UPDATE gasfree_accounts
                    SET pending_trace_id = NULL, pending_nonce = NULL, pending_deadline = NULL,
                        pending_value_usdt = NULL, pending_requested_at = NULL
                  WHERE derivation_index = $1 AND pending_nonce = $2",
            )
            .bind(index)
            .bind(nonce)
            .execute(pool)
            .await
            {
                Ok(_) => tracing::info!("sweeper: the permit for {account} (index {index}) expired without running; a fresh one follows"),
                Err(e) => tracing::error!("sweeper: could not clear the expired permit for {account}: {e}"),
            }
        }
    }
}

/// One GasFree account holding credited deposits: ask for a permit if one could move anything.
pub(super) async fn sweep_account(
    pool: &PgPool,
    config: &AppConfig,
    settings: &gasfree::Settings,
    client: &TronClient,
    signer: &dyn SweepSigner,
    index: i64,
    address: &str,
) {
    let account: Option<(bool, bool)> = match sqlx::query_as(
        "SELECT first_transfer_at IS NOT NULL, pending_nonce IS NOT NULL FROM gasfree_accounts
         WHERE derivation_index = $1 AND gasfree_address = $2",
    )
    .bind(index)
    .bind(address)
    .fetch_optional(pool)
    .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("sweeper: could not read GasFree account {address}: {e}");
            return;
        }
    };
    let Some((seen_first_transfer, in_flight)) = account else {
        // The verifier records every GasFree account it approves a deposit at, so this deposit's
        // account was never recorded, and nothing here can size its fee.
        alert_once(
            pool,
            "warn",
            "sweeper",
            &format!("GasFree account {address} (index {index}) holds credited deposits but is not in gasfree_accounts, so it is not swept"),
            hourly(),
        )
        .await;
        return;
    };
    // One permit at a time (spec §3): a second would carry the same nonce.
    if in_flight {
        return;
    }

    let balance = match client.get_custody_balance(address, &config.usdt_contract).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("sweeper: balance read failed for {address}: {e}");
            return;
        }
    };
    // No more than the relay may take: a sweep would move nothing. The line the signer draws too,
    // here on the treasury's own record, so activation counts until its own first sweep ran.
    let fee = gasfree::fee_to_hold(seen_first_transfer, settings.activate_fee_max_usdt, settings.transfer_fee_max_usdt);
    if balance <= fee {
        return;
    }

    let requested_at = chrono::Utc::now();
    match signer.sweep(index).await {
        SignerReply::Pending { trace_id, gasfree_address, value_usdt, max_fee_usdt, nonce, deadline } => {
            if gasfree_address != address {
                alert(
                    pool,
                    "p1",
                    "sweeper",
                    &format!(
                        "asked to sweep index {index}, the signer sent a permit for GasFree account {gasfree_address}; the \
                         books say {address}. Check the GasFree settings in both services."
                    ),
                )
                .await;
            }
            // Never a higher maxFee than was held back (spec §2): the most any deposit here held is
            // the most the relay may take.
            let held: Option<i64> = sqlx::query_scalar(
                "SELECT MAX(fee_held_usdt) FROM mint_intents
                 WHERE derivation_index = $1 AND deposit_address = $2 AND swept_at IS NULL AND verified_at <= $3",
            )
            .bind(index)
            .bind(address)
            .bind(requested_at)
            .fetch_one(pool)
            .await
            .unwrap_or(None);
            if let Some(h) = held.filter(|h| max_fee_usdt > *h) {
                alert(
                    pool,
                    "p1",
                    "sweeper",
                    &format!(
                        "the signer set maxFee {max_fee_usdt} on the permit for {address}, above the {h} held back for \
                         any deposit it moves. The permit is signed; if it runs, the reserve may fall below supply by \
                         the difference. Check the GasFree maxima in both services."
                    ),
                )
                .await;
            }
            match sqlx::query(
                "UPDATE gasfree_accounts
                    SET pending_trace_id = $2, pending_nonce = $3, pending_deadline = $4,
                        pending_value_usdt = $5, pending_requested_at = $6
                  WHERE derivation_index = $1",
            )
            .bind(index)
            .bind(&trace_id)
            .bind(nonce as i64)
            .bind(deadline as i64)
            .bind(value_usdt)
            .bind(requested_at)
            .execute(pool)
            .await
            {
                Ok(_) => tracing::info!(
                    "sweeper: permit {trace_id} for {address} (index {index}) is with the relay: {value_usdt} \
                     micro-USDT, maxFee {max_fee_usdt}, nonce {nonce}"
                ),
                // Not recorded, so never settled: the deposits stay unswept on the books, still
                // counted, and the next pass finds the relay's transfer in flight and waits.
                Err(e) => {
                    alert(pool, "p1", "sweeper", &format!("permit {trace_id} for {address} is with the relay, but recording it failed: {e}")).await;
                }
            }
        }
        SignerReply::Busy => tracing::info!("sweeper: a transfer from {address} is already in flight; next pass"),
        SignerReply::BelowFee => tracing::info!("sweeper: {address} holds no more than the relay's fee; left for a later deposit"),
        SignerReply::Rejected { reason } => {
            alert_once(
                pool,
                "p1",
                "sweeper",
                &format!(
                    "the relay refused the sweep of GasFree account {address}: {reason}. The deposit stays there, still \
                     counted. If the live fee is above the configured maximum, raise the maximum in the signer and the \
                     treasury together, never in the signer alone."
                ),
                hourly(),
            )
            .await;
        }
        SignerReply::Halted { reason } => {
            alert_once(pool, "p1", "sweeper", &format!("the signer halted GasFree for {address}: {reason}"), hourly()).await;
        }
        SignerReply::Failed(e) => super::sweep_failed(pool, config, address, index, &e).await,
        other => {
            tracing::warn!("sweeper: asked to sweep GasFree account {address} (index {index}), the signer answered {other:?}");
            alert_once(
                pool,
                "warn",
                "sweeper",
                &format!(
                    "asked to sweep GasFree account {address} (index {index}), the signer answered for the plain \
                     address instead: is GasFree on in the signer?"
                ),
                hourly(),
            )
            .await;
        }
    }
}
```

In `crates/treasury-service/src/reconciliation.rs`, replace the whole `unswept_addresses` function (its doc comment through its closing brace) with:

```rust
/// Every address besides custody and the float whose USDT backs CLT, each counted ONCE: every
/// address still holding an unswept deposit, and every GasFree account the treasury has recorded.
///
/// DISTINCT is load-bearing, not tidiness. `get_reserve_balance` sums every entry it is handed, and
/// per-user deposit addresses mean one address legitimately appears on many unswept rows. Summing
/// per row inflates the reserve — and an over-backed reading is the dangerous direction, because it
/// licenses minting that nothing backs. Under-counting merely halts minting, loudly. UNION removes
/// duplicates the same way.
///
/// GasFree accounts count after their deposits are swept (spec §2): a sweep leaves the relay's
/// unused margin at the account, and that still backs CLT.
/// ponytail: one balance read per recorded account per run, so the walk grows with users; count only
/// accounts that may hold something if that ever matters.
pub async fn unswept_addresses(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        // `needs_manual` counts on purpose: it is a verified deposit waiting on a human (over the
        // per-transaction cap, or a GasFree deposit below the minimum), its custody_deposit event is
        // already in the ledger, and its USDT is still at the address. Leaving it out would read the
        // reserve short and halt minting over money that is present.
        "SELECT deposit_address FROM mint_intents
         WHERE deposit_address IS NOT NULL AND swept_at IS NULL
           AND status IN ('approved', 'submitted', 'credited', 'needs_manual')
         UNION
         SELECT gasfree_address FROM gasfree_accounts",
    )
    .fetch_all(pool)
    .await
}
```

In `crates/treasury-service/src/main.rs`, replace:

```rust
        tokio::spawn(treasury_service::sweeper::run(
            pool.clone(),
            config.clone(),
            config.reconciliation_interval_secs.min(3600),
        ));
```

with:

```rust
        //
        // GasFree deposits are swept as soon as they are credited (spec §3), and a permit is valid
        // for minutes, so while GasFree is on the pass runs every minute. The TRX rail keeps the
        // hour, for the funding reason above; with GasFree on, the plain addresses get the same
        // minute, which is still far above a block.
        let sweep_interval_secs = if config.gasfree.is_some() { 60 } else { config.reconciliation_interval_secs.min(3600) };
        tokio::spawn(treasury_service::sweeper::run(pool.clone(), config.clone(), sweep_interval_secs));
```

Do not change the tests.

- [ ] **Step 6: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(treasury): sweep GasFree deposits by permit, settled by nonce

With GasFree on, a credited deposit at a GasFree account is swept as
soon as the account holds more than the fee. The permit is recorded,
and the deposits count as swept only when the owner's nonce on the
controller moves past the permit's; an expired permit that did not run
is replaced. Each pass reads both GasFree implementations and stops
GasFree sweeps on a change, and a relay refusal, a halt, or a maxFee
above what was held back pages a human. The relay's record of what
reached the receiver is checked against the permit on every sweep.
The sweeper asks once per index on both rails, a dry fee account no
longer stops the pass while GasFree is on, the pass runs every minute
on this rail, and every recorded GasFree account stays in the reserve
walk.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/treasury-service/src
git commit -F .superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/commit-msg.txt
```

Controller: run CI. Expected: success; the 12 tests above say `ok` by name, every test from Tasks 1-2 and every earlier test says `ok`, and there is no warning located in `crates/treasury-service`.

---

### Task 4: GasFree payouts in the treasury

**Files:**
- Create: `crates/treasury-service/migrations/0016_gasfree_payouts.sql`
- Modify: `crates/treasury-service/src/tron_verifier.rs` (`from`, `has_contract`, `confirmed_transfer`)
- Modify: `crates/treasury-service/src/payout.rs`
- Modify: `crates/treasury-service/src/api.rs`
- Modify: `crates/treasury-service/src/main.rs`
- Test: `crates/treasury-service/tests/db_redemption.rs`

**Interfaces:**
- Consumes: `AppConfig.gasfree` (Task 2); `Trace`, `fetch_trace`, `TronClient::gasfree_nonce` (Task 3); the signer's payout statuses and `/internal/xpub` (fact 1).
- Produces:
  - `PayoutReply` derives `Clone`, and gains `Submitted { trace_id: String, nonce: u64, deadline: u64 }`, `RelayRefused { reason: String, nonce: u64, deadline: u64 }`, `FloatNotActive { float_address: String }`
  - `PayoutSigner::trace(&self, trace_id: &str) -> Result<Trace, String>` and `PayoutSigner::float_owner(&self) -> Result<(String, Option<String>), String>`, default methods answering `Err`; `HttpPayoutSigner` implements both
  - `pub async fn payout::confirm_gasfree_payouts_once(pool: &PgPool, config: &AppConfig, settings: &gasfree::Settings, client: &TronClient, signer: &dyn PayoutSigner) -> Result<u32, String>`
  - `TronClient::has_contract(&self, address: &str) -> Result<bool, String>` and `TronClient::confirmed_transfer(&self, tx_id: &str, from: &str, to: &str, usdt_contract: &str, amount: i64, since_ms: i64) -> Result<bool, String>`
  - columns `redemption_intents.payout_trace_id TEXT, payout_permit_nonce BIGINT, payout_permit_deadline BIGINT`

- [ ] **Step 1: The migration**

Create `crates/treasury-service/migrations/0016_gasfree_payouts.sql`:

```sql
-- A redemption paid by GasFree permit (spec §4). The trace id leads to the permit's transaction;
-- the nonce and the deadline say, once the deadline has passed, whether the permit can have run.
-- NULL for a TRX payout.
ALTER TABLE redemption_intents
    ADD COLUMN payout_trace_id        TEXT,
    ADD COLUMN payout_permit_nonce    BIGINT,
    ADD COLUMN payout_permit_deadline BIGINT;
```

- [ ] **Step 2: The new answers and reads, as stubs**

In `crates/treasury-service/src/tron_verifier.rs`:

a) In `struct Trc20Transfer`, add after the `to` field:

```rust
    /// Who paid. Only the payout check reads it (`confirmed_transfer`): a GasFree payout's
    /// transaction carries two transfers from the float, the redeemer's and the relay's fee. A
    /// missing field fails that check, closed.
    #[serde(default)]
    from: String,
```

b) In the unit tests' `fn transfer(to: &str, contract: &str, value: &str) -> Trc20Transfer`, add `from: String::new(),` after `transaction_id: "tx1".to_string(),`.

c) Inside `impl TronClient`, after `implementation`, add:

```rust
    /// Whether `address` holds a deployed contract; for a GasFree account, whether it is activated.
    /// `contract_address`, not `bytecode`: an activated GasFree account answers with an EMPTY
    /// bytecode (2026-09-24). Only `{}` means no; any other answer is an error, never "no".
    pub async fn has_contract(&self, address: &str) -> Result<bool, String> {
        todo!("Task 4 Step 5")
    }

    /// Whether `tx_id` carries a confirmed USDT `Transfer` of exactly `amount` from `from` to `to`.
    ///
    /// Read from `from`'s confirmed TRC-20 history since `since_ms`, so the event is checked field by
    /// field: a GasFree payout's transaction holds two transfers from the float, and only the one to
    /// the redeemer pays the redemption.
    pub async fn confirmed_transfer(
        &self,
        tx_id: &str,
        from: &str,
        to: &str,
        usdt_contract: &str,
        amount: i64,
        since_ms: i64,
    ) -> Result<bool, String> {
        todo!("Task 4 Step 5")
    }
```

In `crates/treasury-service/src/payout.rs`:

a) Replace `use crate::ledger::alert;` with:

```rust
use crate::gasfree_rail::Trace;
use crate::ledger::{alert, alert_once};
```

b) Change `#[derive(Debug, PartialEq)]` on `pub enum PayoutReply` to `#[derive(Debug, Clone, PartialEq)]`, and add these variants after `Ambiguous(String),`:

```rust
    /// A GasFree permit paying this redemption is with the relay. Not paid until its transfer from
    /// the float is found confirmed on chain (`confirm_gasfree_payouts_once`).
    Submitted { trace_id: String, nonce: u64, deadline: u64 },
    /// The relay refused a signed permit with one of the refusals its docs list as pre-execution
    /// checks. That is the relay's word, not proof: the permit stays valid until `deadline`, so
    /// nothing is signed before then, and after it the chain decides.
    RelayRefused { reason: String, nonce: u64, deadline: u64 },
    /// The GasFree float has never made a transfer, so its next one would also pay its activation.
    /// Provably nothing was signed. Redemptions wait for the one-time activation (spec §4).
    FloatNotActive { float_address: String },
```

c) Replace the `PayoutSigner` trait with:

```rust
/// The signer boundary, as a trait so the worker is testable without a live service or real keys —
/// same reasoning as `SweepSigner` in sweeper.rs.
#[async_trait::async_trait]
pub trait PayoutSigner: Send + Sync {
    async fn pay(&self, intent_id: Uuid, to: &str, amount_usdt: i64) -> PayoutReply;

    /// The relay's record of a GasFree permit. Only used to find its transaction; the chain decides
    /// whether that paid the redemption.
    async fn trace(&self, _trace_id: &str) -> Result<Trace, String> {
        Err("this signer cannot read GasFree traces".into())
    }

    /// The plain address that owns the payout float (`2/0`), and the GasFree float it owns. The
    /// float's nonce on the GasFree controller is its owner's.
    async fn float_owner(&self) -> Result<(String, Option<String>), String> {
        Err("this signer cannot name the float's owner".into())
    }
}
```

d) In `impl PayoutSigner for HttpPayoutSigner`, add after the `pay` method:

```rust
    async fn trace(&self, trace_id: &str) -> Result<Trace, String> {
        crate::gasfree_rail::fetch_trace(&self.http, &self.base_url, &self.token, trace_id).await
    }

    async fn float_owner(&self) -> Result<(String, Option<String>), String> {
        todo!("Task 4 Step 5")
    }
```

e) In `drain_once`, the `match signer.pay(intent_id, &payout_address, payout_amount_usdt).await` has an arm starting `reply @ (PayoutReply::FloatDry { .. }`. Add directly before that arm:

```rust
            PayoutReply::Submitted { .. } | PayoutReply::RelayRefused { .. } | PayoutReply::FloatNotActive { .. } => {
                todo!("Task 4 Step 5")
            }
```

f) Add after `confirm_payouts_once`:

```rust
/// Settles GasFree payouts (spec §4, §5).
///
/// A permit returns a trace id. The relay's record of it names the transaction, and the payout is
/// paid only when that transaction's transfer — from the float, to the redeemer, of exactly the
/// quoted amount — is confirmed on chain. After a permit's deadline, the float's nonce says whether
/// it can have run: still at the permit's nonce, it never did, and the redemption goes back to be
/// paid again.
pub async fn confirm_gasfree_payouts_once(
    pool: &PgPool,
    config: &AppConfig,
    settings: &gasfree::Settings,
    client: &TronClient,
    signer: &dyn PayoutSigner,
) -> Result<u32, String> {
    todo!("Task 4 Step 5")
}
```

- [ ] **Step 3: The payout tests**

In `crates/treasury-service/tests/db_redemption.rs`:

a) Change the imports at the top so they read:

```rust
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use tower::ServiceExt;
use treasury_service::gasfree_rail::Trace;
use treasury_service::intents::create_redemption_intent;
use treasury_service::payout::{self, HttpPayoutSigner, PayoutReply, PayoutSigner};
use treasury_service::tron_verifier::TronClient;
use treasury_service::watcher::confirm_burn;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
```

b) In `impl PayoutSigner for CountingSigner`, replace the whole `match &self.reply { … }` expression in `pay` with:

```rust
        self.reply.clone()
```

c) Add at the end of the file:

```rust
// --- GasFree payouts (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md §4, §5) ---

/// `config().payout_float_address`: on this rail, F = gasfree(2/0).
const FLOAT: &str = "TT2X2yyubp7qpAWYYNE5JQWBtoZ7ikQFsY";
/// The plain 2/0 address that owns the float, as the signer's /internal/xpub names it.
const FLOAT_OWNER: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";
/// `pending_redemption`'s payout address.
const REDEEMER: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";
const PAYOUT_TRACE: &str = "6ab4c27c-f66b-4328-b40f-ffdc6cf1ca60";
const USDT: &str = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";

fn nile() -> gasfree::Settings {
    gasfree::Settings {
        chain: &gasfree::NILE,
        rail: true,
        activate_fee_max_usdt: 1_500_000,
        transfer_fee_max_usdt: 500_000,
        min_deposit_usdt: 1_000_000,
        expected_beacon_implementation: "b8eda40b467b45af107f198e94cc2fa1378adf50".into(),
        expected_controller_implementation: "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441".into(),
    }
}

fn gasfree_config(trongrid_url: String) -> treasury_service::configuration::AppConfig {
    let mut cfg = config();
    cfg.trongrid_url = trongrid_url;
    cfg.gasfree = Some(nile());
    cfg
}

fn counting(reply: PayoutReply) -> CountingSigner {
    CountingSigner { reply, calls: AtomicUsize::new(0), last_amount: AtomicI64::new(0) }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The signer's GasFree reads, faked: the relay's record names `txn_hash`, and FLOAT_OWNER owns FLOAT.
struct GasFreeReads {
    txn_hash: Option<&'static str>,
}

#[async_trait::async_trait]
impl PayoutSigner for GasFreeReads {
    async fn pay(&self, _intent_id: Uuid, _to: &str, _amount_usdt: i64) -> PayoutReply {
        panic!("settling a payout must never sign another")
    }
    async fn trace(&self, _trace_id: &str) -> Result<Trace, String> {
        Ok(Trace { state: "SUCCEED".into(), txn_hash: self.txn_hash.map(str::to_string), txn_amount: None })
    }
    async fn float_owner(&self) -> Result<(String, Option<String>), String> {
        Ok((FLOAT_OWNER.into(), Some(FLOAT.into())))
    }
}

/// A redemption whose GasFree permit is out: claimed, with the permit's trace (if any), nonce and deadline.
async fn permit_out(pool: &PgPool, amount: i64, trace: Option<&str>, nonce: i64, deadline: i64) -> Uuid {
    let id = pending_redemption(pool, amount).await;
    sqlx::query(
        "UPDATE redemption_intents
            SET status = 'payout_submitted', payout_submitted_at = now(),
                payout_trace_id = $2, payout_permit_nonce = $3, payout_permit_deadline = $4
          WHERE id = $1",
    )
    .bind(id)
    .bind(trace)
    .bind(nonce)
    .bind(deadline)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// The float's confirmed USDT history: one GasFree payout is two transfers from the float in one
/// transaction, the redeemer's and the relay's fee.
async fn mount_float_history(server: &MockServer, tx_id: &str, to: &str, value: i64) {
    let at = chrono::Utc::now().timestamp_millis();
    Mock::given(method("GET"))
        .and(path(format!("/v1/accounts/{FLOAT}/transactions/trc20")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [
            {"transaction_id": tx_id, "from": FLOAT, "to": to, "value": value.to_string(), "type": "Transfer",
             "token_info": {"address": USDT}, "block_timestamp": at},
            {"transaction_id": tx_id, "from": FLOAT, "to": "TLntW9Z59LYY5KEi9cmwk3PKjQga828ird", "value": "300000",
             "type": "Transfer", "token_info": {"address": USDT}, "block_timestamp": at},
        ]})))
        .mount(server)
        .await;
}

async fn mount_float_nonce(server: &MockServer, nonce: u64) {
    Mock::given(method("POST"))
        .and(path("/wallet/triggerconstantcontract"))
        .and(body_string_contains("nonces(address)"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"constant_result": [format!("{nonce:064x}")]})),
        )
        .mount(server)
        .await;
}

/// (status, payout_ref, payout_permit_nonce)
async fn state_of(pool: &PgPool, id: Uuid) -> (String, Option<String>, Option<i64>) {
    sqlx::query_as("SELECT status, payout_ref, payout_permit_nonce FROM redemption_intents WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn payout_alerts(pool: &PgPool, severity: &str, containing: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM alerts WHERE source = 'payout' AND severity = $1 AND message LIKE '%' || $2 || '%'",
    )
    .bind(severity)
    .bind(containing)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_gasfree_payout_answers_are_read_field_by_field() {
    for (body, want) in [
        (
            serde_json::json!({"status": "submitted", "trace_id": PAYOUT_TRACE, "nonce": 4, "deadline": 1_790_000_000u64}),
            PayoutReply::Submitted { trace_id: PAYOUT_TRACE.into(), nonce: 4, deadline: 1_790_000_000 },
        ),
        (
            serde_json::json!({"status": "refused", "reason": "the relay refused the payout permit: NonceNotMatchException",
                               "nonce": 4, "deadline": 1_790_000_000u64}),
            PayoutReply::RelayRefused {
                reason: "the relay refused the payout permit: NonceNotMatchException".into(),
                nonce: 4,
                deadline: 1_790_000_000,
            },
        ),
        (
            serde_json::json!({"status": "float_not_active", "float_address": FLOAT}),
            PayoutReply::FloatNotActive { float_address: FLOAT.into() },
        ),
    ] {
        let (_s, signer) = signer_replying(200, body.clone()).await;
        assert_eq!(signer.pay(Uuid::new_v4(), REDEEMER, 5).await, want, "{body}");
    }

    // A permit this service could not follow is not a clear answer.
    for body in [
        serde_json::json!({"status": "submitted", "trace_id": PAYOUT_TRACE}),
        serde_json::json!({"status": "refused", "reason": "x", "nonce": 4}),
    ] {
        let (_s, signer) = signer_replying(200, body.clone()).await;
        let reply = signer.pay(Uuid::new_v4(), REDEEMER, 5).await;
        assert!(matches!(reply, PayoutReply::Ambiguous(_)), "{body} gave {reply:?}");
    }

    // The float's owner, from the signer's /internal/xpub.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/internal/xpub"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "account_xpub": "xpub-unused", "fee_address": "TFee", "payout_address": FLOAT_OWNER, "payout_gasfree_address": FLOAT,
        })))
        .mount(&server)
        .await;
    let signer = HttpPayoutSigner { http: reqwest::Client::new(), base_url: server.uri(), token: "t".into() };
    assert_eq!(signer.float_owner().await, Ok((FLOAT_OWNER.to_string(), Some(FLOAT.to_string()))));
}

/// One GasFree payout permit alive at a time: a second would carry the same nonce.
#[tokio::test]
async fn a_gasfree_payout_permit_holds_every_other_payout_until_it_is_settled() {
    let pool = pool().await;
    let first = pending_redemption(&pool, 10_000_000).await;
    let second = pending_redemption(&pool, 5_000_000).await;
    let signer = counting(PayoutReply::Submitted { trace_id: PAYOUT_TRACE.into(), nonce: 4, deadline: (now() + 180) as u64 });
    let cfg = gasfree_config("http://unused".into());

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
    let (trace, nonce): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT payout_trace_id, payout_permit_nonce FROM redemption_intents WHERE id = $1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((trace.as_deref(), nonce), (Some(PAYOUT_TRACE), Some(4)));
    assert_eq!(state_of(&pool, second).await.0, "payout_pending");
}

#[tokio::test]
async fn a_gasfree_payout_is_paid_once_its_transfer_from_the_float_is_confirmed() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_float_history(&server, "tx-gf-payout", REDEEMER, 10_000_000).await;
    let id = permit_out(&pool, 10_000_000, Some(PAYOUT_TRACE), 4, now() + 180).await;

    let paid = payout::confirm_gasfree_payouts_once(
        &pool,
        &gasfree_config(server.uri()),
        &nile(),
        &TronClient::new(server.uri(), "k".into()),
        &GasFreeReads { txn_hash: Some("tx-gf-payout") },
    )
    .await
    .unwrap();

    assert_eq!(paid, 1);
    assert_eq!(state_of(&pool, id).await, ("paid".into(), Some("tx-gf-payout".into()), Some(4)));
    let withdrawn: i64 = sqlx::query_scalar(
        "SELECT amount_usdt FROM treasury_events WHERE intent_id = $1 AND kind = 'custody_withdrawal'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(withdrawn, 10_000_000);
}

#[tokio::test]
async fn a_transfer_that_does_not_pay_this_redemption_is_not_taken_as_its_payment() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_float_history(&server, "tx-gf-payout", REDEEMER, 9_999_999).await;
    let id = permit_out(&pool, 10_000_000, Some(PAYOUT_TRACE), 4, now() + 180).await;

    let paid = payout::confirm_gasfree_payouts_once(
        &pool,
        &gasfree_config(server.uri()),
        &nile(),
        &TronClient::new(server.uri(), "k".into()),
        &GasFreeReads { txn_hash: Some("tx-gf-payout") },
    )
    .await
    .unwrap();

    assert_eq!(paid, 0);
    assert_eq!(state_of(&pool, id).await, ("payout_submitted".into(), None, Some(4)));
}

/// After the deadline, a refused permit that did not run never can: the redemption is paid again.
#[tokio::test]
async fn a_refused_permit_goes_back_to_be_paid_once_its_deadline_passed_and_it_never_ran() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_float_nonce(&server, 4).await;
    let id = permit_out(&pool, 10_000_000, None, 4, now() - 120).await;

    payout::confirm_gasfree_payouts_once(
        &pool,
        &gasfree_config(server.uri()),
        &nile(),
        &TronClient::new(server.uri(), "k".into()),
        &GasFreeReads { txn_hash: None },
    )
    .await
    .unwrap();

    assert_eq!(state_of(&pool, id).await, ("payout_pending".into(), None, None));
}

/// The nonce moved and no transfer paying this redemption was found: it may be paid, so it goes to a
/// human and no longer holds the float.
#[tokio::test]
async fn a_permit_whose_nonce_moved_without_a_transfer_is_left_for_a_human() {
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_float_nonce(&server, 5).await;
    let id = permit_out(&pool, 10_000_000, None, 4, now() - 120).await;

    payout::confirm_gasfree_payouts_once(
        &pool,
        &gasfree_config(server.uri()),
        &nile(),
        &TronClient::new(server.uri(), "k".into()),
        &GasFreeReads { txn_hash: None },
    )
    .await
    .unwrap();

    assert_eq!(state_of(&pool, id).await, ("payout_submitted".into(), None, None));
    assert_eq!(payout_alerts(&pool, "p1", "may have been paid").await, 1);
}

#[tokio::test]
async fn a_relay_refusal_is_not_retried_before_its_deadline() {
    let pool = pool().await;
    let id = pending_redemption(&pool, 10_000_000).await;
    let signer = counting(PayoutReply::RelayRefused {
        reason: "the relay refused the payout permit: NonceNotMatchException".into(),
        nonce: 4,
        deadline: (now() + 180) as u64,
    });
    let cfg = gasfree_config("http://unused".into());

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1, "the refused permit is valid until its deadline");
    assert_eq!(state_of(&pool, id).await, ("payout_submitted".into(), None, Some(4)));
}

/// Spec §4: until the float's one-time activation, redemptions are "not available yet".
#[tokio::test]
async fn redemptions_wait_for_the_gasfree_floats_activation_and_it_is_said_once() {
    let pool = pool().await;
    let first = pending_redemption(&pool, 10_000_000).await;
    let second = pending_redemption(&pool, 5_000_000).await;
    let signer = counting(PayoutReply::FloatNotActive { float_address: FLOAT.into() });
    let cfg = gasfree_config("http://unused".into());

    for _ in 0..3 {
        payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    }

    assert_eq!(signer.calls.load(Ordering::SeqCst), 3, "one call per pass: every redemption would get the same answer");
    assert_eq!(state_of(&pool, first).await.0, "payout_pending");
    assert_eq!(state_of(&pool, second).await.0, "payout_pending");
    assert_eq!(payout_alerts(&pool, "warn", "not available yet").await, 1);
}

/// An answer that may have sent a permit, with its nonce unknown: nothing else is signed until that
/// permit could no longer run.
#[tokio::test]
async fn an_ambiguous_gasfree_payout_holds_the_float_for_the_longest_deadline() {
    let pool = pool().await;
    let first = pending_redemption(&pool, 10_000_000).await;
    pending_redemption(&pool, 5_000_000).await;
    let signer = counting(PayoutReply::Ambiguous("the relay gave no clear answer".into()));
    let cfg = gasfree_config("http://unused".into());

    payout::drain_once(&pool, &cfg, &signer).await.unwrap();
    payout::drain_once(&pool, &cfg, &signer).await.unwrap();

    assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
    let deadline: Option<i64> =
        sqlx::query_scalar("SELECT payout_permit_deadline FROM redemption_intents WHERE id = $1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    let held = deadline.expect("a deadline holds the float") - now();
    assert!((590..=610).contains(&held), "held for about 600 s, got {held}");
}

/// Spec §4: a redemption is refused before anything exists to burn against while the float's next
/// transfer would also pay its activation.
#[tokio::test]
async fn a_redemption_is_refused_until_the_gasfree_float_is_activated() {
    let pool = pool().await;
    let request = || {
        Request::builder()
            .method("POST")
            .uri("/internal/redemption-intents")
            .header("authorization", "Bearer i")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "redeemer_address": "0xaaaa000000000000000000000000000000000009",
                    "payout_address": REDEEMER,
                    "amount_clt": 10_000_000,
                })
                .to_string(),
            ))
            .unwrap()
    };

    let never_moved = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/wallet/getcontract"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&never_moved)
        .await;
    let app = treasury_service::api::router(pool.clone(), gasfree_config(never_moved.uri()));
    assert_eq!(app.oneshot(request()).await.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM redemption_intents").fetch_one(&pool).await.unwrap();
    assert_eq!(rows, 0, "nothing exists to burn against");

    let activated = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/wallet/getcontract"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"contract_address": "41ab", "bytecode": ""})))
        .mount(&activated)
        .await;
    let app = treasury_service::api::router(pool.clone(), gasfree_config(activated.uri()));
    assert_eq!(app.oneshot(request()).await.unwrap().status(), StatusCode::CREATED);
}
```

- [ ] **Step 4: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test(treasury): GasFree payouts, against stubs

The payout tests for the GasFree float: the signer's new answers, one
permit alive at a time, paid only when the transfer from the float to
the redeemer is confirmed on chain, a relay refusal settled by the
float's nonce after the deadline, an unclear answer holding the float
for the longest deadline, and redemptions refused until the float is
activated. The new answers' handling and the settlement are todo!().

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/treasury-service/migrations/0016_gasfree_payouts.sql crates/treasury-service/src crates/treasury-service/tests
git commit -F .superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/commit-msg.txt
```

Controller: run CI. Expected: the run fails; these 10 fail by name, and every test that existed before this task is `ok`:

```text
test a_gasfree_payout_is_paid_once_its_transfer_from_the_float_is_confirmed ... FAILED
test a_gasfree_payout_permit_holds_every_other_payout_until_it_is_settled ... FAILED
test a_permit_whose_nonce_moved_without_a_transfer_is_left_for_a_human ... FAILED
test a_redemption_is_refused_until_the_gasfree_float_is_activated ... FAILED
test a_refused_permit_goes_back_to_be_paid_once_its_deadline_passed_and_it_never_ran ... FAILED
test a_relay_refusal_is_not_retried_before_its_deadline ... FAILED
test a_transfer_that_does_not_pay_this_redemption_is_not_taken_as_its_payment ... FAILED
test an_ambiguous_gasfree_payout_holds_the_float_for_the_longest_deadline ... FAILED
test redemptions_wait_for_the_gasfree_floats_activation_and_it_is_said_once ... FAILED
test the_gasfree_payout_answers_are_read_field_by_field ... FAILED
```

- [ ] **Step 5: Replace the stubs**

In `crates/treasury-service/src/tron_verifier.rs`, replace the bodies of `has_contract` and `confirmed_transfer`:

```rust
    pub async fn has_contract(&self, address: &str) -> Result<bool, String> {
        let resp = self
            .http
            .post(format!("{}/wallet/getcontract", self.base_url))
            .header("TRON-PRO-API-KEY", &self.api_key)
            .json(&serde_json::json!({"value": address, "visible": true}))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("trongrid getcontract failed: {status} {text}"));
        }
        let parsed: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        if parsed["contract_address"].as_str().is_some_and(|a| !a.is_empty()) {
            return Ok(true);
        }
        if parsed.as_object().is_some_and(|o| o.is_empty()) {
            return Ok(false);
        }
        Err(format!("getcontract for {address} gave neither a contract nor {{}}: {parsed}"))
    }

    pub async fn confirmed_transfer(
        &self,
        tx_id: &str,
        from: &str,
        to: &str,
        usdt_contract: &str,
        amount: i64,
        since_ms: i64,
    ) -> Result<bool, String> {
        let transfers = self.trc20_transfers(from, usdt_contract, Some(since_ms)).await?;
        Ok(transfers.iter().any(|t| {
            t.transaction_id.eq_ignore_ascii_case(tx_id)
                && t.event_type == TRC20_TRANSFER_EVENT
                && t.from == from
                && t.to == to
                && t.token_info.address == usdt_contract
                && t.value.parse::<i64>() == Ok(amount)
        }))
    }
```

In `crates/treasury-service/src/payout.rs`:

a) In `HttpPayoutSigner::pay`, replace:

```rust
            Some("needs_trx") => PayoutReply::NeedsTrx,
            // The signer proved this happened before it ever attempted a broadcast (a bad
            // recipient, a key derivation failure, a TronGrid read that never got a response) —
            // see sweep.rs's `PayoutOutcome::Refused` doc comment for exactly which class this is.
            Some("refused") => PayoutReply::Refused(
                body["reason"].as_str().unwrap_or("signer reported refused with no reason").to_string(),
            ),
```

with:

```rust
            Some("needs_trx") => PayoutReply::NeedsTrx,
            // A GasFree permit is with the relay. Without its trace id, nonce and deadline nothing
            // here could follow it, so it is not a clear answer.
            Some("submitted") => match (body["trace_id"].as_str(), body["nonce"].as_u64(), body["deadline"].as_u64()) {
                (Some(trace_id), Some(nonce), Some(deadline)) => {
                    PayoutReply::Submitted { trace_id: trace_id.to_string(), nonce, deadline }
                }
                _ => PayoutReply::Ambiguous(format!("signer reported submitted without the permit's trace id, nonce and deadline: {body}")),
            },
            Some("float_not_active") => PayoutReply::FloatNotActive {
                float_address: body["float_address"].as_str().unwrap_or("unknown").to_string(),
            },
            // Without a nonce and a deadline: the signer proved this happened before it ever
            // attempted a broadcast (a bad recipient, a key derivation failure, a TronGrid read that
            // never got a response) — see sweep.rs's `PayoutOutcome::Refused`. With both: the relay
            // refused a signed GasFree permit (`PayoutOutcome::RelayRefused`). With one of the two,
            // the answer is not one the signer gives, so it is not a clear one.
            Some("refused") => {
                let reason = body["reason"].as_str().unwrap_or("signer reported refused with no reason").to_string();
                match (body["nonce"].as_u64(), body["deadline"].as_u64()) {
                    (None, None) => PayoutReply::Refused(reason),
                    (Some(nonce), Some(deadline)) => PayoutReply::RelayRefused { reason, nonce, deadline },
                    _ => PayoutReply::Ambiguous(format!("signer reported a refusal with half a permit: {body}")),
                }
            }
```

b) Replace the body of `HttpPayoutSigner::float_owner`:

```rust
    async fn float_owner(&self) -> Result<(String, Option<String>), String> {
        let resp = self
            .http
            .get(format!("{}/internal/xpub", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("signer unreachable: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("signer returned {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| format!("unreadable signer response: {e}"))?;
        let owner = body["payout_address"]
            .as_str()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| format!("the signer named no payout address: {body}"))?;
        Ok((owner.to_string(), body["payout_gasfree_address"].as_str().map(str::to_string)))
    }
```

c) Add after the `failed_transfer_alerted` function:

```rust
/// The signer refuses `APP_GASFREE_DEADLINE_SECS` above 600, so no payout permit it signs is valid
/// for longer than this after it was sent.
const LONGEST_PERMIT_SECS: i64 = 600;

/// After a permit's deadline the controller refuses it, but a block at the deadline may take this
/// long to show in what TronGrid answers.
const DEADLINE_GRACE_SECS: i64 = 60;
```

d) In `drain_once`, replace:

```rust
    if halted {
        tracing::warn!(halt_reason, "payouts blocked: treasury is halted");
        return Ok(0);
    }
```

with:

```rust
    if halted {
        tracing::warn!(halt_reason, "payouts blocked: treasury is halted");
        return Ok(0);
    }

    // GasFree: one payout permit alive at a time. While a permit may still run, a second would carry
    // the same nonce, and after a refusal the chain could no longer say which of the two ran. So
    // nothing is signed until every permit is settled by `confirm_gasfree_payouts_once`, or, for one
    // whose nonce is unknown, until it can no longer run. On the TRX rail no row carries a permit.
    let (permit_in_doubt,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM redemption_intents
                         WHERE status = 'payout_submitted' AND payout_ref IS NULL
                           AND (payout_permit_nonce IS NOT NULL OR payout_permit_deadline > $1))",
    )
    .bind(chrono::Utc::now().timestamp())
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;
    if permit_in_doubt {
        return Ok(0);
    }
```

e) Replace the stub arm added in Step 2:

```rust
            PayoutReply::Submitted { .. } | PayoutReply::RelayRefused { .. } | PayoutReply::FloatNotActive { .. } => {
                todo!("Task 4 Step 5")
            }
```

with:

```rust
            PayoutReply::Submitted { trace_id, nonce, deadline } => {
                // The float may pay from now on, so it counts against today's budget now.
                day_total += amount_clt;
                if let Err(e) = sqlx::query(
                    "UPDATE redemption_intents
                        SET payout_trace_id = $2, payout_permit_nonce = $3, payout_permit_deadline = $4, updated_at = now()
                      WHERE id = $1",
                )
                .bind(intent_id)
                .bind(&trace_id)
                .bind(nonce as i64)
                .bind(deadline as i64)
                .execute(pool)
                .await
                {
                    alert(pool, "p1", "payout", &format!(
                        "redemption {intent_id}: GasFree permit {trace_id} (nonce {nonce}, valid until {deadline}) is \
                         with the relay, but recording it failed ({e}). Left payout_submitted with no payout_ref: find \
                         the float's transfer for that trace and set payout_ref by hand, or, after the deadline, return \
                         it to payout_pending if the float's nonce is still {nonce}."
                    )).await;
                }
                processed += 1;
                break; // one permit at a time
            }
            PayoutReply::RelayRefused { reason, nonce, deadline } => {
                // Refused on the relay's word; the signed permit stays valid until its deadline, so it
                // may still pay and counts against today's budget.
                day_total += amount_clt;
                match sqlx::query(
                    "UPDATE redemption_intents
                        SET payout_trace_id = NULL, payout_permit_nonce = $2, payout_permit_deadline = $3, updated_at = now()
                      WHERE id = $1",
                )
                .bind(intent_id)
                .bind(nonce as i64)
                .bind(deadline as i64)
                .execute(pool)
                .await
                {
                    Ok(_) => alert(pool, "warn", "payout", &format!(
                        "redemption {intent_id}: {reason}. The signed permit stays valid until {deadline}, so nothing \
                         is paid before then; after it, the float's nonce says whether it ran."
                    )).await,
                    Err(e) => alert(pool, "p1", "payout", &format!(
                        "redemption {intent_id}: {reason}, and recording the refused permit (nonce {nonce}, valid until \
                         {deadline}) failed ({e}). Left payout_submitted: after the deadline, return it to \
                         payout_pending by hand if the float's nonce is still {nonce}."
                    )).await,
                }
                break; // one permit at a time
            }
            PayoutReply::FloatNotActive { float_address } => {
                if let Err(e) = sqlx::query(
                    "UPDATE redemption_intents SET status = 'payout_pending', updated_at = now()
                     WHERE id = $1 AND status = 'payout_submitted'",
                )
                .bind(intent_id)
                .execute(pool)
                .await
                {
                    alert(pool, "p1", "payout", &format!(
                        "redemption {intent_id}: the signer proved nothing was signed (the GasFree float is not \
                         activated), but returning it to payout_pending failed ({e}). It carries no payout_ref, so it \
                         is safe to move back by hand."
                    )).await;
                }
                alert_once(
                    pool,
                    "warn",
                    "payout",
                    &format!(
                        "redemptions are not available yet: the GasFree float {float_address} has never made a transfer, \
                         so its next one would also pay its activation. Its one-time activation comes first."
                    ),
                    chrono::Duration::hours(1),
                )
                .await;
                break; // every redemption would get the same answer
            }
```

f) Replace the `PayoutReply::Ambiguous(msg) => { … }` arm — from `PayoutReply::Ambiguous(msg) => {` through its closing brace; the comment line above it stays — with:

```rust
            PayoutReply::Ambiguous(msg) => {
                // Counts against today's budget: it might have spent real float capacity, and
                // daily_payout_total counts every payout_submitted row as spent from the next
                // pass onward regardless — this just makes the CURRENT pass agree with that.
                day_total += amount_clt;
                // On the GasFree rail a permit may be with the relay. It cannot run past the longest
                // deadline the signer signs, so no other permit is signed before then.
                let gasfree_payouts = config.gasfree.as_ref().is_some_and(|s| s.rail);
                if gasfree_payouts {
                    if let Err(e) = sqlx::query("UPDATE redemption_intents SET payout_permit_deadline = $2 WHERE id = $1")
                        .bind(intent_id)
                        .bind(chrono::Utc::now().timestamp() + LONGEST_PERMIT_SECS)
                        .execute(pool)
                        .await
                    {
                        tracing::error!(%intent_id, "could not hold the float after an unclear GasFree payout: {e}");
                    }
                }
                alert(pool, "p1", "payout", &format!(
                    "redemption {intent_id}: payout outcome UNKNOWN ({msg}). Left payout_submitted \
                     and NOT retried — retrying could pay this burn twice. Claimed at {claimed_at}: \
                     check the payout float ({float}) for an outbound USDT transfer of {payout_amount_usdt} \
                     (micro-USDT: the quoted net, below the {amount_clt} burned when a fee is set) to {payout_address} around that time. Found it? Set \
                     payout_ref to that tx hash — confirm_payouts_once will pick it up from there. \
                     Found nothing? Return the intent to payout_pending by hand.",
                    float = config.payout_float_address
                )).await;
                if gasfree_payouts {
                    break; // one permit at a time
                }
            }
```

g) Replace the body of `confirm_gasfree_payouts_once`:

```rust
pub async fn confirm_gasfree_payouts_once(
    pool: &PgPool,
    config: &AppConfig,
    settings: &gasfree::Settings,
    client: &TronClient,
    signer: &dyn PayoutSigner,
) -> Result<u32, String> {
    let rows: Vec<(Uuid, String, i64, Option<String>, i64, i64, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT id, payout_address, payout_amount_usdt, payout_trace_id, payout_permit_nonce,
                payout_permit_deadline, payout_submitted_at
         FROM redemption_intents
         WHERE status = 'payout_submitted' AND payout_ref IS NULL AND payout_permit_nonce IS NOT NULL
         ORDER BY payout_submitted_at",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut paid = 0u32;
    for (intent_id, to, amount, trace_id, nonce, deadline, submitted_at) in rows {
        // The relay's record names the transaction; the chain says whether it paid THIS redemption.
        if let Some(trace_id) = &trace_id {
            match signer.trace(trace_id).await {
                Ok(Trace { txn_hash: Some(hash), .. }) => {
                    // Ten minutes before the claim, for clock skew between this host and the chain.
                    let since_ms = (submitted_at - chrono::Duration::minutes(10)).timestamp_millis();
                    match client
                        .confirmed_transfer(&hash, &config.payout_float_address, &to, &config.usdt_contract, amount, since_ms)
                        .await
                    {
                        Ok(true) => {
                            match pay_intent(pool, intent_id, amount, &hash).await {
                                Ok(()) => paid += 1,
                                Err(e) => {
                                    alert(pool, "p1", "payout", &format!(
                                        "redemption {intent_id}: GasFree payout {hash} is confirmed on chain, but recording \
                                         it as paid failed ({e}). Safe to retry: the next pass picks it up."
                                    )).await;
                                }
                            }
                            continue;
                        }
                        Ok(false) => {} // not confirmed yet, or not a transfer that pays this redemption
                        Err(e) => {
                            tracing::warn!(%intent_id, %hash, "could not read the float's transfers: {e}");
                            continue;
                        }
                    }
                }
                Ok(_) => {} // no transaction yet
                Err(e) => tracing::warn!(%intent_id, %trace_id, "the relay's record of the payout permit is unreadable: {e}"),
            }
        }

        // Until its deadline, and a margin for the last block, the permit may still run.
        if chrono::Utc::now().timestamp() <= deadline + DEADLINE_GRACE_SECS {
            continue;
        }
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
                    chrono::Duration::hours(1),
                )
                .await;
                continue;
            }
            Err(e) => {
                tracing::warn!(%intent_id, "could not ask the signer for the float's owner: {e}");
                continue;
            }
        };
        let chain_nonce = match client.gasfree_nonce(settings.chain.controller, &owner).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(%intent_id, "the float's nonce is unreadable: {e}");
                continue;
            }
        };
        if chain_nonce <= nonce as u64 {
            // It can no longer run, and it did not: the redemption is paid again on a later pass.
            match sqlx::query(
                "UPDATE redemption_intents
                    SET status = 'payout_pending', payout_trace_id = NULL, payout_permit_nonce = NULL,
                        payout_permit_deadline = NULL, updated_at = now()
                  WHERE id = $1 AND status = 'payout_submitted' AND payout_ref IS NULL",
            )
            .bind(intent_id)
            .execute(pool)
            .await
            {
                Ok(_) => tracing::info!(%intent_id, "GasFree payout permit (nonce {nonce}) expired without running; paid again on a later pass"),
                Err(e) => tracing::error!(%intent_id, "could not return an expired GasFree payout to payout_pending: {e}"),
            }
        } else {
            // One permit at a time, so the nonce moved because this permit ran, or because something
            // else spent from the float (its activation, or a hand-made transfer). No transfer paying
            // this redemption was found either way, so it may be paid, and must not be paid again.
            alert(pool, "p1", "payout", &format!(
                "redemption {intent_id}: the GasFree float's nonce moved past this payout's permit (nonce {nonce}), but \
                 no confirmed transfer of {amount} micro-USDT from the float to {to} was found{}. It may have been paid. \
                 Left payout_submitted and NOT retried: find the float's transfer to {to} and set payout_ref, or return \
                 it to payout_pending by hand.",
                trace_id.as_deref().map(|t| format!(" (trace {t})")).unwrap_or_default()
            )).await;
            // A human's now: no longer a permit this service settles, or one that holds the float.
            if let Err(e) = sqlx::query("UPDATE redemption_intents SET payout_permit_nonce = NULL WHERE id = $1")
                .bind(intent_id)
                .execute(pool)
                .await
            {
                tracing::error!(%intent_id, "could not hand the GasFree payout to a human: {e}");
            }
        }
    }
    Ok(paid)
}
```

In `crates/treasury-service/src/api.rs`, in `create_redemption_intent_handler`, replace:

```rust
    let Some(payout_amount_usdt) = intents::net_payout(body.amount_clt, state.config.redemption_fee_usdt)
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
```

with:

```rust
    let Some(payout_amount_usdt) = intents::net_payout(body.amount_clt, state.config.redemption_fee_usdt)
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    // GasFree payouts (spec §4): until the float has made its first transfer, its next one would also
    // pay its activation, which a redemption's fee does not cover. Refused here, before anything
    // exists to burn against, and 503 because it is "not yet", not "never". The orchestrator shows it
    // as redemptions not being available yet.
    if state.config.gasfree.as_ref().is_some_and(|s| s.rail) {
        let client = crate::tron_verifier::TronClient::new(
            state.config.trongrid_url.clone(),
            state.config.trongrid_api_key.clone(),
        );
        match client.has_contract(&state.config.payout_float_address).await {
            Ok(true) => {}
            Ok(false) => return Err(StatusCode::SERVICE_UNAVAILABLE),
            Err(e) => {
                tracing::warn!("redemption refused: could not read whether the GasFree float is activated: {e}");
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
        }
    }
```

In `crates/treasury-service/src/main.rs`, replace:

```rust
                if let Err(e) = payout::confirm_payouts_once(&pool, &tron_client).await {
                    tracing::error!("payout confirmation failed: {e}");
                }
```

with:

```rust
                if let Err(e) = payout::confirm_payouts_once(&pool, &tron_client).await {
                    tracing::error!("payout confirmation failed: {e}");
                }
                if let Some(settings) = &cfg.gasfree {
                    if let Err(e) =
                        payout::confirm_gasfree_payouts_once(&pool, &cfg, settings, &tron_client, &payout_signer).await
                    {
                        tracing::error!("GasFree payout confirmation failed: {e}");
                    }
                }
```

Do not change the tests.

- [ ] **Step 6: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(treasury): pay redemptions from the GasFree float

A GasFree payout is one permit at a time. Its trace leads to its
transaction, and it is paid only when that transaction's transfer from
the float to the redeemer, of exactly the quoted amount, is confirmed
on chain. A relay refusal is left until the permit's deadline, and then
the float's nonce decides: unmoved, the redemption is paid again; moved
with no such transfer found, it goes to a human. An unclear answer
holds the float for 600 seconds. Until the float is activated, a new
redemption answers 503 and a queued one waits, said once an hour.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/treasury-service/src
git commit -F .superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/commit-msg.txt
```

Controller: run CI. Expected: success; the 10 tests above say `ok` by name, every earlier test says `ok`, and there is no warning located in `crates/treasury-service`.

---

### Task 5: The orchestrator hands out GasFree addresses

**Files:**
- Modify: `crates/payment-orchestrator/Cargo.toml`, `Cargo.lock`
- Modify: `crates/payment-orchestrator/src/configuration.rs`
- Modify: `crates/payment-orchestrator/src/lib.rs`
- Create: `crates/payment-orchestrator/migrations/0014_gasfree_addresses.sql`
- Create: `crates/payment-orchestrator/src/gasfree_chain.rs`
- Modify: `crates/payment-orchestrator/src/addresses.rs`
- Modify: `crates/payment-orchestrator/src/api.rs`
- Modify: `crates/payment-orchestrator/src/main.rs`
- Modify: `crates/payment-orchestrator/src/redemptions.rs`
- Modify: `crates/payment-orchestrator/src/treasury_bridge.rs`
- Modify: tests `db_addresses.rs`, `db_deposit_api.rs`, `db_redemptions.rs`, `db_bridge.rs`
- Test: `crates/payment-orchestrator/src/gasfree_chain.rs` (inline `mod tests`), `tests/db_addresses.rs`, `tests/db_deposit_api.rs`, `tests/db_redemptions.rs`

**Interfaces:**
- Consumes: `gasfree::{Settings, load_settings, fee_to_hold, gasfree_address, NILE}` (Task 1, Plans 1-2); the treasury's 503 for a redemption while the float is not activated (Task 4).
- Produces:
  - `OrchConfig.gasfree: Option<gasfree::Settings>` (`#[serde(skip)]`, set in `OrchConfig::load`)
  - `payment_orchestrator::gasfree_chain::{GasFreeChain, SelfTest}`: `GasFreeChain::new(base_url: String, api_key: String) -> Self`, `has_contract(&self, address: &str) -> Result<bool, String>`, `code_changed(&self, settings: &gasfree::Settings) -> Result<Option<String>, String>`, `self_test(&self, settings: &gasfree::Settings, deriver: &AddressDeriver) -> SelfTest`; `pub enum SelfTest { Passed, Failed(String), Unreachable(String) }` (derives `Debug, PartialEq`)
  - `addresses::address_for_user(pool, deriver, gasfree_for_new_users: Option<&'static gasfree::Chain>, user_pk, clt_address) -> Result<(String, bool), String>` and `pub async fn addresses::existing(pool, user_pk) -> Result<Option<(String, bool)>, String>`
  - `AppState.gasfree_chain: Arc<GasFreeChain>`
  - column `deposit_addresses.gasfree BOOLEAN NOT NULL DEFAULT FALSE`
  - `POST /api/v1/deposits` answers `{"address", "fee_up_to_usdt", "min_deposit_usdt"}` for a GasFree address, and `{"address"}` as before for a plain one

- [ ] **Step 1: The dependency, the setting, the migration and the module skeleton**

In `crates/payment-orchestrator/Cargo.toml`, add after the `bip32` dependency (its comment block and its line):

```toml
# GasFree addresses (addresses.rs, gasfree_chain.rs): G = gasfree(D), computed from public constants,
# so this key-free service can hand one out. Pure: the same one copy the signer and the treasury use.
gasfree = { path = "../gasfree" }
```

In `Cargo.lock`, in the `payment-orchestrator` package's `dependencies` list, add ` "gasfree",` between ` "dotenv",` and ` "hex",`.

In `crates/payment-orchestrator/src/configuration.rs`, add as the last field of `OrchConfig`, after `pub rate_limit_per_minute: u32,`:

```rust
    /// GasFree (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md), read by `load`
    /// from the environment with `gasfree::load_settings`, never from TOML. `None`, the default, is
    /// the TRX rail exactly as before.
    #[serde(skip)]
    pub gasfree: Option<gasfree::Settings>,
```

In `OrchConfig::load`, change `let cfg: Self = Config::builder()` to `let mut cfg: Self = Config::builder()`, and replace the final `Ok(cfg)` with:

```rust
        // From the environment only, like the secrets above: the three services read the same
        // variables from one env file (spec §6), and a half-set rail stops the service here.
        cfg.gasfree = gasfree::load_settings(|name| std::env::var(name).ok()).unwrap_or_else(|e| panic!("{e}"));
        Ok(cfg)
```

In `crates/payment-orchestrator/src/lib.rs`, add `pub mod gasfree_chain;` after `pub mod derive;`.

Create `crates/payment-orchestrator/migrations/0014_gasfree_addresses.sql`:

```sql
-- Whether a user's permanent deposit address is a GasFree account (spec §1). Set once, when the
-- address is issued: new users get one while APP_TRANSFER_RAIL=gasfree, and a user keeps the kind of
-- address they were given, like the address itself (spec §5). The deposit route reads it to put
-- GasFree's tripwire and its fee in front of the address.
ALTER TABLE deposit_addresses ADD COLUMN gasfree BOOLEAN NOT NULL DEFAULT FALSE;
```

Create `crates/payment-orchestrator/src/gasfree_chain.rs`. The three public reads are `todo!()`; Step 4 replaces them.

```rust
//! What the deposit route asks the chain about GasFree (spec §1, §5), over TronGrid.
//!
//! No key and no relay: the orchestrator computes a user's GasFree address itself, with the shared
//! `gasfree` crate, and asks the chain only whether GasFree's code is still the reviewed code,
//! whether an account is activated, and — once, at boot — whether the controller agrees with the
//! derivation.

use crate::derive::AddressDeriver;

pub struct GasFreeChain {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

/// What the boot check found.
#[derive(Debug, PartialEq)]
pub enum SelfTest {
    Passed,
    /// The chain answered, and the answer is wrong for these settings. The service must not start.
    Failed(String),
    /// TronGrid gave no usable answer. Not fatal: the code check before each GasFree address still runs.
    Unreachable(String),
}

impl GasFreeChain {
    pub fn new(base_url: String, api_key: String) -> Self {
        // Bounded: the deposit route waits on these reads.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client builder");
        Self { http, base_url, api_key }
    }

    /// Whether `address` holds a deployed contract; for a GasFree account, whether it is activated.
    /// Only `{}` means no: an activated GasFree account answers with its contract record and an
    /// EMPTY bytecode (2026-09-24), and any other answer is an error, never "no".
    pub async fn has_contract(&self, address: &str) -> Result<bool, String> {
        todo!("Task 5 Step 4")
    }

    /// Why no GasFree address may be handed out, when GasFree's code is not the reviewed code;
    /// `None` when it is (spec §5). Both proxies: the beacon behind every account, and the
    /// controller that moves money out of them.
    pub async fn code_changed(&self, settings: &gasfree::Settings) -> Result<Option<String>, String> {
        todo!("Task 5 Step 4")
    }

    /// Once at boot: are these GasFree constants the ones deployed where this TronGrid points, and
    /// does the controller put index 0's account where this service derives it? A Nile setting on a
    /// mainnet TronGrid fails here, instead of showing users addresses nobody controls.
    pub async fn self_test(&self, settings: &gasfree::Settings, deriver: &AddressDeriver) -> SelfTest {
        todo!("Task 5 Step 4")
    }
}
```

In `crates/payment-orchestrator/src/api.rs`:

a) Add `use crate::gasfree_chain::GasFreeChain;` after `use crate::derive::AddressDeriver;`.

b) In `pub struct AppState`, add after the `limiter` field:

```rust
    /// GasFree's reads: the tripwire and the activation record in front of a GasFree address.
    pub gasfree_chain: Arc<GasFreeChain>,
```

c) In `pub fn router`, replace:

```rust
    let state = AppState { pool, config, deriver, limiter };
```

with:

```rust
    let gasfree_chain = Arc::new(GasFreeChain::new(config.trongrid_url.clone(), config.trongrid_api_key.clone()));
    let state = AppState { pool, config, deriver, limiter, gasfree_chain };
```

d) In `create_deposit_handler`, replace:

```rust
    let address =
        addresses::address_for_user(&state.pool, state.deriver.as_ref(), &user_pk, &clt_address)
            .await
            .map_err(|e| {
                tracing::error!("deposit address for {user_pk}: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
```

with:

```rust
    let (address, _) =
        addresses::address_for_user(&state.pool, state.deriver.as_ref(), None, &user_pk, &clt_address)
            .await
            .map_err(|e| {
                tracing::error!("deposit address for {user_pk}: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
```

In `crates/payment-orchestrator/src/addresses.rs`, replace `address_for_user` and `existing` with the versions below. In this commit they still hand out only plain addresses; Step 4 adds the GasFree branch.

```rust
/// The user's deposit address and whether it is a GasFree account, deriving and storing it on
/// first call.
///
/// `gasfree_for_new_users` is the GasFree deployment whose account a NEW user is given
/// (`APP_TRANSFER_RAIL=gasfree`): `G = gasfree(D)`, where `D` is the plain address of their index. A
/// user who already has an address keeps it, whatever kind it is (spec §5).
///
/// Idempotent by construction: the INSERT is `ON CONFLICT (user_pk) DO NOTHING` followed by a read,
/// so two concurrent first-calls settle on whichever row won rather than deriving twice.
pub async fn address_for_user(
    pool: &PgPool,
    deriver: &AddressDeriver,
    gasfree_for_new_users: Option<&'static gasfree::Chain>,
    user_pk: &str,
    clt_address: &str,
) -> Result<(String, bool), String> {
    if let Some(stored) = existing(pool, user_pk).await? {
        return Ok(stored);
    }

    let index = crate::deposits::allocate_derivation_index(pool)
        .await
        .map_err(|e| format!("allocating a derivation index: {e}"))?;

    let index_u32 =
        u32::try_from(index).map_err(|_| format!("derivation index {index} is out of range"))?;
    let address = deriver.address_at(index_u32)?;
    let _ = gasfree_for_new_users; // Task 5 Step 4

    sqlx::query(
        "INSERT INTO deposit_addresses (user_pk, derivation_index, address, clt_address)
         VALUES ($1, $2, $3, $4) ON CONFLICT (user_pk) DO NOTHING",
    )
    .bind(user_pk)
    .bind(index)
    .bind(&address)
    .bind(clt_address)
    .execute(pool)
    .await
    .map_err(|e| format!("storing the deposit address: {e}"))?;

    // Re-read rather than returning `address`: if a concurrent call won the race, the stored row is
    // the one the poller will watch, and handing back the losing derivation would tell a user to
    // pay an address nothing polls. The burned index is simply skipped — cheaper than a lock.
    existing(pool, user_pk)
        .await?
        .ok_or_else(|| "deposit address vanished immediately after insert".to_string())
}

/// The user's stored deposit address, and whether it is a GasFree account.
pub async fn existing(pool: &PgPool, user_pk: &str) -> Result<Option<(String, bool)>, String> {
    sqlx::query_as("SELECT address, gasfree FROM deposit_addresses WHERE user_pk = $1")
        .bind(user_pk)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("reading the deposit address: {e}"))
}
```

In each of `crates/payment-orchestrator/tests/db_bridge.rs`, `db_deposit_api.rs` and `db_redemptions.rs`, the `OrchConfig { … }` literal in `test_config` ends with `rate_limit_per_minute: 1_000,`. Add after that line, in all three:

```rust
        gasfree: None,
```

In `crates/payment-orchestrator/tests/db_addresses.rs`, every call `addresses::address_for_user(&pool, &deriver, "…", "…")` gains `None` as its third argument, and in `two_simultaneous_first_calls_settle_on_one_address` replace `assert_eq!(a, stored, "the returned address is the stored one, not a losing derivation");` with `assert_eq!(a.0, stored, "the returned address is the stored one, not a losing derivation");`.

- [ ] **Step 2: The orchestrator tests**

In `crates/payment-orchestrator/src/gasfree_chain.rs`, add at the end:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The canonical all-"abandon" test wallet's account xpub, as derive.rs pins it.
    const XPUB: &str = "xpub6D1AabNHCupeiLM65ZR9UStMhJ1vCpyV4XbZdyhMZBiJXALQtmn9p42VTQckoHVn8WNqS7dqnJokZHAHcHGoaQgmv8D45oNUKx6DZMNZBCd";

    fn nile() -> gasfree::Settings {
        gasfree::Settings {
            chain: &gasfree::NILE,
            rail: true,
            activate_fee_max_usdt: 1_500_000,
            transfer_fee_max_usdt: 500_000,
            min_deposit_usdt: 1_000_000,
            expected_beacon_implementation: "b8eda40b467b45af107f198e94cc2fa1378adf50".into(),
            expected_controller_implementation: "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441".into(),
        }
    }

    /// TronGrid answering `getcontract` with `contract`, and `getGasFreeAddress` with `word` if given.
    async fn chain_answering(contract: serde_json::Value, word: Option<String>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/wallet/getcontract"))
            .respond_with(ResponseTemplate::new(200).set_body_json(contract))
            .mount(&server)
            .await;
        if let Some(word) = word {
            Mock::given(method("POST"))
                .and(path("/wallet/triggerconstantcontract"))
                .and(body_string_contains("getGasFreeAddress(address)"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"constant_result": [word]})))
                .mount(&server)
                .await;
        }
        server
    }

    #[tokio::test]
    async fn the_boot_check_passes_only_when_the_controller_agrees() {
        let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();
        let ours = format!(
            "{:0>64}",
            hex::encode(
                &bs58::decode(gasfree::gasfree_address(&gasfree::NILE, &deriver.address_at(0).unwrap()).unwrap())
                    .with_check(Some(0x41))
                    .into_vec()
                    .unwrap()[1..]
            )
        );
        let contract = serde_json::json!({"contract_address": "41575eb3ab6dfe7d69a6dc0e2cc0c72fa9e2d38e8b", "bytecode": ""});

        let agrees = chain_answering(contract.clone(), Some(ours)).await;
        assert_eq!(GasFreeChain::new(agrees.uri(), "k".into()).self_test(&nile(), &deriver).await, SelfTest::Passed);

        let disagrees = chain_answering(contract, Some(format!("{:0>64}", "ab".repeat(20)))).await;
        let got = GasFreeChain::new(disagrees.uri(), "k".into()).self_test(&nile(), &deriver).await;
        assert!(matches!(&got, SelfTest::Failed(e) if e.contains("derives")), "{got:?}");

        let other_network = chain_answering(serde_json::json!({}), None).await;
        let got = GasFreeChain::new(other_network.uri(), "k".into()).self_test(&nile(), &deriver).await;
        assert!(matches!(&got, SelfTest::Failed(e) if e.contains("APP_GASFREE_NETWORK")), "{got:?}");

        let down = MockServer::start().await; // 404 to everything
        let got = GasFreeChain::new(down.uri(), "k".into()).self_test(&nile(), &deriver).await;
        assert!(matches!(got, SelfTest::Unreachable(_)), "{got:?}");
    }

    #[tokio::test]
    async fn an_account_is_activated_only_with_a_contract_record_and_anything_odd_is_an_error() {
        let activated = chain_answering(serde_json::json!({"contract_address": "41ab", "bytecode": ""}), None).await;
        assert_eq!(
            GasFreeChain::new(activated.uri(), "k".into()).has_contract("TX").await,
            Ok(true),
            "an empty bytecode is still a contract"
        );
        let unused = chain_answering(serde_json::json!({}), None).await;
        assert_eq!(GasFreeChain::new(unused.uri(), "k".into()).has_contract("TX").await, Ok(false));
        let odd = chain_answering(serde_json::json!({"Error": "rate limited"}), None).await;
        assert!(GasFreeChain::new(odd.uri(), "k".into()).has_contract("TX").await.is_err(), "an error body is not 'no contract'");
    }
}
```

In `crates/payment-orchestrator/tests/db_addresses.rs`, add at the end:

```rust
#[tokio::test]
async fn with_the_gasfree_rail_a_new_user_gets_the_gasfree_account_of_their_address() {
    let pool = pool().await;
    let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();

    let (address, is_gasfree) =
        addresses::address_for_user(&pool, &deriver, Some(&gasfree::NILE), "0xuser-g", "0xclt-g").await.unwrap();

    let (index, stored_gasfree): (i64, bool) =
        sqlx::query_as("SELECT derivation_index, gasfree FROM deposit_addresses WHERE user_pk = '0xuser-g'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let plain = deriver.address_at(index as u32).unwrap();
    assert_eq!(address, gasfree::gasfree_address(&gasfree::NILE, &plain).unwrap(), "G = gasfree(D), and only G is stored");
    assert!(is_gasfree && stored_gasfree);
}

/// Deposit addresses are permanent (spec §5): switching the rail back leaves a GasFree address where it is.
#[tokio::test]
async fn a_user_given_a_gasfree_address_keeps_it_after_the_rail_is_switched_back() {
    let pool = pool().await;
    let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();

    let first = addresses::address_for_user(&pool, &deriver, Some(&gasfree::NILE), "0xuser-k", "0xclt-k").await.unwrap();
    let second = addresses::address_for_user(&pool, &deriver, None, "0xuser-k", "0xclt-k").await.unwrap();

    assert!(first.1, "given a GasFree address");
    assert_eq!(second, first);
}
```

In `crates/payment-orchestrator/tests/db_deposit_api.rs`:

a) Change `use wiremock::matchers::{method, path};` to `use wiremock::matchers::{body_string_contains, method, path};`.

b) Add at the end of the file:

```rust
// --- GasFree (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md §1, §2, §5) ---

const REVIEWED_BEACON: &str = "b8eda40b467b45af107f198e94cc2fa1378adf50";

fn nile() -> gasfree::Settings {
    gasfree::Settings {
        chain: &gasfree::NILE,
        rail: true,
        activate_fee_max_usdt: 1_500_000,
        transfer_fee_max_usdt: 500_000,
        min_deposit_usdt: 1_000_000,
        expected_beacon_implementation: REVIEWED_BEACON.into(),
        expected_controller_implementation: "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441".into(),
    }
}

fn gasfree_config(treasury_url: String, trongrid_url: String) -> OrchConfig {
    let mut config = test_config(treasury_url, true);
    config.trongrid_url = trongrid_url;
    config.gasfree = Some(nile());
    config
}

/// TronGrid for the deposit route: the beacon answers `implementation()` with `beacon`, the
/// controller with its reviewed implementation, and every account answers `getcontract` with `contract`.
async fn gasfree_trongrid(beacon: &str, contract: Value) -> MockServer {
    let server = MockServer::start().await;
    for (proxy, implementation) in
        [(gasfree::NILE.beacon, beacon), (gasfree::NILE.controller, "2ec1c0ada96ac9c3d6aab8e0c6e18194ed72c441")]
    {
        Mock::given(method("POST"))
            .and(path("/wallet/triggerconstantcontract"))
            .and(body_string_contains("implementation()"))
            .and(body_string_contains(proxy))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"constant_result": [format!("{implementation:0>64}")]})))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/wallet/getcontract"))
        .respond_with(ResponseTemplate::new(200).set_body_json(contract))
        .mount(&server)
        .await;
    server
}

async fn post_deposit(app: axum::Router, pk: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/deposits")
        .header("authorization", bearer_for(pk))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    (status, body_json(res).await)
}

/// A new user under the GasFree rail is shown G with its fee, "up to", and the minimum (spec §2).
#[tokio::test]
async fn a_gasfree_address_is_shown_with_its_fee_and_minimum() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let chain = gasfree_trongrid(REVIEWED_BEACON, json!({})).await;
    let app = router_with(pool.clone(), gasfree_config(treasury.uri(), chain.uri()));

    let (status, body) = post_deposit(app, USER_A).await;

    assert_eq!(status, StatusCode::OK);
    let (index, stored, is_gasfree): (i64, String, bool) =
        sqlx::query_as("SELECT derivation_index, address, gasfree FROM deposit_addresses WHERE user_pk = $1")
            .bind(USER_A)
            .fetch_one(&pool)
            .await
            .unwrap();
    let plain = payment_orchestrator::derive::AddressDeriver::from_account_xpub(TEST_XPUB)
        .unwrap()
        .address_at(index as u32)
        .unwrap();
    assert_eq!(stored, gasfree::gasfree_address(&gasfree::NILE, &plain).unwrap());
    assert!(is_gasfree);
    assert_eq!(body["address"], stored);
    assert_eq!(body["fee_up_to_usdt"], 2_000_000, "not activated: activation and one transfer, as the most");
    assert_eq!(body["min_deposit_usdt"], 1_000_000);
}

#[tokio::test]
async fn an_activated_gasfree_account_shows_one_transfer_fee() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let chain = gasfree_trongrid(REVIEWED_BEACON, json!({"contract_address": "41ab", "bytecode": ""})).await;
    let app = router_with(pool.clone(), gasfree_config(treasury.uri(), chain.uri()));

    let (status, body) = post_deposit(app, USER_A).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["fee_up_to_usdt"], 500_000);
}

/// Spec §5: after GasFree's code changes, no GasFree address is handed out, so no more money goes in.
#[tokio::test]
async fn no_gasfree_address_is_handed_out_after_gasfree_code_changed() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let chain = gasfree_trongrid("00000000000000000000000000000000000000ff", json!({})).await;
    let app = router_with(pool.clone(), gasfree_config(treasury.uri(), chain.uri()));

    let (status, _) = post_deposit(app, USER_A).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM deposit_addresses WHERE user_pk = $1")
        .bind(USER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "no address was issued");
}

/// A guard, green before and after this task: GasFree never stands between a user and the plain
/// address they already have, even with TronGrid unreachable.
#[tokio::test]
async fn a_user_with_a_plain_address_is_not_held_up_by_gasfree() {
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let deriver = payment_orchestrator::derive::AddressDeriver::from_account_xpub(TEST_XPUB).unwrap();
    let (plain, _) = payment_orchestrator::addresses::address_for_user(
        &pool,
        &deriver,
        None,
        USER_A,
        "0x00000000000000000000000000000000000000a1",
    )
    .await
    .unwrap();
    let app = router_with(pool.clone(), gasfree_config(treasury.uri(), "http://localhost:0".into()));

    let (status, body) = post_deposit(app, USER_A).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["address"], plain);
    assert!(body.get("fee_up_to_usdt").is_none());
}
```

In `crates/payment-orchestrator/tests/db_redemptions.rs`, add at the end:

```rust
/// With GasFree payouts the treasury answers 503 until its float has made its first transfer (spec
/// §4). That is "not yet", and the user is told so — not that the treasury refused them.
#[tokio::test]
async fn a_treasury_not_taking_redemptions_yet_reads_as_not_yet_available() {
    let pool = pool().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/internal/redemption-intents"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let app = router_with(pool, test_config(server.uri(), true));

    let res = app.oneshot(post_redemption_request(&bearer_for(FRANK_ADDR), VALID_TRON_ADDRESS, 2_000_000)).await.unwrap();

    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["error"].as_str().unwrap().contains("not yet available"), "{body}");
}
```

- [ ] **Step 3: Commit, and the controller confirms red**

Overwrite the commit message file with:

```text
test(orchestrator): GasFree addresses, against stubs

The orchestrator tests: a new user under the GasFree rail is given
G = gasfree(D) and keeps it, the deposit route shows the fee as "up to"
with the minimum, refuses a GasFree address after GasFree's code
changed, and leaves plain users alone; the boot check against the
controller; and a treasury 503 on a redemption read as "not yet
available". The chain reads are todo!() and address_for_user still
hands out plain addresses in this commit.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add Cargo.lock crates/payment-orchestrator
git commit -F .superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/commit-msg.txt
```

Controller: run CI. Expected: the run fails; these 8 fail by name, `a_user_with_a_plain_address_is_not_held_up_by_gasfree` is `ok` (it is a guard), and every test that existed before this task is `ok`:

```text
test gasfree_chain::tests::an_account_is_activated_only_with_a_contract_record_and_anything_odd_is_an_error ... FAILED
test gasfree_chain::tests::the_boot_check_passes_only_when_the_controller_agrees ... FAILED
test a_user_given_a_gasfree_address_keeps_it_after_the_rail_is_switched_back ... FAILED
test with_the_gasfree_rail_a_new_user_gets_the_gasfree_account_of_their_address ... FAILED
test a_gasfree_address_is_shown_with_its_fee_and_minimum ... FAILED
test an_activated_gasfree_account_shows_one_transfer_fee ... FAILED
test no_gasfree_address_is_handed_out_after_gasfree_code_changed ... FAILED
test a_treasury_not_taking_redemptions_yet_reads_as_not_yet_available ... FAILED
```

- [ ] **Step 4: Replace the stubs**

In `crates/payment-orchestrator/src/gasfree_chain.rs`, replace the three stubbed methods, and add `post`, `view_word` and `abi_address`, so that everything between `pub fn new` and `#[cfg(test)]` reads:

```rust
    pub fn new(base_url: String, api_key: String) -> Self {
        // Bounded: the deposit route waits on these reads.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client builder");
        Self { http, base_url, api_key }
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base_url))
            .header("TRON-PRO-API-KEY", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("trongrid {path} returned {}", resp.status()));
        }
        resp.json().await.map_err(|e| e.to_string())
    }

    /// Whether `address` holds a deployed contract; for a GasFree account, whether it is activated.
    /// Only `{}` means no: an activated GasFree account answers with its contract record and an
    /// EMPTY bytecode (2026-09-24), and any other answer is an error, never "no".
    pub async fn has_contract(&self, address: &str) -> Result<bool, String> {
        let v = self.post("/wallet/getcontract", serde_json::json!({"value": address, "visible": true})).await?;
        if v["contract_address"].as_str().is_some_and(|a| !a.is_empty()) {
            return Ok(true);
        }
        if v.as_object().is_some_and(|o| o.is_empty()) {
            return Ok(false);
        }
        Err(format!("getcontract for {address} gave neither a contract nor {{}}: {v}"))
    }

    /// The first 32-byte word a view function returns, as 64 lowercase hex characters.
    async fn view_word(&self, contract: &str, selector: &str, parameter: Option<&str>) -> Result<String, String> {
        // TronGrid wants an `owner_address`, and a view does not care who asks, so the contract is
        // named as its own caller.
        let mut body = serde_json::json!({
            "owner_address": contract,
            "contract_address": contract,
            "function_selector": selector,
            "visible": true,
        });
        if let Some(p) = parameter {
            body["parameter"] = serde_json::Value::from(p);
        }
        let v = self.post("/wallet/triggerconstantcontract", body).await?;
        let word = v["constant_result"][0]
            .as_str()
            .ok_or_else(|| format!("{selector} on {contract} returned nothing: {v}"))?;
        if word.len() != 64 || !word.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{selector} on {contract} returned {word:?}, not one 32-byte word"));
        }
        Ok(word.to_ascii_lowercase())
    }

    /// Why no GasFree address may be handed out, when GasFree's code is not the reviewed code;
    /// `None` when it is (spec §5). Both proxies: the beacon behind every account, and the
    /// controller that moves money out of them.
    pub async fn code_changed(&self, settings: &gasfree::Settings) -> Result<Option<String>, String> {
        let checks = [
            ("beacon", settings.chain.beacon, &settings.expected_beacon_implementation),
            ("controller", settings.chain.controller, &settings.expected_controller_implementation),
        ];
        for (what, proxy, expected) in checks {
            let word = self.view_word(proxy, "implementation()", None).await?;
            let now = &word[24..];
            if now != expected.as_str() {
                return Ok(Some(format!("the GasFree {what} {proxy} now runs 0x{now}, not the reviewed 0x{expected}")));
            }
        }
        Ok(None)
    }

    /// Once at boot: are these GasFree constants the ones deployed where this TronGrid points, and
    /// does the controller put index 0's account where this service derives it? A Nile setting on a
    /// mainnet TronGrid fails here, instead of showing users addresses nobody controls.
    pub async fn self_test(&self, settings: &gasfree::Settings, deriver: &AddressDeriver) -> SelfTest {
        let controller = settings.chain.controller;
        match self.has_contract(controller).await {
            Ok(true) => {}
            Ok(false) => {
                return SelfTest::Failed(format!(
                    "the GasFree controller {controller} is not a contract on this TronGrid: APP_GASFREE_NETWORK does \
                     not match APP_TRONGRID_URL"
                ))
            }
            Err(e) => return SelfTest::Unreachable(e),
        }
        let owner = match deriver.address_at(0) {
            Ok(a) => a,
            Err(e) => return SelfTest::Failed(e),
        };
        let ours = match gasfree::gasfree_address(settings.chain, &owner).and_then(|g| abi_address(&g)) {
            Ok(word) => word,
            Err(e) => return SelfTest::Failed(e),
        };
        let parameter = match abi_address(&owner) {
            Ok(p) => p,
            Err(e) => return SelfTest::Failed(e),
        };
        match self.view_word(controller, "getGasFreeAddress(address)", Some(&parameter)).await {
            Ok(word) if word[24..] == ours[24..] => SelfTest::Passed,
            Ok(word) => SelfTest::Failed(format!(
                "the controller puts the GasFree account of {owner} at 0x{}, this service derives 0x{}",
                &word[24..],
                &ours[24..]
            )),
            Err(e) => SelfTest::Unreachable(e),
        }
    }
}

/// A TRON address as one 32-byte ABI word, after its base58check checksum is checked.
fn abi_address(address: &str) -> Result<String, String> {
    let bytes = bs58::decode(address)
        .with_check(Some(0x41))
        .into_vec()
        .map_err(|e| format!("address {address} failed base58check: {e}"))?;
    if bytes.len() != 21 {
        return Err(format!("address {address} decoded to {} bytes, want 21", bytes.len()));
    }
    Ok(format!("{:0>64}", hex::encode(&bytes[1..])))
}
```

In `crates/payment-orchestrator/src/addresses.rs`, in `address_for_user`, replace:

```rust
    let address = deriver.address_at(index_u32)?;
    let _ = gasfree_for_new_users; // Task 5 Step 4

    sqlx::query(
        "INSERT INTO deposit_addresses (user_pk, derivation_index, address, clt_address)
         VALUES ($1, $2, $3, $4) ON CONFLICT (user_pk) DO NOTHING",
    )
    .bind(user_pk)
    .bind(index)
    .bind(&address)
    .bind(clt_address)
```

with:

```rust
    let plain = deriver.address_at(index_u32)?;
    // G = gasfree(D) (spec §1): computed from the plain address and public constants, with no key.
    // Only one of the two is stored, so the poller never watches both addresses of one index.
    let (address, is_gasfree) = match gasfree_for_new_users {
        Some(chain) => (gasfree::gasfree_address(chain, &plain)?, true),
        None => (plain, false),
    };

    sqlx::query(
        "INSERT INTO deposit_addresses (user_pk, derivation_index, address, clt_address, gasfree)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (user_pk) DO NOTHING",
    )
    .bind(user_pk)
    .bind(index)
    .bind(&address)
    .bind(clt_address)
    .bind(is_gasfree)
```

In `crates/payment-orchestrator/src/api.rs`, in `create_deposit_handler`:

a) Replace:

```rust
    let (address, _) =
        addresses::address_for_user(&state.pool, state.deriver.as_ref(), None, &user_pk, &clt_address)
            .await
            .map_err(|e| {
                tracing::error!("deposit address for {user_pk}: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
```

with:

```rust
    // GasFree (spec §1, §5). A user keeps the kind of address they were given; a new user is given
    // the GasFree account of their address while the rail is on. A GasFree address is handed out
    // only while GasFree's code is the reviewed code: after a change, that code could take what is
    // paid in, and the tripwire exists to put no more in.
    let stored = addresses::existing(&state.pool, &user_pk).await.map_err(|e| {
        tracing::error!("deposit address for {user_pk}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let wants_gasfree = match &stored {
        Some((_, is_gasfree)) => *is_gasfree,
        None => state.config.gasfree.as_ref().is_some_and(|s| s.rail),
    };
    let settings = if wants_gasfree {
        let Some(settings) = state.config.gasfree.as_ref() else {
            tracing::error!("{user_pk} has a GasFree deposit address, but GasFree is not configured in this service");
            return Ok(deposits_unavailable());
        };
        match state.gasfree_chain.code_changed(settings).await {
            Ok(None) => Some(settings),
            Ok(Some(reason)) => {
                tracing::error!("not handing out a GasFree address: {reason}");
                return Ok(deposits_unavailable());
            }
            Err(e) => {
                tracing::warn!("not handing out a GasFree address: GasFree's code could not be read: {e}");
                return Ok(deposits_unavailable());
            }
        }
    } else {
        None
    };

    let (address, _) = addresses::address_for_user(
        &state.pool,
        state.deriver.as_ref(),
        settings.map(|s| s.chain),
        &user_pk,
        &clt_address,
    )
    .await
    .map_err(|e| {
        tracing::error!("deposit address for {user_pk}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
```

b) Replace the handler's last line:

```rust
    Ok((StatusCode::OK, Json(serde_json::json!({ "address": address }))))
}
```

with:

```rust
    let mut body = json!({ "address": address });
    if let Some(settings) = settings {
        // "Up to", never a fixed fee (spec §2): the treasury holds back the configured maximum, and
        // the relay may take less. Read from the chain, so a returning user whose account is
        // activated sees one transfer fee; unreadable, the larger fee is shown.
        let activated = state.gasfree_chain.has_contract(&address).await.unwrap_or(false);
        body["fee_up_to_usdt"] =
            json!(gasfree::fee_to_hold(activated, settings.activate_fee_max_usdt, settings.transfer_fee_max_usdt));
        body["min_deposit_usdt"] = json!(settings.min_deposit_usdt);
    }
    Ok((StatusCode::OK, Json(body)))
}

fn deposits_unavailable() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "deposits are temporarily unavailable"})))
}
```

In `crates/payment-orchestrator/src/redemptions.rs`, in `create_redemption`, replace:

```rust
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
```

with:

```rust
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        // The treasury says "not yet": with GasFree payouts, its float has not made its first
        // transfer (spec §4). The user is told redemptions are not available yet.
        Ok(r) if r.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE => {
            tracing::warn!("redemptions: the treasury is not taking redemptions yet (503)");
            return RedemptionOutcome::Disabled;
        }
        Ok(r) => {
```

In `crates/payment-orchestrator/src/treasury_bridge.rs`:

a) In the doc comment on `poll_step`, replace:

```rust
/// - `needs_manual` → deposit `needs_manual` + a P1 that says the opposite about recovery:
///   the treasury intent is over the per-transaction mint cap and is approvable again once a
///   human raises the cap, so this bridge keeps polling and credits the deposit when the
///   treasury does.
```

with:

```rust
/// - `needs_manual` → deposit `needs_manual` + a P1 that says the opposite about recovery: the
///   treasury is holding the intent for a human — over the per-transaction mint cap, or a GasFree
///   deposit below the minimum after the fee — and it is approvable again, so this bridge keeps
///   polling and credits the deposit when the treasury does.
```

b) Replace the `needs_manual` alert text:

```rust
                        "deposit {} treasury mint intent {treasury_id} needs manual review: it is over the per-transaction mint cap. The user's USDT is at their deposit address, unswept and still counted in the reserve; no CLT has been minted. To release it, raise the cap (set-mint-caps) and approve intent {treasury_id} again (mint-intent-approve). This bridge keeps polling and credits the deposit when the treasury does.",
```

with:

```rust
                        "deposit {} treasury mint intent {treasury_id} needs manual review on the treasury side: it is over the per-transaction mint cap, or it is a GasFree deposit below the minimum after the fee (the treasury's own alert says which). The user's USDT is at their deposit address, still counted in the reserve; no CLT has been minted. Over the cap: raise the cap (set-mint-caps) and approve intent {treasury_id} again (mint-intent-approve). Below the minimum: approving it mints at most what arrived less the fee. This bridge keeps polling and credits the deposit when the treasury does.",
```

In `crates/payment-orchestrator/src/main.rs`, add after the `let deriver = Arc::new(…);` statement:

```rust
    // GasFree's boot check (spec §1): its constants must be the ones deployed where this TronGrid
    // points, or users would be shown addresses nobody controls. A chain that answers wrong stops the
    // service. One that does not answer leaves it to the code check before each GasFree address,
    // which also fails on the wrong network, where the proxies are not contracts.
    if let Some(settings) = &config.gasfree {
        let chain = payment_orchestrator::gasfree_chain::GasFreeChain::new(
            config.trongrid_url.clone(),
            config.trongrid_api_key.clone(),
        );
        match chain.self_test(settings, &deriver).await {
            payment_orchestrator::gasfree_chain::SelfTest::Passed => {
                tracing::info!(chain_id = settings.chain.chain_id, rail = settings.rail, "GasFree self-test passed")
            }
            payment_orchestrator::gasfree_chain::SelfTest::Failed(e) => panic!("GasFree self-test failed: {e}"),
            payment_orchestrator::gasfree_chain::SelfTest::Unreachable(e) => tracing::warn!(
                "GasFree self-test could not reach TronGrid, so it did not run; the code check before each GasFree \
                 address still runs: {e}"
            ),
        }
    }
```

Do not change the tests.

- [ ] **Step 5: Commit, and the controller confirms green**

Overwrite the commit message file with:

```text
feat(orchestrator): hand out GasFree addresses behind the tripwire

With APP_TRANSFER_RAIL=gasfree a new user is given G = gasfree(D) for
their index, computed here with the gasfree crate, and a user keeps the
kind of address they were given. Before a GasFree address is shown,
both GasFree implementations must be the reviewed ones; otherwise the
route answers 503 and issues nothing. The reply carries the fee as "up
to" (activation plus one transfer until the account is activated) and
the minimum deposit. At boot the controller must agree with the
derivation. A treasury 503 on a redemption now reads as "not yet
available", and the needs_manual page names both reasons it can have.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

```bash
cd /d/source/clutch/clutch-treasury
git add crates/payment-orchestrator/src
git commit -F .superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/commit-msg.txt
```

Controller: run CI. Expected: success; the 9 orchestrator tests above say `ok` by name (the 8 plus the guard), every earlier test says `ok`, and there is no warning located in `crates/payment-orchestrator`, `crates/treasury-service` or `crates/gasfree`.

- [ ] **Step 6 (controller): Open the pull request**

Write the body to `.superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/pr-body.md`, filling in the final run id:

```markdown
Plan 3 of 4 for `docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md` (plan: `docs/superpowers/plans/2026-09-25-gasfree-treasury-orchestrator.md`): `treasury-service` and `payment-orchestrator` learn the GasFree rail the signer got in #51.

**Merging this changes nothing in production.** GasFree is off in both services unless `APP_GASFREE_NETWORK` is set, and no `.env` sets it yet. Two changes do reach the TRX rail, on purpose: CI runs with `--no-fail-fast`, and the sweeper asks the signer once per index per pass and marks every row of a swept index (a second row of a swept address used to stay unswept for good).

## What it does

- **Orchestrator:** with `APP_TRANSFER_RAIL=gasfree`, a new user is given `G = gasfree(D)`; a user keeps the kind of address they were given. A GasFree address is shown only while both GasFree implementations are the reviewed ones, with the fee "up to" and the minimum. A boot check asks the controller for index 0's account.
- **Treasury, minting:** the verifier asks the signer which address of the index was paid. A GasFree deposit mints `observed − fee_to_hold(the treasury's own record of the first sweep, maxima)`, capping the orchestrator. Below `APP_MIN_DEPOSIT_USDT` it waits for a human, amount already lowered.
- **Treasury, sweeping:** a GasFree account is swept as soon as it holds more than the fee, every 60 seconds. Its deposits count as swept only when the owner's nonce passes the permit's. The tripwire is read every pass. A relay refusal, a halt, or a maxFee above the hold pages a human, and the relay's `txn_amount` is compared with the permit's `value` on every sweep. Every recorded GasFree account stays in the reserve walk.
- **Treasury, payouts:** one GasFree permit at a time, paid only when its transfer from the float is confirmed on chain; a relay refusal is settled by the float's nonce after the deadline; redemptions answer 503 until the float is activated.

## Evidence

CI run <id>: every test by name — 6 `gasfree` settings tests, 19 for minting, 12 for sweeping, 10 for payouts, 9 in the orchestrator — every other test binary green, no warning in the three crates. Each task was first seen failing by name against stubs.

## Decisions the plan made where the spec is silent

1. The hold follows the treasury's own record of an account's first sweep, not the chain's contract record (Plan 2's final review, I2).
2. Recorded GasFree accounts stay in the reserve count after their deposits are swept.
3. Below the minimum: `needs_manual` with the amount lowered; a deposit the fee takes whole is rejected.
4. An address its index does not lead to is rejected; with GasFree on, an intent with no index waits.
5. GasFree accounts ignore the sweep threshold; the pass runs every 60 seconds while GasFree is on.
6. One signer call per index per pass, on both rails.
7. A permit's maxFee above every hold it moves pages a human.
8. One payout permit at a time; an unclear answer holds the float for 600 seconds.
9. A payout is paid on its on-chain transfer from the float, not on the relay's word.
10. Redemptions answer 503 until the float is activated; the orchestrator says "not yet available".
11. The shown fee comes from the chain's activation record; in one rare case the treasury holds more than shown, never more than activation plus one transfer.
12. Both implementations are watched in both services.
13. Only the orchestrator runs a boot check.
14. One settings parser, in the `gasfree` crate, for the treasury and the orchestrator.
15. CI runs with `--no-fail-fast`.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
```

Then:

```bash
cd /d/source/clutch/clutch-treasury
gh pr create --repo clutchprotocol/clutch-treasury --base main --head feat/gasfree-treasury --title "feat: the GasFree rail in the treasury and the orchestrator" --body-file .superpowers/sdd/2026-09-25-gasfree-treasury-orchestrator/pr-body.md
```

The maintainer merges it. The stage images rebuild on merge, but with no GasFree settings in any `.env` the two services behave as before, apart from the two TRX-rail changes named above.

---

## After this plan

**Plan 4 — deploy, deposit panel, rollout.**
- Compose settings for all three services, from one env file, mapped from names without the `APP_` prefix: `TRANSFER_RAIL`, `GASFREE_NETWORK`, both maxima, `MIN_DEPOSIT_USDT`, both expected implementations; the signer's API key, secret, URL, provider and `PAYOUT_FLOAT_TARGET_USDT`.
- The treasury and the orchestrator get the GasFree settings together, always. An orchestrator handing out GasFree addresses to a treasury without them would see those deposits minted in full, with no fee held back. And the settings stay for as long as any GasFree address exists (spec §5), even after `TRANSFER_RAIL` goes back to `trx`.
- `check-cap-invariants.sh`: `REDEMPTION_FEE_USDT ≥ GASFREE_TRANSFER_FEE_MAX_USDT`, positive maxima and minimum. `PROBE=gasfree`: the live fees against the maxima.
- Provisioning `PAYOUT_FLOAT_ADDRESS` from `payout_gasfree_address`. That drops the plain `2/0` from the reserve count while `fund-float` still targets it (carried from Plan 2).
- The typed-confirmation float activation workflow. It refuses unless `reserve − supply ≥ activate + transfer` at their maxima; the reserve now includes every recorded GasFree account.
- The deposit panel: `fee_up_to_usdt` shown as "up to", `min_deposit_usdt`, and what a user must send to get anything minted.
- `sweep-address.sh` exits 1 on the new statuses; a self-test timeout before the port binds (carried from Plan 2).
- The five Nile rollout steps in spec §9, with these checks: the fee is on top of `value` (the sweeper now pages if not); TronGrid's `from` field on the float's transfers (fact 8); a never-used account gets a relay reply; the relay's nonce returns to the chain's after an expired permit; the signature is accepted as 130 hex; documented refusals arrive as body code 400; 1 micro-USDT is accepted for activation; `implementation()` reads work on the controller's proxy.
- Mainnet: its own API key and its own fee reading before its rail is switched on.
