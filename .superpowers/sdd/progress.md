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

## Plan C T1 orchestrator scaffold — COMPLETE (1955466, pushed) — 28/28 still green, crate compiles
Controller-verified the trust split is REAL, not just documented: the orchestrator's config has NO
approver token (the only match for "approver" is a comment saying so) and holds exactly
treasury_initiator_token + treasury_readonly_token. So a compromised orchestrator can PROPOSE a mint
and never approve one — the treasury's independent verification and breakers are the actual authority.
Claims struct is {pk, exp} matching the hub's; all four orchestrator secrets are boot-validated and
the agent exercised the panic live rather than reading it.

### ACCUMULATING TAX worth a decision later (not now)
Rust 1.86 in the rig has now required FOUR Cargo.lock-only precise pins (home, and the
jsonwebtoken->simple_asn1->time chain, plus wiremock's let-chains). All lockfile-only, no Cargo.toml
ranges touched, all cargo-suggested. Kept 1.86 deliberately: it is the workspace convention
(clutch-node's Dockerfile ARG and clutch-hub-api's rust-toolchain.toml both pin 1.86), and matching
that is worth more than avoiding occasional pins. If the pins keep multiplying, the escape hatch is
bumping this repo's rig + Dockerfile to 1.89 as clutch-explorer already does.

## Plan C T2 deposit intents — COMPLETE (c6a41b5, pushed) — 10 new tests, all green
Agent died before committing; work was already finished and green, so it was reviewed and landed
rather than redone. Controller-verified the money-safety property is genuinely TESTED, not just
indexed: `expired_but_not_bitcart_terminal_keeps_amount_slot_reserved`, backed by the partial index
`WHERE status IN ('created','invoiced','paying','expired') AND NOT bitcart_terminal`.
That is the one that prevents crediting one user with another user's funds: Bitcart matches on AMOUNT
against a single shared custody address (Tron watch-only has no per-invoice derivation), so releasing
the slot at OUR soft expiry — while the old payer's transfer is still inbound — would let a new intent
claim the same amount.
Other tests: replay returns stored status AND body; same-key-different-body 409; crash-resume takes a
FRESH discriminator not the orphan's amount; compare-and-set second writer loses; concurrent
same-amount intents diverge; expired->confirmed late-honour legal; confirmed->failed refused.

### CONTROLLER LESSON (cost me two tool calls, worth recording)
`tail -N` on the suite output HID this crate's results: cargo runs packages alphabetically, so
payment-orchestrator's section comes BEFORE treasury-service's and gets cut by a tail. I briefly
concluded the 371 lines of tests were dead weight. Grep for the specific binary name, do not tail.

### CONTROLLER ORDERING CORRECTION: T3 before T2b
Plan C lists T2b (public deposit endpoints) before T3 (PaymentAdapter + BitcartAdapter), but T2b's
create-flow calls `adapter.create_invoice(...)`. The trait must exist first, so the order is
T3 -> T2b. Also splitting the cross-service concern: T2b does bounds checks only; the daily-headroom
check needs the treasury's reserve-status to expose daily_headroom_clt, which Plan B did not build,
so that lands in T5 which already touches the treasury. Keeps T2b to wire + idempotency.

## Plan C T3 PaymentAdapter + BitcartAdapter — COMPLETE (062b010, pushed) — 15/15 binaries, 13 new tests
Controller-verified: exception sub-status OVERRIDES main status (map_status takes both; a
complete+paid_over invoice maps PaidOver, so an overpayment cannot be silently read as exact);
all nine statuses mapped plus Unknown(String) explicitly commented "NEVER silently treat as benign";
usdt_decimal_string is pure integer arithmetic (format!("{}.{:06}", n/1_000_000, n%1_000_000)) and
grep confirms ZERO float ops in the file.

### STANDING RISK (agent flagged honestly; mitigated by design, close it in T7)
These Bitcart field names are UNVERIFIED against a live instance: `payments[].tx_hash`, the
`expiration` request field, and `payment_address` / `expiration_seconds` in the response. Wiremock
tests pin OUR side of the contract, not Bitcart's.
Why this is not blocking: T5's verifier already has a NULL-tx_hash fallback by design — match on
amount + custody address + time window via the trc20 endpoint it already calls. So a wrong hash field
degrades to the fallback rather than breaking the deposit path.
ACTION FOR T7: confirm all four names against the real Swagger before trusting the tx hash, and note
the adapter is pinned to Bitcart 0.10.3.0 (single-maintainer project — do not auto-update).
- Agent added deposit_ttl_minutes to BitcartAdapter's struct because create_invoice has no ttl param
  while a later brief line required sending the expiration. Correct reading of intent.
