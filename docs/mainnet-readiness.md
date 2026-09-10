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

### A5. Sweep signer review — **Required**

`SweepSigner` (`crates/treasury-service/src/sweeper.rs`) moves deposits into custody. The sweep API
deliberately takes an index and nothing else, so owning the orchestrator cannot redirect a deposit.
That property must be re-confirmed against the mainnet configuration, since it is the reason the
sweep path is not a second payout path.

**Verification:** a reviewer confirms the mainnet sweep endpoint still takes no destination,
amount, or contract parameter, and that its destination comes only from `tron-signer` config.

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

### B4. Mainnet caps set deliberately — **Blocker**

The stage caps were sized for test money. Mainnet caps are the loss ceiling for every failure mode
above, so they are the last line of defence and must be chosen on purpose.

**Verification:** each cap has a written rationale naming the worst case it bounds, and the pair of
redemption bounds is still aligned across the two services that enforce them.

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

**Verification:** authorities run on hosts that do not share an operator, a provider, or a power
supply, each with its own key, and the network has been observed continuing to produce blocks with
one authority stopped.

### C3. Key rotation path — **Required**

Consensus parameters are genesis-committed and peers compare the genesis hash at handshake, so
rotating an authority key is not a config edit. The procedure needs to exist before it is needed.

**Verification:** a written rotation procedure, rehearsed on a throwaway network.

---

## D. Data durability and recovery

### D1. Off-host ledger backup — **Blocker**

Both databases sit on named local Docker volumes with `restart: unless-stopped`, so data survives
container recreation. It does not survive disk loss, host loss, or `docker compose down -v`. The
treasury ledger is the record of who deposited what and which mints were approved; losing it means
losing the ability to honour redemptions, and the chain does not carry the off-chain half. There is
no `pg_dump` schedule, no off-host copy, and no restore procedure anywhere in `clutch-deploy`.

**Verification:** encrypted off-host backups on a schedule, with a restore performed into a clean
environment and reconciliation run green against it. The restore is what closes this, not the
backup job.

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

### D3. Reconciliation runs unattended and alerts — **Required**

Reconciliation reads `ok` whenever reserve covers liability, and a reserve below liability is the
one condition that halts minting. That check needs to run on a schedule and page someone, not be
run by hand.

**Verification:** a scheduled reconciliation with an alert route that has been tested by forcing a
failure.

---

## E. Abuse and rate controls

### E1. Rate limiting — **Required** (orchestrator done 2026-09-10)

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

### E2. Deposit-address growth is bounded — **Recommended**

Addresses are permanent and polled on a rotation with a fixed per-pass budget. Detection latency
grows with the number of addresses handed out, and addresses can be requested for free.

**Verification:** a measured relationship between address count and worst-case detection latency,
and a decision on the acceptable ceiling.

---

## F. Client-side key handling

### F1. Demo app key storage — **Blocker for any app handling real funds**

The reference app generates or imports keys and stores them in plaintext `localStorage` under
`clutch_{passenger|driver}_privateKey`. Its own notes call this demo-grade and say not to make it a
real wallet without discussion. Any XSS, any malicious extension, or a shared machine is total loss
of that user's funds. This does not block a mainnet chain, but it blocks shipping this app to
real users, and it is the app people will copy.

**Verification:** either the reference app moves to a real key boundary (hardware wallet, OS
keychain, or an external signer), or it carries an unmissable warning and is not presented as the
way to hold real CLT.

### F2. SDK pin in the demo app's production build — **Required**

`package.prod.json` pins `clutch-hub-sdk-js` at `^1.15.0`, which cannot resolve to any 4.x release.
Since 3.0.0 changed the signed wire format, a production build from that file would produce
transactions the node rejects. Tracked separately; listed here because a broken prod build path is
not something to discover during a launch.

**Verification:** the pin matches the SDK the node accepts, or the prod build path is deleted.

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

### H1. Dispute resolution — **Blocker for a public launch**

Cancellations are on-chain, but there is no arbitration when two parties disagree and no no-show or
fraud handling. Passengers also give up card-issuer chargebacks by signing payment directly. With
test money that is a design conversation; with real money the passenger has no recourse at all.

**Verification:** a dispute mechanism specified, implemented, and documented, with the passenger's
recourse stated plainly in the app before they pay.

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

### I2. Test coverage where money moves — **Required**

Counts today: 276 tests in `clutch-treasury`, 99 in `clutch-node`, 20 in `clutch-hub-api`. The SDK
got its first two test files on 2026-09-10. The treasury is the best covered, which is the right
priority; the Hub API is the thinnest and sits on the path every user takes.

**Verification:** the mint, burn, sweep, and payout paths each have tests covering the failure
branches, not just the happy path, and the ambiguous-payout branch that pages a human is tested.

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
