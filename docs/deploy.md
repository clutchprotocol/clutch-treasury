# Deploy — clutch-deploy compose fragment

This is the fragment `clutch-deploy` will adopt to run `clutch-treasury` alongside the
rest of the stack. It is documented here only; editing `clutch-deploy` itself is a
separate cross-repo change the user reviews on its own branch (see `.superpowers/sdd`
follow-ups).

## Network posture

- **Private zone, no published ports.** This service holds the mint authority secret
  and moves real reserve-backed liability — it must not be reachable from the host or
  the public internet, only from other containers on the compose network. No `ports:`
  section is used anywhere below (contrast with `node1`, `clutch-hub-api`, etc., which
  are intentionally published).
- `treasury-network` is `internal: true` — containers on it cannot reach the outside
  world (or be reached from it) except through another network a container is also
  attached to.
- `clutch-treasury` is attached to **both** `treasury-network` (to reach its own
  Postgres) and `clutch-network` (to reach `node1`'s WebSocket RPC at
  `ws://node1:8081/ws`). This mirrors the existing `explorer-postgres` precedent: a
  private-zone Postgres plus one service that also needs the shared network.
- Secrets (`APP_MINT_AUTHORITY_SECRET`, `APP_INITIATOR_TOKEN`, `APP_APPROVER_TOKEN`,
  `APP_READONLY_TOKEN`, `TREASURY_POSTGRES_PASSWORD`) are sourced from the environment
  (`.env` in `clutch-deploy`, following the same convention as its other services) —
  never hardcoded in the compose file.

## Fragment

```yaml
# Private zone: internal network, NO published ports (spec §3). Attach node1 to
# treasury-network too, or route via the shared clutch-network (pilot: shared network,
# no ports published, matching the explorer-postgres precedent).
services:
  treasury-postgres:
    image: postgres:16-alpine
    networks: [treasury-network]
    environment:
      POSTGRES_DB: treasury
      POSTGRES_PASSWORD: ${TREASURY_POSTGRES_PASSWORD}
    volumes: [treasury-postgres-data:/var/lib/postgresql/data]

  clutch-treasury:
    build: ../clutch-treasury
    networks: [treasury-network, clutch-network]   # clutch-network only to reach node1
    environment:
      APP_DATABASE_URL: postgres://postgres:${TREASURY_POSTGRES_PASSWORD}@treasury-postgres:5432/treasury
      APP_NODE_WS_URL: ws://node1:8081/ws
      APP_MINT_AUTHORITY_SECRET: ${TREASURY_MINT_SECRET}
      APP_INITIATOR_TOKEN: ${TREASURY_INITIATOR_TOKEN}
      APP_APPROVER_TOKEN: ${TREASURY_APPROVER_TOKEN}
      APP_READONLY_TOKEN: ${TREASURY_READONLY_TOKEN}
    depends_on: [treasury-postgres]

networks:
  treasury-network:
    internal: true

volumes:
  treasury-postgres-data:
```

## Notes for the clutch-deploy follow-up

- `clutch-network` here refers to the existing external network the rest of the stack
  (node1-3, hub-api, explorer, etc.) already shares — `clutch-treasury` joins it
  read-only-in-effect (it only ever calls `get_chain_info`, `get_next_nonce`,
  `get_block_by_index`, `send_raw_transaction` against `node1`; it never publishes a
  port onto it).
- `AppConfig::load` (`crates/treasury-service/src/configuration.rs`) fails loudly if any
  of the four secret env vars are empty — there is no half-configured boot state to
  worry about at deploy time.
- The container listens on `0.0.0.0:8090` inside the network (see `Dockerfile`
  `EXPOSE 8090` and `config/default.toml` `http_addr`) but that port is never mapped to
  the host in this fragment — only other containers on `treasury-network` /
  `clutch-network` can reach it.
- Not covered by this fragment (tracked as separate follow-ups, see
  `.superpowers/sdd/task-8-brief.md` "Cross-repo follow-ups"): a `/metrics` endpoint +
  Prometheus scrape + Grafana dashboard, and the KMS signer swap before mainnet
  (`docs/keys.md`).

## payment-orchestrator (Plan C)

The orchestrator is a **public-zone** service — it terminates Bitcart's webhook and the
app's deposit/redemption requests, and holds no chain keys and no treasury approver token
(only an initiator token to propose mints and a readonly token to check status). Its
network posture is the opposite of `clutch-treasury` above in one respect and the same in
another:

- **It DOES get a published route**, unlike the treasury: nginx routes the public `/pay/`
  prefix to it (see the clutch-deploy follow-up note below — the actual nginx route change
  lives in that repo, not here).
- **It joins BOTH networks**: `clutch-network` (public zone, to be reachable via nginx and
  to reach `node1` if ever needed) AND `treasury-network` (to reach the treasury's internal
  API at `http://clutch-treasury:8090` — the same address the treasury listens on inside
  the compose network, never published to the host). The treasury itself gains no new
  exposure from this — it stays exactly as unpublished as the section above describes;
  the orchestrator is simply another container already inside `treasury-network` that can
  reach it, the same way `clutch-treasury` itself reaches `treasury-postgres`.

### Fragment

```yaml
services:
  clutch-orchestrator:
    build:
      context: ../clutch-treasury
      dockerfile: Dockerfile.orchestrator
    networks: [clutch-network, treasury-network]
    environment:
      # Deployed port is 8091 (Dockerfile.orchestrator EXPOSEs 8091); the checked-in
      # config/default.toml binds 8095 for local dev alongside a treasury instance on the
      # same host. Override here rather than editing the checked-in default.
      APP_HTTP_ADDR: "0.0.0.0:8091"
      APP_DATABASE_URL: postgres://postgres:${ORCHESTRATOR_POSTGRES_PASSWORD}@orchestrator-postgres:5432/orchestrator
      APP_JWT_SECRET: ${APP_JWT_SECRET}                       # shared HS256 secret with clutch-hub-api
      APP_BITCART_URL: http://bitcart-backend:8000
      APP_BITCART_TOKEN: ${BITCART_TOKEN}
      APP_BITCART_STORE_ID: ${BITCART_STORE_ID}
      APP_PUBLIC_BASE_URL: https://<public-host>/pay        # used to build Bitcart's notification_url
      APP_TREASURY_URL: http://clutch-treasury:8090
      APP_TREASURY_INITIATOR_TOKEN: ${TREASURY_INITIATOR_TOKEN}
      APP_TREASURY_READONLY_TOKEN: ${TREASURY_READONLY_TOKEN}
      APP_CUSTODY_TRON_ADDRESS: ${CUSTODY_TRON_ADDRESS}
    depends_on: [orchestrator-postgres]

  orchestrator-postgres:
    image: postgres:16-alpine
    networks: [treasury-network]
    environment:
      POSTGRES_DB: orchestrator
      POSTGRES_PASSWORD: ${ORCHESTRATOR_POSTGRES_PASSWORD}
    volumes: [orchestrator-postgres-data:/var/lib/postgresql/data]

volumes:
  orchestrator-postgres-data:
```

`orchestrator-postgres` sits on `treasury-network` only (private zone), same reasoning as
`treasury-postgres` above — its own database never needs to be reachable from the public
network, only from the orchestrator container that is itself attached to both.

### Bitcart placement

Bitcart's backend (`docs/bitcart.md`) is co-hosted on `clutch-network` alongside the
orchestrator. Bitcart's IPN webhook URL is:

```
http://payment-orchestrator:8091/webhooks/bitcart
```

Same docker network, container-name resolution — **the IPN never needs public exposure**
when Bitcart is co-hosted this way. Do not add an nginx route for `/webhooks/bitcart`; only
`/pay/` (the orchestrator's own JWT-authenticated API) is meant to be public.

### Notes for the clutch-deploy follow-up

- The nginx `/pay/` route itself, and confirming `/webhooks/bitcart` is not accidentally
  swept into a public route by a broad proxy rule, are tracked as cross-repo follow-up #3
  in the plan — not done in this fragment.
- `OrchConfig::load` (`crates/payment-orchestrator/src/configuration.rs`) fails loudly if
  `APP_JWT_SECRET`, `APP_BITCART_TOKEN`, `APP_TREASURY_INITIATOR_TOKEN`, or
  `APP_TREASURY_READONLY_TOKEN` are empty — same no-half-configured-boot guarantee as the
  treasury service.
- `redemptions_enabled` defaults `false` in `config/default.toml` and both redemption
  routes 503 while it is off — see `docs/bitcart.md`'s warning section for why. Do not set
  an `APP_REDEMPTIONS_ENABLED=true` override in any deployment following this fragment; the
  treasury's payout rail (`payout::StubRail`) sends nothing.

## Manual smoke test (testnet only — Shasta USDT)

This is a **testnet-smoke-only** check per the plan's LAUNCH GATE constraint: deposits are
not open to real users until the real Tron payout rail exists (see `docs/bitcart.md`'s
redemptions warning). Run this against a deployment pointed at Shasta, with
`usdt_contract` overridden to Shasta's own USDT-TRC20 contract address (see
`docs/bitcart.md` — the default is mainnet's).

1. **Create a deposit intent** with a hub-issued JWT (`{pk, exp}`, HS256, the same
   `APP_JWT_SECRET` the orchestrator validates against):

   ```bash
   curl -X POST https://<public-host>/pay/api/v1/deposits \
     -H "Authorization: Bearer <JWT>" \
     -H "Idempotency-Key: smoke-test-001" \
     -H "Content-Type: application/json" \
     -d '{"clt_address": "<your-testnet-clt-address>", "amount_usdt": 5000000}'
   ```

   Expect `201` with `{id, pay_address, pay_amount_usdt, expires_at, status}`.
   `pay_amount_usdt` is `amount_usdt` (5,000,000 = $5.00) **plus the discriminator** — pay
   exactly this amount, not the round number you asked for; the discriminator is what lets
   Bitcart's single watch-only address tell concurrent deposits apart.

2. **Pay from a Shasta testnet wallet**: send exactly `pay_amount_usdt` of testnet USDT-TRC20
   to `pay_address` (the custody Tron address). Use a Shasta faucet + testnet USDT contract,
   not mainnet funds.

3. **Watch the webhook/poller do its job**:

   ```bash
   curl https://<public-host>/pay/api/v1/deposits/<id> -H "Authorization: Bearer <JWT>"
   ```

   Expect `status` to progress `invoiced → paying → confirmed → mint_requested → credited`
   over the next couple of poll intervals (`poll_interval_secs`, default 30s, on both the
   Bitcart poller and the treasury bridge). The webhook (if Bitcart's IPN reaches the
   orchestrator) speeds this up; the poller reaches the same states on its own regardless
   — that is deliberate (`crates/payment-orchestrator/src/poller.rs`), since Bitcart's IPN
   is unsigned and never retried.

4. **Confirm the mint credited on the node.** The treasury's `tron_verifier` independently
   re-confirms the deposit against TronGrid before auto-approving — this is the spec §7.1
   "second pair of eyes," not the webhook or poller themselves minting anything. Check the
   node directly (via the hub's GraphQL or the explorer) for a `Mint` transaction crediting
   `clt_address` for the intended `amount_usdt` (par: 5,000,000 CLT for a $5 deposit).

5. **Confirm `reserve-status` shows liability AND custody both up by the amount**:

   ```bash
   curl http://clutch-treasury:8090/internal/reserve-status \
     -H "Authorization: Bearer <TREASURY_READONLY_TOKEN>"
   ```

   (from inside the compose network, or via whatever internal access path the deployment
   uses — this route is never published to the host). Expect `balances.clt_liability` up by
   5,000,000 and `balances.custody_usdt` up by the **observed on-chain amount**
   (`pay_amount_usdt`, including the discriminator — `tron_verifier` ledgers what actually
   arrived on-chain, not the rounder `amount_clt`, so custody sits fractionally above the
   liability by design). Both numbers moving together, by the same deposit, is the actual
   proof this smoke test exists to produce — a mint with no matching custody increase (or
   vice versa) is exactly the failure mode reconciliation and the backing-ratio breaker
   exist to catch.
