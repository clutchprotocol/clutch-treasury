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
