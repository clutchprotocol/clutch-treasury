# Bitcart — pinned deployment (Tron/USDT-TRC20 only)

This is the deployment runbook for the self-hosted Bitcart instance the payment
orchestrator (`crates/payment-orchestrator/`) talks to for USDT-on-Tron deposit invoices.
Bitcart is a **third-party open-source payment processor** (single maintainer) — we run
our own copy, watch-only, with no ability to spend.

## Version pin

**`0.10.3.0`** — the latest release as of 2026-03-05 (verified today against the project's
releases). Pin the image tag explicitly; do not track `latest`.

Upgrade only after reading the diff between tags. This is a single-maintainer project:
there is no team review backstop upstream, and the orchestrator's `BitcartAdapter`
(`crates/payment-orchestrator/src/adapter.rs`) is written against this exact tag's response
shapes — verified today against the real `api/schemas/invoices.py` (`DisplayInvoice`) and
`api/models.py` at this tag (see the `2026-07-30` field-name fix in this repo's history for
what "written against assumption instead of the real schema" cost: three of five guessed
field names were wrong). A version bump can silently change those shapes again.

## Required configuration

Environment variables passed to the Bitcart backend container:

```
BITCART_CRYPTOS=trx
BITCART_INSTALL=backend
BITCART_REVERSEPROXY=none
```

- **`BITCART_CRYPTOS=trx` only.** Explicitly no `btc`, no `xmr`. Spec §6 scopes this pilot
  to a single stablecoin rail on Tron — do not enable other chains in this deployment, even
  though Bitcart supports them. Fewer running daemons is also less that can go wrong in a
  box that (see below) holds no keys but does hold API access to invoice state.
- **`BITCART_INSTALL=backend`** — we run Bitcart's API/daemon layer only. We do not need or
  want Bitcart's own admin dashboard or store-front web UI in this deployment; the
  orchestrator is the only client of Bitcart's REST API.
- **`BITCART_REVERSEPROXY=none`** — no bundled nginx/Caddy in front of Bitcart. The
  orchestrator reaches Bitcart directly over the docker network (see `docs/deploy.md`); Bitcart
  itself is never exposed publicly.

### Joining `clutch-network`

Bitcart's backend service needs to reach the same docker network the orchestrator is on, so
the orchestrator's `POST {bitcart_url}/invoices` calls and Bitcart's IPN callback to the
orchestrator's webhook both resolve by container name. Compose fragment:

```yaml
services:
  bitcart-backend:
    image: bitcartcc/bitcart:0.10.3.0
    networks: [clutch-network]
    environment:
      BITCART_CRYPTOS: trx
      BITCART_INSTALL: backend
      BITCART_REVERSEPROXY: none
      BITCART_TRX_NODE: ${TRONGRID_URL_FOR_BITCART}   # own Tron RPC — see "Two RPC providers" below
      BITCART_TRX_NODE_API_KEY: ${TRONGRID_API_KEY_FOR_BITCART}

networks:
  clutch-network:
    external: true
```

`clutch-network` here is the same external network `docs/deploy.md` already documents the
orchestrator joining (public zone; nginx routes `/pay/` to it). Bitcart is **not** routed
through nginx and is **not** published to the host — only containers already on
`clutch-network` (i.e. the orchestrator) can reach it.

## Store and wallet setup

1. Create a Bitcart store via its API (one-time setup, done through Bitcart's own CLI/API,
   not this repo's code).
2. Add a **Tron wallet to the store using only the custody address — watch-only, no private
   key ever imported.** This is the property that makes co-hosting Bitcart next to the
   orchestrator acceptable at all: the box that terminates public webhooks and holds a
   restricted API token has **no signing capability**, so a compromise of this container
   cannot move funds. It can at most see invoice state and see the address balance — it
   cannot spend. State this plainly to anyone provisioning the store: do not add a Tron
   private key to this wallet, ever, under any circumstance, including "just for testing."
3. Generate a **restricted API token** scoped to invoice creation/read for this store —
   not an admin credential. The orchestrator's `bitcart_token` config value
   (`APP_BITCART_TOKEN`, env-only per this repo's secrets convention) is this restricted
   token, never a full-admin login.
4. Configure Bitcart's own Tron daemon with a **TronGrid API key** (`BITCART_TRX_NODE_API_KEY`
   above) so its own on-chain watching (used for its `paid`/`confirmed` status transitions)
   works. This is Bitcart's key for Bitcart's own daemon — a separate credential from the
   treasury's `APP_TRONGRID_API_KEY` used by `tron_verifier.rs`'s independent confirmation.

### Two RPC providers, not one (spec §7.1 mitigation)

Point Bitcart's daemon and the treasury's `tron_verifier` at **different** upstream Tron RPC
providers (e.g. TronGrid for the verifier, a different provider such as GetBlock/TronStack
for Bitcart's daemon). The treasury's independent on-chain confirmation is only a genuine
second pair of eyes if a single compromised or lying upstream can't satisfy both checks at
once. This is a pilot-scale mitigation, not a substitute for the "own Tron lite node"
follow-up tracked in the plan.

## Webhook wiring

Bitcart's IPN target for every invoice this orchestrator creates is:

```
http://payment-orchestrator:8091/webhooks/bitcart
```

Same docker network, container-name resolution — **the webhook never needs public
exposure** when Bitcart is co-hosted with the orchestrator on `clutch-network`, exactly as
`docs/deploy.md` describes for the orchestrator's own placement. Do not add an nginx route
for `/webhooks/bitcart`; it is reachable only container-to-container.

Bitcart's IPN is **unsigned and never retried** — the orchestrator's webhook handler treats
the payload as a bare trigger to refetch `GET /invoices/{id}`, never as a trusted amount or
status by itself (see `crates/payment-orchestrator/src/webhook.rs`). Nothing about this
deployment doc changes that; it is called out here only so whoever operates Bitcart
understands why the orchestrator refetches instead of trusting the POST body.

## Warnings — read before any non-test deploy

### `usdt_contract` defaults to the real MAINNET USDT-TRC20 address

The treasury's `usdt_contract` config value (`crates/treasury-service/config/default.toml`)
defaults to `TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t` — **the real mainnet USDT-TRC20 contract
address.** This is correct for a production deployment and wrong for testnet.

**Confirm the intended network before any non-test deploy.** On Shasta (or any other Tron
testnet), `usdt_contract` must be overridden to that testnet's own USDT-TRC20 contract
address — the real mainnet address will never match a testnet transfer, so
`tron_verifier`'s TRC-20 matching will simply never find evidence and every deposit will age
into the stuck-intent sweep. This fails closed (no bad mint), but it also means "nothing
verifies, ever" looks identical to "the deposit hasn't arrived yet" until someone checks the
contract address. Same applies to Bitcart's own `BITCART_TRX_NODE` — point it at the
correct network's RPC to match.

### Redemptions are OFF by default — the payout rail does not exist yet

`crates/payment-orchestrator/config/default.toml` ships `redemptions_enabled = false`, and
both redemption routes 503 while it is off (`crates/payment-orchestrator/src/api.rs`). This
is deliberate and load-bearing, not a placeholder to flip casually:

`crates/treasury-service/src/payout.rs`'s only implementor of `PayoutRail` is `StubRail`,
which **fabricates `payout_ref = "stub:<uuid>"` and sends nothing** — no USDT leaves
custody, no real Tron transaction is built or signed. Spec §7.6 requires a working off-ramp
before real (non-testnet-smoke) deposits are enabled, precisely because a burn is
irreversible: a user allowed to redeem destroys their CLT claim on the reserve and receives
a `redemption_ref` for a payout that cannot actually happen.

**Do not describe the off-ramp as available in any deployment using this doc.** It is not
live. Flipping `redemptions_enabled` to `true` anywhere outside a rail-development sandbox
is a launch-gate violation (see the plan's LAUNCH GATE constraint and cross-repo follow-up
#1 — the real Tron payout rail — which does not exist in this codebase yet).

## What this doc does not cover

- The actual `clutch-deploy` compose file — this doc, like `docs/deploy.md`, is the fragment
  `clutch-deploy` will adopt on review, not a change made in that repo directly.
- Bitcart's own backup/restore or database sizing — out of scope for a pilot-size deployment
  note; consult Bitcart's own docs if this grows past pilot scale.
