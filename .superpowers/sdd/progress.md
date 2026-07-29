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

## Plan C T2b public deposit endpoints — COMPLETE (38e07ad) + review fix (a32a101, both pushed)
15 binaries green; new db_deposit_api 7/7 (replay-preserves-status, same-key-different-body 409,
still-processing 409 + Retry-After: 2, GET owner-check, +3 guard tests). db_deposits now 10.
Routes: POST /api/v1/deposits, GET /api/v1/deposits/:id. Handler is thin — every decision lives in
deposits::create_and_invoice, which returns a DepositOutcome that api.rs maps to status+headers, so
the module stays axum-free.
Agent's two unspecified judgment calls, both KEPT: GET owner-mismatch returns 404 not 403 (don't
confirm a resource exists to someone not allowed to see it); bounds violation 400 not 422.
It also fixed main.rs, which had never connected the pool or run migrations — the binary could not
boot with the new routes otherwise. Legitimate, not scope creep.

### CRITICAL found in review (a32a101) — orphan discriminator slot was released
The migration states the invariant: a slot frees ONLY when the Bitcart invoice is terminal, because
amount is the ONLY thing Bitcart can match a payment by on the shared static Tron custody address.
T2's resume branch (response_body IS NULL) violated it: it UPDATEd pay_amount_usdt to a FRESH
discriminator, releasing the old amount from uq_active_pay_amount while a possibly-live orphan
invoice still carried it. A later, DIFFERENT user allocated that amount then collides with a
stranger's invoice = cross-user misattribution, the exact hazard the discriminator exists to stop.
Reachable WITHOUT a crash: an adapter timeout where Bitcart DID create the invoice leaves the same
NULL-response_body state, so a Bitcart hiccup + ordinary client retries mints live orphans and frees
a slot each time. T2b is what made it reachable at all, by putting create() on a public route.
FIX: resume keeps the same pay_amount_usdt (net -16 lines). Two live invoices at one amount for the
SAME intent is not the hazard — same order_id, user and credit; store_invoice's CAS + the per-invoice
webhook event key still credit once. A second genuine payment strands on the transition guards into
needs_manual: funds held and flagged, never paid to a stranger.
The old test ASSERTED the bug (assert_ne on the amount, and a comment calling the freed slot
"legitimate" for another user). Rewritten to assert the money property: after a resume a direct
INSERT by another user at the orphan's amount is rejected, and the rejection is asserted to come from
uq_active_pay_amount BY NAME so it can't pass on an incidental error. Mutation-checked: fails on
pre-fix code at the amount assert.
- LESSON: T2's comment argued the opposite ("reusing pay_amount would risk two live invoices sharing
  one amount") and was persuasive but conflated cross-INTENT ambiguity (the real hazard) with
  same-intent duplication (benign). A plausible safety rationale in a comment is not a proof.
- The agent correctly did NOT change this itself — brief said stop rather than reshape T2's guards.

### CARRY TO T4 (webhook intake)
Look up the paid invoice by invoice_id AND fall back to matching on pay_amount_usdt when the id is
unknown. An orphan invoice's webhook arrives with an invoice_id no row holds; with the fix above the
amount now resolves to the correct intent, so the fallback is what actually collects that payment
instead of dropping it. Also: notification_url is built as {public_base_url}/webhooks/bitcart —
T4 must mount the route at exactly that path.

### CORRECTION to the CARRY TO T4 note above — do NOT add an amount fallback to the webhook
Reasoned it through again and the note was wrong. Matching on the payload's amount would trust an
attacker-supplied amount, which spec §6 forbids outright. Matching on a REFETCHED amount is
trustworthy but means calling Bitcart for every unknown id an internet spammer sends — it converts
the plan's indexed-lookup-first guard into a DoS amplifier aimed at our own payment processor.
Keep T4 exactly as planned: unknown invoice_id stores nothing, calls nothing, returns 200.
The orphan-invoice payment is collected by T5's TronGrid verifier, which scans the custody address's
trc20 transfers and matches by amount — off the attacker's path entirely, and only reachable by
someone who actually moved USDT on-chain. Consequence worth stating: that fallback is LOAD-BEARING,
not a nicety. It is the only collector for a payment against an invoice whose id we never recorded.

## Plan C T4 webhook intake + poll backstop — COMPLETE, 22 new tests, all green
Files: src/webhook.rs (apply_invoice_update shared by both entry points + handle_webhook),
src/poller.rs (interval loop: refetch due invoices, sweep past-expiry to expired, sweep
webhook_events >30d), src/alerts.rs (own alerts table/fn, same shape as treasury's ledger::alert —
separate DB, can't share the function), migrations/0002_alerts.sql, route wiring in api.rs,
poller spawn in main.rs, tests/support/mod.rs (scriptable FakeAdapter), tests/db_webhook.rs (12
tests) + 3 new deposits.rs helpers (find_by_invoice_id, mark_bitcart_terminal, set_tron_tx_id).

Confirmed the CORRECTION above (no amount fallback) was followed exactly — handle_webhook does the
indexed lookup by invoice_id BEFORE any DB write or Bitcart call; an unknown id returns having
touched nothing (unknown_invoice_id_writes_nothing_and_calls_nothing test, using FakeAdapter's
Err-on-unscripted-id property to catch a stray refetch call).

State table implemented (all from apply_invoice_update, both webhook and poller path):
Pending -> no-op. Paid -> paying (from invoiced|expired). Confirmed -> confirmed (from
invoiced|paying|expired, late-honour), stores tron_tx_id. PaidOver -> confirmed, same from-set,
credits amount_clt (untouched by the overpayment) + warn alert naming the invoice for manual
surplus lookup (T3's InvoiceStatus has no observed-amount field, only state + tron_tx_id, so the
alert can't quote a number — it points a human at the invoice instead of guessing one). Expired
(Bitcart-side) -> bitcart_terminal=TRUE unconditionally + our status -> expired only from
created|invoiced (brief's literal from-set — NOT from paying, since a payment already in flight
there isn't undone by Bitcart independently expiring the invoice). Invalid|Refunded ->
bitcart_terminal=TRUE + failed, from every status except credited. PaidPartial|FailedConfirm|
Unknown(_) -> needs_manual (same from-set, i.e. also excludes credited) + p1 alert.

Two required tests, both against the LIVE guard, not asserted by reading transition's from-set:
late_confirm_from_expired_reaches_confirmed — soft-expires an intent (same transition call the
poller's sweep uses), scripts FakeAdapter's refetch to Confirmed, drives apply_invoice_update
directly, asserts status=confirmed AND tron_tx_id recorded.
slot_reserved_while_expired_not_terminal_then_frees_on_bitcart_terminal — same setup, but first
refetches Pending (slot must still reject a second user's direct INSERT at the same
pay_amount_usdt, asserted by constraint name), then re-scripts to Expired and refetches again
(bitcart_terminal flips true, and NOW the second user's INSERT succeeds). Both driven through
apply_invoice_update, not by hand-setting bitcart_terminal.

Poller property (poller_alone_reaches_confirmed_with_no_webhook_delivered): identical Confirmed
scenario run via poller::poll_once with zero calls to webhook::handle_webhook anywhere in the
test, proving the reliability-path claim rather than asserting it from the shared-function
structure alone.

### One judgment call worth flagging: the money-safety boundary is "never FROM credited", not
"never FROM confirmed". First draft of the out-of-order test assumed Invalid arriving after
confirmed should be refused — it failed, and on rereading the brief the guard is written exactly
"never from credited" since that's the only status meaning CLT is actually minted; confirmed is
pre-mint (T5 hasn't necessarily run yet). Code was already correct per the brief's literal
from-set; the test's assumption was the bug. Rewrote it to prove the REAL out-of-order case
(a stale Paid arriving after Confirmed must not regress confirmed back to paying — Paid's own
from-set is invoiced|expired and doesn't include confirmed) and kept a separate test
(invalid_never_moves_a_credited_row) for the actual credited boundary.

### Ambiguity decided: PaidOver's alert can't quote a surplus figure
T3's InvoiceStatus (out of this task's scope to change) carries only `state` and `tron_tx_id` — no
observed/received amount. So "record the surplus" is implemented as an alert naming the deposit id
and invoice id for a human to look up the actual paid amount in Bitcart directly, rather than
computing a number this crate cannot actually observe. Flagging this as a natural fit for T5 or a
follow-up: if BitcartInvoice's payments[] entries carry an amount field (unverified per the T3
STANDING RISK note above), get_invoice could surface it and this alert could quote the real figure.

### ponytail: left in webhook.rs's confirm_and_credit
`// ponytail: T5's bridge picks up confirmed intents from here (treasury_bridge.rs, out of this
task's scope) — nothing to call, it polls this status itself.` — one-line marker per the brief's
"leave a one-line comment, no stub function, no dead code" instruction for where T5 hooks in.

## Plan C T4 review fixes (d903bae, pushed) — 13/13 db_webhook, 57 workspace tests green
T4 itself (7cc44d9) is good: apply_invoice_update is the SINGLE place Bitcart state becomes ours
and both the webhook and the poller call it, so "every state the webhook reaches is reachable by the
poller alone" holds by construction rather than by keeping two paths in sync. Refetch-only, nothing
read from the payload but the id.

### CRITICAL #2 — same class as a32a101, from the other end
uq_active_pay_amount's status list omitted needs_manual and failed, so reaching either freed the
discriminator slot on STATUS ALONE, ignoring bitcart_terminal. Wrong exactly where it matters most:
paid_partial (user underpaid) sends the intent to needs_manual while Bitcart's invoice is STILL LIVE
and can still take the remainder at that amount. Freeing the slot hands a later, different user the
amount a stranger's live invoice is waiting on.
FIX (migration 0003): add needs_manual + failed to the index. confirmed/mint_requested/credited stay
OUT — a completed invoice can't match a new payment, and holding those would leak one slot per
successful deposit, forever.
Follow-on 1: the poller now also refetches needs_manual/failed rows that aren't bitcart_terminal,
purely so terminality releases the slot. Without it an underpaid deposit reserves its amount forever.
Follow-on 2: that reintroduced an auto-close risk, so the Invalid/Refunded transition now uses a
from-set EXCLUDING needs_manual. A human was asked to look at money that may sit in custody; Bitcart
calling the invoice invalid is not that human answering.
- PATTERN, now twice: this index is the single load-bearing guard against cross-user misattribution
  on the shared static Tron address, and BOTH bugs were a status path quietly stepping outside its
  predicate rather than anything wrong with the index. Any new status, or any new writer of `status`,
  must be checked against it. Third time, consider a trigger that refuses to clear the slot while
  bitcart_terminal is false.

### Also fixed: unauthenticated route spawned detached work
/webhooks/bitcart has no auth (Bitcart can't sign). The handler tokio::spawn'd BEFORE the
known-invoice lookup, so a spammer could fire and disconnect while each detached task took one of 5
pool connections — starving the deposit routes through the one route with no auth in front of it.
Now the lookup is awaited in the handler (one indexed SELECT) and only real work is backgrounded.
handle_webhook remains as the composition of is_known_invoice + process_known_webhook so tests still
cover the unknown-id case end to end.

### TOOLING GOTCHA (cost me a false result)
sqlx::migrate! embeds migration SQL at COMPILE time and cargo does not reliably re-fingerprint the
migrations directory. A mutation run that edits a migration and restores it can silently execute the
PREVIOUS binary's embedded schema — I got a spurious FAILED on already-correct code this way. Force
a rebuild (touch a source file in the crate) when mutation-testing a migration. Also: point
DATABASE_URL at a throwaway database name for such runs, since editing an applied migration's
content trips sqlx's checksum bookkeeping on the existing test DB.
Litter to know about: databases mut1/mut2/mut3 now exist in the TEST postgres (compose project
clutch-treasury, not the user's clutch-dev stack). Harmless.

### Agent's own flagged concern, accepted
PaidOver's alert can't quote a surplus figure — T3's InvoiceStatus carries only state + tron_tx_id,
no observed amount. It names the invoice for manual lookup instead of fabricating a number, which is
the right call. Natural T5 follow-up if the TronGrid verifier surfaces the received amount anyway.

## Plan C T5 (treasury half) — tron_verifier — 82/82 workspace tests green, mutation-checked
Files: migrations/0002_deposit_evidence.sql (client_ref/deposit_tx_id/verified_at columns + the
uq_mint_intents_deposit_tx partial unique index the brief required to close a double-mint hole the
plan left open), src/tron_verifier.rs (new), modified intents.rs/api.rs/configuration.rs/lib.rs/
main.rs/reconciliation.rs/breakers.rs, tests/db_tron_verifier.rs (12 new tests).

The gap: the plan's `deposit_tx_id TEXT` had no uniqueness, so one real on-chain transfer could
back two mint intents. Added the partial unique index; the fallback-match backfill path treats
losing that race as a HARD mismatch for the losing intent (reject, never retry into a second
approval) — this is exactly the "code path steps outside the constraint's predicate" bug class
a32a101 and d903bae already hit twice in the sibling crate, so it got mutation-tested rather than
trusted on inspection: swallowed the backfill-race error to simulate exactly that bug, confirmed
the test (`fallback_backfill_losing_the_race_rejects_not_approves`) fails and correctly reports
`left: "approved", right: "rejected"`, then restored the real code.

Also mutation-tested the "custody event exactly-once across a rerun" requirement. Turns out this
property is defended FOUR independent layers deep (verify_once's `WHERE status='created'` SELECT;
approve_and_ledger's own `WHERE status='created'` UPDATE guard; chain_outbox.intent_id UNIQUE,
which forces the whole transaction to roll back on a second attempt; treasury_events' existing
uq_events_intent_kind ON CONFLICT) — stacking all three inner mutations together and rerunning
still passed only because of the outbox UNIQUE, and only removing all four made the rerun test
fail. Confirms the property is genuinely proven, not assumed, and that nothing in this codebase
ever moves mint_intents.status backward (checked every writer), so those inner layers are
legitimate defense-in-depth rather than dead code the test coincidentally exercises.

Central distinction (hard mismatch -> reject vs transient -> retry) is structural, not just
tested: verify_once's match has exactly two arms that write `status` (Pass, HardMismatch);
Transient falls through to a debug log only. "Reschedule with backoff" needed no new column —
it's the existing poll loop itself (same shape as outbox.rs/watcher.rs), and the brief's
warn-at-30m/p1-at-24h is a pure age check against created_at, no new state.

daily_headroom_clt reuses breakers::daily_mint_total (extracted from check_mint_inner) rather
than reimplementing the 24h sum — verified it actually shrinks with real approved-mint rows, not
just that the field exists.

TronGrid endpoint shapes are ASSUMED (flagged in the task report), not verified against a live
call — trc20 transfer list fields (transaction_id, to, value as decimal string, token_info.address),
the transactions/{id} confirmed boolean, and the accounts/{address} trc20 balance map. wiremock
pins OUR side of the contract only, same caveat T3's Bitcart integration carries.

## Plan C T5a treasury TronGrid verifier — 40e1469 + review fix cb497e3 (both pushed), 15/15 db_tron_verifier
Verified good in T5a as delivered: all four evidence conditions checked with no short-circuit;
hard-vs-transient split is clean (an unmatched tx hash is treated as TRANSIENT, not a mismatch —
correct, absence of evidence is not evidence against); created_by derives from the authenticated
role, never the body; approve + verified_at + outbox + custody event in ONE transaction with the
rerun proven by calling verify_once twice and counting rows; daily_headroom_clt added reusing the
breaker's existing daily total; trongrid_balance recorded as a cross-check column that no branch
reads (correctly NOT wired into the breaker). Agent closed the deposit_tx_id uniqueness gap I
flagged, and mutation-tested both required proofs itself.

### CRITICAL #3 — verifier matched the WRONG amount (cb497e3)
The treasury never received the discriminator. amount_clt is the intended figure (10_000_000); what
the user was told to pay is amount + discriminator (10_000_123), and on the shared static custody
address that discriminated amount is the ONLY thing separating payers. pay_amount_usdt lived solely
in the orchestrator, so the fallback matched `>= amount_clt`: "any confirmed transfer to custody of
at least the intended amount".
Impact: the first NULL-tx_id intent claims whichever qualifying transfer TronGrid lists first, is
approved on a stranger's money, and (verifier ledgers the OBSERVED amount) records that stranger's
FULL transfer as this deposit's custody. Rightful depositor then locked out of their own transfer by
uq_mint_intents_deposit_tx. A 50 USDT deposit could back a 2 USDT mint and inflate reserve by 48.
FIX (migration 0003 + code):
- expected_amount_usdt travels with the intent; fallback matches EXACTLY and only within
  deposit_match_window_hours (24). Slots are recycled after terminality, so an old unclaimed
  transfer at a reused amount must not back a later intent.
- amount_clt REMOVED from DepositBackedIntent so no path can match on it again. This is the
  structural half of the fix — the comment explains why the struct omits it.
- deposit-backed create without expected_amount_usdt = 400 + DB CHECK; NULL fails closed in the
  verifier (Transient, never approve/reject, escalates via stuck sweep).
- approving off the fallback raises a warn EVERY time: weaker evidence path, and a run of them means
  the processor stopped returning hashes — worth seeing before it backs much of the reserve.
- Consequence accepted: an overpayment with NO tx hash no longer auto-verifies. It ages into the
  stuck sweep and a human. Right side to fail on.
- CHECK is NOT VALID: still enforced on every INSERT/UPDATE (the point), only skips scanning
  pre-column rows. No safe backfill exists for those — amount_clt is exactly the value whose use as
  a match key is the bug.
Mutation-checked by restoring the >= match: fallback_must_not_claim_a_larger_transfer_from_another_payer fails.
- LESSON: three Criticals now, all the same shape — a money-identity value that one service owns and
  another silently substitutes something weaker for. Discriminator slot freed early (a32a101),
  freed on status alone (d903bae), and now matched against the undiscriminated amount. Whenever the
  discriminated amount crosses a service boundary, check it actually arrived.
- Test fixtures now keep amount_clt != expected_amount_usdt everywhere; that difference is what
  lets them catch a regression to the intended amount.

### RIG GOTCHAS
- treasury-service tests connect straight to DATABASE_URL and do NOT create the database (unlike the
  orchestrator's, which append _orchestrator and create). Overriding DATABASE_URL to a throwaway
  name makes all 8 db_breakers tests fail at PgPool::connect — an environmental failure that looks
  like a code failure. Only the orchestrator tolerates that trick.
- Standing: sqlx::migrate! embeds SQL at compile time; force a rebuild when mutating a migration.

### STILL OPEN from T5a (agent flagged, accepted)
- TronGrid endpoint/field shapes are ASSUMED, not verified live — including block_timestamp, which
  cb497e3 now depends on for the window. Missing field defaults to 0, which fails closed (outside
  every window), so a wrong name degrades to "fallback never matches" rather than a bad approval.
  Verify in T7 alongside the Bitcart field names.
- usdt_contract default is the real MAINNET USDT-TRC20 address. Confirm intended network before any
  non-test deploy.
- chain_outbox.intent_id UNIQUE turned out to be an extra unplanned layer of rerun-safety.

## Plan C T5b payment-orchestrator: deposit->mint bridge — 74 workspace tests green (14 new)
Files: src/treasury_bridge.rs (new), migrations/0004_mint_intent_link.sql (treasury_intent_id,
attempts, next_attempt_at), modified deposits.rs (headroom check + bridge DB helpers), main.rs
(spawn), lib.rs. Treasury side (small, per brief): api.rs gains GET /internal/mint-intents/:id
(any role), intents.rs gains find_by_id. tests/db_bridge.rs (13 new) + 1 new test in
db_tron_verifier.rs for the new GET route (16 total there now).

THE line (brief's own words): the POST body sends `expected_amount_usdt: intent.pay_amount_usdt`
— the DISCRIMINATED amount — never amount_clt. Proven on the wire, not by reading the code:
post_sends_pay_amount_usdt_as_expected_amount_usdt_proven_on_the_wire seeds a fixture with
amount_clt=1_000_000 != pay_amount_usdt=1_000_391 (same fixture discipline db_tron_verifier.rs
already uses) and wiremock's body_json matcher requires the EXACT JSON — sending amount_clt in
that field would 404 the mock and fail the test on the resulting stuck-at-confirmed path, not
silently pass.

client_ref idempotency: added NO dedup layer on top of the treasury's existing client_ref replay,
per the brief's explicit instruction. due_for_mint_request only selects status='confirmed' rows,
so a row the create step already moved to mint_requested is naturally excluded from re-selection
on the next tick — retried_run_does_not_create_a_second_mint_intent proves the POST route sees
exactly one call across two run_once passes via wiremock's .expect(1).

Reliability: attempts + next_attempt_at columns, jittered exponential backoff (base 5s, cap 300s),
p1 alert fires at exactly the 10th CONSECUTIVE failure (not before, not repeated on the 11th) and
resets on any success — all four properties proven directly, not asserted from the constants.

Status transitions, every one through the existing guarded `transition` (no bare UPDATE added):
confirmed -> mint_requested (create success), mint_requested -> credited, mint_requested ->
needs_manual (rejected/failed) + p1 alert whose text says funds are in custody unminted and that
re-minting needs a human-created NEW treasury intent (client_ref is burned) — asserted by
substring match on the actual alert row, not by reading the format string.

Daily-headroom check (T2b's deferred ponytail comment, now implemented): GETs
/internal/reserve-status with the readonly token before ever calling the PaymentAdapter. FAILS
CLOSED both ways — unreachable treasury and insufficient headroom both refuse the deposit (503+
Retry-After vs 422) rather than proceeding. Proven with a PanicsIfCalledAdapter that would fail
the test immediately if Bitcart were ever invoked in either failure case — stronger than checking
the returned outcome alone, since it structurally guarantees the check runs strictly before any
invoice creation.

### BUG CAUGHT BEFORE IT SHIPPED — debug-build integer overflow in the backoff formula
First draft computed `BACKOFF_BASE_SECS * 2i64.saturating_pow(attempts as u32)` then `.min(cap)`.
`saturating_pow` only saturates the POWER; the subsequent multiplication by 5 then overflows i64
once attempts gets into the hundreds (a multi-day outage at a 30s poll interval), and Rust panics
on overflow in debug builds — exactly the profile `cargo test` uses in this rig. Caught by tracing
the arithmetic through by hand before running, not by a failing test (nothing in this task's test
set runs attempts into the hundreds). Fixed by capping the EXPONENT itself
(`min(BACKOFF_MAX_SECS.ilog2()+1)`) before the `pow`, so the intermediate value can never leave a
range `.min(cap)` can safely clamp. Worth a note for whoever writes the next backoff formula in
this codebase: cap the input to pow(), not just the output.

### Test-authoring correction made during this task (not an implementation bug)
`run_once` drives BOTH due_for_mint_request and due_for_status_poll every call (same shape as
outbox.rs/watcher.rs's existing two-pass-per-tick convention) — so a row the create step just
moved to mint_requested is immediately eligible for that SAME call's poll half. Three early test
drafts assumed create and poll were strictly separate ticks and asserted an intermediate state
that the same-tick poll silently altered from underneath them (one via an unmounted GET route
causing a spurious failure + backoff that then made the NEXT call skip the row's backoff window
entirely). Fixed by making every test's HTTP fixtures answer whichever half of run_once might
touch the row at each point in the test, not just the half the test means to isolate. Two of the
three failures reproduced with genuinely wrong error messages the first time (right diagnosis
took reading create_step/poll_step's control flow line by line, not guessing from the assertion
text alone) — recorded here so the next task in this file doesn't have to rediscover the
two-loops-per-tick property the hard way.

### Existing tests updated for the new headroom dependency (not scope creep)
db_deposit_api.rs's 7 tests all call create_and_invoice (indirectly via the real router), which
now GETs reserve-status before ever reaching the adapter — treasury_url: "http://unused" made
every one of them 503 instead of reaching their real assertions. Added a
mock_treasury_with_generous_headroom() wiremock helper (same convention bitcart_adapter.rs already
uses) and threaded treasury_url through test_config(); db_deposits.rs/db_webhook.rs call
deposits::create directly and never touch create_and_invoice, so they were unaffected and left
alone.

## Plan C T5b deposit->mint bridge — 3666ee8 + review fix 287ae02 (both pushed), 13/13 db_bridge, 74 workspace
THE contract holds: create_step POSTs "expected_amount_usdt": intent.pay_amount_usdt (treasury_bridge.rs:72),
proven on the wire by asserting the body wiremock received with amount_clt=1_000_000 vs
expected=1_000_391 — deliberately DIFFERENT values, so a regression to amount_clt fails the test.
Critical cb497e3's treasury-side fix is now actually fed the right value by its sole producer.
Also good: headroom check fails closed both ways (TreasuryUnavailable = 503+Retry-After,
InsufficientHeadroom = 422 since retrying now won't help); every status change via guarded
transition; client_ref replay is the only idempotency layer, no second one stacked on top.
Agent's flagged ambiguity #2 (create and poll both run each tick, so confirmed -> credited can
happen within one tick) reviewed and ACCEPTED — matches outbox.rs/watcher.rs convention, guards hold.

### Review fixes (287ae02) — alerting, not logic
1. P1 was edge-triggered at EXACTLY 10 consecutive failures: `attempts == ALERT_AFTER`. Fired once,
   then silence forever while the row retried. The reported state is "user's USDT in custody, no CLT
   minted against it" — it does not self-resolve, and one page can be missed/acked/lost. Now
   `attempts % ALERT_AFTER == 0`: repeated signal, not every tick. Test extended through the 20th
   failure (one page at 10, still one through 19, second at 20).
2. Every failure said "treasury may be down" regardless of cause — this was the agent's flagged
   ambiguity #1 and it is worse than it looks: a non-2xx means the treasury ANSWERED and refused us
   (deploy/config fault, never self-heals) and a failed local DB write means the treasury is fine
   and OUR database isn't. Both sent the responder to the wrong system. Added FailureCause
   {Unreachable, Rejected, Local}; alert text names which, and states funds are in custody unminted.
   Local exists specifically because Rejected's text would have been an actively wrong diagnosis.
- Kept the shared backoff for Rejected: the bridge genuinely cannot fix its own request, so
  retry-with-backoff is right; only the DIAGNOSIS needed splitting, not the retry policy.

## Plan C T6 brief written (.superpowers/sdd/planc-task-6-brief.md) — NOT yet dispatched
Held back deliberately while 5b was running: T6 touches orchestrator api.rs / lib.rs /
configuration.rs and the next migration number, all of which 5b was mid-edit on. Two agents in the
same files means one silently overwrites the other. Dispatch now that 5b has landed.
Three additions to the plan, recorded so the reasoning survives:
1. Base58CHECK validation, not the plan's "T..., base58, 34 chars" shape check. One mistyped
   character passes a shape check and sends USDT to an address nobody holds — unrecoverable, and the
   user's CLT is already burned by then. Verify the 4-byte double-SHA256 checksum and the 0x41
   version byte. Required test uses a ONE-CHARACTER CORRUPTION of a valid address: exactly the case
   a regex passes and a checksum catches. Small bs58 dep beats hand-rolling base58.
2. redemptions_enabled config flag, DEFAULT FALSE, 503 while off. payout.rs:22 still fabricates
   payout_ref = "stub:<uuid>" and sends nothing. Spec §7.6 wants a working off-ramp before real
   deposits; an endpoint handing out redemption_refs invites users to burn real CLT for a payout
   that cannot happen, and the burn is irreversible. The flag is what stops a half-built off-ramp
   being reachable in production by accident. Stays false until a real TRC-20 rail lands.
3. redeemer_address from the JWT, never the body — same class as created_by. A body field lets a
   caller redeem against someone else's balance and the treasury cannot detect the substitution.
Deliberately NOT added: a client-key idempotency layer. A duplicate POST creates a second intent
with its own redemption_ref, and payout requires a Burn carrying that specific ref — the user burns
once, so only one can ever be fulfilled. Litter, not a money bug. Must be explained in a ponytail:
comment so nobody "fixes" the absence later.

## Plan C T6 redemption proxy endpoints — dispatched and landed, 10/10 db_redemptions, 24 workspace
Real gap found and worked around rather than stopping the whole task: treasury-service's router
(`crates/treasury-service/src/api.rs`) has `POST /internal/redemption-intents` but NO
`GET /internal/redemption-intents/:id` — confirmed by reading the whole route table, not assuming.
The brief's `GET /api/v1/redemptions/:id` says "proxy the treasury status", which is literally
impossible with no treasury route to proxy from. Both plan and brief explicitly forbid touching
treasury-service in this task, so the fix isn't to add one. Since only the GET side is blocked
(POST matches the treasury's actual `{redeemer_address, payout_address, amount_clt}` shape exactly),
built GET as an owner-checked read of `redemption_map`'s own stored snapshot (captured once, from
the treasury's CREATE response) rather than a live re-fetch — documented as a concern in the task
report, not silently papered over. Flagged for follow-up: add the treasury GET route, then swap
this GET handler to a live proxy.

Address validation: `bs58 = { version = "0.5", features = ["check"] }` added clean (not in the lock
file before this task), resolves fine under the 1.86 pin — only new transitive dep is `sha2 0.10`.
Read bs58 0.5.1's actual decode.rs from the cargo registry cache rather than trusting memory of the
API shape: `with_check(Some(0x41))` verifies the double-SHA256 checksum AND the version byte, but
NOT total payload length — independently constructed (via a throwaway Node script, no Python on this
box) a base58check string with a genuinely valid checksum and the correct 0x41 version byte but only
10 address bytes instead of Tron's 20, and confirmed `bs58` alone accepts it. Added an explicit
`DECODED_LEN_WITH_VERSION == 21` check plus a regression test for exactly that case — the
checksum/version check alone was NOT sufficient. Also independently verified (same Node script) that
the "valid Tron address" and "one-character-corrupted" test fixtures actually have real/broken
checksums rather than trusting a plausible-looking string.

Flaky pre-existing test, not a regression: `db_bridge.rs`'s
`p1_alert_fires_at_ten_consecutive_failures_and_again_at_twenty` (T5b's own test, untouched by this
task) failed once in a full-workspace run (`left: 1, right: 2` on the 20th-failure repeat-alert
assertion) and passed clean on an isolated `--test db_bridge` rerun and two subsequent full-workspace
reruns. Nothing in this task touches `treasury_bridge.rs`, `deposits::record_attempt_failure`, or
`alerts.rs`. Recorded here rather than silently re-run-until-green: it drives 20 sequential real HTTP
calls against a MockServer with tight `next_attempt_at` resets between them, which is exactly the
shape of test that flakes under container resource contention. Left as-is per the brief's scope
(not this task's file), but worth someone's attention if it recurs.

`redemptions_enabled` gated in front of auth in BOTH handlers (not just validation) — a disabled
route 503s the same way regardless of whether the caller's JWT would otherwise have been valid, so
there's no behavioral asymmetry between the two routes to notice or exploit.

## Plan C T6 redemption proxy — fe12dda PUSHED; my review fix is UNVERIFIED WIP (docker daemon down)
T6 as delivered is good. Verified by reading: redeemer_address has NO body field at all (nothing to
ignore — stronger than ignoring it); base58check via bs58 with_check(Some(0x41)); redemptions_enabled
defaults false and BOTH routes 503; GET owner-mismatch 404 like deposits; bounds enforced.
Agent found something I had not specified and it was a real hole: with_check verifies the checksum
and version byte but NOT the payload length, so version 0x41 + 10 address bytes + a valid checksum
passes. It added an explicit 21-byte length check and a test built from an independently constructed
short address. Good catch — that is the kind of thing a "checksum validated" claim hides.

### GAP FOUND IN REVIEW (agent flagged it; fix written, NOT verified, NOT pushed)
Treasury had only POST /internal/redemption-intents, no GET, so the proxy's GET served the status
CAPTURED AT CREATION. watcher::confirm_burn is what advances a redemption, so a user polling their
own redemption would see `created` forever — including after their CLT was burned and the payout
made. Someone checking on their own money would be told nothing had happened.
FIX WRITTEN (5 files, patch saved — see below):
- treasury: intents::find_redemption_by_id + GET /internal/redemption-intents/:id (any role, mirrors
  the mint GET added in 5b).
- orchestrator: redemptions::fetch_treasury_status (READONLY token — a status read must never need
  the initiator credential) and GET now serves live status, falling back to the stored snapshot when
  the treasury can't be reached, with a `status_live` bool saying which.
- Deliberate asymmetry vs the create path's fail-closed 503: reading a status moves no money, so
  refusing the read is the wrong trade — but the client must be able to tell "nothing has happened
  yet" from "we could not ask". Hence status_live rather than a silent stale value.
- 2 new tests written: live status overrides a deliberately stale stored snapshot; unreachable
  treasury falls back and flags it.

### STATE: BLOCKED ON DOCKER, NOTHING PUSHED
`docker version` fails: "cannot find //./pipe/dockerDesktopLinuxEngine" — Docker Desktop is not
running, so the user's clutch-dev stack is down too. The test rig is the only way this repo is
allowed to build (user memory: never host cargo), so the fix above is UNVERIFIED. Not committed and
not pushed on purpose: it is money-path code and the whole point of the rig is that I do not claim
green without it.
RESUME: start Docker Desktop, then
  git apply "C:/Users/MEHRAN~1/AppData/Local/Temp/claude/D--source-clutch/986f4c73-c408-4dd1-afa8-338738fc5e0a/scratchpad/t6-live-status.patch"
  docker compose -f docker-compose.test.yml run --rm test
(the working tree already holds the change; the patch is a backup against losing it)

### ALSO OUTSTANDING from T6
Agent reported a FLAKY pre-existing test in db_bridge.rs (T5b, which it did not touch): passed on 2
of 3 full runs, passed on isolated rerun. Un-investigatable until docker is back. Worth chasing
rather than shrugging at — db_bridge covers the deposit->mint boundary, and a flake there erodes the
signal on exactly the path three Criticals already came from. Candidate cause to check first: the
alert-threshold tests drive run_once repeatedly and rewrite next_attempt_at, so a shared-clock/
ordering assumption is the likeliest culprit.
