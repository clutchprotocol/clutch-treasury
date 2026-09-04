# Deposit history in the top-up panel — design

Status: implemented — live on stage 2026-09-04
Date: 2026-09-04
Scope: `clutch-treasury` (payment-orchestrator), `clutch-hub-demo-app` (DepositPanel)

## Problem

The top-up panel hands out a permanent address and says the money "is credited automatically and
appears in your balance". When that takes two minutes, the claim is fine. On 2026-09-03 it took
forty-one, and the panel said nothing at all for the whole of it — no row, no status, no
acknowledgement that the transfer had even been seen. The operator had to read the database to learn
that the deposit was fine and waiting behind a cap.

That silence is the worst possible behaviour for a money page. A user who has just sent USDT and sees
nothing cannot tell "still working" from "lost".

**No existing view can close this.** `TransactionHistory.jsx` is ride-centric — nothing in it or in
the SDK's history query mentions mints. And even a perfect chain-history view would not help: between
the USDT landing on Tron and the CLT minting on Clutch there is nothing on the Clutch chain to show.
That window is exactly the anxious one. Only `deposit_intents` in the orchestrator knows anything
during it.

## What already exists

Do not rebuild these:

- `deposit_intents` rows already carry everything needed: `status`, `amount_usdt`, `received_usdt`,
  `tron_tx_id`, `created_at`, `user_pk`.
- `authenticated_pk` and the owner-check pattern in `get_deposit_handler` — a deposit belonging to
  someone else returns 404, not 403, and that convention stays.
- The panel already calls the backend when it opens (the hot-window signal), so there is a natural
  place to fetch a list without inventing new polling behaviour.
- `credit_transfer` writes one row per on-chain transfer keyed by `tron_tx_id`, so the list is
  per-transfer, which is what a user recognises.

## The status progression, in the user's words

The row's life is already legible; it just needs honest labels. Mapping happens in the UI, not the
API — the API keeps returning the real status so the next reader is not lied to.

| Row status | Panel says | Means |
|---|---|---|
| `confirmed` | Detected | Seen on Tron, credited in the ledger, mint not requested yet |
| `mint_requested` | Minting | The treasury has been asked to mint |
| `credited` | Credited | CLT is in the balance |
| `needs_manual` | Needs review | A human has to act; the money is safe |
| `expired` | not shown | A legacy invoice nobody paid — not a deposit. Excluded in SQL, not in the UI: with `LIMIT 20` a client-side filter would still spend the cap on rows the user never sees (stage holds 33 of them) |
| anything else | its raw status | An unrecognised state is shown as-is rather than guessed at |

`Needs review` is the state that matters most. It is what the 1,000 USDT deposit became, and a user
seeing that word with the amount and the transaction beside it knows something specific and true,
instead of watching a balance that never moves.

## 1. The endpoint

`GET /api/v1/deposits` — the caller's own deposits, newest first.

```json
{ "deposits": [ { "id": "…", "status": "credited", "amount_usdt": 50000000,
                  "tron_tx_id": "e5ebca…", "created_at": "2026-09-03T22:09:26Z" } ] }
```

- Scoped by `user_pk = authenticated_pk(...)`, the same string `credit_transfer` writes. A user can
  never see another's deposits, and there is no id in the URL to guess.
- `amount_usdt` reports `received_usdt` when set and `amount_usdt` otherwise — what arrived, not what
  was once asked for.
- Newest first, `LIMIT 20`. No paging: a panel is not a ledger, and twenty rows is more history than
  a top-up page has any reason to show.
- Gated by `permanent_deposit_addresses_enabled` like the create route, and for the same reason: when
  deposits are off, every part of this page is off.
- No new table, no new state, no migration.

## 2. The panel

Under the address, a short list: amount, label, relative time, truncated transaction id. Nothing when
the list is empty — a first-time user sees the address and the instruction, exactly as now.

Refreshed while the panel is open, on the same effect that already fetches the address. A deposit
lands within a poll, so a modest interval is honest; the list is not a live ticker and must not
pretend to be.

The existing copy stays. This adds evidence beneath it, it does not replace the instruction.

## 3. What this deliberately does not do

- **No per-intent polling loop returns.** Task 9 removed that on purpose. This is one list request
  for the signed-in user, not a status poll per deposit.
- **No new status vocabulary.** The UI maps existing statuses; if the backend gains a state later,
  the panel shows the raw word rather than inventing one.
- **No estimated times.** "Usually about two minutes" is a promise the system cannot keep, as the
  forty-one-minute case proved.

## 4. Testing

Following the orchestrator's existing `db_*` suites:

- **Owner scoping, the one that matters** — two users with deposits; each list returns only its own.
  Written to fail if the `WHERE user_pk` clause is dropped.
- Newest first, and the limit holds at twenty-one rows.
- `received_usdt` wins over `amount_usdt` when both are set.
- An empty list is `{"deposits": []}` and 200, never 404.
- The flag being off returns 503 before authentication, like the create route.

## Out of scope

The ride-oriented `TransactionHistory`. Redemptions. Any change to crediting, minting or sweeping.
