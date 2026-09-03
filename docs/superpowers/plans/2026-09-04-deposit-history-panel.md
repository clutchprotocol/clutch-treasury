# Deposit history in the top-up panel — implementation plan

Spec: `docs/superpowers/specs/2026-09-04-deposit-history-panel-design.md`
Date: 2026-09-04

Two tasks, one per repo. Task 1 ships alone safely (an endpoint nothing calls yet); Task 2 needs
Task 1 deployed before its list has anything to read.

---

### Task 1: The list endpoint

**Files:**
- Modify: `crates/payment-orchestrator/src/deposits.rs` (a query), `src/api.rs` (a handler + route)
- Test: `crates/payment-orchestrator/tests/db_deposit_api.rs`

- Consumes: `authenticated_pk` (`src/auth.rs`), the owner-check convention in `get_deposit_handler`,
  the `permanent_deposit_addresses_enabled` gate in `create_deposit_handler`.
- Produces: `GET /api/v1/deposits`.

- [ ] **Step 1: Write the failing tests**

In `tests/db_deposit_api.rs`, using the file's existing helpers (`pool()`, `test_config(url, flag)`,
`bearer_for(pk)`, `router_with`, `body_json`, `tower::ServiceExt::oneshot`) and its address-shaped
constants:

```rust
#[tokio::test]
async fn the_list_returns_only_the_callers_own_deposits() {
    // Seed deposits for USER_A and for a second address-shaped user. GET as USER_A.
    // Assert: every returned row belongs to USER_A, and the other user's tron_tx_id appears nowhere
    // in the response body. Fails if the WHERE user_pk clause is dropped — that is the point.
}

#[tokio::test]
async fn the_list_is_newest_first_and_capped_at_twenty() {
    // Seed 21 rows for USER_A with distinct created_at. Assert 20 returned, and that the first is
    // the newest and the oldest row is absent.
}

#[tokio::test]
async fn the_list_reports_what_arrived_not_what_was_asked_for() {
    // One row with amount_usdt = 20000000 and received_usdt = 25000000.
    // Assert the response's amount_usdt is 25000000.
}

#[tokio::test]
async fn an_empty_list_is_two_hundred_not_a_miss() {
    // A user with no deposits: 200 and {"deposits": []}.
}

#[tokio::test]
async fn the_list_is_gated_by_the_rollout_flag() {
    // test_config(..., false) plus a VALID token -> 503; and with NO Authorization header -> 503,
    // not 401, which is what proves the gate runs before authentication.
}
```

Seed with a raw INSERT (there is no create path any more) covering every NOT NULL column without a
default: `id, user_pk, clt_address, amount_usdt, amount_clt, client_key, expires_at`. Mirror
`seed_intent` in this file; give each row a distinct `client_key` — `(user_pk, client_key)` is unique.

- [ ] **Step 2: Run to verify they fail**

CI. They must fail for the stated reason, not on a missing route compiling away.

- [ ] **Step 3: The query**

In `deposits.rs`, beside the other finders:

```rust
/// The caller's own deposits, newest first. Scoped by user_pk — the only thing standing between one
/// user's deposit history and another's, so it is not optional and not a filter applied later.
pub async fn recent_for_user(pool: &PgPool, user_pk: &str, limit: i64)
    -> Result<Vec<DepositIntent>, sqlx::Error>
```

`SELECT` the existing struct's columns `WHERE user_pk = $1 ORDER BY created_at DESC LIMIT $2`. Read
how the file's other queries select into `DepositIntent` and follow that exactly.

- [ ] **Step 4: The handler and route**

In `api.rs`, modelled on `get_deposit_handler` and on `create_deposit_handler`'s gate:

- The `permanent_deposit_addresses_enabled` 503 check FIRST, before `authenticated_pk`.
- `let user_pk = authenticated_pk(&headers, &state.config)?;`
- `recent_for_user(&state.pool, &user_pk, 20)`, mapped to
  `{"deposits": [{id, status, amount_usdt, tron_tx_id, created_at}]}` where `amount_usdt` is
  `received_usdt.unwrap_or(amount_usdt)`.
- Register `.route("/api/v1/deposits", post(create_deposit_handler).get(list_deposits_handler))` —
  same path, added method; do NOT invent a second path.

Return the raw `status` string. The panel maps it to words a person reads; the API stays honest so the
next reader is not misled.

- [ ] **Step 5: Verify and commit**

CI. Then: `feat(orchestrator): the caller can list their own deposits`.

---

### Task 2: The list in the panel

**Files:**
- Modify: `../clutch-hub-demo-app/src/components/DepositPanel.jsx`

- Consumes: Task 1's endpoint, deployed.
- Produces: a short deposit list under the address.

- [ ] **Step 1: Fetch alongside the address**

Branch from the demo app's current `main`. In the effect that already runs while `open` is true, also
`GET ${ORCHESTRATOR_BASE_URL}/api/v1/deposits` with the same auth headers, into a `deposits` state.
Refresh on an interval while the panel is open (10s is fine), cleared on close by the existing
cleanup. Keep the `cancelled` guard pattern that is already there. A failed list fetch must never
disturb the address that is already shown — log it, leave the list as it was.

- [ ] **Step 2: Render it**

Under the address block, only when the list is non-empty. Per row: the amount in USDT, the label from
the spec's table, a relative time, and a truncated `tron_tx_id`. Reuse the file's existing class names
and the `timeAgo`/truncation helpers if `TransactionHistory.jsx` already exports usable ones — read it
first; copy the smallest thing that works rather than importing a ride-shaped component.

Map statuses exactly as the spec's table says, and render any unknown status as its raw string rather
than guessing.

- [ ] **Step 3: Verify and commit**

No local build (host cannot). Verify by reading: hooks unconditional, deps complete, no unused
imports, every identifier defined, JSX balanced. Then:
`feat: show recent deposits and their state in the top-up panel`.

**Merging:** Task 1 merges and deploys first. Task 2 merges only after the endpoint is live on stage,
or the panel calls a route that 404s.
