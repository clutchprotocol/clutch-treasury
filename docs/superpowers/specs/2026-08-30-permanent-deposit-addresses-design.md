# Permanent per-user deposit addresses — design

Status: approved design, not yet implemented
Date: 2026-08-30
Scope: `clutch-treasury` (payment-orchestrator, treasury-service), `clutch-hub-demo-app` (UI), `clutch-deploy` (ops)

## Problem

The top-up flow asks a user for an amount before it will show them anywhere to send money. Exchanges
do not work that way: they give you an address, you send what you like, whenever you like, and it is
credited. This design makes the deposit path work the same way.

Most of the arithmetic for this already exists. `deposit_intents.received_usdt` is documented in the
code as *"What actually arrived on chain, which is what gets credited — `amount_usdt` is only what
the [user asked for]"*, and the demo app already tells users the amount is a minimum rather than an
exact figure. The amount is advisory today.

What ties the flow to an amount is structural:

- **Addresses are per-intent.** `allocate_derivation_index` takes `nextval('deposit_derivation_index_seq')`
  on every deposit request.
- **Two unique indexes enforce that**: `uq_deposit_address` / `uq_deposit_derivation_index` on
  `deposit_intents`, and `uq_mint_intents_deposit_address` on the treasury side.
- **Intents expire** (`expires_at`; stage currently holds 26 expired rows).
- **Bounds apply to the requested amount** (`min_deposit_usdt` / `max_deposit_usdt`) — a check that
  can only be made before money moves.

An exchange-style address receives many deposits over its life, which those two unique indexes
forbid. That is the load-bearing change, and it reaches the schema, the watcher, the public API and
the identity of a mint.

## What already exists

Do not rebuild these:

- Address derivation from the account xpub (`derive.rs`), matched by `tron-signer`'s private-key
  derivation at the same path. That agreement is pinned by tests on both sides.
- Confirmation gating per transfer (`deposit_confirmations`), `received_usdt`, and the whole
  credited → mint_requested → credited lifecycle.
- The treasury's four-eyes mint flow, daily and per-tx caps, reconciliation and the breaker. All sit
  downstream of this change and are untouched by it.
- `uq_mint_intents_deposit_tx` on `deposit_tx_id` — already the per-transaction guarantee this
  design leans on.
- `wiremock` as the way TronGrid is faked across the orchestrator's `db_*` suites.

## Decisions

**Permanent address per user**, not per deposit. The address is derived once and stored.

**Credit everything, cap nothing.** Any arriving USDT is credited in full. The daily mint cap and
the reconciliation breaker remain the only ceilings.

**Tiered polling now, with a seam for contract-event watching later.** Rejected: polling every
address every pass (cost grows with users), and building a cursor-based indexer immediately (a real
indexer with reorg handling, which this project has already been bitten by once).

## 1. Address model

```sql
CREATE TABLE deposit_addresses (
    user_pk          TEXT PRIMARY KEY,
    derivation_index BIGINT NOT NULL UNIQUE,
    address          TEXT   NOT NULL UNIQUE,
    hot_until        TIMESTAMPTZ,
    last_polled_at   TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

The index still comes from `nextval('deposit_derivation_index_seq')`, but is consumed **once per
user** and persisted. Same derivation path (`m/44'/195'/0'/0/i`), same xpub, so `tron-signer` signs
for it with no change — that agreement property must not be disturbed.

### The constraints being dropped, and what replaces them

- `uq_deposit_address` and `uq_deposit_derivation_index` on `deposit_intents` — dropped. An address
  now legitimately appears on many rows.
- `uq_mint_intents_deposit_address` on the treasury side — dropped.

The second is a money control, not bookkeeping: it exists so one address cannot be minted against
twice. That guarantee moves to **`uq_mint_intents_deposit_tx`**, which already exists and is
enforced. Every credit is keyed to exactly one on-chain transaction.

This is a strengthening, not a weakening. Under the current model two transfers to one address are
one mint by construction — which is quietly wrong. Under the new one they are correctly two. Both
index changes land in a single migration, so no window exists where neither applies.

Stated plainly: reusing an address links a user's deposits together on a public chain. Exchanges
accept this. It is inherent to the request, not a detail that can be designed away.

## 2. The watcher

`transfers_to(address)` is **one TronGrid request per address**
(`/v1/accounts/{addr}/transactions/trc20`). That is affordable today only because intents close.
Permanent addresses never close.

**Bound the cost per pass, not per address.** Keep `MAX_ADDRESSES_PER_PASS` as a hard budget and
change only what fills it:

```sql
SELECT address, derivation_index FROM deposit_addresses
 ORDER BY COALESCE(hot_until > now(), false) DESC, last_polled_at ASC NULLS FIRST
 LIMIT $1
```

Stamp `last_polled_at` after each pass. Hot addresses first, the rest rotating oldest-first.
The `COALESCE` is load-bearing: a bare `(hot_until > now()) DESC` sorts TRUE, FALSE, NULL as
*three* tiers, so an address that was hot once and expired would permanently outrank one that was
never hot, whatever their `last_polled_at` — starving never-hot users once the once-hot count
reaches the budget. Caught in Task 5 review. Cost
per pass is constant regardless of user count, and the cold rotation period is
`(addresses ÷ budget) × poll_interval` — a figure that can be told to an operator rather than
discovered.

**Hotness is a signal, not a guess.** Opening the deposit panel sets
`hot_until = now() + deposit_hot_window` — the moment a user is about to send. The common case stays
near-real-time; a deposit from someone who never opened the app is still credited, on the cold
rotation.

`deposit_hot_window` defaults to **24 hours**, configurable. Long enough that a user who opens the
panel, goes to fetch USDT and returns the next day is still on the fast path; short enough that the
hot set stays a small fraction of all addresses, which is what keeps the per-pass budget meaningful.
Setting it very large collapses the tiering back into polling everything — that is the failure mode
to watch for, and it degrades cost rather than correctness.

Pass `last_polled_at` as `min_timestamp` so a long-lived address does not re-fetch its whole history
every rotation. This matters more here than today, because addresses now accumulate.

### Correction: the seam is not where I first said it was

The approach was originally justified by saying `CustodyWatcher::transfers_to` was already the seam
for migrating to contract-event watching later. **That was wrong.** `transfers_to(address)` is
address-oriented by construction; a cursor-based watcher asks "all transfers since cursor X" and
filters locally. Keeping that trait would mean the migration rewrites the caller anyway.

For the option to genuinely exist, the seam sits one level up:

```rust
trait DepositWatcher {
    /// Everything credit-worthy observed since the last call.
    async fn poll(&self) -> Result<Vec<ObservedTransfer>, String>;
}
```

Tiered polling is one implementation; a cursor-based one is another; the credit logic never learns
which.

## 3. The credit path

**`evaluate_payment` loses its expected amount, and most of it goes with it.** Partial payments,
two-part settlement, rounding tolerance, underpayment-never-settles — every one of those branches
exists only because an expected amount exists. Each unseen transfer becomes its own credit, in full.

This is a deletion, not a modification. These tests go with the logic they cover:
`underpayment_is_partial_never_settled`, `two_part_payment_settles_once_the_sum_reaches_expected`,
`a_rounded_payment_settles_instead_of_stranding`, `exact_amount_settles`. Removing them is correct.
The survivors were never about matching: `a_duplicate_transaction_id_is_counted_once`,
`absurd_amounts_saturate_rather_than_overflow`, `approval_events_are_dropped`.

**The creation direction inverts.** This is the conceptually largest change here. Today a user
creates an intent and then pays it; after this, the watcher observes a transfer and writes the row.
A `deposit_intents` row stops meaning "a request that may be paid" and starts meaning "USDT that
arrived".

That reshapes the API. `POST /api/v1/deposits` no longer creates an intent — it returns the caller's
address, deriving and storing it on first call, and sets `hot_until`. It becomes idempotent by
nature, because a user has exactly one address.

The CLT beneficiary is the authenticated identity itself: the JWT `pk`, which the demo app already
sets to the user's `0x` address, validated against the node's address rule (40 hex digits, optional
`0x`) and normalized. The request body carries nothing and is ignored. A client cannot bind its
deposits to a different CLT address, by design — under permanence a typo or a stranger's address
would be that user's mint destination forever, and nothing between the body and the node validated
the string. Tokens carrying a 130-hex public key instead of an address are refused with 400 until a
client needs them (the derivation is keccak-256 of the key, as the demo app's wallet code does).
*(Amended after Task 6 review, R14.)*

Its min/max bounds have nothing left to check: there is no longer a figure supplied before money
moves. `min_deposit_usdt` and `max_deposit_usdt` therefore become dead config. **Delete them rather
than leaving them set** — a bound that is read by nothing but still appears in `.env` and the
compose file reads to the next person like a live control on how much a user may deposit, and they
would be wrong. If a per-deposit ceiling is ever wanted, it has to be enforced somewhere that sees
the money *after* it arrives, which is a different mechanism and a different decision.

**Identity is the transaction.** Each row is keyed by `deposit_tx_id`; add the matching unique index
on `deposit_intents.deposit_tx_id` to mirror the treasury's existing one.

The rest of the lifecycle is untouched: confirmations still gate crediting per transfer,
`received_usdt` is already the credited amount, and the four-eyes flow, caps and breaker sit
downstream unchanged.

**A consequence of "credit everything, cap nothing".** A one-cent deposit is credited in full. But
sweeping it costs TRX for energy that may exceed its value, and that cost lands on the fee account
an operator tops up by hand. Dust does not break crediting; it slowly drains the TRX float. The
existing sweep threshold already handles this — it just becomes load-bearing rather than an
optimisation.

## 4. Reconciliation

### Correction: the risk is double-counting, not scaling

This was first flagged as a scaling problem — one `balanceOf` call per unswept address, growing with
users. That is wrong. The unswept set is bounded by unswept *deposits*, not by users, and per-user
addresses **deduplicate** it: five unswept transfers from one user are five addresses today and one
address after. The count goes down.

The real problem is worse and points the other way. `get_reserve_balance` sums every entry in the
slice it is given. Today duplicates are impossible — one address per intent, enforced by the index
being dropped. After the change, an address on five unswept rows would be summed **five times**.

An inflated reserve is far more dangerous than a deflated one. The float bug made the treasury look
under-backed, which halts minting: loud and safe. This makes it look over-backed, which **permits
minting that is not backed** — the one failure a fully-reserved token cannot tolerate.

The fix is `SELECT DISTINCT deposit_address`, with a test that seeds two unswept rows sharing an
address and asserts the balance is counted once, written to fail against the non-distinct query.

### Migrating stage's existing rows: do not

Stage holds 26 expired, 1 created and 1 credited intent, each with a per-intent address that may
still hold unswept USDT, derived from indexes the sequence has already issued. Leave them exactly
where they are:

- **The derivation sequence keeps advancing**, so a new user address can never collide with a legacy
  per-intent one. No renumbering, no reuse, no migration of key material.
- **Legacy addresses stay watched and swept until drained** — a separate, permanently shrinking set
  alongside the per-user rotation. Nothing new joins it.

Reconciliation must include those legacy unswept addresses in the reserve sum during the transition,
or it under-counts and trips the breaker — the failure the payout float's change was written to
prevent, arriving by another route.

## 5. Testing

`wiremock`, following the orchestrator's existing `db_*` suites. The first two matter most:

- **The double-count regression** — two unswept rows sharing an address, reserve counted once.
  Must fail against the non-`DISTINCT` query.
- **Two transfers to one address are two credits** — the core of the new model, and impossible to
  express under the constraint being dropped.
- The same transfer observed twice is **one** credit (`deposit_tx_id` idempotency).
- A user's address is **stable** across repeated calls to the endpoint.
- A new user address **never collides** with a legacy per-intent address.
- Tiering: hot before cold, per-pass budget respected.
- Legacy unswept addresses still counted in the reserve during the transition.

Plus the deletions from section 3.

## 6. Rollout

**Two deploys, not one.**

**Deploy A — schema and reserve, behaviour-neutral.** The `deposit_addresses` table, both index
drops, `SELECT DISTINCT` on the reserve query, the unique index on `deposit_intents.deposit_tx_id`,
and legacy addresses included in the reserve sum. Nothing changes behaviour — dropping a constraint
permits something no code yet does.

**Then verify reconciliation still reads `ok`.** That is the abort point, and it is the same
function the payout float already changed once.

**Deploy B — the inversion.** Watcher tiering, per-user derivation, the API change, the UI. Behind
an orchestrator flag shaped like `redemptions_enabled`, so the endpoint can be switched back.

**The flag protects the rollout, not the decision.** Once a user has been handed a permanent address
and sent USDT to it, that address must be watched and swept **forever**, whatever is decided later.
Turning the flag off stops new addresses being issued; it does not un-issue the ones already out,
and money sent to an unwatched address is money nobody credits. The reversible part is the endpoint;
the irreversible part is every address it ever returned.

## Out of scope

Redemptions and the payout rail (separate work, already shipped). Any change to the four-eyes mint
flow, the caps, or the breaker. Mainnet posture — this remains testnet/Nile with the same key
custody as today.
