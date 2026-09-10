# Dispute resolution — design options

Status: **proposal, not accepted.** Written to make readiness item H1 a decision instead of an open
question. Nothing here is implemented.
Date: 2026-09-11
Scope: `clutch-node` (transaction types), `clutch-hub-api`, `clutch-hub-sdk-js`,
`clutch-hub-demo-app`. No treasury involvement — this is fare settlement, not the peg.

## Problem

Readiness item H1 blocks a public launch: there is no arbitration when a rider and driver disagree,
and no no-show or fraud handling. Riders also give up card-issuer chargebacks by signing payment
directly.

The item has sat open because it reads as an implementation task and is not one. Three questions
have no answers, and picking a mechanism before answering them produces a mechanism that makes
things worse.

## What the protocol already does, which the framing has been missing

This matters because it changes what has to be built.

`RideAcceptance` **debits the full fare from the rider immediately** and holds it against the trip.
`RidePay` then releases it to the driver in installments — one or many, rider-initiated. The trip
completes when `farePaid` equals `fare`. Either party may `RideCancel` before that, and **the unpaid
remainder returns to the rider.**

So there is already an escrow, already partial settlement, and already a remedy:

| Situation | What exists today |
|---|---|
| Ride goes wrong mid-trip | The rider stops sending `RidePay` and cancels. They lose only what they already released. |
| Ride never starts | Same, and nothing has been released. |
| Rider completes the ride and refuses to pay | **Nothing.** The driver cancels and is refunded... to the rider. The driver has performed and is unpaid. |
| Either party disputes after full payment | **Nothing.** `RidePay` is final. |

**The gap is asymmetric in the opposite direction to the one the item describes.** A rider's
protection is partial payment, which already works and was simply never documented as recourse —
that disclosure landed on 2026-09-11. The party with no protection at all is **the driver**, against
a rider who takes the ride and declines to release the held fare.

Any design that only adds rider protection makes the actual imbalance worse.

## The three questions, and what constrains the answers

**1. Who decides?**

- An operator role is a trusted third party, which is the thing this architecture removed. It also
  makes the operator liable for outcomes in a way that changes the legal picture (see
  `docs/LEGAL-BRIEF.md`).
- A staked juror set is a second protocol: staking, selection, incentives, appeals, and a token to
  stake. CLT is fully reserved and non-speculative, so staking it means locking backed value in a
  bond — not obviously acceptable.
- **No one deciding** is a real option, and given the two above, it should be the default assumption
  rather than the fallback.

**2. What can a decision do?**

A burn is irreversible and `RidePay` is final, so a remedy cannot undo. It must be either a new
transaction, or a change to *when* settlement happens. That rules out anything shaped like a refund
after the fact and points at the release timing.

**3. What stops the mechanism being the attack?**

A dispute that freezes a driver's payout, and costs the rider nothing to file, is a denial of
service against drivers. Any mechanism has to make disputing cost something.

## Options

### A. Do nothing more, and say so louder

Rider protection is partial payment; driver protection is nothing. Document both plainly, cap fares,
and launch without arbitration.

- **Cost:** none.
- **Against:** drivers carry the whole risk of a rider who does not pay. At scale that is a driver
  supply problem, not a support problem. It also reads badly against the project's own claim to be
  driver-first.

### B. Timed auto-release of the held fare

The held remainder releases to the driver automatically after a window (say N blocks after
acceptance), unless the rider has cancelled. Riders keep partial payment as their lever *during* the
window; drivers stop being exposed to silent non-payment.

- **Needs:** one new node-side rule and no new transaction type. `RideAcceptance` gains an expiry;
  a settlement pass releases the remainder when it passes.
- **Answers Q1** with "nobody", **Q2** with "settlement timing", **Q3** with "the rider must act to
  dispute, and inaction favours the driver rather than the rider".
- **Against:** a rider who is genuinely wronged and does nothing loses. The window length is a
  policy choice that will be wrong for some rides. It also weakens "paid in seconds" into "paid in
  seconds if the rider pays, otherwise in N blocks" — which is honest, and still far better than a
  weekly payout cycle.
- **This is the recommendation.** It is the only option that fixes the real gap, needs no arbiter,
  and adds one rule rather than a subsystem.

### C. Rider-funded dispute bond

A rider disputing must post a bond, refunded if the dispute succeeds. Requires someone to decide
success, so it does not stand alone — it is a modifier on D.

### D. Operator arbitration, explicitly scoped

An operator role that can, within a window, split a held fare. Bounded by: only the *unreleased*
remainder, only within the window, only a split between the two parties, never to a third address,
and every decision on chain.

- **Answers Q3** if combined with C.
- **Against:** reintroduces the trusted third party, and the legal consequences of adjudicating
  consumer disputes are a question for counsel, not for this document. Worth quoting only if B
  proves insufficient in practice.

## Recommendation

**Ship B. Document A's honesty in the meantime. Defer C and D until there is evidence B is not
enough.**

B is one rule in the node, no new transaction type, no arbiter, no staking, and it closes the gap
that actually exists rather than the one the readiness item described. C and D are a subsystem and a
liability question respectively, and neither should be built on speculation about a market that has
not run yet.

## What has to be decided before this can be built

1. **The window length.** Blocks, not wall-clock, since the node reasons in slots. It has to be long
   enough for a rider to notice a problem and short enough that a driver is not financing the rider.
2. **Whether a cancel during the window is free.** If it is, a rider can always cancel to avoid
   auto-release, and B collapses back into A. Probably it should cost the rider the fee, or a
   fraction of the fare, but this is a product decision with fairness consequences either way.
3. **Whether the window is per-ride or global config.** Genesis-committed consensus values cannot
   change without a new chain, so if this is a consensus parameter it must be right the first time.
   That argues for putting it in `ChainInit` only if we are confident, and otherwise per-acceptance.
4. **What "the ride is finished" means.** There is no completion signal today other than payment, so
   the window starts at acceptance rather than at drop-off. A long ride and a short one get the same
   window, which may be acceptable and may not.

Question 3 is the one with a deadline attached: it interacts with readiness item C1, because if the
window is a consensus parameter it belongs in the mainnet genesis and cannot be added later without a
new chain.

## What this does not address

No-show handling, fraud detection and reputation (H2) are separate. B makes non-payment expensive to
repeat only if identity persists, and identity is a free keypair — so B without H2 fixes the
single-ride case and not the serial abuser. That is a reason to build H2, not a reason to hold B.
