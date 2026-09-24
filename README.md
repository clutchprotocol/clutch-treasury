# clutch-treasury

The money side of Clutch Protocol: USDT in, CLT out, and the accounting that has to hold between
them.

**CLT is fully reserved.** 1 USD = 1,000,000 CLT, integer only, and every CLT in existence is meant
to be matched by USDT held in custody. That claim is worth nothing unless it is checked, so this
repo's real product is not the mint — it is the reconciliation that compares what the chain says
exists against what the ledger says is owed and what custody actually holds, on an interval, with a
mismatch treated as a P1 that halts minting rather than as a dashboard metric.

## Crates

| Crate | Does | Holds |
|-------|------|-------|
| `treasury-service` | The ledger, four-eyes minting, reconciliation, sweeping, redemption payouts | the mint authority key |
| `payment-orchestrator` | The browser-facing deposit and redemption API; derives and polls deposit addresses | the account **xpub** only — it cannot spend |
| `tron-signer` | Signs TRON transactions: sweeps and payouts | the deposit wallet **mnemonic** |
| `clutch-chain` | Shared chain client: builds, signs and submits CLT transactions | — |
| `gasfree` | GasFree address derivation and permit hashing, one copy shared by the orchestrator and the signer | — |

The split is the point. Owning `payment-orchestrator` gets you an xpub, which derives addresses and
cannot move a coin. The mnemonic exists only in `tron-signer`, which publishes no port.

## How a deposit becomes CLT

1. A user opens the deposit panel. `payment-orchestrator` derives **one permanent address** for them
   at `m/44'/195'/0'/0/i` from the account xpub and stores it against their `user_pk`.
2. It polls that address for USDT paid **to** it — the user's own address is polled first while the
   panel is open, everyone else on a bounded rotation, oldest first, so cost stays flat as the
   address set grows.
3. `treasury-service` verifies the transfer on chain, records it against its `tron_tx_id`, and mints
   the matching CLT through the four-eyes ledger.
4. Its sweeper later moves the USDT from the derived address into custody. A freshly derived address
   holds no TRX and cannot pay for its own sweep, so `tron-signer` funds it first from the fee
   account at `<account>/1/0` — a different change level from deposit addresses, so nothing there
   can collide with an address a depositor was told to pay into.

Any amount is credited in full, and each on-chain transfer is its own row, so repeated top-ups to
the same address all count rather than just the first.

**`POST /api/v1/deposits` takes no body.** The CLT beneficiary is always the caller's authenticated
identity. There is deliberately no `clt_address` field; it was removed as a foot-gun, not forgotten.

## Redemption

Live since 2026-09-04. A redemption burns CLT and pays USDT back, and **the burn is irreversible and
happens before the payout**, which drives every decision in that path: the ordering is fixed, a burn
carrying no reference is CLT destroyed with nothing pointing at it, and a second burn against one
reference is just as bad — so the attempt is written to storage before the broadcast, never only to
memory.

A single redemption is bounded **twice, in services that do not share the value**:
`APP_MAX_REDEMPTION_CLT` refuses the request before any burn, and `tron-signer`'s own per-transaction
cap refuses the payout. Both are \$25 today and were deliberately aligned, so a request the signer
would reject can never become a burn nobody can pay.

## The two API rules that matter

**`tron-signer`'s sweep endpoint takes an index and nothing else.** The destination is its own
config. Do not add a `to`, `contract` or `amount` parameter: each one individually deletes the reason
that endpoint exists, which is that owning the orchestrator must never move a deposit.

**`/internal/payout` is the deliberate exception** and does take `to` and `amount`, because a
redemption has no other way to express them. Its bound is different, not absent: it can only spend
from the payout float at `2/0` — never a deposit address, never custody — so the float balance caps
the loss and a per-transaction cap bounds one request. `contract` is still never a parameter.

## Endpoints worth knowing

Nothing here publishes a host port; `payment-orchestrator` is reached through nginx's `/payment/`
route and the rest only from inside the compose network.

| Endpoint | Auth | For |
|----------|------|-----|
| `GET /public/reconciliation` | none | The reserve position: supply, liability, custody, status, and the time of the run. Republished publicly by the explorer |
| `GET /internal/reserve-status` | any role token | The above **plus** breaker state, daily mint headroom and outbox depth — operational internals, which is why it is not the public one |
| `POST /internal/mint-intents` + `/approve` | two different tokens | The four-eyes mint. Two separate calls so one actor cannot be both roles |
| `POST /internal/halt` / `/resume` | role token | Stop and restart minting. Halting is ungated and resuming is not — halting is cheap and reversible, being slow to halt is not |

## Tests

```bash
docker compose -f docker-compose.test.yml run --rm test cargo test --workspace -- --test-threads=1
```

Database-backed tests share tables, so each test binary uses its own database and serialisation
matters — `--test-threads=1` only serialises *within* a binary, which is why each `pool()` helper
suffixes its own name.

## Documents

- [`docs/mainnet-readiness.md`](docs/mainnet-readiness.md) — the canonical gap list between this
  stack and one that could hold real money, with a verification step for each item. A
  [public version](https://docs.clutchprotocol.io/reference/mainnet-readiness) is published with six
  items generalised, and which six and why is recorded at the top of the canonical file.
- [`docs/KEY-CEREMONY.md`](docs/KEY-CEREMONY.md) — generating and recovering the mint authority.
- [`docs/keys.md`](docs/keys.md) — what key exists where, and what each one can do.

## Status

**Not ready for real funds**, and the readiness document says exactly why rather than implying
otherwise. The gate is five conditions — keys behind a hardware or KMS boundary with tested
recovery, a real mainnet payout receipt, a fresh mainnet genesis with a validator set that is not
three containers on one host, an off-host ledger backup with a restore that has actually been
performed, and a second person who can halt minting. None of them is met today.
