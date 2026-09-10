# Audit brief

Readiness item **I1**. What a security auditor needs to know before quoting, and what to point them
at. Handing this over instead of a repository URL is the difference between paying a firm to discover
the architecture and paying them to attack it.

Written to be given to an external reviewer as-is.

## What the system is

An open-source ride-sharing blockchain with a fully-reserved token. Two halves that meet at a peg:

**The chain.** A custom non-EVM Rust node, Aura consensus (authority round-robin, permissioned set).
Ride operations are first-class transaction types rather than smart-contract calls: `RideRequest`,
`RideOffer`, `RideAcceptance`, `RidePay`, `RideCancel`, `RideRequestCancel`, plus `Mint`, `Burn` and a
genesis-only `ChainInit`. Clients sign locally; the GraphQL Hub API forwards signed RLP and holds no
keys.

**The treasury.** CLT is a micro-dollar (1 USD = 1,000,000 CLT), backed 1:1 by USDT on TRON. Three
services, split so none can both decide and move funds:

- `payment-orchestrator` — derives one permanent TRON deposit address per user from an account
  **xpub** (cannot spend), polls those addresses, and serves the two public REST routes.
- `treasury-service` — verifies deposits on chain, runs a four-eyes mint ledger, reconciles reserve
  against liability, and drives payout workers.
- `tron-signer` — holds the deposit mnemonic. The only component that writes to TRON.

## The invariants worth attacking

These are the properties the design claims. Breaking any one is a finding; the first three are the
ones that cost money.

1. **Every CLT in existence is backed by USDT in reserve.** Supply changes only via authority-gated
   `Mint` and permissionless `Burn`. Genesis pre-mints nothing (`faucet_allocation = 0`), so every
   CLT was minted against a verified deposit. Reconciliation halts minting when reserve stops
   covering liability.
2. **A burn can never be paid twice.** A TRC-20 transfer has no memo to deduplicate against, so the
   payout path distinguishes *proved not broadcast* (retryable) from *unknown* (never retried, pages
   a human). Attack the boundary between those two classifications.
3. **Nothing in the stack can move a deposit to an attacker's address.** `tron-signer`'s sweep
   endpoint takes an address *index* and nothing else — no destination, amount or contract. The
   payout endpoint is the deliberate exception and can only spend the float at `m/44'/195'/0'/2/0`,
   never custody, never a deposit address.
4. **Nothing can spend reserve custody.** There is no code path to it at all; top-ups are manual.
5. **A user's private key never leaves their client** for ride transactions, and the Hub cannot alter
   a signed transaction without invalidating it. The SDK's `verifyUnsignedTransaction` lets a client
   check what the Hub built before signing it.
6. **A signed transaction is bound to one chain.** `chain_id` is in the signed payload and in the
   auth challenge, so a signature captured on testnet cannot authenticate on another chain.
7. **The fee split is exact.** Referrer basis points plus the driver's remainder sum to the fare for
   every fare, with floor rounding — property-tested, so look for the case the property misses rather
   than for an arithmetic slip.

## Where to look first

Ordered by what a finding would cost, not by lines of code.

| Area | Files | Why |
|---|---|---|
| Signing and encoding | `clutch-node/src/node/rlp_encoding.rs`, `signature_keys.rs`, `clutch-hub-sdk-js/src/sdk.ts` | An encoding disagreement between signer and verifier is a forged or malleable transaction. The convention is unusual: the digest is Keccak-256 of the hash **hex string's** UTF-8 bytes, not of the bytes. |
| Four-eyes mint | `clutch-treasury/crates/treasury-service/src/api.rs`, `intents.rs`, `breakers.rs` | Two role tokens, a DB `CHECK` enforcing distinct actors, per-transaction and daily caps, a latching breaker. Look for a path that mints with one role, or that bypasses the caps. |
| Payout bounds | `crates/treasury-service/src/payout.rs`, `crates/tron-signer/src/sweep.rs` | Invariants 2 and 3. The `Refused` / `Ambiguous` split is the whole safety property. |
| Reconciliation arithmetic | `crates/treasury-service/src/reconciliation.rs` | It decides whether minting continues. An over- or under-count of unswept deposit addresses changes the answer. |
| Deposit attribution | `crates/payment-orchestrator/src/custody.rs`, `poller.rs` | Identity is *address plus transaction id*, with no expected amount to match. Look for a way to be credited for someone else's transfer. |
| Auth | `clutch-hub-api/src/hub/auth.rs`, `crates/payment-orchestrator/src/auth.rs` | Signed-challenge JWT with a ±120s window; the orchestrator trusts the JWT `pk` as the beneficiary. |
| Consensus | `clutch-node/src/node/aura.rs`, `blockchain.rs` | Slot-to-author binding, and the block-timestamp check that stops an authority authoring out of turn. |

## Known gaps — do not spend time rediscovering these

All tracked in `mainnet-readiness.md`, and all open as of 2026-09-11. Findings that restate them are
not useful; findings that show one is *worse than recorded* are.

- **Keys are environment variables**, not KMS. Mint authority and deposit mnemonic both. The
  signature plumbing for a KMS signer exists and is tested; the API call does not (A1, A2).
- **No dispute resolution, no reputation, no fraud handling.** Passengers give up chargebacks by
  signing payment directly (H1, H2).
- **The reference demo app stores private keys in plaintext `localStorage`** (F1).
- **Everything runs on one VPS**, including three validators, both databases and custody-adjacent
  services (G2).
- **The edge nginx belongs to another compose project** and its config is not owned by any repo (G1).
- **Rate limits exist but are unmeasured**; no load test (E1).
- **No off-host backup has been restored yet**; the tooling exists (D1).

## Testing posture

Roughly 300 tests in the treasury, ~100 in the node, 26 in the Hub API, 6 in the SDK. The money
paths are covered at the failure branches, not just the happy path: every arm of the payout
`Refused`/`Ambiguous` split, the over-cap intent routing to manual review, the reverted-payout case,
and the daily-cap boundaries. The node's fee split and exactly-once refs are property-tested.

`docker-compose.test.yml` runs the treasury suite against a real Postgres. CI runs it on every push.

## Practical notes

- **The public testnet is live** at `app-stage.clutchprotocol.io` and settles on TRON's **Nile**
  testnet, whose USDT has no value. It is a legitimate target — please use it rather than mainnet,
  because there is no mainnet.
- Getting test funds needs no wallet: the Nile faucet pays any address, including a deposit address
  the app shows you.
- Everything is open source under the `clutchprotocol` organisation. Apache-2.0 for the node, MIT for
  the JS.
- One maintainer, so please send findings as they are confirmed rather than batched at the end.
- `clutch-treasury/docs/superpowers/specs/` holds the design specs for the deposit and redemption
  rails, which are the two flows where money moves.

## What a useful report looks like here

The reserve model is the product. A finding that lets someone mint unbacked CLT, be paid twice for
one burn, or redirect a deposit outranks everything else, however unlikely the path. Below that:
anything that halts the treasury without recourse, or that makes reconciliation report a state that
is not true.

Please state, for each finding, which of the seven invariants above it breaks. If it breaks none of
them but is still a problem, say that too — it means one of the invariants is not the right one.
