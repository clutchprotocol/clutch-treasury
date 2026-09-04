# Funding the payout float from the fee account — design

Status: approved design, not yet implemented
Date: 2026-09-04
Scope: `clutch-treasury` (`tron-signer`), plus an operator workflow in `clutch-deploy`

## Problem

The payout float at `m/44'/195'/0'/2/0` has never been funded — on 2026-09-04 the address did not
exist on chain at all. Redemptions cannot go live until it holds USDT, and the rollout checklist
says to fund it from custody.

Custody is deliberately an external wallet: `provision-treasury-secrets.sh` refuses to invent
`CUSTODY_TRON_ADDRESS` because "it has to be a wallet you already hold", and nothing in this stack
holds its key. That separation is the reason owning the whole stack does not let anyone drain the
treasury, and it is not being weakened here.

Meanwhile the **fee account at `1/0` holds 1,000 USDT**. That account exists to pay TRX energy;
USDT there is misplaced, and — because reserve is custody plus unswept deposits plus float — it is
counted in no reserve bucket at all. Real treasury money, sitting outside the accounting, in an
address the signer already controls.

## Decision

A third signer endpoint, `POST /internal/fund-float`, that moves the fee account's **entire** USDT
balance to the payout float.

**It takes no parameters.** Not a destination, not an amount, not a contract:

| | Source | Destination | Amount | Token |
|---|---|---|---|---|
| `/internal/sweep` | index (caller-chosen) | fixed config | whole balance | fixed config |
| `/internal/payout` | fixed `2/0` | **caller** | **caller** | fixed config |
| `/internal/fund-float` | fixed `1/0` | fixed `2/0` | whole balance | fixed config |

This is the most constrained of the three. Sweep lets a caller choose which address to drain into
custody; payout lets a caller name a destination and an amount. Here a compromised caller can do
exactly one thing: move treasury USDT from one treasury-controlled address to another
treasury-controlled address. **There is no input to redirect, so there is nothing to exfiltrate.**

Both addresses are derived by the signer from its own mnemonic, not read from config. A config
value can be edited; a derivation path cannot be, without the mnemonic.

### Why the whole balance, and not an amount

USDT in the fee account is misplaced by definition — nothing in the system ever intends to put it
there. "Move all of it to where it belongs" is the entire meaning of the operation, and an amount
parameter would add a way to get it wrong in exchange for no capability anyone needs.

It also keeps the endpoint honest about what it is: a correction, not a treasury transfer facility.
If someone later wants to move a specific amount between treasury addresses, that is a different
request and deserves its own argument.

## Outcomes

Mirrors `SweepOutcome`, for the same reason it exists: a caller must be able to tell "nothing was
broadcast" from "something was".

```rust
pub enum FundFloatOutcome {
    Funded { tx_id: String, amount_usdt: i64 },
    NothingToMove,
    FeeAccountDry { fee_address: String, have_sun: i64, need_sun: i64 },
    Refused(String),
}
```

- `NothingToMove` — the fee account holds no USDT. Not an error; re-running must be a no-op.
- `FeeAccountDry` — the fee account cannot pay TRX for its own transfer. Unlike a deposit address,
  it cannot be funded from anywhere: it *is* the funding source. Only an operator resolves this.
- `Refused` — provably nothing broadcast: derivation failed, or a TronGrid read never returned.
  Never used at or after `sign_and_broadcast`, matching `PayoutOutcome::Refused`'s rule exactly.

**Check the USDT balance before the TRX balance**, exactly as `sweep` does. An empty fee account
returns `NothingToMove` without moving a single sun, so calling this repeatedly cannot disperse TRX.

## What this does to the reserve

Funding from custody would be reserve-neutral: custody down, float up, both counted.

**Funding from the fee account is not.** Fee-account USDT is in no bucket; float USDT is counted.
Moving 1,000 USDT this way therefore *raises* the reserve by 1,000 against unchanged liability, and
reconciliation will read over-backed rather than `ok`.

That is the correct answer, not a problem to suppress: the treasury really does hold that USDT, and
counting it is more accurate than leaving it invisible. Over-backing is the safe direction — it
does not halt minting — and `over_backed_drift` escalates only if it persists. Expect it, and do
not "fix" it by excluding the float.

## Operator surface

A `fund-float` workflow in `clutch-deploy`, modelled on `sweep-address.yml`: a typed confirmation,
a mandatory reason recorded in the run log, and no other input — because the endpoint has none.

## Testing

Following the signer's existing suites:

- The whole balance moves, and the destination is the derived `2/0` address — asserted against the
  address the signer derives, not a string in the test, so a derivation change cannot pass silently.
- An empty fee account returns `NothingToMove` and broadcasts nothing.
- A fee account below the TRX threshold returns `FeeAccountDry` and broadcasts nothing.
- The USDT balance is read before the TRX balance, so an empty account never triggers funding.
- The wire form of each outcome, like `payout_response`'s literals.

## Out of scope

Moving USDT out of custody, which nothing in this stack can do and which this change deliberately
does not add. Funding the float from custody stays a human action.
