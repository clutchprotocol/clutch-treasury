# Redeeming CLT for USDT in the demo app — design

Status: approved design, not yet implemented
Date: 2026-09-04
Scope: `clutch-hub-demo-app` (a Withdraw panel beside Top up)

## Problem

The redemption rail is real — `POST /api/v1/redemptions` exists, the treasury pays out from a
derived float through `tron-signer`, and the SDK can build a `Burn`. Nothing in the demo app calls
any of it. There is no way for a user to turn CLT back into USDT, so from where they sit the token
only goes one way.

Flipping `redemptions_enabled` on its own changes nothing anyone can see. This is the missing half.

## The one thing that makes this different from Top up

**A burn is irreversible and it happens before the payout.** A deposit that goes wrong leaves the
user's money sitting at an address someone can sweep later. A redemption that goes wrong has
already destroyed the user's CLT.

Every decision below follows from that. The panel's job is not to make redeeming fast; it is to
make sure nobody burns by accident, and that anyone who has burned can always see that their money
is accounted for.

## The sequence

Five steps, in this order, and the order is not negotiable:

1. `POST /api/v1/redemptions` with `{payout_tron_address, amount_clt}` and the user's JWT.
   Returns `201 {id, redemption_ref, amount_clt, status}`.
2. `sdk.createUnsignedBurn({ amount, redemptionRef })` using the `redemption_ref` from step 1.
3. `sdk.signTransaction(unsignedTx, privateKey)` — local, as every other transaction in this app.
4. `sdk.submitTransaction(signature.rawTransaction)`.
5. Poll `GET /api/v1/redemptions/:id` until it settles.

**The redemption must be created before the burn.** The burn carries the ref, and a burn without
one is CLT destroyed with nothing on the treasury side pointing at it. Never reorder these, and
never build the burn from a ref the server did not return.

## What the user sees

| Status | Panel says | What it means |
|---|---|---|
| `created` | Awaiting your burn | The redemption exists; the CLT has not been destroyed yet. Nothing is at risk. |
| `burn_confirmed` | Burn confirmed | The chain has the burn. The treasury owes a payout. |
| `payout_pending` | Sending USDT | Queued for the payout worker. |
| `payout_submitted` | Sending USDT | Broadcast, awaiting confirmation. Deliberately the same words as `payout_pending`: the distinction matters to an operator, not to someone waiting for money. |
| `paid` | Paid | USDT delivered. Show the payout transaction. |
| `expired` / `failed` | Needs review | A person is handling it. Say the funds are accounted for, because they are — see below. |
| anything else | the raw status | An unrecognised state is shown as-is rather than guessed at. |

**"Needs review" must never read like "lost".** A redemption that ends `failed` after a burn means
the CLT is gone and the treasury owes USDT that a human will settle. The panel says the amount, the
reference, and that support can complete it — not "failed", full stop.

## Failure modes the panel has to survive

- **Created, then the user closes the tab before burning.** Recoverable, and common. The redemption
  sits at `created` and the CLT is untouched. On reopening, list it and offer to complete the burn
  with the same ref. Do NOT create a second redemption for the same intent.
- **Burn submitted, response lost.** The user must not be invited to burn again — a second burn is
  a second destruction of CLT. Once submit has been attempted for a redemption, the panel stops
  offering the burn for it and shows status instead.
- **Payout ambiguous** (`payout_submitted` with no resolution). By design the treasury never
  auto-retries this; a person resolves it. The panel keeps showing "Sending USDT" rather than
  inventing a failure.
- **The feature is off.** Both routes answer `503` while `redemptions_enabled` is false. The panel
  shows the same "not available yet" treatment the deposit panel uses for its own gate, and offers
  nothing to click.

## Confirmation

One confirmation step before the burn, stating the exact amount of CLT to be destroyed, the exact
Tron address, and that it cannot be undone. The address is echoed back **in full** — the same
reasoning as the deposit address: an address you cannot read in full is one you cannot check.

No quick-amount buttons. Top up has them because sending more USDT is harmless; burning more CLT is
not.

## Bounds

`min_redemption_clt` and `max_redemption_clt` are server config (1 and 50 CLT today). The panel does
not hardcode them: it surfaces the server's own `amount_clt must be between {min} and {max}`
message. A limit duplicated in the client is a limit that will drift.

## What this deliberately does not do

- **No new SDK method.** `createUnsignedBurn` already takes `redemptionRef`.
- **No address book, no saved payout addresses.** A wrong address here sends real money to a
  stranger; making it easy to reuse a possibly-mistyped one is not a convenience.
- **No estimate of how long a payout takes.** The same reason the deposit panel gives none.
- **No cancel button.** After a burn there is nothing to cancel; before one, closing the panel is
  already the cancel.

## Testing

No test framework exists in this repo, so verification is by reading, as with the deposit panel:
hooks unconditional, dependencies complete, no unused identifiers, JSX balanced, every failure
branch reachable. Specifically confirm by inspection that no code path can call
`createUnsignedBurn` before a `redemption_ref` exists, and that no path offers the burn twice for
one redemption.

## Out of scope

The rollout itself — funding the float, verifying reconciliation, the end-to-end test, flipping the
flag. Those are the payout-rail design's section 5 and stay there.
