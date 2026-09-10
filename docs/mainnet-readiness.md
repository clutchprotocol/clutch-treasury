# Mainnet readiness

Status as of 2026-09-10: **not ready for real funds.** This document lists every gap between
the stack as deployed on stage and a deployment that could hold money, with a verification step
for each one, so readiness is a checklist rather than a judgement call.

Scope is the whole Clutch stack, not only the treasury, because the treasury cannot be safe on
its own. It lives here because this is the repo that holds funds.

## This file is the canonical one

A public version is published at
[docs.clutchprotocol.io/reference/mainnet-readiness](https://docs.clutchprotocol.io/reference/mainnet-readiness)
(`clutch-docs/docs/reference/mainnet-readiness.md`). It shares this structure and severities, but
six items are generalized or omitted there because publishing them verbatim would be targeting
information rather than honest disclosure:

| Item | Why it is not public verbatim |
|------|------------------------------|
| D2 | Names the scripts and file pattern under which mnemonic copies accumulate on the host |
| E1 | States that no rate limiting exists and which endpoints are reachable |
| G1 | States that the live edge config is drifted and owned by no repo |
| G2 | States that consensus, custody, and both databases share one host |
| G3 | States that only one person can halt minting |
| J1 | A written admission that no legal advice has been taken is quotable by a regulator |

B3 is internal because it is housekeeping, not posture. Everything else appears publicly in some
form, most of it already published before this document existed.

**When an item here changes, update the public page in the same change.** A public page that
understates a closed blocker is merely stale; one that overstates progress is worse than none.

## How to read this

| Severity | Meaning |
|----------|---------|
| **Blocker** | Real funds cannot be accepted until this is done. No exceptions, no caps small enough. |
| **Required** | Must be done before a public launch. May be deferred for a bounded pilot if the cap is low enough that total loss is acceptable. |
| **Recommended** | Reduces risk. Skipping is a decision to record, not a gap to hide. |

Every item names a **verification**: the specific artefact or observation that closes it. An item
is done when someone other than its author can point at that artefact. "It looks right" does not
close anything.

## The gate

Five things must all be true before any mainnet address is handed to a user:

1. The mint key and the payout key are behind a hardware or KMS boundary, with a tested recovery.
2. A real mainnet payout receipt has been read, and the energy and fee model matches it.
3. The genesis is a fresh mainnet genesis, with a validator set that is not three containers on
   one host.
4. The treasury ledger has an off-host backup and a restore that has actually been performed.
5. Someone other than the maintainer can halt minting and knows how.

Everything below expands these, plus the product and legal work that sits outside them.

---

## A. Key custody and signing

The named blocker, already tracked in [`keys.md`](keys.md).

### A1. KMS-backed mint signer — **Blocker**

The mint authority is an environment variable (`ChainSigner` /
`EnvKeySigner`, `crates/clutch-chain/src/signer.rs`). It is the only key that can create CLT, so a
host compromise is unbounded issuance against a fixed reserve. `keys.md` names the intended
replacement: a `KmsSigner` on AWS KMS `ECC_SECG_P256K1`, following the `alloy-signer-aws` pattern.
The trait is the swap boundary; nothing on the other side of it exists.

**Verification:** the mint path runs through a `KmsSigner` in the mainnet compose file, the private
key material has never existed outside KMS, and `EnvKeySigner` is unreachable in that configuration
(a config that would select it refuses to boot).

### A2. KMS-backed payout signer — **Blocker**

`PayoutSigner` (`crates/treasury-service/src/payout.rs`) is the matching seam for the payout key.
Today the payout float is derived from the deposit mnemonic at `m/44'/195'/0'/2/0` and held by
`tron-signer` as an environment variable. Exposure is bounded by the float balance and a
per-transaction cap, which is a real bound and the reason this is survivable on stage, but the key
is still a plaintext secret on a VPS.

**Verification:** payouts are signed through KMS, and the float's key material has never been on
disk. The per-transaction cap and float balance remain in place; the KMS boundary is in addition to
them, not a replacement.

### A3. Key ceremony and tested recovery — **Blocker**

`keys.md` requires a real ceremony and tested recovery before any real-funds deployment. Neither
has happened.

**Verification:** a written ceremony record (who was present, what was generated, where each share
went), plus a recovery rehearsal in which the mint and payout keys were restored from backup into a
fresh environment and used to sign a test operation. The rehearsal is the artefact, not the plan
for one.

### A4. Custody key stays absent — **Required, by design**

Nothing in this stack can spend from `APP_TREASURY_ADDRESS`. That is deliberate and should survive
to mainnet: the float exists precisely so that a compromised `treasury-service` cannot reach
custody. The consequence is that topping up the float is a human operation, and on mainnet that
operation needs to be as controlled as the ceremony above.

**Verification:** a written top-up procedure with two-person authorisation, a documented maximum
top-up, and no code path in this repo that can move custody funds.

### A5. Sweep signer review — **Reviewed 2026-09-11, holds**

All three write endpoints on `tron-signer` were read against this property. It holds:

| Endpoint | Request body | Destination | What bounds it |
|---|---|---|---|
| `/internal/sweep` | `{index}` and nothing else | `cfg.treasury_address`, from config | Cannot name a destination, amount or contract at all |
| `/internal/payout` | `{intent_id, to, amount_usdt}` | caller-named `to` | Source is always the float; a per-transaction cap is checked before signing |
| `/internal/fund-float` | none — a body sent anyway is ignored | the float | Cannot name anything |

So a caller who owns the orchestrator, or anything else that can reach the signer, cannot redirect
a deposit. Payout is the deliberate exception and its blast radius is the float balance, not
custody. `intent_id` on payout is not used for signing; it exists so a broadcast can be tied back
to the redemption that caused it, which is the only way an ambiguous payout is ever resolved.

**Re-verify** whenever any of those three request types gains a field. The property is a shape,
not a check, so it is broken by an addition rather than by a change.

---

## B. The payout rail on mainnet

### B1. First real payout receipt — **Blocker**

This cannot be tested on stage, and the runbook says so. Nile's test USDT contract sponsors its own
energy, so every Nile payout reports `energy_fee: 0` and non-zero `origin_energy_usage` whether or
not delegation works — a payout made before delegating reads identically to one made after. Mainnet
USDT makes the sender pay. The first mainnet payout is therefore the first real test of the energy
model, the delegation, and the fee.

**Verification:** a mainnet payout receipt showing non-zero `energy_usage` (sender-supplied, own or
delegated) with `energy_fee: 0`, captured and attached to this document. `clutch-deploy`'s
`inspect-stage.yml` already has an `energy` probe that reads the most recent payout's receipt and
reports who supplied the energy; point the mainnet equivalent at that. Until the receipt exists,
keep a fee that covers burning TRX outright.

### B2. Re-measure the redemption fee — **Blocker**

The current fee was measured, not chosen: on 2026-09-10 a TRC-20 transfer into an address holding
no USDT burned 130,285 energy units at a `getEnergyFee` of 100 sun, about 13 TRX or $4.43 at a TRX
price near $0.34. `getEnergyFee` is a TRON governance parameter that has already halved once, from
210 sun. Scaling the stage number is not valid; it has to be re-measured against mainnet.

**Verification:** a fresh measurement on mainnet, dated, with the energy units, `getEnergyFee`, and
TRX price recorded, and the fee set from it. Include the two cases separately: a recipient address
that already holds USDT, and one that does not.

### B3. Reconcile documented values against deployed values — **Closed 2026-09-10**

The docs and the compose defaults disagreed about live economics: the compose file defaults to a $1
fee with a $5 minimum, while `clutch-node/clt-economics.md` described $5 and $10. The maintainer
confirmed the live values are **$1 with a $5 minimum**, so the documents were the stale side, not
the config. `clt-economics.md` and `clutch-treasury/redemptions.md` were corrected, and the $5
figure now appears there as what it actually is: the fee a deployment needs when its payout float
burns TRX for energy, which is where mainnet starts until delegation is proven on a real receipt.

Re-open this item on any deployment whose `.env` overrides these, since the host `.env` is not in
the repo.

| Setting | Compose default | Unit |
|---------|-----------------|------|
| `APP_REDEMPTION_FEE_USDT` | 1000000 | micro-USDT ($1) |
| `APP_MIN_REDEMPTION_CLT` | 5000000 | CLT ($5) |
| `APP_MAX_REDEMPTION_CLT` | 25000000 | CLT ($25) |
| `APP_PER_TX_PAYOUT_CAP_USDT` | 25000000 | micro-USDT ($25) |
| `APP_PER_TX_MINT_CAP_CLT` | 50000000 | CLT ($50) |
| `APP_DAILY_MINT_CAP_CLT` | 500000000 | CLT ($500) |

**Verification:** the live values are read off the host with `inspect-stage.yml`'s `treasury` probe,
which prints the non-secret settings of all three services, then recorded here with a date, and
whichever document is wrong is corrected. The two independent redemption
bounds must remain numerically aligned, for the reason given in the workspace notes: a request the
signer would reject must never become a burn nobody can pay.

### B4. Mainnet caps set deliberately — **Blocker** (analysis and a checker done 2026-09-11)

The stage caps were sized for test money. Mainnet caps are the loss ceiling for every failure mode
above them, so they are the last line of defence and have to be chosen on purpose.

**What each one actually bounds**, which is the part that was missing:

| Cap | Stage value | The worst case it bounds |
|---|---|---|
| `PER_TX_MINT_CAP_CLT` | $50 | One erroneous mint. Mints are four-eyes approved against a verified deposit, so the realistic failure is a bug computing the amount, not a rogue approval. |
| `DAILY_MINT_CAP_CLT` | $500 | **A compromised mint authority.** This is the important one: the per-transaction cap does nothing against an attacker willing to submit repeatedly. Total exposure is this cap multiplied by the time until someone notices, which is why it and the D3 alert route are the same decision. |
| `MAX_REDEMPTION_CLT` | $25 | One redemption. Must equal the signer's cap — see below. |
| `PER_TX_PAYOUT_CAP_USDT` | $25 | One payout out of the float, enforced independently in `tron-signer`. |
| `MIN_REDEMPTION_CLT` | $5 | Nothing, on its own. It exists so the smallest allowed redemption is not one the treasury refuses for failing to cover its fee. |
| `REDEMPTION_FEE_USDT` | $1 | Nothing. It is a cost recovery, and it is capped from below by the on-chain payout cost (B2). |
| Rolling 24-hour ceiling | above both | The same compromise scenario as the daily mint cap, from the payout side. |

The float balance is a cap nobody set: **a compromised `treasury-service` cannot move more than the
float holds**, because it cannot reach custody at all (A4, A5). Keeping the float small is
therefore a safety control and not just an operational convenience.

**The relationships are now checked mechanically** rather than remembered:
`clutch-deploy/scripts/check-cap-invariants.sh`, which `set-mint-caps.sh` runs after every change.
It enforces five things, each with the quiet failure it prevents:

1. `per_tx_mint <= daily_mint` — otherwise no mint clears both and minting is simply off.
2. `max_redemption <= per_tx_payout_cap` — **the one that costs a user money.** The two live in
   services that do not derive from each other, so a request between them burns the CLT and then
   cannot be paid.
3. `fee < min_redemption` — otherwise the smallest allowed redemption is one the treasury refuses,
   reaching the user as a bare 502 with no explanation.
4. `min_redemption <= max_redemption` — otherwise every redemption is refused, silently.
5. The fee as a share of the smallest redemption, because that ratio is what a user experiences.

The stage set passes all five.

**What is still a decision, and it is yours:** the mainnet numbers. The method is above; the inputs
are expected deposit sizes and how much a single error or a compromise window may cost. Two things
worth deciding together rather than separately:

- **The daily mint cap and the alert route (D3) bound the same risk.** A cap of X with an alert that
  reaches someone in minutes is a very different exposure from the same cap with no route at all.
  Set them in the same sitting.
- **A higher per-transaction mint cap is needed for larger real deposits**, and it widens the blast
  radius of an amount-computation bug by exactly that much. There is no clever answer; there is only
  a number with a reason attached.

**Verification:** each mainnet cap recorded here with the worst case it bounds, and
`check-cap-invariants.sh` passing against the mainnet `.env`.
---

## C. Chain and genesis

### C1. Fresh mainnet genesis — **Blocker**

`is_testnet = true` and `chain_id = 2077` are committed into the genesis hash, alongside
`tx_fee`, `mint_authority`, `faucet_address`, `faucet_allocation`, and both referrer bps rates.
Mainnet needs a new genesis with `is_testnet = false`, a distinct `chain_id`, and the KMS-backed
mint authority from A1. `faucet_allocation` must be `0`, as it now is on stage.

**Verification:** the mainnet genesis parameters are reviewed and recorded before first boot, all
nodes report the same genesis hash, and a node configured with a nonzero faucet allocation refuses
to start.

### C2. Validator set — **Blocker**

Aura is an authority round-robin, so the validator set is permissioned by construction. Stage runs
three authorities that are three containers in one compose project on one VPS. That is a single
point of failure and a single point of control.

Two couplings found while reviewing this on 2026-09-11, neither of them documented anywhere before:

- **The authority count sets the block cadence.** `step_duration = 60 / authorities.len()` seconds,
  so the current three authorities own 20-second slots. Changing the set size changes that cadence,
  which makes "add a validator" a consensus-timing change rather than a roster edit. Note that the
  README's and marketing site's "~1s blocks" describes transaction inclusion latency, not the slot
  duration.
- **There is a hard ceiling of 60 authorities**, and past it the node used to boot fine and then
  panic on its first slot calculation, because the step duration truncated to zero. An empty set
  divided by zero at construction. Both are now refused at startup with a message naming the reason
  (`clutch-node` `ff53d5c`), along with a duplicate authority, which silently took two slots per
  round.

**Verification:** authorities run on hosts that do not share an operator, a provider, or a power
supply, each with its own key, and the network has been observed continuing to produce blocks with
one authority stopped. Decide the mainnet set size deliberately, since it picks the block cadence.

### C3. Key rotation path — **Required** (premise corrected 2026-09-11)

An earlier version of this item said rotation was hard because consensus parameters are
genesis-committed. That was wrong, and worth correcting because it made the job look bigger than
it is. `ChainInit` — the genesis-committed set — carries `chain_id`, `is_testnet`, `tx_fee`, both
referrer bps rates, `mint_authority`, `faucet_address` and `faucet_allocation`. It does **not**
carry the authority set, which is per-node config read at startup. So rotating an authority key
needs no chain reset.

The real hazard is different: `authorities[slot % len]` means the slot-to-author mapping depends on
both the order and the length of that list. A node with a stale list rejects blocks from a new
authority and expects blocks from a departed one, so the set has to change on every node together.
Rotation is therefore a coordinated restart, not a rolling one.

The procedure is now written: `clutch-deploy/docs/AUTHORITY-ROTATION.md`. It covers the same-size
key replacement and the size change separately, insists on one variable at a time, and names the
60-authority ceiling. Its rehearsal section includes rehearsing the *failure* — change the list on
one node only and watch it reject blocks — because knowing what that looks like in the logs is the
point of rehearsing at all.

**Verification:** the rehearsal performed on a throwaway network, with the date recorded in that
document. Writing it down is not the same as having done it once.

---

## D. Data durability and recovery

### D1. Off-host ledger backup — **Blocker** (tooling done 2026-09-11)

Both databases sit on named local Docker volumes with `restart: unless-stopped`, so data survives
container recreation. It does not survive disk loss, host loss, or `docker compose down -v`. The
treasury ledger is the record of who deposited what and which mints were approved; losing it means
losing the ability to honour redemptions, and the chain does not carry the off-chain half.

Built in `clutch-deploy` — see [`docs/BACKUP-RESTORE.md`](https://github.com/clutchprotocol/clutch-deploy/blob/main/docs/BACKUP-RESTORE.md):

- `scripts/backup-treasury-db.sh` dumps both databases in Postgres custom format, pipes straight
  into `openssl enc` so the plaintext never touches disk, writes mode 600 into a mode-700
  gitignored directory, optionally pushes off host with `rclone`, and prunes to 14.
- `scripts/restore-treasury-db.sh` restores into `<db>_restore_<stamp>` and cannot target a live
  database. It prints row counts, because a restore that loads cleanly and is empty is the failure
  the rehearsal exists to catch.
- `.github/workflows/backup-treasury-db.yml` runs it daily at 03:17 UTC and on dispatch.

Two guards in the dump path are the ones that matter: `pipefail`, so a failing `pg_dump` cannot
leave a valid encryption of a truncated dump, and a 1 KB size floor. Both of those failures look
exactly like a good backup otherwise. The script also refuses to run without `BACKUP_PASSPHRASE`.

**Still open, and this is the whole point of the item:**

1. **`BACKUP_REMOTE` is not set on the host**, so today's dumps share a disk with the databases
   they came from. The script warns about this on every run. Set it to an rclone destination.
2. **`BACKUP_PASSPHRASE` must be stored somewhere that is not the host.** A passphrase next to the
   dump it protects is decoration.
3. **No restore has been performed.** A dump nobody has restored is a hypothesis.

**Verification:** the rehearsal in the runbook, performed — restore into a clean database, then
point a `treasury-service` instance at it and get reconciliation green *against the restored
ledger*. That is the verification; a loadable dump is not. Record the date here when done.

### D2. Plaintext mnemonic copies on the host — **Blocker** (mitigated 2026-09-10)

`provision-treasury-secrets.sh` and `set-mint-caps.sh` each copied `.env` to a timestamped
`.env.bak.*` before editing. `.env` holds `DEPOSIT_MNEMONIC`, so every run left another plaintext
copy of the mnemonic on the host, with no upper bound on the count.

Done on 2026-09-10, in `clutch-deploy`:

- Both scripts now keep exactly one `.env.bak`, overwritten per run, and delete the timestamped
  pile earlier runs left. The copy happens before the delete, so a failed copy cannot leave
  nothing recoverable.
- `.env.bak` and `.env.bak.*` are gitignored. They were not, so `.env` was protected from being
  committed while its backups were not.
- **The host is not clean until one of those workflows runs again.** Re-dispatching *Provision
  treasury secrets (stage)* sweeps it; that script writes a variable only if it is absent and can
  never overwrite `DEPOSIT_MNEMONIC`, so re-running it is safe by construction.

Still open, which is why this stays a blocker: the remaining backup is plaintext, and so is `.env`
itself. Encrypting the backup only moves the problem while the source file is readable.

**Verification:** A1 and A2 land and the mnemonic is not in `.env` at all. Until then, confirm with
`inspect-stage.yml` that exactly one `.env.bak` exists on the host and no timestamped copies remain.

### D3. Reconciliation runs unattended and alerts — **Required** (rules done 2026-09-11)

Reconciliation already ran unattended: a worker loop on `reconciliation_interval_secs`, with a
short retry on failure rather than the full interval, and a mismatch already called
`ledger::alert`, which logs at error level and writes an `alerts` row that `metrics.rs` gauges.

What did not exist was anything that **evaluated** any of it. Ten Prometheus alerting rules now
do, in `clutch-deploy` (`config/monitoring/prometheus/rules/treasury.yml`, `db1bd1a`): reconciliation
mismatch and staleness, a latched breaker, p1s from either service, either service failing scrapes,
sweeping stalled, a stuck chain outbox, and deposit polling stalled or never having run. Verified
loaded on stage — all ten report `health: ok` — via the `metrics` probe.

**Still open, and it is the part that matters:** nothing delivers them. The rules fire into
Prometheus and Grafana and stop there. `clutch-deploy/docs/ALERTING.md` sets out the two routes
(Grafana contact points, or Alertmanager) and what closes the item.

**Verification:** a delivery route, tested by forcing a failure. Stopping `treasury-service` for
four minutes and confirming `TreasuryServiceDown` reaches a human is the cheapest forcing function.
An untested route is in exactly the state the metrics were in before these rules existed.

:::warning Found while verifying this
Prometheus was in state `created` — created and never started, no logs, no restarts — so **stage
had no monitoring at all** for some window before 2026-09-11, and nothing said so. Three wrong
diagnoses preceded finding it, because the `metrics` probe's Prometheus queries had *also* been
broken (the `prom/prometheus` image dropped `wget`) and the probe reported "could not query
Prometheus" for both causes identically. A clean deploy restored it.

Two lessons went into the probe rather than into this document: it now prints Prometheus's own
container state and last log lines before querying anything, and it no longer sends its own stderr
to `/dev/null`. A probe whose failure output cannot distinguish "I could not ask" from "the answer
is empty" is the same defect it exists to catch, and its output gets read as evidence.

This is also an argument for finishing the delivery route above rather than trusting dashboards:
a dashboard nobody is looking at and a Prometheus that is not running look identical.
:::

---

## E. Abuse and rate controls

### E1. Rate limiting — **Required** (both services done; edge and load test open)

Nothing had rate limiting. The deposit-address endpoint, the redemption endpoints, and
`generateToken` are all reachable by anyone with a keypair, and keypairs are free. The per-address
polling budget makes address enumeration a cost the treasury carries rather than the caller.

Done in `payment-orchestrator`: a fixed-window limiter keyed on the authenticated `pk`, checked
immediately after `authenticated_pk` in both POST handlers, returning 429. Default 10 per minute,
`APP_RATE_LIMIT_PER_MINUTE` to override. Standard library only, capped at 10,000 tracked
identities, evicting the entry closest to expiry when full rather than refusing a new identity —
refusing would let an identity-churning attacker lock out real users. Five tests, verified by name
in CI.

Done in `clutch-hub-api`: `generateToken` is bounded twice before any cryptography, since it is
the only mutation not behind `AuthGuard` and therefore the only one that does secp256k1 recovery
before it knows whether the caller is anybody. Globally at 120 per minute, and at 10 per minute
per claimed `publicKey`. Both bounds are needed: `publicKey` is a caller-supplied string, so the
per-key limit alone is bypassed by varying it, and only the global cap actually bounds recovery
work. Six tests, verified by name in CI — which that repo did not have until this change, since
its image build never compiles `cfg(test)` code or runs a test.

Not keyed on client IP anywhere, deliberately. Behind Cloudflare and nginx the peer address is a
proxy, so an IP bound would have to trust a forwarded header, and a spoofable one lets an attacker
both mint unlimited buckets and lock a chosen victim out of logging in. Source limiting belongs at
the edge, where the hop is actually known — which makes it partly an item for G1, since the live
edge config is not owned by a repo today.

Reviewed and deliberately NOT done in `treasury-service`, on 2026-09-10. Rejecting an
unauthenticated request there costs a header parse and three fixed-length byte comparisons, with no
cryptography, so an unauthenticated flood is an ordinary HTTP flood that belongs to the edge rather
than to a per-identity limiter. There are only three identities in the first place — one token per
role — so keying a limiter on them bounds nothing useful, and a caller holding a role token is
already bounded by four-eyes approval, a per-transaction cap, a daily cap, and the halt breaker.
Adding a limiter there would be code with no threat behind it. Revisit if a per-person token scheme
ever replaces the three shared ones.

The token comparison itself was hardened in the same pass, since it was the thing actually worth
fixing: `==` on `&str` short-circuits at the first differing byte, so its timing was a function of
how much of the token a caller had already guessed. It is now a constant-time comparison over equal
lengths. A weak oracle against a long random token, but these three tokens gate the mint ledger.

Still open:

- **Source limiting at the edge**, per the paragraph above.
- **The load test.** Every limit here is argued for, not yet measured under load, and the numbers
  are guesses in the honest sense: chosen well above observed client behaviour and well below what
  a flood needs to hurt, with nothing between those two bounds measured.

**Verification:** source limiting at the edge, and a load test showing every limit holds with the
chosen numbers recorded here.

### E2. Deposit-address growth is bounded — **Measured 2026-09-11; ceiling is a decision**

Addresses are permanent and never stop being watched, so the set only grows. Cost per pass is
constant because `due_addresses` rotates through a fixed budget, oldest-checked-first — which means
growth shows up as *latency*, not as load. The relationship is exact rather than empirical:

> worst-case cold detection latency = ceil(addresses / `MAX_ADDRESSES_PER_PASS`) × `poll_interval_secs`

With the deployed values (`MAX_ADDRESSES_PER_PASS = 50`, a compile-time constant in `poller.rs`;
`poll_interval_secs = 30`):

| Addresses handed out | Worst-case cold latency |
|---|---|
| 50 | 30 seconds |
| 500 | 5 minutes |
| 3,000 | 30 minutes |
| 6,000 | 1 hour |
| 12,000 | 2 hours |

This is the *cold* path only. Opening the deposit panel marks an address hot for 24 hours and hot
addresses are polled first, so anyone actually in the act of depositing is unaffected. The cold
number is what governs a payment to an address whose owner has not looked at the panel recently.

**This ties directly to the alert added in D3.** `OrchestratorPollingStalled` fires when the oldest
poll age exceeds one hour, so at roughly **6,000 addresses the alert becomes a false positive** — it
would fire continuously against a perfectly healthy rotation. The alert is therefore also the
tripwire for this ceiling, which is a better arrangement than a separate threshold nobody maintains,
but it means the two numbers have to move together.

Headroom exists if the ceiling needs raising: 50 addresses per 30 seconds is about 1.7 requests per
second at TronGrid, well inside a keyed tier. `MAX_ADDRESSES_PER_PASS` is a constant rather than
config, so raising it is a code change and a release — deliberate, since it is also the thing
protecting an unkeyed endpoint from being hammered into throttling, a failure that already cost a
day of debugging once and which looks exactly like "nobody is paying".

**The decision left:** accept ~6,000 as the ceiling and treat the D3 alert as the signal to revisit,
or pick a higher target now and move `MAX_ADDRESSES_PER_PASS`, `poll_interval_secs` and the alert
threshold together. Record whichever here.

---

## F. Client-side key handling

### F1. Demo app key storage — **Second branch satisfied 2026-09-11**

The reference app generates or imports keys in the browser and stores them in plaintext
`localStorage` under `clutch_{passenger|driver}_privateKey`. Any XSS, any malicious extension, or a
shared machine is total loss of that user's funds. This does not block a mainnet chain, but it
blocks shipping this app to real users, and it is the app people copy.

This item offered two branches. The second is now done (`clutch-hub-demo-app` `feat/key-storage-notice`,
live on stage): a non-dismissible notice at wallet setup, before the key exists, which is the only
moment a warning can change what someone does. It says the key is plaintext in this browser and
readable by any script, extension or other user of the machine; that clearing site data destroys it
irrecoverably; and never to put real funds behind it. It also addresses builders rather than riders:
the SDK signs locally, so a hardware wallet, an OS keychain or an external signer substitutes in
without changing how transactions are built. The README's storage section carried the same gap plus
stale key names and a "remember keys" option that does not exist; both are fixed.

There is no dismiss button on purpose. A remembered dismissal hides the warning from exactly the
person who arrives on a shared machine later.

**The first branch — an actual key boundary — remains the better answer** and is required before any
app built on this pattern holds real funds. Nothing about the disclosure makes plaintext storage
safe; it makes it *known*, which is the most that documentation can do.

**Verification:** met as written. Re-open as a blocker against any deployment that intends real
funds through this app, where only the first branch counts.

### F2. SDK pin in the demo app's production build — **Closed 2026-09-10**

`package.prod.json` pinned `clutch-hub-sdk-js` at `^1.15.0`, which cannot resolve to any 4.x
release, and 3.0.0 changed the signed wire format — so a production build from that file would have
produced transactions the node rejects.

Re-pinning was the obvious fix until reading what the pin fed. Every part of that path was broken
independently of the version: `build:prod` ran `npm install --production` and then `vite build`,
and vite is a devDependency, so the first half removed what the second half invoked;
`deploy-prod.ps1` deleted `package-lock.json` and then ran `npm ci`, which requires one, and
switched to a `package.json` with no build script before running `npm run build`; and
`restore-dev.ps1` depended on a file only the broken script created, which was neither tracked nor
ignored. Nothing referenced any of it — not the Dockerfile, not the image workflow, not
`clutch-deploy` — and there is no production demo app deployed for it to target.

So it was deleted rather than given a second pin to go stale (`clutch-hub-demo-app` `cec9151`). The
real production path is the image, which copies both repos, builds the SDK from source and runs
`npm run build`, making an image a snapshot of two checkouts rather than of a version range.

---

## G. Infrastructure and deploy

### G1. nginx ownership — **Required**

The nginx serving stage belongs to the `v2ray` compose project, not `clutch-deploy`. It mounts a
hand-maintained config that the deploy script patches in place, so the checked-in copy has already
drifted from the host. Editing `clutch-deploy/config/nginx/*.conf` changes nothing on stage. A money
system should not have its edge config in a file no repo owns.

**Verification:** the mainnet edge config lives in a repo, is deployed from it, and the live config
read back from the host matches the checked-in one byte for byte.

### G2. Single host — **Required**

Everything runs on one VPS: nodes, Hub API, treasury services, both databases. That host is a
single point of failure for consensus, custody, and the ledger simultaneously.

**Verification:** a topology where losing any one host loses neither block production nor the
ledger, and a documented recovery time for each component.

### G3. Someone else can operate it — **Blocker**

`treasury-service` has a manual halt (`minting_halted` in `breaker_state`) and a daily mint cap.
That machinery is worth little if one person knows it exists. A money system needs a second person
who can stop it.

**Verification:** a named second operator with access, and a rehearsal in which that person halts
minting and resumes it without the maintainer's help.

---

## H. Market operations

These are product gaps, already listed honestly in the org README. They do not endanger the
reserve; they decide whether a real ride market is usable.

### H1. Dispute resolution — **Blocker for a public launch** (disclosure half done 2026-09-11)

Cancellations are on-chain, but there is no arbitration when two parties disagree, and no no-show
or fraud handling. Passengers also give up card-issuer chargebacks by signing payment directly,
which is a fair trade for instant settlement on a testnet and a serious gap with real money: the
passenger would have no recourse at all.

This item had two halves, and one was purely a disclosure failure. That half is done
(`clutch-hub-demo-app` `feat/state-the-recourse`): a note now sits with the offers, immediately
above the Accept buttons, saying that accepting holds the full fare on chain at once, that the
passenger signs the payment themselves so no issuer can reverse it and there is no arbitration if
the two sides disagree, and that either side can cancel before the fare is fully paid with the
unpaid part returning to the passenger. That last part is the recourse that *does* exist, and it was
as undocumented as the parts that do not.

It is inline rather than a confirmation modal on purpose. For play money, a dialog demanding
acknowledgement of "you have no recourse" is theatre, and it trains people to click through exactly
the dialog that would matter on a real deployment. **A real deployment needs acknowledgement rather
than display** — a material term of the transaction, acknowledged once per account rather than
displayed and scrolled past.

**The blocking half is untouched: a dispute mechanism still has to exist.** It is a design problem
before it is an implementation one, and the design is not settled. The questions it has to answer,
none of which have answers yet:

- Who decides? An operator role is a trusted third party, which is the thing this architecture
  removed. A staked juror set is a whole second protocol. A time-locked default (say, funds release
  to the driver unless the passenger objects within N blocks) needs no arbiter but rewards whoever
  is more patient.
- What can a decision *do*? A burn is irreversible and a `RidePay` is final, so any remedy has to
  be a new transaction rather than an undo — which means either an escrow window before settlement
  or a separate insurance pool, and the first contradicts "paid in seconds".
- What stops the mechanism itself being the attack? A free dispute that freezes a driver's payout
  is a denial-of-service against drivers.

**Verification:** a dispute mechanism specified, implemented and documented, with the passenger's
recourse acknowledged rather than merely shown. The disclosure above is a floor, not the fix.

### H2. Reputation — **Required**

No driver or rider scoring exists, so nothing distinguishes a first-time counterparty from a
repeatedly bad one.

**Verification:** a reputation design that does not become a new attack surface, implemented.

### H3. Matching — **Recommended**

Matching is simple, with no surge, pricing engine, or geospatial optimisation. Acceptable at small
scale; it becomes an operational problem before it becomes a technical one.

**Verification:** a recorded decision on the ride volume or city count past which this needs work,
so the limit is chosen rather than discovered.

---

## I. External review

### I1. Security audit — **Blocker**

No audit is referenced in any repo. The roadmap lists "audited crypto" as unstarted. The areas that
most need external eyes: the signing and encoding path, the four-eyes mint flow, the payout
endpoint's bounds, and the reconciliation arithmetic.

**Verification:** an audit report from an external firm, its findings triaged, and every critical
and high finding either fixed or accepted in writing.

### I2. Test coverage where money moves — **Verification met 2026-09-11**

This item was written from test *counts*, which understated what exists. Read against the actual
suites, the verification it asked for is already satisfied.

**The payout path**, including the branch this item singled out. `db_redemption.rs` alone carries 24
tests, and the failure branches are covered individually rather than as a group:
`an_ambiguous_payout_is_never_retried`, `a_refusal_returns_the_intent_for_retry`,
`a_paid_reply_without_a_tx_id_is_ambiguous_not_paid`, `a_500_is_ambiguous_not_refused`,
`an_unrecognised_status_is_ambiguous`, `a_400_is_refused_because_the_signer_rejected_the_shape`,
`a_reverted_payout_tx_is_never_paid_and_alerts_a_human_instead`,
`mismatched_burn_fails_intent_never_pays`, and three separate daily-cap branches. The
Refused-versus-Ambiguous distinction is what decides whether a burn can be paid twice, and every arm
of it has its own test.

**Mint, sweep and reconciliation**: `db_tron_verifier.rs` 24, `db_sweeper.rs` 10,
`db_reconciliation.rs` 11, `db_breakers.rs` 8, `db_outbox.rs` 5 — including an over-cap intent
routing to `needs_manual` rather than burning ten retries on something no retry can fix.

**The arithmetic itself**, in `clutch-node`, is property-tested rather than example-tested:
`fee_split_sums_exactly` asserts the three shares sum to the fare across *all* fares and both bps
rates, and `floor_fee_bounded_by_fare` bounds the floor. Exactly-once refs are covered in both
directions by `first_duplicate_ref_spans_mint_and_burn` and `keeps_every_ref_less_burn`.

**What is actually thin**, and it is not what the counts suggested:

- **`clutch-hub-sdk-js`**, at 6 tests, all added 2026-09-10. It builds and signs what users
  authorise, so a defect there is a wrong transaction carrying a valid signature. It had no test CI
  before this week either.
- **`clutch-hub-demo-app`**, at zero. It is the reference every builder copies.
- Neither moves money server-side — the Hub forwards signed transactions and cannot alter one
  without invalidating the signature — which is why they rank below the paths above rather than
  above them.

**Verification:** met for the server-side money paths. Re-open against the SDK if it grows beyond
transaction construction, and treat F1 as the demo app's real risk rather than its test count.
---

## J. Legal and regulatory

### J1. Get advice before accepting a dollar — **Blocker**

Not engineering, and not something anyone on this repo can sign off. A fully-reserved token that is
redeemable for USDT, issued and custodied by an identifiable operator, is money transmission or
e-money in most jurisdictions, with registration, KYC, AML, safeguarding, and reporting
consequences. The reserve model being honest does not exempt it.

**Verification:** written advice from a qualified lawyer in the operating jurisdiction, and whatever
registrations that advice names, in place.

---

## What already holds

Worth stating so nobody rebuilds it or assumes the whole system is provisional. These are the parts
that were designed for this and appear sound:

- **The mnemonic is only in `tron-signer`.** `payment-orchestrator` holds the account xpub, which
  derives receive addresses and cannot spend. Owning the orchestrator does not move a deposit.
- **The sweep endpoint takes an index and nothing else.** No destination, amount, or contract
  parameter, so it cannot be turned into a payout path.
- **Custody is unreachable from code.** The payout float is a separate derivation path, so the worst
  case for a compromised treasury service is the float balance, not the reserve.
- **Redemptions are bounded twice, in services that do not share the value**, so a request the
  signer would refuse cannot become a burn nobody can pay.
- **Ambiguous payouts stop rather than retry.** Only a reply that proves nothing was broadcast
  returns a redemption to the queue; anything else pages a human. That trade accepts a stuck
  redemption to avoid a double payment, which is the right way round.
- **Mint has four-eyes approval, a per-transaction cap, a daily cap, and a manual halt.**
- **Genesis pre-mints nothing.** Every CLT that exists was minted against a verified deposit.
- **Keys never leave the client for ride transactions.** The Hub forwards signed RLP and cannot
  alter a transaction without the signature failing, and `verifyUnsignedTransaction` closes the
  blind-signing gap for every field a client can pin.

## Open questions for the maintainer

1. **What does mainnet mean here** — a persistent public testnet that stops being reset, a bounded
   pilot in one city with low caps, or a public launch? The blockers differ enormously.
2. **Who is the second operator?** G3 cannot be closed alone.
3. **Which jurisdiction** is the operating one, for J1.
4. **Is the reference app in scope** as a wallet real users hold funds in, or is it a demo that
   points at something else? F1 depends entirely on the answer.

---

*Maintained by hand. Update the status line and the dated measurements when anything here closes;
an item is closed by its verification artefact, not by intent.*
