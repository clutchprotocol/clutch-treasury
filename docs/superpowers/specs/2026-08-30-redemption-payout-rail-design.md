# Redemption payout rail — design

Status: implemented (merged 2026-08-30); redemptions stay disabled until the section 5 rollout checklist has run
Date: 2026-08-30
Scope: `clutch-treasury` (treasury-service, tron-signer), `clutch-deploy` (ops), workspace `CLAUDE.md`

## Problem

`APP_REDEMPTIONS_ENABLED` is `false` because `treasury-service`'s outbound leg is
`payout::StubRail`, which fabricates `stub:{uuid}` and sends nothing. A user who redeemed would
burn CLT irreversibly and receive no USDT. This design replaces the stub with a working TRC-20
payout so the flag can be turned on for Nile.

The task is not "implement `send_usdt`". The trait is the easy half. **Nothing in the stack can
spend from the custody address.** `APP_TREASURY_ADDRESS` is `CUSTODY_TRON_ADDRESS` from the host
`.env`, externally chosen and not derived from the deposit mnemonic. `tron-signer` holds keys for
exactly two things: deposit addresses at `m/44'/195'/0'/0/i` and the fee account at `1/0`. The
sweeper moves USDT *into* custody; no key in these services moves it *out*.

## What already exists

Do not rebuild these. The request path is complete and careful:

- Address validation (`redemptions::is_valid_tron_address`, base58check with version byte, not a
  shape regex), min/max bounds, and `redeemer_address` taken from the JWT `pk` — never the body.
- Burn-before-pay ordering: `watcher::confirm_burn` is the sole path into `payout_pending`, so the
  payout worker only ever sees intents whose burn is already ledgered.
- Breaker gating in `payout::drain_once` — a treasury that stopped minting must not ship money out
  the other door.
- P1 alerting on payout failure, with the intent left `payout_pending` rather than orphaned.
- `breakers.rs` rolling-24h window (`daily_mint_total`) and per-tx cap, to mirror.
- `chain_outbox` (`pending|submitted|confirmed|failed`, `attempts`, `next_attempt_at`,
  `last_error`) as the retry pattern to follow.
- `sign_and_broadcast` in `sweep.rs`, including txID recomputation, plus `usdt_balance`,
  `trx_balance_sun`, `fund()`, and `transfer_parameter`.
- `wiremock 0.6` as the established way to fake TronGrid in tests.

## Decisions

**Posture: testnet-grade now, KMS seam kept.** Matches the existing mint-authority env-var stub.
`PayoutRail` stays the swap boundary so `KmsSigner` drops in later. The `docs/keys.md` mainnet
blocker (KMS + key ceremony + tested recovery before real funds) stands unchanged.

**Funding source: a new derived hot float**, not a signable custody key and not a separate payout
mnemonic. No new key material, no new secret to provision or rotate, and exposure is capped at
whatever is floated.

**Signer interface: a thin payout endpoint** rather than giving `tron-signer` a database (rejected:
adds Postgres and a second failure mode to the one process whose value is being small) or a signed
authorization scheme (rejected: a fourth key to bound a risk the float cap already bounds).

## 1. Key topology and reserve accounting

### Float account: `m/44'/195'/0'/2/0`

`keys.rs` already treats the change level as a namespace rather than the BIP44 0/1 convention, and
states why: deposit addresses are `0/i` for every i, so a high external index eventually collides
and hands a depositor an address already in use. Change level `2` cannot collide with deposits at
`0/i` or the fee account at `1/0`.

Add `PAYOUT_CHANGE_LEVEL = 2` and `PAYOUT_INDEX = 0`, plus `payout_address()` and
`payout_signing_key()` mirroring `fee_address()` and `fee_signing_key()`. Add `payout_address` to
`/internal/xpub` alongside `fee_address` so ops can find and fund it.

### Reserve accounting

`get_reserve_balance(main_address, unswept_addresses, usdt_contract)` sums custody plus unswept
deposits. The float is a third bucket and **must be counted**, or the first top-up from custody
reads as a shortfall and trips the breaker.

Add an explicit `float_address: &str` parameter rather than folding it into `unswept_addresses`:
that slice's error path attributes failures as "unswept deposit address {addr}", and misreporting
a float RPC failure as a deposit problem is the wrong corner to cut.

Counting the float means float USDT is reserve backing CLT, not spare money. Payouts reduce
liability and reserve together, which the existing `custody_withdrawal` event already records.

## 2. Signer endpoint and caps

`POST /internal/payout {intent_id, to, amount_usdt}` on `tron-signer`, spending only from the
float. Reuses `sign_and_broadcast`, `usdt_balance`, `trx_balance_sun`, and `transfer_parameter`.
The only new code is the handler and its guards.

`contract` stays off the parameter list — the USDT contract comes from config, as it does for
sweep.

### The security amendment, stated plainly

The `sweep.rs` module docs claim: *the request needs no authentication beyond reaching the service,
because the worst a hostile request achieves is sweeping a real deposit into the real treasury
slightly early.*

The payout endpoint **cannot** make that claim. Its safety depends on the token and the
internal-only network holding. For sweep those were defence in depth; here they are load-bearing.
That is a real weakening, and the caps exist because of it.

What still holds, and why this is proportionate: the endpoint can address neither deposit
addresses nor custody, so maximum loss from a fully compromised caller is the float balance. The
caller is `treasury-service`, which already holds mint authority — an attacker there can mint
arbitrarily, making reach into a deliberately small float a bounded marginal escalation. **The
float size is the security control that index-only provided for deposits.**

### Caps, split by where state lives

- **Per-transaction cap in `tron-signer`** — stateless, config-driven, the last line before
  signing. Denominated in **micro-USDT**, matching the `amount_usdt: i64` the endpoint receives and
  the units `usdt_balance` returns.
- **Rolling 24h cap in `treasury-service`** — mirrors `breakers::daily_mint_total`. Denominated in
  **CLT base units**, matching `daily_mint_cap_clt` and the `amount_clt` column it sums. The two
  are numerically equal at the 1:1 par `drain_once` already assumes, but they are different units
  and must not be configured from a shared value.

The daily cap is deliberately not in the signer: it has no database, in-memory state resets on
restart (a cap an attacker clears with a crash), and a per-payout TronGrid tally costs a round trip
for something the treasury can already compute. The float balance is the outermost bound and needs
no code.

### Comments this invalidates

- The `fund()` comment claiming the fee account has only one spender.
- The `CLAUDE.md` rule forbidding `to`/`contract`/`amount`, in both the workspace root and
  `clutch-deploy` — amended to scope the index-only constraint to the sweep endpoint and state why
  payout differs. Amend it as a rule; do not leave a quiet exception.

## 3. Payout lifecycle

Migration `0008` adds **`payout_submitted`** to the `redemption_intents.status` CHECK constraint
(currently `created, burn_confirmed, payout_pending, paid, expired, failed`). `payout_ref` already
exists and becomes the Tron tx id — no new column.

### The double-pay window

Treasury calls the signer, the signer broadcasts, the response is lost, the current loop retries,
and pays twice. `StubRail` never broadcasts, so today's code cannot hit it; the moment the rail is
real, that retry is a money bug. The `drain_once` docs say orphaning a burn is the one outcome the
function must never produce — double-paying is the mirror-image sin and needs equal weight.

TRC-20 `transfer()` has no memo, so an intent cannot be tagged on chain for signer-side dedupe.
Matching on (address, amount) is unsafe: a user legitimately redeeming the same amount twice to the
same address is normal, not a duplicate.

The fix is to separate **definitely did not broadcast** from **do not know**, and never auto-retry
the second:

1. `payout_pending` becomes `payout_submitted`, committed **before** the signer call, so a crash
   mid-call also lands in the ambiguous state rather than looking retryable.
2. The signer returns structured outcomes as `SweepOutcome` already does: `Paid { tx_id }`,
   `FloatDry { .. }`, `CapExceeded { .. }`.
3. `FloatDry` and `CapExceeded` are proof of no broadcast, so the intent returns to
   `payout_pending`, retryable, with an alert.
4. `Paid { tx_id }` writes the tx id to `payout_ref` while the status stays `payout_submitted`. A
   later confirmation pass verifies that tx on chain, and only then writes `paid` plus the
   `custody_withdrawal` event in the existing single transaction with its
   `ON CONFLICT (intent_id, kind)`. Status and ledger move together, as they do today.
5. Anything ambiguous — timeout, transport failure, unclassifiable 5xx — **stays `payout_submitted`
   with no tx id and raises P1. No automatic retry, ever.** A human or reconciler reads the float's
   outbound transfers and resolves it either way.

Step 5 will occasionally wake someone up. That is the deliberate trade: an ambiguous payout is
rare, a stuck redemption is recoverable, a double payment is not.

### Dry float

Mirrors `FeeAccountDry`: distinct outcome, P1, intent stays `payout_pending`, burn never orphaned.
The float needs TRX for energy like any sending address, so generalize the existing `fund()` to top
it from the fee account rather than adding a second funding mechanism.

Breaker gating is unchanged and stays first in `drain_once`.

## 4. Testing

`wiremock 0.6` for TronGrid, following `db_sweeper.rs` and `db_tron_verifier.rs`. The burn side is
already covered by `db_redemption.rs`.

- **`keys.rs` unit tests** — direct copies of the fee account's four: the float address is never a
  deposit address, never the fee address, is stable across instances, is valid base58check, and its
  key controls it.
- **Double-pay regression, the one that matters most** — simulate a lost response after a
  successful broadcast; assert **exactly one** transfer is broadcast and the intent lands in
  `payout_submitted` with a P1 rather than being retried.
- **State machine** — `payout_submitted` is claimed before the call; `FloatDry` and `CapExceeded`
  return the intent to `payout_pending`; confirmation writes `paid` and the ledger event exactly
  once.
- **Signer guards** — the per-tx cap refuses; the endpoint cannot be induced to spend from a
  deposit index or from custody.
- **Reconciliation** — top up the float, assert reconciliation stays `ok`. This turns the section 1
  trap into a test, and that failure mode has already tripped the breaker on stage once.

## 5. Rollout

The flag flip is last:

1. Ship the rail with `APP_REDEMPTIONS_ENABLED` still `false`. **Not dead code**:
   `treasury-service` panics at boot without `payout_float_address` and `daily_payout_cap_clt` set
   (no serde default, absent from `config/default.toml`), so both must be configured for the
   service to start at all — and its payout workers (`drain_once` / `confirm_payouts_once`) then
   run on their normal poll cadence unconditionally, whether or not anyone can create a
   redemption. `redemptions_enabled` lives only in `payment-orchestrator`, gating just the two
   HTTP routes that create/read one; it never reaches `treasury-service`. Verify `payout_pending`
   has zero rows before deploying — nothing else stops these workers from acting on one.
2. Surface the float in `/internal/xpub`; add it to the `treasury` probe in `inspect-stage.yml`,
   beside the existing TRX float line.
3. Fund it — USDT from custody, TRX from the fee account.
4. **Confirm reconciliation still reads `ok` with the float counted.** If section 1 is wrong, this
   is where it shows, with the flag off and nothing at risk.
5. One small end-to-end redemption on Nile, verified on chain.
6. Flip the flag.

## 6. Docs to update in the same change

- `docs/keys.md` — the payout key now exists as a derived float; the custody key still does not;
  `KmsSigner` remains the mainnet blocker.
- The `fund()` comment in `sweep.rs`.
- `CLAUDE.md` in the workspace root and in `clutch-deploy` — the amended sweep/payout rule.

## Out of scope

`KmsSigner`, the separate custody key, and any mainnet posture. This makes redemptions work on Nile
with the seam intact. The mainnet blocker stands unchanged.
