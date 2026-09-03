# Permanent Per-User Deposit Addresses Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give each user one permanent TRON deposit address and credit whatever arrives at it, so the top-up flow stops asking for an amount up front.

**Architecture:** A `deposit_addresses` table keyed by `user_pk` replaces per-intent derivation. The two unique indexes that forbid address reuse are dropped, and the double-mint guarantee moves to the already-enforced `uq_mint_intents_deposit_tx`. A tiered poller keeps TronGrid cost constant per pass regardless of user count, behind a new `DepositWatcher` seam. Each observed transfer becomes its own credit.

**Tech Stack:** Rust, axum, sqlx/Postgres, reqwest, `wiremock` for faking TronGrid, `async_trait` for the watcher seam.

**Spec:** `docs/superpowers/specs/2026-08-30-permanent-deposit-addresses-design.md`

## Global Constraints

- **Verification runs in CI, never on this machine.** No working linker, no Docker daemon. Implementers must NOT run `cargo`. The controller pushes the branch and dispatches `gh workflow run test.yml --ref <branch>` on clutchprotocol/clutch-treasury, which runs `cargo test --workspace -- --test-threads=1` with a Postgres service. A branch push builds no image and triggers no stage deploy (`docker-build-push.yml` is main/tags only).
- **Credit everything, cap nothing.** Any arriving USDT is credited in full. The daily mint cap and the reconciliation breaker remain the only ceilings.
- Derivation path stays `m/44'/195'/0'/0/i` from the account xpub. `tron-signer` derives the matching private keys at the same path — that agreement must not be disturbed.
- `nextval('deposit_derivation_index_seq')` keeps advancing. Never reset it, never reuse an index: a new user address must never collide with a legacy per-intent one.
- Scope is testnet/Nile. No change to the four-eyes mint flow, the caps, or the breaker.
- **Tasks 1–3 are Deploy A and must land and be verified before Tasks 4+.** Deploy A is behaviour-neutral; Deploy B inverts who creates a deposit row.

---

## File Structure

**`crates/treasury-service/`**
- `src/reconciliation.rs` — the unswept query gains `DISTINCT` (Task 1)
- `migrations/0010_drop_deposit_address_uniqueness.sql` — drops `uq_mint_intents_deposit_address` (Task 3)
- `tests/db_reconciliation.rs` — double-count regression (Task 1)

**`crates/payment-orchestrator/`**
- `migrations/0010_deposit_addresses.sql` — new table + `tron_tx_id` unique index (Task 2)
- `migrations/0011_drop_intent_address_uniqueness.sql` — drops the two orchestrator indexes (Task 3)
- `src/addresses.rs` — **new**: per-user address derivation and storage (Task 4)
- `src/custody.rs` — `DepositWatcher` seam; `evaluate_payment` deleted (Tasks 5, 7)
- `src/poller.rs` — tiered selection, per-transfer crediting (Tasks 5, 7)
- `src/api.rs`, `src/deposits.rs` — the endpoint returns an address (Task 6)
- `src/configuration.rs` — hot window, feature flag, bounds removed (Tasks 5, 8)

**Other repos** — `clutch-hub-demo-app` (Task 9), `clutch-deploy` + docs (Task 10)

---

# DEPLOY A — behaviour-neutral

### Task 1: Stop the reserve double-counting addresses

**Files:**
- Modify: `crates/treasury-service/src/reconciliation.rs:174-177`
- Test: `crates/treasury-service/tests/db_reconciliation.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: no signature change. `get_reserve_balance` receives a de-duplicated address list.

**Why this is Task 1 and not Task 9.** Today duplicates are impossible, because one address per intent is enforced by an index Task 3 drops. The moment that index is gone, an address on several unswept rows is summed once per row and the reserve reads HIGH. A deflated reserve halts minting — loud and safe. An inflated one **permits minting that is not backed**, which is the one failure a fully-reserved token cannot tolerate. This lands first so the window never exists.

- [ ] **Step 1: Write the failing test**

Add to `crates/treasury-service/tests/db_reconciliation.rs`:

```rust
#[tokio::test]
async fn one_address_on_two_unswept_rows_is_counted_once() {
    // Per-user deposit addresses make this the NORMAL case: one user, several deposits, one
    // address. Summing per row inflates the reserve, and an over-backed reading permits minting
    // that nothing backs — strictly worse than the under-backed reading, which only halts minting.
    let pool = pool().await;
    let server = MockServer::start().await;
    mount_balance(&server, CUSTODY, 500).await;
    mount_balance(&server, SHARED_ADDR, 300).await;

    // Two credited-but-unswept mints at the SAME address.
    seed_unswept_mint(&pool, SHARED_ADDR, "tx-a").await;
    seed_unswept_mint(&pool, SHARED_ADDR, "tx-b").await;

    let config = test_config(server.uri());
    let run = treasury_service::reconciliation::run_once(&pool, &config).await.unwrap();

    assert_eq!(run.custody_reported, 800, "custody 500 + the shared address 300, counted ONCE");
}
```

`mount_balance`, `CUSTODY` and `test_config` already exist in that file — reuse them under their real names. Add `const SHARED_ADDR: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";` and a `seed_unswept_mint` helper inserting a `mint_intents` row with `status = 'credited'`, `swept_at = NULL`, the given `deposit_address` and `deposit_tx_id`. Model it on however that file already seeds mint rows; do not invent a second seeding idiom.

- [ ] **Step 2: Run test to verify it fails**

Verification runs in CI. Expected: FAIL asserting `1100 == 800` — the shared address counted twice.

- [ ] **Step 3: Add DISTINCT**

In `crates/treasury-service/src/reconciliation.rs`, the unswept query:

```rust
    // DISTINCT is load-bearing. get_reserve_balance sums every entry it is handed, and per-user
    // deposit addresses mean one address legitimately appears on many unswept rows. Summing per
    // row would inflate the reserve, and an over-backed reading is the dangerous direction: it
    // licenses minting that nothing backs. Under-counting merely halts minting, loudly.
    let unswept: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT deposit_address FROM mint_intents
         WHERE deposit_address IS NOT NULL AND swept_at IS NULL AND status IN ('approved', 'submitted', 'credited')",
    )
```

- [ ] **Step 4: Run tests to verify they pass**

CI. Expected: PASS, including every pre-existing reconciliation test.

- [ ] **Step 5: Commit**

```bash
git add crates/treasury-service/src/reconciliation.rs crates/treasury-service/tests/db_reconciliation.rs
git commit -m "fix: count each unswept deposit address once in the reserve"
```

---

### Task 2: The deposit_addresses table

**Files:**
- Create: `crates/payment-orchestrator/migrations/0010_deposit_addresses.sql`

**Interfaces:**
- Produces: table `deposit_addresses(user_pk PK, derivation_index BIGINT UNIQUE, address TEXT UNIQUE, hot_until TIMESTAMPTZ, last_polled_at TIMESTAMPTZ, created_at)`, and a unique index on `deposit_intents.tron_tx_id`. Tasks 4–7 depend on both.

- [ ] **Step 1: Write the migration**

```sql
-- One permanent address per user, replacing one address per deposit.
--
-- The derivation index still comes from deposit_derivation_index_seq, but is consumed once per
-- USER and kept. The sequence is never reset: legacy per-intent addresses already hold issued
-- indexes, and reusing one would hand a new user an address a previous deposit was sent to.
CREATE TABLE deposit_addresses (
    user_pk          TEXT PRIMARY KEY,
    derivation_index BIGINT NOT NULL UNIQUE,
    address          TEXT   NOT NULL UNIQUE,
    -- Where the CLT is minted. Previously the user supplied this per deposit; now the chain creates
    -- the row and nothing would carry it, so it is captured once when the address is issued. Without
    -- it a credited deposit has no destination and the mint cannot be built.
    clt_address      TEXT   NOT NULL,
    -- Set when the user opens the deposit panel: the moment they are about to send. The poller
    -- serves hot addresses first, so the common case stays near-real-time without polling every
    -- address every pass.
    hot_until        TIMESTAMPTZ,
    -- Stamped after each poll. Doubles as the rotation key for cold addresses and as the
    -- min_timestamp bound, so a long-lived address does not re-fetch its whole history.
    last_polled_at   TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Identity moves from address to TRANSACTION. The treasury already enforces the same thing with
-- uq_mint_intents_deposit_tx; this is the orchestrator's half, and it is what makes address reuse
-- safe: two transfers to one address are two credits, and the same transfer seen twice is one.
CREATE UNIQUE INDEX uq_deposit_intents_tron_tx_id
    ON deposit_intents (tron_tx_id) WHERE tron_tx_id IS NOT NULL;
```

Before writing, confirm `deposit_intents.tron_tx_id` exists and its exact name by reading
`migrations/0001_orchestrator.sql` and `0009_received_usdt.sql`. If a partial unique index on it
already exists, skip that half and say so in your report rather than creating a duplicate.

- [ ] **Step 2: Verify it applies**

CI runs every test's `sqlx::migrate!`. Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/payment-orchestrator/migrations/0010_deposit_addresses.sql
git commit -m "feat: add deposit_addresses, one permanent address per user"
```

---

### Task 3: Drop the three address-uniqueness indexes

**Files:**
- Create: `crates/payment-orchestrator/migrations/0011_drop_intent_address_uniqueness.sql`
- Create: `crates/treasury-service/migrations/0010_drop_deposit_address_uniqueness.sql`

**Interfaces:**
- Consumes: Task 1's `DISTINCT` (must already be deployed), Task 2's `uq_deposit_intents_tron_tx_id`.
- Produces: address reuse becomes representable. Tasks 6–7 depend on it.

- [ ] **Step 1: Write the orchestrator migration**

```sql
-- Address reuse is the point: one user, one address, many deposits over its life.
--
-- What these indexes were protecting is not lost, it changes key. uq_deposit_intents_tron_tx_id
-- (migration 0010) makes every credit unique per TRANSACTION, which is a strictly better guarantee:
-- under the old model two transfers to one address were ONE deposit by construction, which is
-- quietly wrong. Under this one they are correctly two.
DROP INDEX IF EXISTS uq_deposit_address;
DROP INDEX IF EXISTS uq_deposit_derivation_index;
```

- [ ] **Step 2: Write the treasury migration**

```sql
-- The treasury's half of the same change. This one is a money control, not bookkeeping: it exists
-- so one address cannot be minted against twice.
--
-- That guarantee moves to uq_mint_intents_deposit_tx (migration 0002), which is already enforced
-- and already tested — every mint keyed to exactly one on-chain transaction. Both drops land in
-- their own migrations but the same deploy, so no window exists where neither key applies.
--
-- Reserve safety depends on reconciliation.rs's SELECT DISTINCT, which MUST already be deployed:
-- without it, an address on several unswept rows is summed once per row and the reserve reads high,
-- licensing mints nothing backs.
DROP INDEX IF EXISTS uq_mint_intents_deposit_address;
```

Confirm both index names against `crates/payment-orchestrator/migrations/0007_derivation_index.sql` and `crates/treasury-service/migrations/0004_deposit_address.sql` before relying on them. `IF EXISTS` keeps the migration idempotent, but a wrong name would silently drop nothing — verify, do not assume.

- [ ] **Step 3: Prove the drop is safe — the double-count test (moved here from Task 1)**

This test cannot run before this task: it seeds two `mint_intents` rows at ONE address, which the
index dropped in Step 2 forbade. It lands with the drop so the `DISTINCT` from Task 1 is proven the
moment duplicates become representable. Add to `crates/treasury-service/tests/db_reconciliation.rs`:

```rust
/// A real, base58check-valid derived address — the same fixture value db_sweeper.rs's ADDRS uses.
const SHARED_ADDR: &str = "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK";

/// Inserts a `credited`, unswept mint intent at `address` evidenced by `tx_id`. Names every NOT NULL
/// column mint_intents has with no default (`beneficiary`, `amount_clt`, `credit_ref`, `created_by`)
/// plus `approved_by`, since four_eyes requires a distinct approver past `created`. `swept_at` is
/// omitted so it takes its NULL default — the "still unswept" state this test needs.
async fn seed_unswept_mint(pool: &PgPool, address: &str, tx_id: &str) {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mint_intents (id, beneficiary, amount_clt, status, credit_ref, created_by, approved_by,
                                    deposit_tx_id, deposit_address)
         VALUES ($1, 'TBeneficiary1111111111111111111111', 1000000, 'credited', $2, 'orchestrator', 'tron-verifier',
                 $3, $4)",
    )
    .bind(id)
    .bind(format!("ref-{id}"))
    .bind(tx_id)
    .bind(address)
    .execute(pool)
    .await
    .unwrap();
}

/// Per-user deposit addresses make this the NORMAL case: one user, several deposits, one address.
/// Counting per row inflates the reserve, and an over-backed reading licenses mints that nothing
/// backs — strictly worse than under-counting, which only halts minting.
#[tokio::test]
async fn one_address_on_two_unswept_rows_is_counted_once() {
    let pool = pool().await;
    seed_unswept_mint(&pool, SHARED_ADDR, "tx-a").await;
    seed_unswept_mint(&pool, SHARED_ADDR, "tx-b").await;

    let addrs = treasury_service::reconciliation::unswept_addresses(&pool).await.unwrap();

    assert_eq!(addrs, vec![SHARED_ADDR.to_string()], "one address, counted once");
}
```

- [ ] **Step 4: Verify and commit**

CI. The new test must PASS here and would have FAILED on 23505 before Step 2. Then:

```bash
git add crates/payment-orchestrator/migrations/0011_drop_intent_address_uniqueness.sql crates/treasury-service/migrations/0010_drop_deposit_address_uniqueness.sql crates/treasury-service/tests/db_reconciliation.rs
git commit -m "feat: allow address reuse, keyed on transaction instead"
```

**DEPLOY A ENDS HERE.** Deploy, then verify reconciliation still reads `ok` before starting Task 4. That is the abort point.

---

# DEPLOY B — the inversion

### Task 4: Per-user address derivation and storage

**Files:**
- Create: `crates/payment-orchestrator/src/addresses.rs`
- Modify: `crates/payment-orchestrator/src/lib.rs` (add `pub mod addresses;`)
- Test: `crates/payment-orchestrator/tests/db_addresses.rs`

**Interfaces:**
- Consumes: `AddressDeriver::address_at(&self, index: u32) -> Result<String, String>` (`src/derive.rs`); Task 2's table.
- Produces: `pub async fn address_for_user(pool: &PgPool, deriver: &AddressDeriver, user_pk: &str, clt_address: &str) -> Result<String, String>` and `pub async fn mark_hot(pool: &PgPool, user_pk: &str, window_hours: i64) -> Result<(), String>`. Tasks 5 and 6 use both.

- [ ] **Step 1: Write the failing tests**

Create `crates/payment-orchestrator/tests/db_addresses.rs`:

```rust
use payment_orchestrator::addresses;
use payment_orchestrator::derive::AddressDeriver;
use sqlx::PgPool;

/// The published account xpub for the canonical test mnemonic — the same wallet derive.rs's own
/// tests pin, so these addresses are checkable against that table.
/// The fixture account xpub, copied verbatim from `src/derive.rs`'s own test module — the same
/// wallet whose addresses that file already pins, so these results are checkable against it.
const XPUB: &str = "xpub6D1AabNHCupeiLM65ZR9UStMhJ1vCpyV4XbZdyhMZBiJXALQtmn9p42VTQckoHVn8WNqS7dqnJokZHAHcHGoaQgmv8D45oNUKx6DZMNZBCd";

async fn pool() -> PgPool { /* copy the pool() helper from tests/db_deposits.rs verbatim, changing only the database-name suffix to _orch_addresses (the file uses _orch_deposits) */ }

#[tokio::test]
async fn a_users_address_is_stable_across_calls() {
    // The whole point of the change: a user has ONE address, forever. If a second call derived a
    // second address, every deposit sent to the first would arrive somewhere nothing watches.
    let pool = pool().await;
    let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();

    let first = addresses::address_for_user(&pool, &deriver, "0xuser-a", "0xclt-a").await.unwrap();
    let second = addresses::address_for_user(&pool, &deriver, "0xuser-a", "0xclt-a").await.unwrap();

    assert_eq!(first, second);
}

#[tokio::test]
async fn two_users_get_different_addresses() {
    let pool = pool().await;
    let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();

    let a = addresses::address_for_user(&pool, &deriver, "0xuser-a", "0xclt-a").await.unwrap();
    let b = addresses::address_for_user(&pool, &deriver, "0xuser-b", "0xclt-b").await.unwrap();

    assert_ne!(a, b, "sharing an address between users would credit one user's deposit to another");
}

#[tokio::test]
async fn indexes_come_from_the_shared_sequence_and_never_repeat() {
    // Legacy per-intent addresses already hold issued indexes. Reusing one would hand a new user an
    // address a previous depositor was told to pay into.
    let pool = pool().await;
    let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();

    // Burn an index the way a legacy deposit would have.
    let burned: i64 = sqlx::query_scalar("SELECT nextval('deposit_derivation_index_seq')")
        .fetch_one(&pool).await.unwrap();

    addresses::address_for_user(&pool, &deriver, "0xuser-a", "0xclt-a").await.unwrap();
    let got: i64 = sqlx::query_scalar("SELECT derivation_index FROM deposit_addresses WHERE user_pk = '0xuser-a'")
        .fetch_one(&pool).await.unwrap();

    assert!(got > burned, "index {got} must be past the already-issued {burned}");
}

#[tokio::test]
async fn marking_hot_sets_a_future_window() {
    let pool = pool().await;
    let deriver = AddressDeriver::from_account_xpub(XPUB).unwrap();
    addresses::address_for_user(&pool, &deriver, "0xuser-a", "0xclt-a").await.unwrap();

    addresses::mark_hot(&pool, "0xuser-a", 24).await.unwrap();

    let hot: bool = sqlx::query_scalar("SELECT hot_until > now() FROM deposit_addresses WHERE user_pk = '0xuser-a'")
        .fetch_one(&pool).await.unwrap();
    assert!(hot);
}
```

Read `crates/payment-orchestrator/src/derive.rs`'s test module for the real xpub constant and copy it verbatim — do not invent one, since a wrong xpub derives a wallet nothing can sweep.

- [ ] **Step 2: Run to verify they fail**

CI. Expected: FAIL — `addresses` module does not exist.

- [ ] **Step 3: Implement the module**

Create `crates/payment-orchestrator/src/addresses.rs`:

```rust
//! One permanent deposit address per user.
//!
//! Replaces per-intent derivation. The index still comes from `deposit_derivation_index_seq` but is
//! consumed once per user and stored, so the address a depositor was given keeps working for every
//! later deposit. The sequence is never reset — legacy per-intent addresses hold issued indexes,
//! and reusing one would hand a new user an address someone else already paid into.
use sqlx::PgPool;

use crate::derive::AddressDeriver;

/// The user's deposit address, deriving and storing it on first call.
///
/// Idempotent by construction: the INSERT is `ON CONFLICT (user_pk) DO NOTHING` followed by a read,
/// so two concurrent first-calls settle on whichever row won rather than deriving twice.
pub async fn address_for_user(
    pool: &PgPool,
    deriver: &AddressDeriver,
    user_pk: &str,
    clt_address: &str,
) -> Result<String, String> {
    if let Some(addr) = existing(pool, user_pk).await? {
        return Ok(addr);
    }

    let index: i64 = sqlx::query_scalar("SELECT nextval('deposit_derivation_index_seq')")
        .fetch_one(pool)
        .await
        .map_err(|e| format!("allocating a derivation index: {e}"))?;

    let index_u32 =
        u32::try_from(index).map_err(|_| format!("derivation index {index} is out of range"))?;
    let address = deriver.address_at(index_u32)?;

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

async fn existing(pool: &PgPool, user_pk: &str) -> Result<Option<String>, String> {
    sqlx::query_scalar("SELECT address FROM deposit_addresses WHERE user_pk = $1")
        .bind(user_pk)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("reading the deposit address: {e}"))
}

/// Put a user's address on the fast poll tier, because they are about to send.
pub async fn mark_hot(pool: &PgPool, user_pk: &str, window_hours: i64) -> Result<(), String> {
    sqlx::query(
        "UPDATE deposit_addresses SET hot_until = now() + make_interval(hours => $2::int)
         WHERE user_pk = $1",
    )
    .bind(user_pk)
    .bind(window_hours)
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|e| format!("marking the deposit address hot: {e}"))
}
```

Add `pub mod addresses;` to `src/lib.rs` beside the existing module declarations.

- [ ] **Step 4: Verify and commit**

CI. Then:

```bash
git add crates/payment-orchestrator/src/addresses.rs crates/payment-orchestrator/src/lib.rs crates/payment-orchestrator/tests/db_addresses.rs
git commit -m "feat: derive and store one permanent deposit address per user"
```

---

### Task 5: The DepositWatcher seam and tiered selection

**Files:**
- Modify: `crates/payment-orchestrator/src/custody.rs`
- Modify: `crates/payment-orchestrator/src/poller.rs`
- Modify: `crates/payment-orchestrator/src/configuration.rs`
- Test: `crates/payment-orchestrator/tests/db_poller.rs`

**Interfaces:**
- Consumes: Task 2's `hot_until` / `last_polled_at`; the existing `CustodyWatcher::transfers_to`.
- Produces: `pub trait DepositWatcher { async fn poll(&self) -> Result<Vec<ObservedTransfer>, String> }`, `pub struct TieredPoller`, and `OrchConfig.deposit_hot_window_hours: i64`. Task 7 consumes `DepositWatcher`.

**Why a new trait rather than reusing `CustodyWatcher`.** `transfers_to(address)` is address-oriented by construction. A future cursor-based watcher asks "everything since cursor X" and filters locally, so it cannot implement that signature — keeping it would mean the migration rewrites the caller anyway, and calling it a seam would be false comfort. `TieredPoller` keeps using `CustodyWatcher` internally; the seam sits one level up.

- [ ] **Step 1: Write the failing test**

Add to `crates/payment-orchestrator/tests/db_poller.rs`:

```rust
#[tokio::test]
async fn hot_addresses_are_polled_before_cold_ones_and_the_budget_is_respected() {
    // Cost per PASS is what is bounded, not cost per address — otherwise a permanent address per
    // user means one TronGrid request per user who ever existed, every pass.
    let pool = pool().await;
    seed_address(&pool, "0xcold-1", "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH", None).await;
    seed_address(&pool, "0xcold-2", "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", None).await;
    seed_address(&pool, "0xhot", "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx", Some(24)).await;

    let due = payment_orchestrator::poller::due_addresses(&pool, 2).await.unwrap();

    assert_eq!(due.len(), 2, "the per-pass budget is a hard cap");
    assert_eq!(due[0].address, "TYJPRrdB5APNeRs4R7fYZSwW3TcrTKw2gx", "hot first");
}

#[tokio::test]
async fn cold_addresses_rotate_oldest_polled_first() {
    let pool = pool().await;
    seed_address(&pool, "0xa", "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH", None).await;
    seed_address(&pool, "0xb", "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK", None).await;
    sqlx::query("UPDATE deposit_addresses SET last_polled_at = now() WHERE user_pk = '0xa'")
        .execute(&pool).await.unwrap();

    let due = payment_orchestrator::poller::due_addresses(&pool, 1).await.unwrap();

    assert_eq!(due[0].user_pk, "0xb", "never-polled sorts ahead of just-polled");
}
```

Add a `seed_address(pool, user_pk, address, hot_hours: Option<i64>)` helper to that file inserting into `deposit_addresses` with the next sequence value, setting `hot_until = now() + hours` when `Some`.

- [ ] **Step 2: Run to verify they fail**

CI. Expected: FAIL — `due_addresses` does not exist.

- [ ] **Step 3: Add the config field**

In `crates/payment-orchestrator/src/configuration.rs`, beside `poll_interval_secs`:

```rust
    /// How long a user's address stays on the fast poll tier after they open the deposit panel.
    ///
    /// Long enough that someone who opens the panel, goes to fetch USDT and comes back the next day
    /// is still on the fast path; short enough that the hot set stays a small fraction of all
    /// addresses, which is what makes the per-pass budget mean anything. Setting this very large
    /// collapses tiering back into polling everything — that degrades cost, not correctness.
    pub deposit_hot_window_hours: i64,
```

`OrchConfig` fields have no serde defaults — `redemptions_enabled` boots only because
`config/default.toml` carries `redemptions_enabled = false`. So add to `crates/payment-orchestrator/config/default.toml`:

```toml
# Hours a user's deposit address stays on the fast poll tier after they open the deposit panel.
# See OrchConfig::deposit_hot_window_hours. Very large values collapse tiering into polling everything.
deposit_hot_window_hours = 24
```

Without this line the orchestrator PANICS at boot the moment this field exists, before any env var is
consulted. This is the same trap the payout rail hit twice; do not rely on compose alone.

- [ ] **Step 4: Add the selection query**

In `crates/payment-orchestrator/src/poller.rs`:

```rust
/// One address due for polling this pass.
pub struct DueAddress {
    pub user_pk: String,
    pub address: String,
    pub last_polled_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Hot addresses first, then the coldest. The LIMIT is the whole cost control: permanent addresses
/// never stop being watched, so without a per-pass budget the request count grows with every user
/// who has ever existed. With it, cost per pass is constant and the cold rotation period is simply
/// (addresses / budget) * poll_interval — a number an operator can be told rather than discover.
pub async fn due_addresses(pool: &PgPool, budget: i64) -> Result<Vec<DueAddress>, String> {
    sqlx::query_as::<_, (String, String, Option<chrono::DateTime<chrono::Utc>>)>(
        "SELECT user_pk, address, last_polled_at FROM deposit_addresses
         ORDER BY COALESCE(hot_until > now(), false) DESC, last_polled_at ASC NULLS FIRST
         LIMIT $1",
    )
    .bind(budget)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|(user_pk, address, last_polled_at)| DueAddress { user_pk, address, last_polled_at })
            .collect()
    })
    .map_err(|e| format!("selecting due addresses: {e}"))
}
```

- [ ] **Step 5: Widen `CustodyWatcher::transfers_to` to take a lower bound**

In `crates/payment-orchestrator/src/custody.rs`, change the trait method to
`async fn transfers_to(&self, address: &str, min_timestamp_ms: Option<i64>) -> Result<Vec<ObservedTransfer>, String>;`
and have the TronGrid implementation pass it as the `min_timestamp` query parameter when `Some`.
Update every existing caller and test double to pass `None` — `grep -rn "transfers_to(" crates/` finds
them, including the sweeper's. Behaviour for `None` is unchanged.

- [ ] **Step 6: Add the DepositWatcher seam**

In `crates/payment-orchestrator/src/custody.rs`, below `CustodyWatcher`:

```rust
/// Everything credit-worthy observed since the last call.
///
/// Deliberately NOT address-oriented, unlike `CustodyWatcher`. A future implementation that follows
/// the USDT contract's Transfer events from a stored cursor cannot express itself as
/// `transfers_to(address)` — it asks for everything since a point in time and filters locally. With
/// the seam here instead, that implementation drops in without the credit path learning about it.
#[async_trait]
pub trait DepositWatcher: Send + Sync {
    async fn poll(&self) -> Result<Vec<ObservedTransfer>, String>;
}
```

- [ ] **Step 7: Implement TieredPoller behind the seam**

Without this the trait is dead code and the seam is decorative. In `crates/payment-orchestrator/src/poller.rs`:

```rust
/// The `DepositWatcher` this deployment runs: poll a bounded slice of addresses per pass, hot first.
///
/// Owns the tier state — it stamps `last_polled_at` for every address it polled, whether or not
/// anything arrived, because an address that is never stamped is re-polled every pass forever and
/// the cold rotation never advances.
pub struct TieredPoller {
    pub pool: PgPool,
    pub inner: Arc<dyn CustodyWatcher>,
    pub budget: i64,
}

#[async_trait]
impl DepositWatcher for TieredPoller {
    async fn poll(&self) -> Result<Vec<ObservedTransfer>, String> {
        let due = due_addresses(&self.pool, self.budget).await?;
        let mut found = Vec::new();

        for a in &due {
            // Only transfers since we last looked, minus an hour of overlap. Permanent addresses
            // otherwise re-fetch their entire history every rotation. The overlap is free: a
            // transfer landing between the query and the stamp is re-observed next pass, and
            // credit_transfer is idempotent on tron_tx_id. Epoch MILLISECONDS, per ObservedTransfer.
            let since = a.last_polled_at.map(|t| (t - chrono::Duration::hours(1)).timestamp_millis());
            match self.inner.transfers_to(&a.address, since).await {
                Ok(mut ts) => found.append(&mut ts),
                // One unreadable address must not abort the pass: the others are still due, and a
                // TronGrid blip on one address would otherwise stall every deposit behind it.
                Err(e) => tracing::warn!("polling {}: {e}", a.address),
            }
        }

        let polled: Vec<String> = due.iter().map(|a| a.address.clone()).collect();
        sqlx::query("UPDATE deposit_addresses SET last_polled_at = now() WHERE address = ANY($1)")
            .bind(&polled)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("stamping last_polled_at: {e}"))?;

        Ok(found)
    }
}
```

Stamp even the addresses whose read failed. Otherwise a permanently unreadable address pins itself
to the front of the rotation and starves every other address of its budget slot.

- [ ] **Step 8: Verify and commit**

CI. Then:

```bash
git add crates/payment-orchestrator/src/custody.rs crates/payment-orchestrator/src/poller.rs crates/payment-orchestrator/src/configuration.rs crates/payment-orchestrator/tests/db_poller.rs
git commit -m "feat: tiered address selection behind a DepositWatcher seam"
```

---

### Task 6: The endpoint returns an address

> **Amendment R14 (landed as the Task 6 fix loop, after review).** The body field is GONE.
> `POST /api/v1/deposits` takes no body (any body an old client still sends is ignored — no `Json`
> extractor). The beneficiary is the authenticated identity: `user_pk` must satisfy the node's address
> rule (`clutch-node/src/node/transactions/address.rs::is_valid_address` — strip optional `0x`/`0X`,
> 40 ASCII hex digits) or the route answers 400; `clt_address` stored on `deposit_addresses` is the
> canonical lowercase `0x`-prefixed form, computed by a private helper in `api.rs` (`clutch-chain`'s
> `normalize_address` is private and the orchestrator does not depend on that crate — do not add the
> dependency for two lines). Route-level test fixtures therefore use real-shaped
> addresses, not `"0xuser-a"`. Everything below that says `{"clt_address": ...}` is superseded.

**Files:**
- Modify: `crates/payment-orchestrator/src/api.rs:71-95`
- Modify: `crates/payment-orchestrator/src/deposits.rs`
- Test: `crates/payment-orchestrator/tests/db_deposit_api.rs`

**Interfaces:**
- Consumes: `addresses::address_for_user`, `addresses::mark_hot` (Task 4); `OrchConfig.deposit_hot_window_hours` (Task 5).
- Produces: `POST /api/v1/deposits` responds `{"address": "<base58>"}` and takes no amount.

- [ ] **Step 1: Write the failing test**

Add to `crates/payment-orchestrator/tests/db_deposit_api.rs`, following that file's existing request idiom:

```rust
#[tokio::test]
async fn the_deposit_endpoint_returns_a_stable_address_and_needs_no_amount() {
    // The user asks where to send, not how much they promise to send. Two calls must give the same
    // address, or money sent to the first arrives somewhere nothing watches.
    let pool = pool().await;
    let treasury = mock_treasury_with_generous_headroom().await;
    let config = test_config(treasury.uri());
    let app = router_with(pool.clone(), config);

    let post = |app: axum::Router| async move {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/deposits")
            .header("authorization", bearer_for("0xuser-a"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"clt_address":"0xclt-a"}"#))
            .unwrap();
        body_json(tower::ServiceExt::oneshot(app, req).await.unwrap()).await
    };

    let first = post(app.clone()).await;
    let second = post(app).await;

    assert_eq!(first["address"], second["address"]);
    assert!(first["address"].as_str().unwrap().starts_with('T'));
    assert!(first.get("amount_usdt").is_none(), "no amount is asked for or echoed");
}
```

These helpers already exist in that file: `pool()`, `mock_treasury_with_generous_headroom()`,
`test_config(treasury_url)`, `bearer_for(pk)`, `router_with(pool, config)`, `body_json(resp)`. Read how
the existing tests there build and send a request — match that exactly (they may use a different
request idiom than the sketch above; the sketch shows intent, the file shows the convention).

**Retire exactly these FIVE tests in that file** — every one exercises the amount-bearing,
idempotency-keyed create flow this task removes, so deleting them is the point, not collateral:
`replay_same_key_same_body_returns_original_status_and_body`, `same_key_different_body_returns_409`,
`retry_while_processing_returns_409_with_retry_after`, `out_of_bounds_amount_returns_400`,
`missing_idempotency_key_returns_400`.

**Keep** `get_deposit_rejects_non_owner` (the GET path is unchanged) and `missing_auth_returns_401`
(auth still gates the route) — but update the latter's request to the new body shape
(`{"clt_address": ...}`, no amount, no `idempotency-key` header) so it exercises the route as it now
exists rather than a request the handler no longer parses.

- [ ] **Step 2: Run to verify it fails**

CI. Expected: FAIL — the handler still requires `amount_usdt` and returns an intent.

- [ ] **Step 3: Rewrite the handler**

Replace `create_deposit_handler` in `crates/payment-orchestrator/src/api.rs`:

```rust
/// `POST /api/v1/deposits` — where to send USDT.
///
/// No amount, and no intent. The user has one permanent address; whatever arrives at it is credited
/// in full by the poller. This is idempotent by nature rather than by an idempotency key: a user has
/// exactly one address, so a repeat call is the same answer.
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
```

Reduce `CreateDepositBody` to a single field — `clt_address` — and delete the `amount_usdt` field
and the `idempotency-key` handling. The CLT destination is still the caller's to state; only the
AMOUNT is gone. Add `use crate::addresses;`.

On a repeat call the stored `clt_address` wins: `ON CONFLICT (user_pk) DO NOTHING` means a user who
sends a different one later keeps their original destination. That is deliberate — silently
re-pointing where someone's future deposits mint is worse than ignoring the change — but it must be
stated in the doc comment so it is a decision rather than an accident.

- [ ] **Step 4: Remove the now-unreachable intent-creation path**

`deposits::create_and_invoice` and its `insert_new` are no longer called by the API. Delete them
along with BOTH deposit-side bounds variants — `ApiError::OutOfBounds` (`deposits.rs`, raised inside
`insert_new`) and `DepositOutcome::OutOfBounds` (`deposits.rs`, matched by the old handler at
`api.rs:107`) — and the `same_body` idempotency helper. If `CreateOutcome` or `DepositOutcome` has no
remaining constructor after that, delete the enum too rather than leaving an unreachable variant set.

**Do NOT touch `RedemptionOutcome::OutOfBounds`** (`redemptions.rs`, matched at `api.rs:220`) — that is
the redemption bounds check, a different flow, and it stays.

Leave everything that reads or advances existing intents — `deposits::find_by_id` (used by the kept
`get_deposit_handler`), the poller, and the treasury bridge all still use those.

If any test covers only the deleted path, delete that test too; do not keep a test alive by pointing it at something else.

- [ ] **Step 5: Verify and commit**

CI. Then:

```bash
git add crates/payment-orchestrator/src/api.rs crates/payment-orchestrator/src/deposits.rs crates/payment-orchestrator/tests/
git commit -m "feat: the deposit endpoint hands back an address, not an invoice"
```

---

### Task 7: Credit each transfer on its own

**Files:**
- Modify: `crates/payment-orchestrator/src/custody.rs` (delete `evaluate_payment` and `PaymentOutcome`)
- Modify: `crates/payment-orchestrator/src/poller.rs`
- Modify: `crates/payment-orchestrator/src/main.rs` (build the `TieredPoller { pool, inner, budget: MAX_ADDRESSES_PER_PASS }` and hand it to `run` — Step 5 already requires this; listed here so the file is in scope)
- Test: `crates/payment-orchestrator/tests/db_poller.rs`

**Interfaces:**
- Consumes: `due_addresses`, `DepositWatcher` (Task 5); `uq_deposit_intents_tron_tx_id` (Task 2).
- Produces: `pub async fn credit_transfer(pool: &PgPool, user_pk: &str, clt_address: &str, t: &ObservedTransfer) -> Result<bool, String>` — `Ok(true)` when a new row was created.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn two_transfers_to_one_address_are_two_credits() {
    // The core of the change, and impossible to express under the uniqueness index dropped in
    // Task 3. One user, one address, two deposits.
    let pool = pool().await;
    seed_address(&pool, "0xuser-a", ADDR, None).await;

    let a = observed("tx-one", ADDR, 1_000_000);
    let b = observed("tx-two", ADDR, 2_500_000);
    assert!(payment_orchestrator::poller::credit_transfer(&pool, "0xuser-a", "0xclt-a", &a).await.unwrap());
    assert!(payment_orchestrator::poller::credit_transfer(&pool, "0xuser-a", "0xclt-a", &b).await.unwrap());

    let (rows, total, off_par): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), COALESCE(SUM(received_usdt),0)::BIGINT,                 count(*) FILTER (WHERE amount_clt <> received_usdt OR amount_usdt <> received_usdt)            FROM deposit_intents WHERE deposit_address = $1")
        .bind(ADDR).fetch_one(&pool).await.unwrap();
    assert_eq!(rows, 2);
    assert_eq!(total, 3_500_000, "each transfer credited in full — credit everything, cap nothing");
    // Task 6 retired `amount_clt_equals_amount_usdt_at_par` with the create() flow; the par rule
    // (1 micro-USDT = 1 CLT base unit) now lives in credit_transfer's INSERT, so it is pinned here.
    assert_eq!(off_par, 0, "amount_clt and amount_usdt must both equal what arrived");
}

#[tokio::test]
async fn the_same_transfer_seen_twice_is_one_credit() {
    // A poll pass re-reads an address's recent history every rotation, so re-observation is the
    // normal case, not an edge case. Identity is the transaction.
    let pool = pool().await;
    seed_address(&pool, "0xuser-a", ADDR, None).await;
    let t = observed("tx-one", ADDR, 1_000_000);

    assert!(payment_orchestrator::poller::credit_transfer(&pool, "0xuser-a", "0xclt-a", &t).await.unwrap());
    assert!(!payment_orchestrator::poller::credit_transfer(&pool, "0xuser-a", "0xclt-a", &t).await.unwrap(),
        "the second sighting creates nothing");

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM deposit_intents WHERE deposit_address = $1")
        .bind(ADDR).fetch_one(&pool).await.unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn a_one_cent_deposit_is_credited_in_full() {
    // "Credit everything, cap nothing" — there is no floor. Dust costs more TRX to sweep than it is
    // worth, but that lands on the sweep threshold and the fee account, never on the user.
    let pool = pool().await;
    seed_address(&pool, "0xuser-a", ADDR, None).await;

    payment_orchestrator::poller::credit_transfer(&pool, "0xuser-a", "0xclt-a", &observed("tx-dust", ADDR, 10_000)).await.unwrap();

    let got: i64 = sqlx::query_scalar("SELECT received_usdt FROM deposit_intents WHERE tron_tx_id = 'tx-dust'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(got, 10_000);
}
```

A fourth test, added after Task 5's review: `TieredPoller::poll` itself had no coverage, and its
"stamp `last_polled_at` even when TronGrid fails" property is what stops one broken address from being
re-polled every pass while the others starve. Cover it here, where the poller is finally wired in:

```rust
#[tokio::test]
async fn a_failing_address_is_still_stamped_and_does_not_block_the_others() {
    // Two users with permanent addresses (via `address_for_user`), budget covering both.
    // wiremock: 500 for A's `/v1/accounts/{A}/transactions/trc20`, one valid transfer for B.
    // Run one pass over the per-user loop (poll_once, or TieredPoller::poll + credit_transfer —
    // whichever Step 5 makes natural).
    // Assert: B's transfer is credited (one deposit_intents row carrying its tx id);
    //         last_polled_at IS NOT NULL for BOTH A and B;
    //         the pass returned Ok — A's failure was logged, not propagated.
    // Fails if stamping moves inside the success branch, or if a `?` on one address aborts the loop.
}
```

Add `const ADDR: &str = "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH";` and an `observed(tx_id, to, amount)` helper building an `ObservedTransfer` with the configured USDT contract and a fixed `block_timestamp`.

- [ ] **Step 2: Run to verify they fail**

CI. Expected: FAIL — `credit_transfer` does not exist.

- [ ] **Step 3: Implement crediting**

In `crates/payment-orchestrator/src/poller.rs`:

```rust
/// Record one observed transfer as its own deposit.
///
/// Returns `Ok(false)` when the transaction was already recorded. That is the normal case, not an
/// error: a poll pass re-reads an address's recent history every rotation, so the same transfer is
/// seen many times. `uq_deposit_intents_tron_tx_id` is what makes re-observation free.
///
/// The amount credited is what ARRIVED. There is no expected figure to reconcile against any more —
/// the user was never asked for one.
pub async fn credit_transfer(
    pool: &PgPool,
    user_pk: &str,
    clt_address: &str,
    t: &ObservedTransfer,
) -> Result<bool, String> {
    let done = sqlx::query(
        // client_key is NOT NULL and was the user's idempotency key when users created intents. The
        // chain creates them now, so the tx id IS the idempotency key — and it makes the pre-existing
        // UNIQUE (user_pk, client_key) a second guard behind uq_deposit_intents_tron_tx_id.
        // expires_at is NOT NULL and meaningless for an observed transfer; now() reads as "already
        // settled" rather than inventing a deadline nothing enforces.
        "INSERT INTO deposit_intents
            (id, user_pk, clt_address, amount_usdt, amount_clt, status, client_key,
             deposit_address, tron_tx_id, received_usdt, expires_at)
         VALUES ($1, $2, $6, $3, $3, 'confirmed', $5, $4, $5, $3, now())
         ON CONFLICT (tron_tx_id) WHERE tron_tx_id IS NOT NULL DO NOTHING",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(user_pk)
    .bind(t.amount_usdt)
    .bind(&t.to)
    .bind(&t.tx_id)
    .bind(clt_address)
    .execute(pool)
    .await
    .map_err(|e| format!("crediting {}: {e}", t.tx_id))?;

    Ok(done.rows_affected() == 1)
}
```

Read `deposit_intents`' columns in `migrations/0001_orchestrator.sql` first and match the real NOT NULL set — the column list above must be adjusted to whatever that table actually requires, and `clt_address` may need the user's registered CLT address rather than `user_pk`. State in your report which columns you had to add and why.

- [ ] **Step 4: Delete the amount-matching logic**

Remove `evaluate_payment` and `PaymentOutcome` from `custody.rs`, and these tests with them:
`exact_amount_settles`, `a_rounded_payment_settles_instead_of_stranding`,
`overpayment_settles_at_the_observed_total`, `underpayment_is_partial_never_settled`,
`two_part_payment_settles_once_the_sum_reaches_expected`.

`a_duplicate_transaction_id_is_counted_once` and `absurd_amounts_saturate_rather_than_overflow` are
deleted WITH `evaluate_payment`: both call it (custody.rs:312, :319) and there is no surviving pure
function to move them to. Do not invent one. Their properties have moved: de-duplication is now the
database's job (`uq_deposit_intents_tron_tx_id` + `ON CONFLICT DO NOTHING`, pinned by
`the_same_transfer_seen_twice_is_one_credit` above), and there is no sum left to saturate — each
transfer is credited on its own at its own `amount_usdt`.
`approval_events_are_dropped` does NOT call `evaluate_payment` — it exercises the decoder — so it
stays exactly where it is, untouched.

Every deleted branch existed only because a user promised a figure in advance. Deleting them is the
point of the change, not collateral damage.

- [ ] **Step 5: Rewrite `poll_once` around addresses**

`poll_once` currently walks due intents and takes a `&dyn CustodyWatcher`. It becomes TWO loops, and
the second one is easy to delete by mistake — do not.

**(a) Per-user addresses.** Take a `&dyn DepositWatcher`, call `watcher.poll().await?` once, and pass
each returned transfer to `credit_transfer`, resolving `user_pk` and `clt_address` from
`deposit_addresses` by `t.to`. A transfer whose `to` has no `deposit_addresses` row is NOT an error —
it is a legacy address, handled by (b). Address selection and `last_polled_at` stamping live inside
`TieredPoller` (Task 5), so this loop never learns how transfers were found — the point of the seam.

**(b) Legacy per-intent addresses.** Keep the existing `due_intents` loop — the SELECT over
`deposit_intents WHERE deposit_address IS NOT NULL AND NOT payment_window_closed AND status IN (...)`
— so stage's 28 legacy rows stay watched until their payment windows close. Nothing in
`deposit_addresses` covers them, and silently unwatching a payable address is the exact loss this
change exists to prevent. Keep the settle-by-intent mechanics (`set_tron_tx_id`, `set_received_usdt`,
the `transition` to confirmed) but drop the expected-amount arithmetic: "credit everything" applies to
legacy too, so the FIRST unseen transfer to a legacy address settles that intent at the full arrived
amount. `Partial` is meaningless when nothing is expected; delete that arm with `evaluate_payment`.
Pass `None` as `min_timestamp` here — a legacy address has no `last_polled_at`.

Give this loop its own small cap (the legacy set is 28 rows and only shrinks) so it cannot eat the
per-user budget. **Keep `sweep_expired` and `close_stale_watch_windows` running each pass** — they are
what retires legacy rows (`created` → `expired` → window closed `WATCH_WINDOW_HOURS` later; never
`needs_manual`). Without them the legacy set never shrinks and loop (b) runs forever. When
`due_intents` returns nothing for good this loop is dead code — leave a comment saying so and when it
can be removed.

Update `run()`'s signature and its caller in `main.rs` to construct a `TieredPoller` wrapping the
existing `CustodyWatcher`, and pass the raw `CustodyWatcher` through as well for loop (b).

Pass each address's previous `last_polled_at` to the TronGrid call as `min_timestamp` (epoch
MILLISECONDS — `ObservedTransfer::block_timestamp` documents that unit). Addresses are permanent
now, so without this every cold rotation re-fetches an address's entire history. Subtract a
generous overlap (an hour) before sending it: a transfer landing between the query and the stamp
would otherwise be skipped forever, and re-observing one is free because `credit_transfer` is
idempotent on `tron_tx_id`.

Resolve each transfer's `user_pk` AND `clt_address` by looking up `t.to` in `deposit_addresses` (both columns live there). A transfer to an
address with no row is a legacy per-intent address — leave those to the existing intent path and do
not credit them here.

- [ ] **Step 6: Verify and commit**

CI. Then:

```bash
git add crates/payment-orchestrator/src/poller.rs crates/payment-orchestrator/src/custody.rs crates/payment-orchestrator/tests/
git commit -m "feat: credit every arriving transfer on its own"
```

---

### Task 8: Feature flag and config cleanup

**Files:**
- Modify: `crates/payment-orchestrator/src/configuration.rs`
- Modify: `crates/payment-orchestrator/src/api.rs`

**Interfaces:**
- Produces: `OrchConfig.permanent_deposit_addresses_enabled: bool`.

- [ ] **Step 1: Add the flag, gate the route**

Mirror `redemptions_enabled`'s existing shape exactly — read it in `configuration.rs` and `api.rs`
and follow it. When the flag is false, the deposit route returns `503` with a body saying deposits
are temporarily unavailable, before authentication, the same ordering `redemptions_enabled` uses.

Add `permanent_deposit_addresses_enabled = false` to `crates/payment-orchestrator/config/default.toml`
next to `redemptions_enabled = false` — `OrchConfig` has no serde defaults, and a bool field absent from
both TOML and env panics `OrchConfig::load()` at boot. `false` is the only safe default: the flag
protects the rollout, and a service that boots with new addresses enabled by accident has already
issued addresses nothing planned to watch.

Document on the field that **the flag protects the rollout, not the decision**: once a user has been
handed an address and sent USDT to it, that address must be watched and swept forever regardless of
what is switched off later. Turning it off stops new addresses being issued; it does not un-issue
the ones already out.

- [ ] **Step 2: Delete the dead deposit bounds**

Remove `min_deposit_usdt` and `max_deposit_usdt` from `OrchConfig` and from `config/default.toml`.
`docker-compose.treasury.yml` has NO corresponding env lines (verified) — confirm with a grep and skip;
do not go hunting for them. Nothing reads them after Task 6, and a
bound that is read by nothing but still appears in config reads to the next person like a live
control on how much a user may deposit — and they would be wrong.

Also remove `deposit_ttl_minutes` — verified: after Task 6 nothing in `src/` reads it.

Exact deletions, verified against the tree:
- `config/default.toml` lines `deposit_ttl_minutes = 30`, `min_deposit_usdt = 1000000`, `max_deposit_usdt = 50000000`.
- The three fields from every `OrchConfig { .. }` literal under `tests/` — at the time of writing `tests/db_bridge.rs`, `tests/db_deposit_api.rs`, `tests/db_redemptions.rs`
  (`tests/db_deposits.rs` no longer builds one since Task 6 recreated it) — a literal naming a removed field
  fails to compile.
- The three `pub` fields from `OrchConfig` itself.
Re-grep `min_deposit_usdt|max_deposit_usdt|deposit_ttl_minutes` across `crates/` afterwards; it must
return nothing. Do NOT touch `min_redemption_clt` / `max_redemption_clt` — those are the redemption
bounds and stay.

Do not trust that list: run `grep -nE 'min_deposit_usdt|max_deposit_usdt|deposit_ttl_minutes' crates/payment-orchestrator/tests/*.rs` and fix EVERY hit (four files at the time of writing, `db_redemptions.rs` included), or the crate's test suites stop compiling the moment the fields leave `OrchConfig`.

- [ ] **Step 3: Verify and commit**

CI. Then:

```bash
git add crates/payment-orchestrator/src/configuration.rs crates/payment-orchestrator/src/api.rs crates/payment-orchestrator/config/default.toml
git commit -m "feat: gate permanent deposit addresses, drop the dead amount bounds"
```

---

### Task 9: The top-up UI

**Files:**
- Modify: `../clutch-hub-demo-app/src/components/DepositPanel.jsx`

- [ ] **Step 1: Remove the amount step**

Branch from the demo app's CURRENT `main` first. A separate map-tile fix (`fix/map-tiles-no-api-key`,
`src/config.js`) may have merged since this plan was written; it touches a different file, but basing
on stale `main` invites a needless conflict on merge.

Read the file first — it currently collects an amount and calls `POST /api/v1/deposits` with it.
Replace that flow with: on mount (or on opening the panel), call the endpoint with no body, show the
returned address plus a QR code if the component already has one, and keep the existing testnet
guidance block.

Keep the warning that only Nile USDT (TRC-20) is creditable — that text is unchanged by this work
and remains true.

Remove any copy implying an exact figure must be sent, including the "minimum" wording added
earlier: there is no minimum any more. Also remove the discriminator-era copy about an "exact
`pay_amount_usdt`" — that mechanism is long gone and the words are simply false now.

**Remove the per-intent status polling.** The panel currently POSTs, keeps the returned intent `id`,
and polls `GET /api/v1/deposits/{id}` every 5s while a deposit is "open". After this change the POST
returns `{"address": ...}` and no id — the user never creates an intent, so there is nothing to poll
and no "open deposit" state to track. Delete the poll loop, the `idempotencyKeyRef`, the
`Idempotency-Key` header, and the amount input/validation. The panel's job now ends at showing the
address; crediting appears where balances and transaction history already do. Do not invent an id to
keep the loop alive.

- [ ] **Step 2: Verify and commit**

Run `npm run lint` in `../clutch-hub-demo-app` and compare warnings against `main` for the SAME
files rather than trusting a total. Then commit in that repo:

```bash
git add src/components/DepositPanel.jsx
git commit -m "feat: show a permanent deposit address instead of asking for an amount"
```

---

### Task 10: Config, ops and docs

**Files:**
- Modify: `../clutch-deploy/docker-compose.treasury.yml`
- Modify: `../clutch-deploy/CLAUDE.md`, `../CLAUDE.md`
- Modify: `crates/payment-orchestrator/config/default.toml`

- [ ] **Step 1: Environment**

Add to the orchestrator's env block, and REMOVE `MIN_DEPOSIT_USDT` / `MAX_DEPOSIT_USDT` if present:

```yaml
      # How long a user's address stays on the fast poll tier after they open the deposit panel.
      # Larger values collapse tiering back into polling every address every pass — a cost
      # regression, not a correctness one.
      - APP_DEPOSIT_HOT_WINDOW_HOURS=${DEPOSIT_HOT_WINDOW_HOURS:-24}
      # Rollout gate. Off means no NEW addresses are issued; it does not un-issue the ones already
      # handed out, and money sent to an address nothing watches is money nobody credits.
      - APP_PERMANENT_DEPOSIT_ADDRESSES_ENABLED=${PERMANENT_DEPOSIT_ADDRESSES_ENABLED:-false}
```

Confirm each name against the actual `OrchConfig` field names — `APP_` + the field name uppercased.
Both fields now have `config/default.toml` entries (Tasks 5 and 8), so these compose lines are
OVERRIDES, not requirements: a misnamed one does not panic the orchestrator, it silently fails to
override — which is quieter and therefore worth getting right the first time.

- [ ] **Step 2: Correct the deposit-detection docs**

`../clutch-deploy/CLAUDE.md`'s "Deposit detection" section (verified: lines ~133–134) says **"Every
deposit intent gets its own freshly derived TRON address... one unique index per intent"**, and line
~169 says `get_reserve_balance` "sums unswept addresses plus the treasury". Both are now wrong: addresses are per USER, and the reserve
also sums the payout float. Rewrite that section to describe the tiered poller, the per-user
address, and transaction-keyed identity.

In `../CLAUDE.md` (workspace root — not a git repo, edit in place) there are TWO passages, both
verified present:
- the repo table row for `clutch-treasury/` (around line 15): "deposit intents on per-intent derived
  TRON addresses" → "one permanent derived TRON address per user";
- the data-flow diagram (around lines 28–29): "derives a TRON address per intent from the account
  xpub → watches TronGrid for USDT paid TO it" → "derives one TRON address per user from the account
  xpub → polls it (hot first, then a bounded cold rotation) for USDT paid TO it".
Fix both. A grep for `per-intent` and `per intent` across that file must return nothing afterwards.
In BOTH files, also state in one sentence that the CLT beneficiary of a deposit is the authenticated
identity (the JWT `pk`, address form), that the deposit request carries no body, and that a public-key
token is refused — so nobody later "adds back" a `clt_address` field thinking it was forgotten (R14).

- [ ] **Step 3: Commit**

```bash
cd ../clutch-deploy && git add docker-compose.treasury.yml CLAUDE.md && git commit -m "feat: config and docs for permanent deposit addresses"
```

---

### Task 11: Branch cleanup before the final review

**Files:**
- Modify: `crates/payment-orchestrator/src/api.rs` (unit tests for `canonical_clt_address`; CORS header list)
- Modify: `crates/treasury-service/tests/db_sweeper.rs`, `crates/treasury-service/tests/db_tron_verifier.rs` (stale comments only)
- Modify: `crates/payment-orchestrator/tests/db_derivation_index.rs` (one test rename)

- Consumes: everything above; runs after Task 9 has stopped the UI sending `idempotency-key`.
- Produces: nothing new — this is the debt the reviews deferred, paid in one commit so the final
  whole-branch review reads clean code.

- [ ] **Step 1: Pin `canonical_clt_address`'s edge cases**

Add a `#[cfg(test)] mod tests` block in `api.rs` with plain unit tests (no DB, no HTTP) for the
helper: `"0X"` uppercase prefix accepted and lowercased; a bare 40-hex string with no prefix accepted
and given `0x`; 39 and 41 hex digits rejected; empty string rejected; a non-ASCII hex lookalike
rejected. Each is one `assert_eq!` on `canonical_clt_address(..)`. The review of 811a96c found these
cases verified only by reading — a future edit to the prefix stripping would compile and pass every
existing test.

- [ ] **Step 2: Remove `idempotency-key` from the CORS allow-headers**

Task 6 removed the only route that read it and Task 9 stopped the UI sending it. Delete it from the
allow-list in `build_cors` and from its doc comment. Grep the whole repo for `idempotency-key` and
`idempotency_key` afterwards — the only hits may be in the demo app's git history and in these plan
documents.

- [ ] **Step 3: Fix comments that cite constraints Task 3 dropped**

`db_sweeper.rs` (around lines 17 and 261 at the time of writing) and `db_tron_verifier.rs` (around
line 430) still explain behaviour in terms of `uq_mint_intents_deposit_address` /
`uq_deposit_address`, which no longer exist. Re-read each comment against what the test actually
asserts now and rewrite it in one or two sentences. Do not change any test logic; if a test's
assertion itself turns out to depend on a dropped constraint, STOP and report it instead of editing.

- [ ] **Step 4: Rename the overclaiming test**

`indexes_come_from_the_shared_sequence_and_never_repeat` in `tests/db_derivation_index.rs` asserts
less than its name promises. Read what it asserts and rename it to exactly that — nothing else in
the file changes.

- [ ] **Step 5: Verify and commit**

CI. Then one commit: `chore(treasury): branch cleanup — validator unit tests, dead CORS header, stale comments`.

---

## Rollout

**Deploy A — after Task 3.** Behaviour-neutral. Then **verify reconciliation reads `ok`** via
`inspect-stage.yml` `PROBE=sweeper`. That is the abort point: if the `DISTINCT` change is wrong,
this is where it shows, with no user-visible change deployed.

**Deploy B — after Task 10**, with `APP_PERMANENT_DEPOSIT_ADDRESSES_ENABLED=false`. Confirm the
stack is healthy and reconciliation still `ok`, then flip the flag.

Legacy per-intent addresses are left alone throughout. They stay in `mint_intents`, so the reserve
sum already includes them with no extra work, and they are swept until drained by the existing
sweeper. Nothing new joins that set.
