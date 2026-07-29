# Plan B progress ledger (Treasury Service)

Plan: D:\source\clutch\plans\2026-07-27-plan-b-treasury-service.md
New repo: D:\source\clutch\clutch-treasury (sibling layout — clutch-deploy builds from ../)
Branch: main (fresh repo). Nothing pushed without user review.

## Upstream state (all DONE, 7 PRs open, none merged)
Plan A clutch-node PR #9 (23 commits) + Plan D PRs: deploy #1, explorer #5, hub-api #3, sdk #4,
demo-app #1, docs #1.
Contract this service must match, VERIFIED LIVE against a running node during Plan D T6:
- Mint = RLP tag 6, args [to, amount, credit_ref]; Burn = tag 7, args [amount, ref-or-""]
- tx hash preimage = 4-item [from(no 0x), nonce, chain_id, data]; wire = 8-item
  [from, nonce, chain_id, r, s, v, hash, data], chain_id at index 2, minimal big-endian
- get_chain_info: total_supply is a decimal STRING; every other numeric field is a bare number
  (Plan B's ChainInfo struct was patched to deserialize_with before starting — a plain u64 would
  have failed at runtime)
- node stores the wire hash WITHOUT 0x -> the watcher matches Mints by credit_ref, never by hash
- chain_id 2077, is_testnet true, tx_fee 1000, mint_authority 0x9b6e8afff8329743cac73dbef83ca3cbf9a74c20
- one tx per sender per block -> mint throughput is 1 per block per treasury key (accepted)

## Decisions already locked (from planning + user §0 answers)
- Reserve ledger = plain Postgres append-only event table + SQL views, NOT a double-entry engine.
- KMS = env-var stub behind a ChainSigner trait. MAINNET BLOCKER recorded in docs/keys.md.
- Four-eyes = two disjoint role tokens + a DB CHECK (approver <> initiator).
- Reconciliation is built BEFORE the mint path (spec build order).
- Pilot caps: $500/day, $50/tx; backing target 10050 bps, halt below 10000.
- Peg: 1 USD = 1,000,000 CLT. Amounts are BIGINT. Never floats.
