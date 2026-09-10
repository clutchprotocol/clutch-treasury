# Reputation — design options

Status: **proposal, not accepted.** Readiness item H2. Nothing implemented.
Date: 2026-09-11
Scope: candidate scopes differ by option — see below. The recommendation touches
`clutch-explorer` and `clutch-hub-api` only, and deliberately **not** `clutch-node`.

Companion to `2026-09-11-dispute-resolution-design.md`. That one concluded its recommendation fixes
the single-ride case and not the serial abuser, which is this document.

## Problem

No driver or rider scoring exists, so nothing distinguishes a first-time counterparty from a
repeatedly bad one. A rider cannot tell whether a driver has completed one ride or a thousand; a
driver cannot tell whether a rider habitually takes rides and declines to release the held fare.

## The constraint that decides everything

**An account is a free keypair.** Any score attached to an address is discardable: an actor with a
bad score generates a new key and starts clean. So reputation is only meaningful if it rests on
something a fresh keypair *cannot* have, and the cost of manufacturing that thing has to exceed the
value of a clean slate.

Every design below is really an answer to that one question. Options that are not answers to it —
star ratings stored per address, a `RideRating` transaction type — are worse than nothing, because
they *look* like protection while a new keypair defeats them for the price of one transaction.

## What is already costly in this system

Two facts worth more here than any new mechanism, because they exist and are already on chain:

1. **Deposit history.** Every account has one permanent TRON address, and CLT enters circulation
   only against a verified USDT deposit. On mainnet that is real money. A fresh keypair has no
   deposit history and cannot fabricate one without paying.
2. **Completed rides need a funded counterparty.** A `RidePay` moves real CLT from someone who had
   to obtain it. A driver's history of having been paid cannot be built without counterparties who
   spent.

Both are public and verifiable by anyone reading the chain.

### The attack that breaks the naive version

Self-dealing. Create two keys, ride with yourself, and both sides accumulate history. The fare
returns to you because you are both parties, so the only cost is the flat transaction fee: five
transactions at 1,000 CLT is **$0.005 per fabricated ride**, or about $5 for a thousand of them.
Far too cheap.

What makes it expensive is counting **distinct counterparties, weighted by whether those
counterparties have their own deposit history.** Fabricating a graph then requires many separately
funded identities, and funding is the cost a keypair cannot dodge. This is the EigenTrust shape, and
it does not need to be implemented as elaborately as that — the first-order version is "how many
distinct funded counterparties have paid this driver".

## Options

### A. Nothing, indefinitely

- **Against:** the serial abuser is free. H1's recommendation makes one bad ride cost the abuser
  nothing beyond that ride, so repetition is the whole exposure.

### B. Ratings as a protocol feature

A `RideRating` transaction type, scores in chain state, enforced by consensus.

- **Against:** the worst option, and it is the one that looks most like "adding reputation". It puts
  a new attack surface in the node, permanently commits a scoring rule to consensus where it cannot
  be changed without a new chain, and still does not answer the constraint — a new keypair has a
  clean score. It also invites griefing: a one-star rating with no cost attached is free damage.

### C. Reputation derived from public chain history, computed off chain

No new transaction type, no consensus change, no new state. The explorer already indexes every
block; it exposes, per address, facts that are already true:

- completed trips as driver, and as rider
- distinct counterparties, and how many of those have deposit history
- cancellations after acceptance, split by which side cancelled
- first-seen block, and whether the account has ever deposited

The Hub API surfaces those facts, and an application decides what to make of them. Two apps may
weight them differently, and neither can corrupt the other's view, because the underlying data is
just the chain.

- **Answers the constraint** through the distinct-funded-counterparty count, which is the expensive
  thing to fake.
- **Adds no attack surface to the node**, which is what H2's own verification asks for.
- **Scope:** `clutch-explorer` (a per-address aggregate query), `clutch-hub-api` (expose it),
  demo app (display it). No `clutch-node` change at all.
- **Against:** it is not a *score*, it is a set of facts, and someone will ask for a single number.
  A single number is exactly what invites gaming, so the resistance to producing one is a feature.
  It also cannot capture anything that never reached the chain — a rude driver who completed the
  ride and got paid looks identical to a good one.

**This is the recommendation.** It is the only option that answers the constraint without putting a
scoring rule into consensus.

### D. A stake or bond per account

Post CLT to be a driver; lose it on adjudicated misbehaviour.

- **Against:** requires adjudication, so it depends on H1 going further than its own recommendation.
  It also locks fully-reserved, backed CLT into a bond, which is a different question about the peg
  and should not be decided as a side effect of a reputation design. Revisit only if C proves
  insufficient.

## Recommendation

**C.** Expose the facts the chain already contains, weight by distinct funded counterparties, and
keep scoring out of consensus. Defer D. Never do B.

## What has to be decided

1. **Which facts are exposed.** Cancellation counts are the sensitive one: a driver who cancels on
   riders is exactly what a rider wants to know, and a raw count punishes a driver who cancelled for
   good reason. Exposing the count and the side is defensible; deriving blame is not.
2. **Whether "has deposited" is public.** It is the Sybil signal that makes the rest work, and it is
   also a fact about someone's finances. It is already publicly derivable from the chain by anyone
   who cares, so exposing it changes convenience rather than privacy — but say that out loud rather
   than assuming it.
3. **Whether the demo app shows a number.** Recommend not. Show the counts.

## Interaction with H1

H1's recommendation, timed auto-release, means a rider who takes a ride and does not pay ends up
paying anyway after the window. That already removes most of the incentive this design is guarding
against, which is why H2 is **Required** rather than a blocker: with H1 shipped, reputation stops
being the primary defence and becomes information a rider or driver can use to decline a
counterparty in advance.

Build H1 first. If it works, C may be all that reputation ever needs to be.
