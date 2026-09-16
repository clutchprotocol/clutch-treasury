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
seven items are generalized or omitted there because publishing them verbatim would be targeting
information rather than honest disclosure:

| Item | Why it is not public verbatim |
|------|------------------------------|
| D2 | Names the scripts and file pattern under which mnemonic copies accumulate on the host |
| E1 | States that no rate limiting exists and which endpoints are reachable |
| G1 | States that the live edge config is drifted and owned by no repo |
| G2 | States that consensus, custody, and both databases share one host |
| G3 | States that only one person can halt minting |
| G4 | Closed the day it was found, and the public page has no infrastructure section for it to sit in. The generic lesson — a green deploy is not evidence the deploy shipped anything — is worth more as a blog post than as a line on a readiness page |
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
4. ~~The treasury ledger has an off-host backup and a restore that has actually been performed.~~
   **Met 2026-09-15** — see D1. The remaining four are unchanged.
5. Someone other than the maintainer can halt minting and knows how.

Everything below expands these, plus the product and legal work that sits outside them.

## What to do next, in order

Several of these block each other, so the order is not a preference. Everything below needs a
person, an account, a host, a firm or a lawyer — the engineering side of each item is done and
recorded in its own section.

~~**Start here, because nothing depends on it and its failure mode is the worst.**~~

1. ~~**Perform the D1 restore rehearsal.**~~ **Done 2026-09-15.** Off-host backups run to
   Cloudflare R2 nightly, and a dump fetched back out of the remote restores and reconciles `ok`.
   Re-run `rehearse-restore.yml` with `source: remote` after anything that touches that path.

   It also found a real 10,000,000 CLT discrepancy that had been raising a p1 since 2026-09-14 —
   a mint stuck in `submitted` that nothing re-drives — which is the argument for the item rather
   than an aside. See D1.
2. ~~**Force a failure (D3).**~~ **Done 2026-09-17.** Alertmanager delivers to Telegram, and
   `TreasuryServiceDown` was watched firing, routing and arriving during a real four-minute outage.
   D3 and D4 are both closed.

   This unblocks the caps decision below: the daily mint cap's exposure is that cap multiplied by
   how long a compromise runs unnoticed, and that is now minutes rather than however long until
   someone next looked.
3. **Name a second operator and rehearse a halt (G3).** One person knowing the breaker exists is
   not a control. Everything they need now exists — a halt workflow and `docs/ON-CALL.md` — so what
   is left is a conversation and one rehearsal, not a task.

**Then the chain of things that unblock each other.**

4. **Provision key custody for the three mint keys** (A, B and C — see 12b and
   `docs/KEY-CEREMONY.md`). AWS is declined as a provider (maintainer, 2026-09-11), so the
   requirement is stated by property rather than by vendor: a key that signs **secp256k1**, whose
   material cannot be exported, and whose deletion is not grantable to the principal that signs
   with it.

   Options that meet it, cheapest first. Azure Key Vault software-protected keys use curve
   `P-256K` with algorithm `ES256K`, no per-key monthly charge and transaction-priced. Google Cloud
   KMS software keys use `EC_SIGN_SECP256K1_SHA256` at roughly $0.06 per key per month. Verify
   curve support and pricing before committing; both were quoted from memory. A self-hosted signer
   on a host that is not the application host is the no-vendor option — weaker, since it does not
   give non-exportability, but it still means compromising the web stack does not yield the key.

   Key C is offline and needs no provider at all.

   This unblocks the signer API call (A1, A2), which unblocks the key ceremony (A3 — it needs two
   people, so it also needs step 3), which unblocks the mainnet genesis (C1), and it is what
   finally closes D2 by taking the mnemonic out of `.env` entirely.

   `external_signature.rs` is deliberately vendor-neutral: it turns a DER `(r, s)` from any
   external signer into the `(r, s, v)` the node needs, so only the API call itself changes with
   the provider.
5. **Decide the mainnet validator set (C2).** Hosts that share no operator, provider or power
   supply. Its *size* picks the block cadence at `60 / len`, so decide the number deliberately, and
   it is the other value C1 is waiting on.
6. **Commission the audit (I1).** Long lead time, so start it while the above is in flight rather
   than after. Send `docs/AUDIT-BRIEF.md`, which is written to be handed over as-is and states the
   seven invariants a finding should be measured against.
7. **Get legal advice (J1).** Also long lead time, and it can invalidate assumptions underneath
   everything else, so it is cheaper early than late. Send `docs/LEGAL-BRIEF.md` — facts and
   questions, no analysis, so counsel is not billing to discover the architecture.

**Then the decisions that only need someone to make them.**

8. ~~**Set the mainnet caps (B4).**~~ **Decided 2026-09-17**, in the same sitting as the alert route
   from step 2, which is what the daily mint cap depended on. The relationships are checked
   mechanically; applying them needs a mainnet host, so B4 now waits on C1 rather than on anyone.
9. **Pick the deposit-address ceiling (E2).** Accept ~6,000 with the D3 alert as the tripwire, or
   raise `MAX_ADDRESSES_PER_PASS`, `poll_interval_secs` and that alert threshold together.
10. **Choose how the edge config gets an owner (G1).** An include of a directory `clutch-deploy`
    owns, or Clutch taking port 80. Host reorganisation either way.

**Then the two that are genuinely unfinished design work, not configuration.**

11. **Decide on the dispute mechanism (H1).** A proposal now exists
    (`docs/superpowers/specs/2026-09-11-dispute-resolution-design.md`) with a recommendation, so
    this is a review rather than a design exercise. **One decision inside it has to happen before
    step 13**, not after: if the release window is a consensus parameter it belongs in `ChainInit`,
    and genesis-committed values cannot be added later without a new chain. Settle that before the
    mainnet genesis is fixed even if the rest of the mechanism ships later.
12. **Decide on reputation (H2).** A proposal exists
    (`docs/superpowers/specs/2026-09-11-reputation-design.md`) recommending it be derived off chain
    from public history rather than put into consensus. Read it *after* step 11: H1's auto-release
    removes most of the incentive reputation guards against, which may make the light version
    sufficient.

**And one decision that has to be made before the genesis, not after.**

12b. **~~Choose N and the mint threshold (A1).~~ Decided 2026-09-12: a 2-of-3.** What remains is
    generating the three keys in the ceremony (A3), in the three separate locations named in
    `docs/KEY-CEREMONY.md`. Their addresses are genesis-committed, so all three must exist and be
    tested before step 13.

**Last, and only after all of the above.**

13. **Boot the mainnet genesis and read the first payout receipt (C1, B1, B2).** Check step 11's
    consensus-parameter question is answered before this, since the genesis cannot be amended. The first mainnet
    payout is the first real test of the energy model, because Nile cannot show it. Re-measure the
    redemption fee from that receipt rather than scaling the testnet number.

Nothing in this list is blocked on further engineering. Where an item still needs code — the KMS
API call, a dispute mechanism — that is named in its own section along with what has to exist first.

---

## A. Key custody and signing

The named blocker, already tracked in [`keys.md`](keys.md).

### A1. Mint authority custody — **Blocker** (M-of-N shipped 2026-09-11; KMS hard half done)

The mint authority is an environment variable (`ChainSigner` / `EnvKeySigner`,
`crates/clutch-chain/src/signer.rs`). It is the only key that can create CLT, so a host compromise
is unbounded issuance against a fixed reserve. `keys.md` names the replacement: a `KmsSigner` on
AWS KMS `ECC_SECG_P256K1`, following the `alloy-signer-aws` pattern.

**The part that was hard is now done and verified**: `crates/clutch-chain/src/external_signature.rs`,
eight tests, no AWS dependency. An external signer returns a DER `(r, s)` and nothing else, while
the node needs `(r, s, v)` in low-s form because it identifies a signer by *recovering* the public
key. Neither the recovery id nor the normalisation comes back from KMS, and getting either wrong
produces a signature that is valid ECDSA and recovers to the **wrong address** — rejected as an
unauthorised mint, at the worst possible moment rather than an obvious one.

- `digest_for_hash_hex` fixes the convention in one place, so a signer cannot quietly disagree with
  `EnvKeySigner` about what is being signed. It is Keccak-256 of the hex *string's* UTF-8 bytes, not
  of the bytes that hex encodes, and a test pins that the two differ.
- `address_from_uncompressed`, with a test that it agrees with `EnvKeySigner` for the same key.
- `recoverable_from_der` normalises `s` and then **finds** the recovery id by trying both and
  keeping the one that recovers to the signer's own key. A search rather than a calculation: it
  cannot be off by one, and it doubles as proof the signature came from the expected key. A
  signature from another key, and a digest other than the one signed, both fail there.

#### M-of-N minting, shipped 2026-09-11

Custody was only ever half the problem, and the smaller half. `Mint::verify_state` authorised
against **one** address, and every other control on minting — four-eyes approval, the
per-transaction cap, the daily cap, the halt breaker — lives off-chain in `treasury-service`. A
holder of that key submits a Mint straight to the node and not one of them runs. Non-exportable
custody lowers the probability of theft; it does nothing about the consequence.

The chain now supports M-of-N. `ChainInit` gained `mint_cosigners` and `mint_threshold`; a Mint is
authorised when submitted by one member of the set and carrying `threshold - 1` further approval
signatures from distinct other members. Design:
[`2026-09-11-m-of-n-mint-authority-design.md`](https://github.com/clutchprotocol/clutch-node/blob/main/docs/superpowers/specs/2026-09-11-m-of-n-mint-authority-design.md).

- **clutch-node** ([#11](https://github.com/clutchprotocol/clutch-node/pull/11)), 25 tests. Cosigners
  sign a separate approval digest over `[chain_id, to, amount, credit_ref]` rather than the
  transaction hash, which covers the arguments they live in and so has no fixed point. Backward
  compatible: a single-signer chain encodes byte-identically to before, pinned by a test against a
  hand-built legacy encoding, so the running testnet keeps its genesis hash.
- **clutch-treasury**, 3 new tests. `treasury-service` never holds an approver key — the signature
  arrives already made on the approve call, is checked to recover to a configured authority, and is
  relayed. If this service could produce the second signature, the second signature would mean
  nothing. Migration 0013 enforces one signature per signer per intent by primary key, the same rule
  the node enforces, in both places.

**Decided 2026-09-12: a 2-of-3.** Three authorities, any two of which must sign. Recorded in
`docs/KEY-CEREMONY.md`, which now runs three times rather than once.

The placement is the security property, not the number. Three keys in one cloud account is a 2-of-3
on paper and a 1-of-1 in practice:

| Key | Where | Role |
|---|---|---|
| A | Cloud KMS, the account `treasury-service` can reach | Signs the transaction envelope; must be callable by the running service |
| B | A different provider or account, separate credentials | An attacker holding the service's cloud account still has one key |
| C | Offline, never on a networked machine | Cold spare, so losing A or B costs availability rather than the chain |

Routine minting is A plus B. C is the recovery path.

**Still open:** generating the three keys, which happens in the ceremony and needs two people, so
it depends on G3. Each address goes into the genesis configuration and cannot be corrected
afterwards, so all three must exist and be tested against a throwaway chain before the mainnet
genesis is committed.

One honest limit. If one person holds all three, this is multi-*place* control, not
multi-*person*: an attacker must breach two separate stores rather than read one file, which is
real, but a compromised operator still mints. G3 and the public disclosure stay necessary until a
second human holds one of these keys — and when one does, it should be B.

The strongest test is the equivalence — same key, same transaction hash, byte-identical `r`, `s`
and `v` to the in-process signer — plus a high-s input constructed as `n - s`, because KMS makes no
low-s promise and normalising flips which recovery id is correct.

**What remains is the API call, and only that.** `KMS_SIGNER_SHAPE` in that file documents it beside
the code it depends on, including the detail most likely to be got wrong: `MessageType` must be
`DIGEST`, not `RAW`, or KMS hashes the digest again and signs the wrong preimage. It cannot be
written here because there is no KMS to call, and an unexercised AWS code path in the mint flow is
worse than an absent one.

**Verification:** signing through a `KmsSigner` in the mainnet configuration, key material that has
never existed outside KMS, and `EnvKeySigner` unreachable in that configuration — a config that
would select it refuses to boot.

**Needed from the operator before that can be built:** an AWS account, a key created with
`KeySpec = ECC_SECG_P256K1` and `KeyUsage = SIGN_VERIFY`, and a key policy that does **not** grant
`kms:ScheduleKeyDeletion` to the signing principal.

**A real key is unavoidable — the emulator route is closed.** Checked on 2026-09-11: LocalStack's
KMS cannot create an `ECC_SECG_P256K1` key at all. `CreateKey` fails with
`Failed to generate key material: Curve not supported: secp256k1`
([localstack#11678](https://github.com/localstack/localstack/issues/11678)), and a related report
shows its `Sign` and `GetPublicKey` returning values that do not match for that curve even with
custom key material. So there is no way to exercise the AWS integration in CI, and writing it
against an emulator would in any case prove the wrong thing: an emulator-verified signer in the
mint path is precisely the false confidence this whole module was factored to avoid.

Recorded so nobody spends a day rediscovering it. The pure logic in
`external_signature.rs` is the answer to that constraint — it is the part that *can* be tested
without AWS, and it is tested.

### A2. KMS-backed payout signer — **Blocker** (same plumbing applies)

`PayoutSigner` (`crates/treasury-service/src/payout.rs`) is the matching seam for the payout key.
Today the payout float is derived from the deposit mnemonic at `m/44'/195'/0'/2/0` and held by
`tron-signer` as an environment variable. Exposure is bounded by the float balance and a
per-transaction cap, which is a real bound and the reason this is survivable on stage, but the key
is still a plaintext secret on a VPS.

The signature plumbing above is Ethereum-style secp256k1 and TRON uses the same curve and the same
recoverable-signature shape, so `external_signature.rs` covers this key too — with one difference
to check when it is wired: TRON's digest is a plain SHA-256 of the transaction's raw bytes, **not**
this stack's Keccak-over-hex-string convention, so `digest_for_hash_hex` is the wrong helper there.
`recoverable_from_der` and the recovery-id search apply unchanged.

**Verification:** payouts signed through KMS, and the float's key material never on disk. The
per-transaction cap and the float balance stay in place; the KMS boundary is in addition to them,
not a replacement for them.

### A3. Key ceremony and tested recovery — **Blocker** (procedure written 2026-09-11)

`keys.md` requires a real ceremony and tested recovery before any real-funds deployment. Neither
has happened, and neither could have: there was no written procedure, and you cannot hold a ceremony
you have not written down.

`docs/KEY-CEREMONY.md` is now that procedure. Writing it surfaced something worth stating, because
it changes what this item actually asks for:

**A KMS key has no key material to hold.** It is generated inside the HSM and cannot be exported —
that is the whole reason for using it. So the traditional centre of a ceremony, splitting and
escrowing a seed among custodians, does not apply and should not be simulated. What replaces it is
narrower and easier to get wrong: witnessing that the key was created with the *right configuration*
(a wrong `KeySpec` signs happily and produces signatures the node cannot verify, surfacing as a
rejected mint rather than an error at creation), recording the key's *identity*, and testing that
*access* recovers.

That last one is the step people skip and it is the one `keys.md` means by "tested recovery". A KMS
key with no exercised access-recovery path is exactly as lose-able as a seed phrase in one person's
drawer; the failure just arrives as an IAM misconfiguration rather than a house fire. The procedure
requires a second principal, in a separate identity, to actually sign from a machine that has never
held the first one's credentials — not merely to be configured.

Six steps, each with values to record, and deliberately only the last one irreversible: retiring
`APP_MINT_AUTHORITY_SECRET` from the host happens after everything else is proven. That step is also
what finally closes D2, since the mint secret and the deposit mnemonic leave `.env` together.

Two ordering constraints the procedure enforces: the mint key goes first and alone, end to end
including recovery, before the payout key is started — two ceremonies in one afternoon is how a step
gets skipped on the second. And it refuses to proceed with one person present, noting that this is
the same problem as G3 and is not solved by continuing anyway.

**Verification:** the ceremony performed, with the register it describes existing outside the AWS
account and holding the key ARN, the derived address, who was present, and the date recovery was
last exercised. The derived address is also the mainnet `mint_authority` in C1, so an error there
is baked into the genesis hash.

**Blocked on:** the AWS account and key from A1, and a second person — which is G3.

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

### B4. Mainnet caps set deliberately — **Blocker** (decided 2026-09-17; applies when a mainnet host exists)

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

#### The decided set — accepted by the maintainer, 2026-09-17

Each line names the assumption it rests on. Where an assumption turns out to be wrong, the number
moves with it rather than the table being rewritten.

| Cap | Stage | Mainnet | Why this number |
|---|---|---|---|
| `PER_TX_MINT_CAP_CLT` | $50 | **$1,000** | Has to clear the largest deposit anyone legitimately makes, or the rail simply refuses real users. Assumes nobody deposits more than $1,000 in one transfer at launch. A single amount-computation bug mints this much once. |
| `DAILY_MINT_CAP_CLT` | $500 | **$2,000** | A compromised mint authority, running until someone notices. **Read this as "how much unbacked CLT can I afford to buy back", because that is what it costs to keep the peg honest.** Two days unnoticed is $4,000. |
| `MAX_REDEMPTION_CLT` | $25 | **$200** | One payout out of the float. Must equal the row below. |
| `PER_TX_PAYOUT_CAP_USDT` | $25 | **$200** | Enforced independently in `tron-signer`. Equal by necessity, not by preference — a request between the two burns CLT that cannot then be paid. |
| Payout float balance | — | **~$1,000** | Not a configured cap, and the real bound on a compromised `treasury-service`: it cannot reach custody at all, so the float is the ceiling on theft. Five maximum redemptions deep; top it up rather than raising it. |
| Rolling 24h payout ceiling | above both | **$1,000** | The float, from the other side. A day that wants more than the float holds is a day to look at by hand. |
| `MIN_REDEMPTION_CLT` | $5 | **$25** | Set by the fee, not by product preference. At a $2 fee this is 8%, which is defensible; at $5 it would be 20%, which is not. |
| `REDEMPTION_FEE_USDT` | $1 | **blocked on B2** | Cannot be proposed. Nile sponsors its own energy, so every testnet payout reports zero fee whether or not delegation works. The first mainnet receipt is the first real measurement. |

**Checked mechanically, 2026-09-17**, not by hand. `check-cap-invariants.sh` now takes values from
the process environment as well as from `.env`, because the mainnet numbers are decided long before
there is a mainnet `.env` to put them in — and "we will check the relationships when we provision
it" is how a set gets provisioned unchecked:

```
PER_TX_MINT_CAP_CLT=1000000000 DAILY_MINT_CAP_CLT=2000000000   MAX_REDEMPTION_CLT=200000000 PER_TX_PAYOUT_CAP_USDT=200000000   MIN_REDEMPTION_CLT=25000000 bash scripts/check-cap-invariants.sh

OK    a single mint can clear both mint gates
OK    the two redemption bounds are aligned exactly
OK    the fee is below the minimum redemption
OK    the redemption window is non-empty
OK    fee is 4% of the smallest allowed redemption
```

That run used the stage fee of $1, which is a placeholder. **Re-run it when B2 sets the real one**:
the fee is the only input that can break invariants 3 and 5, and a mainnet fee is expected to be
higher than a testnet number that was measured on a chain sponsoring its own energy.

**Three tensions worth seeing before agreeing:**

The **daily mint cap and the alert route were one decision**, and that is why this set was agreed
only after D3 closed. `TreasuryServiceDown` was watched firing, routing and arriving on a phone
during a real outage on 2026-09-17, so the compromise window is now minutes rather than however
long until someone next looked. $2,000/day rests on that and should be revisited if the route ever
stops being tested.

The **peg damage and the theft are different numbers.** An attacker who mints unbacked CLT has to
redeem it to extract value, and every redemption is bounded twice and paid from a float that cannot
reach custody. So the mint cap bounds what you owe to make the peg whole; the float bounds what
actually leaves. They are often confused, and conflating them makes the float look less important
than it is.

**A $25 minimum redemption is high for ride fares**, and that is inherent rather than a choice: the
on-chain payout cost is fixed regardless of size, so small redemptions are uneconomic. The honest
options are a higher minimum, a subsidised fee, or batching — not a smaller number.

**Verification:** half met. Each cap is recorded above with the worst case it bounds, and the
relationships are checked mechanically. What remains is mechanical too and cannot be done yet —
`check-cap-invariants.sh` passing against the mainnet `.env`, which requires a mainnet host to
exist. This item now waits on C1 rather than on a decision.
---

## C. Chain and genesis

### C1. Fresh mainnet genesis — **Blocker** (checkable as of 2026-09-11)

`is_testnet = true` and `chain_id = 2077` are committed into the genesis hash, alongside `tx_fee`,
`mint_authority`, `faucet_address`, `faucet_allocation` and both referrer bps rates. Mainnet needs a
new genesis with `is_testnet = false`, a distinct `chain_id`, and the KMS-backed mint authority from
A1. `faucet_allocation` must be `0`, as it now is on stage.

The eight committed values cannot be changed after the chain starts without a new genesis, and a
disagreement of one character between two nodes means they cannot peer — the hash is compared at
handshake. Until now the only thing enforcing that was a comment in `node1.toml` saying the values
must be byte-identical.

`clutch-deploy/scripts/check-genesis.sh` now enforces it before boot. It compares every committed
field across all node configs, compares the `authorities` list too — not genesis-committed, but
`authorities[slot % len]` depends on order and length, so a divergent list makes a node reject
blocks the others accept — and applies the value rules the node otherwise asserts at boot. Under
`MAINNET=1` it adds the three that only matter for a real chain: `is_testnet` false,
`faucet_allocation` zero, and a `chain_id` that is not 2077, because sharing the testnet id would
let an auth challenge captured there authenticate the same key here.

Verified in both directions: it passes on the current config and exits non-zero both on a config
where one `tx_fee` differs and on the testnet config under `MAINNET=1`.

**What is still needed, and each is a decision rather than a task:**

| Value | Who decides, and on what |
|---|---|
| `chain_id` | Yours. Any value that is not 2077 and not another live chain's. |
| `mint_authority` | Blocked on A1 — it must be the KMS key's address, so this cannot be filled before that key exists. |
| `tx_fee` | Currently 1,000 CLT ($0.001). Validator compensation, so it depends on what running an authority costs. |
| Referrer bps | Currently 200 + 200. An economic choice about app-builder incentive, not a safety one. |
| `faucet_address` | Inert once the allocation is zero, but it stays a committed field, so pick something deliberately rather than carrying the testnet's over. |
| `authorities` | Blocked on C2 — the mainnet set, whose size also picks the block cadence at `60 / len`. |

**Verification:** the mainnet genesis parameters recorded here, `MAINNET=1 check-genesis.sh` passing
against them, every node reporting the same genesis hash after first boot, and a node configured
with a nonzero faucet allocation refusing to start.

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

### D1. Off-host ledger backup — **Closed 2026-09-15**

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

#### Rehearsed on 2026-09-12, and it found a live bug

`.github/workflows/rehearse-restore.yml` runs the whole loop against stage: dump both databases,
encrypt, decrypt, restore into a throwaway database, count rows, drop it. The passphrase is
generated per run and never written anywhere, which keeps it a test of the machinery rather than a
backup nobody can decrypt.

**The first run failed two lines in, with no output at all.** `env_get` is a grep, and under
`set -euo pipefail` a grep matching nothing fails the pipeline, which inside a command substitution
kills the script. Reading the unset optional `BACKUP_REMOTE` aborted the entire backup before a
single line printed.

This was not hypothetical. The scheduled job had already run once, at 08:07 UTC on 2026-09-11, and
died exactly that way — exit 1, no message, on a host where nobody was watching. Had nobody
rehearsed, the first evidence would have been a restore that found no backups.

Fixed, and the error path now works too: a run without `BACKUP_PASSPHRASE` prints the abort naming
the variable and how to generate one, instead of exiting silently.

The second run succeeded end to end against real data:

| Database | Restored rows (sample) |
|---|---|
| `treasury` | alerts 523, reconciliation_runs 33, treasury_events 24, mint_intents 8, chain_outbox 8, redemption_intents 5 |
| `orchestrator` | deposit_intents 8, deposit_addresses 6 |

Both throwaway copies were dropped. Nothing touched a live database.

#### Closed 2026-09-15

All three open conditions are met.

1. **`BACKUP_PASSPHRASE` is set**, generated off-host and held in a password manager. The nightly
   job had been aborting every night since 2026-09-11 for want of it.
2. **`BACKUP_REMOTE` is set** to Cloudflare R2 (`r2:clutch-treasury-backups`), an account API token
   scoped to that one bucket.
3. **Reconciliation ran green against a restored ledger**, which is the verification:
   `treasury-20260915T195834Z.dump.enc` was fetched **from R2**, decrypted with the passphrase in
   `.env`, restored into a throwaway database, and reconciled `ok` against the real chain and real
   custody. Both copies were dropped; nothing touched a live database.

`.github/workflows/rehearse-restore.yml` with `source: remote` performs the whole loop
(`scripts/verify-restored-ledger.sh`). Re-run it after anything that touches the backup path. It
reads the dumps back out of the remote deliberately — restoring the local copies tests nothing
about surviving the loss of the host, which is the only failure this item is about.

Reconciliation runs as `treasury-service --reconcile-once`, which starts no workers and no HTTP
server. An ordinary instance would start the sweeper, the chain outbox and the payout workers, all
of which act on chain, so a service reading a **copy** of `chain_outbox` would re-broadcast
transactions already submitted and re-sweep addresses already swept. Verifying a backup must not be
able to move money.

:::warning What the first green run took
Three things had to be fixed first, and each was invisible until something tried to use it.

**The off-host copy failed with `403 AccessDenied` on `CreateBucket`.** rclone verifies a bucket
exists before uploading; a token scoped to one bucket cannot list buckets account-wide, so rclone
read that as "bucket missing" and tried to create it. The error names authentication and the cause
is a permission nobody should grant. Fixed with `no_check_bucket` on the remote — not a flag in the
script, because it is an S3 backend option and the backend is deliberately the operator's choice.

**`--reconcile-once` could not run at all.** `NodeClient` connects on a spawned task, so the
one-shot call lost the race every time and reported "WebSocket connection not established". The
long-running service absorbs this with a 30s retry; a single run had nothing to absorb it.

**The verdict was too generous.** It exited 0 on `over_backed_drift` and printed "RECONCILED" over
a p1 reading *"a mint the ledger recorded is missing on chain"*. The mint gate accepts any status
that is not `mismatch` — correct for minting, wrong for an operator asking whether a backup is
trustworthy, where only `ok` is yes. Exit codes now distinguish clean, mismatch, ran-but-not-clean
and could-not-run.

That third one is the one to remember: the verification would have reported success over a ledger
that did not balance, and the item would have been recorded as closed on it.
:::

:::note The drift it found was real, and was not the backup
The first verification run returned `over_backed_drift` — and so did live, with identical numbers,
which is what proved the restore faithful rather than broken.

The gap was 10,000,000 CLT: one mint intent stuck in `submitted` since 2026-09-14, three identical
intents created 26ms apart, which is the signature of the stale-nonce defect fixed in clutch-node
the same day (`get_next_nonce` answered `confirmed + 1` while ignoring the pool, so an outbox pass
handed every mint the same nonce). Two landed. The third could not.

It stayed stuck because **`submitted` is terminal in practice**: the outbox retries `pending` and
`failed`, nothing re-drives `submitted`, nothing times it out, and nothing checks whether the
transaction landed. Only the watcher writes `confirmed` — and the watcher's cursor was itself
stranded at 4083 against a chain head of 3993, left there by a testnet reset, crediting nothing
silently until the chain grew past it and resumed having skipped every block below.

Re-driven with `redrive-mint.yml`; it confirmed in 11 seconds and reconciliation went `ok`. The
re-drive cannot double-mint: `credit_ref` is UNIQUE and the chain holds `processed_ref_<credit_ref>`
as an exactly-once marker, so resubmitting a mint that really did land is rejected rather than
minted twice.

**Two defects remain open from this, and both matter more on mainnet than here:** the outbox needs
to time a `submitted` row out, check `chain_tx_hash` against the chain, and re-drive or park with
an alert; and a watcher cursor above the chain head needs to alert rather than heal by accident.
Today a stuck mint needs a human to notice, which does not scale.
:::

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

### D3. Reconciliation runs unattended and alerts — **Closed 2026-09-17**

Reconciliation already ran unattended: a worker loop on `reconciliation_interval_secs`, with a
short retry on failure rather than the full interval, and a mismatch already called
`ledger::alert`, which logs at error level and writes an `alerts` row that `metrics.rs` gauges.

What did not exist was anything that **evaluated** any of it. Ten Prometheus alerting rules now
do, in `clutch-deploy` (`config/monitoring/prometheus/rules/treasury.yml`, `db1bd1a`): reconciliation
mismatch and staleness, a latched breaker, p1s from either service, either service failing scrapes,
sweeping stalled, a stuck chain outbox, and deposit polling stalled or never having run. Verified
loaded on stage — all ten report `health: ok` — via the `metrics` probe.

**Delivery exists and is proven, 2026-09-16.** Alertmanager runs in the stack, Prometheus posts to
it, and the receiver is Telegram. A synthetic alert sent through the real route arrived FIRING and
then RESOLVED. That covers every silent failure mode in the receiver half — bot reachability, chat
id, token, message format, file permissions — and `test-alert-route.yml` re-runs it on demand,
which is worth doing periodically: a route that worked once and has since broken quietly is the
same as no route.

It took three attempts, and the point is that **not one of the failures was visible from outside**.
A placeholder `chat_id: 0` crash-looped the container. A generic webhook receiver could never have
worked with the platforms it named, because Alertmanager posts its own JSON and Slack wants
`{"text":…}`. And mode 600 owned by root, against an image that runs as `nobody`, failed every
notification with `permission denied` — visible only in Alertmanager's own log, which the probe had
been printing for exactly one commit when it was needed. Each time: alert accepted, alert active,
nothing delivered, no error anywhere anyone would look.

**Verification met 2026-09-17.** `treasury-service` was stopped for four minutes.
`TreasuryServiceDown` went to `firing` in Prometheus, routed through Alertmanager, and arrived on
Telegram — then `RESOLVED` when the service came back. A real rule, evaluated and delivered, which
is the bar this item sets rather than the one it is easy to settle for.

Two things worth keeping from the exercise. The inhibit rules worked: none of the other treasury
alerts arrived alongside it, so the notification said "the service is down" rather than burying
that under six consequences of it. And reconciliation returned to `ok` on its own once the service
was back, without intervention.

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

### D4. The chain is producing blocks — **Closed 2026-09-17**

D3 covered the money path and nothing covered the chain underneath it. So the single most
consequential fact a validator set can report — that it has stopped — was the one condition
nothing evaluated, while ten rules watched the treasury reading from it.

**The property.** Aura authors a block every `60 / authorities` seconds (20s on the three-node
testnet) and authors an **empty** block when there is nothing to include. Height advancing is
therefore a heartbeat, not a measure of traffic: a quiet chain still climbs, and a chain that
stops climbing is broken. That is what makes "height has not changed" a usable alert at all,
and it is a property of Aura rather than of this deployment — a consensus that only produced on
demand would need a different signal.

Four rules now evaluate it, in `clutch-deploy/config/monitoring/prometheus/rules/chain.yml`:
`ChainHeightNotAdvancing` (no node has advanced in 5 minutes — the halt itself),
`ChainNodeBehind` (validators disagree by more than 50 blocks),
`ChainNodeDown`, and `ChainLatestBlockHashMissing`.

**What happened on 2026-09-14.** The stage chain halted at block 84 and ran halted for most of a
day. It was reported by a human, in the words "the explorer has no data" — three layers below the
actual fault. Four things had to be wrong at once for that to be the first signal:

1. **No alert covered chain liveness.** This item.
2. **The authoring error was logged at `debug!`.** Most authoring attempts legitimately fail —
   a node that is not the slot's author must fail — so the error was routine by construction and
   had been turned down to match. The one case that is not routine went down with it. The node now
   warns after 300 seconds of *continuous* failure and logs the reason.
3. **The chain probe read node1 only.** node1 is the p2p bootstrap, which says nothing about which
   node is worth reading; node3 owned the slot and held the real reason. The probe reads all three.
4. **The explorer had no probe at all**, so the symptom the user saw could not be checked from the
   inspection workflow that exists for this.

The fault itself was a transaction whose nonce the chain had already consumed sitting in the pool.
`author_new_block` puts the whole pool into the candidate block, so it failed every block, and the
only thing that removes a pooled transaction is the import of a block carrying it — a deadlock no
restart could break. `evict_permanently_invalid` now drops those at authoring time.

**Its cause was upstream of the node.** `get_next_nonce` answered `confirmed + 1` while ignoring
the pool, so the treasury's outbox, sending several mints in one pass, was handed the same nonce
for every one of them; the first landed and the rest could never be valid. That is fixed at the
node (`Blockchain::next_nonce_for` is pool-aware), which is the only place all callers route
through — the treasury was not doing anything wrong.

**Verification met 2026-09-17, by inheritance and by evaluation.** These rules route through the
same Alertmanager receiver as D3's, and D3's path was proven end to end with a real firing rule.

What that inheritance does NOT cover is whether each chain rule's own expression can ever match —
a rule loads with `health: ok` whether or not its query is capable of returning anything, and
`TreasuryWatcherCursorStranded` shipped in exactly that state, comparing a labelled vector against
a bare aggregate. So the `metrics` probe now evaluates each chain rule's body with the comparison
stripped, and all four return values: blocks produced in five minutes, the spread between the
fastest and slowest node, how many nodes publish the hash-labelled series, and how many are down.

Halting the chain to watch `ChainHeightNotAdvancing` fire would add little on top of that and costs
a deliberate outage. Left undone on purpose.

---

## E. Abuse and rate controls

### E1. Rate limiting — **Required** (edge limiter live in dry run, 2026-09-14)

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

#### Measured against stage, 2026-09-13

`clutch-deploy/scripts/loadtest-rate-limits.py`, run twice.

| Check | Result |
|---|---|
| Per-key limit (10/min) | First refusal on request **11**, both runs. Exact. |
| Global limit (120/min) | Holds inside a window. 150 requests with distinct keys: 43 refused on run one, **0 on run two**. |
| Service under saturation | `/health` stayed 200 throughout. A saturated auth endpoint does not take the process down. |
| Recovery | Requests accepted again once the window rolled. |

**The difference between the two runs is the finding.** The global limiter uses a fixed window, so
its counter resets at the boundary and a burst spanning one gets up to **twice** the configured
limit. Whether 150 requests were refused at all came down to where they fell relative to the reset.

The limiter is unchanged, deliberately: twice a limit chosen well below what hurts is still well
below what hurts, and a token bucket would carry per-key state between windows for little gain. But
the code claimed window-edge behaviour was "uninteresting at these thresholds", and that claim was
wrong, so it now records the measurement instead. Read the configured value as *about* this many
per minute, up to twice that across a boundary.

One request in 300 returned a 502 from the edge under 8-way concurrency. It did not reproduce on
the second run and it is not the limiter — a refusal is a GraphQL error, not a 502. Recorded rather
than diagnosed, because one occurrence is not a pattern, and worth a look if it recurs.

#### Edge limiting, in dry run, 2026-09-14

G1 closed, so the edge config is owned by a repo and this became possible. A per-client limiter now
runs on every clutch vhost — **and refuses nothing yet, on purpose.**

**The key is the hard part, and it is the same objection the app raised.** At nginx the hop is
Cloudflare, not the client, so a naive `$binary_remote_addr` key would put every visitor on the
planet in one bucket. An nginx `geo` block matches on the *real peer address*, so a trusted flag is
set only when the request genuinely came from Cloudflare, and a `map` honours `CF-Connecting-IP`
only in that case. A request reaching the origin directly with a forged header is not from those
ranges, its header is ignored, and the key falls back to the peer. That is trusting a header **from
a known hop**, which is precisely the distinction the application could not make and why it was
right to refuse.

Not `set_real_ip_from`, which would achieve the same by rewriting `$remote_addr`. It is only legal
at http, server or location level: at http it changes `$remote_addr` for the v2ray vhosts sharing
that file, and per-server it means twenty-two CIDRs copied into six server blocks, which is how one
of the six silently falls behind. `geo` and `map` only define variables, so they sit at http level
and affect nothing that does not read them.

**Measured 2026-09-14:** 250 requests at 30 concurrent from one address produced **169
`REJECTED_DRY_RUN`**, every one served with 200. So the limiter evaluates, the key tracks a single
client correctly, and nothing is being refused.

Seeing that took its own work. A dry run is invisible by construction, and nginx reports it at
`warn` while the default `error_log` level is `error` — so the first attempt produced no evidence at
all and looked like a limiter that was not loaded. Raising that level means editing a v2ray
directive, so the limiter writes its own log via `$limit_req_status` instead, in a format carrying
**no client address**: the measurement needs how often and where, not who.

**Still open: the enforcement flip.** The rate — 600 a minute per client, burst 20 — is chosen to be
far above a person and far below a flood, but that is reasoning, not evidence. Guessing low means
refusing real users at the edge with a 429 and no application log to explain it. What is missing is
a period of real traffic with the dry run watched: if no genuine client trips it, turn
`limit_req_dry_run` off; if some do, raise the rate first. That flip is one line.

**Verification:** the limiter enforcing, with a dry-run period on record showing it would not have
refused real users. The load test and the dry-run measurement are done and their numbers are above.

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

### G1. nginx ownership — **Closed 2026-09-13**

The nginx serving stage belongs to the `v2ray` compose project, not `clutch-deploy`. It mounts a
hand-maintained config that `deploy-stage.sh` patches in place each deploy, so the checked-in copy
has already drifted from the host. Editing `clutch-deploy/config/nginx/*.conf` changes nothing on
stage. A money system should not have its edge config in a file no repo owns.

**What is actually on that host**, read through the `nginx` probe on 2026-09-11 so the move is a
known quantity rather than an estimate. One file, thirteen `server` blocks:

| Clutch vhosts (8) | v2ray vhosts (5) |
|---|---|
| `app-stage`, `api-stage`, `explorer-stage`, `node1-stage`, `node2-stage`, `node3-stage`, `seq-stage`, `grafana-stage` | `de2.clutchprotocol.io`, `de.wenda.ir`, `3x`, `sub`, `de-grpc` (the only `listen 80 http2`) |

The `/payment/` route is at line 37, inside the `app-stage` block — the route that was added to this
repo's copy, deployed, confirmed present on the server, and still 405'd for a full cycle before
anyone checked which file was mounted.

**The reason this is not simply copied into the repo:** five of the thirteen vhosts are not Clutch's,
and one of them is a different domain entirely. Moving the file wholesale would mean this repo owning
someone else's proxy configuration; leaving it means Clutch's edge is owned by nothing. Two ways out,
and it is a host-reorganisation decision rather than a task:

1. **Split by include.** The v2ray nginx keeps `nginx.conf` and adds an `include` of a directory that
   `clutch-deploy` owns and deploys. Smallest change, and the Clutch vhosts become reviewable in git.
   The v2ray project has to accept the include.
2. **Clutch takes port 80.** Move nginx into `clutch-deploy` (the overlay for it already exists,
   `docker-compose.stage.nginx.yml`, unusable today because :80 is taken) and have it proxy the v2ray
   vhosts instead. Cleaner ownership, larger blast radius if it goes wrong, and it makes Clutch's
   deploy able to break someone else's service.

Nothing was deliberately dumped into a CI log here. The probe reports structure — sizes, server
names, locations — rather than the file's contents, because a production edge config in a build log
is a different problem from the one being solved.

**Verification:** Clutch's edge config lives in a repo, is deployed from it, and the live config read
back from the host matches the checked-in one. Until then, keep using the `nginx` probe rather than
either copy, as the workspace notes already say.

#### A managed block, live on the host since 2026-09-13

Clutch routes now live in `clutch-deploy/config/nginx/clutch.d/` and are injected into the mounted
file between markers by `scripts/ensure-nginx-clutch-block.sh` on every deploy. Everything outside
the markers belongs to the `v2ray` project and is never touched; everything inside is replaced
wholesale, so a route file deleted from the repo disappears from the host rather than lingering
where no diff would show it.

**The obvious design does not work here, and was shipped before that was noticed.** An
`include /etc/nginx/clutch.d/*.conf;` with the directory synced from the repo passed `nginx -t`,
reloaded cleanly, and could never have loaded a thing: the container bind-mounts exactly **one**
path, the single `nginx.conf`, so no host directory is visible inside it and the include resolved
against the container's own filesystem. A glob matching nothing is valid nginx, which is why
nothing complained. Adding a mount means editing another project's compose file. Confirmed by
inspecting the container's mounts, then replaced with the managed block and the dead include
removed.

Guards, in order: the set of `server_name`s must be identical before and after, then `nginx -t`,
then reload, with a restore from backup on either failure. The name check runs on the candidate
before anything is written, because a config can be syntactically perfect and have quietly lost a
server block — and this file serves **14** vhosts that are not ours while the deploy's own health
gate only reaches a clutch route.

That guard was itself wrong at first: it matched `server_name` only at the start of a line, so a
server block written on one line was invisible to it — precisely the vhost that could then vanish
unnoticed. Widened, and the count went from 13 to 14, which is the evidence that at least one such
block exists on that host.

`PROBE=nginx` now prints the live managed block and reports any stale include by name. Neither was
visible before: markers are comments, and an include that loads nothing still passes validation.

#### `/payment/` migrated, 2026-09-13

The route now lives in `config/nginx/clutch.d/payment.conf`, and
`ensure-nginx-payment-route.sh` is retired and deleted. The live host was read first rather than
trusting the generator: the block there was byte-identical to what that script produced, with no
hand-tuning accumulated, so this was a change of owner and not a rewrite.

The strip is conditional on the repo's content, not a date or a flag — the old inline block goes
exactly when the managed block carries a replacement, so the route is never absent even briefly.
Two mechanisms able to write the same `location` is how you get a duplicate and a config nginx
refuses, which is why the old script had to go rather than sit dormant. Brace counting is bounded
at 40 lines: if the marker survived but its block did not, unbounded counting would eat whatever
came next, so overrunning aborts and writes nothing. A post-check asserts exactly one `/payment/`
location remains.

Cleaning up after myself took a second pass. Removing the dead include line left four orphan
comments on the host describing a directive that no longer existed — config nothing accounts for,
which is the drift this item exists to end, introduced while ending it.

**Larger than it looked.** The probe lists Clutch routes across **six** vhosts in that file: the
demo app, the Hub API, the explorer, and the three nodes with their `/ws` and `/metrics` endpoints.
The managed block is injected into **one** server block, so covering the rest needed a different
shape from what existed.

#### One block per vhost, and the Hub API moved in, 2026-09-13

`config/nginx/clutch.d/` is now one subdirectory per vhost, named for the vhost, and each gets its
own block anchored at that vhost's `server_name`. Blocks are deleted and rebuilt on every run
rather than edited in place: one code path instead of two, it migrated the old single-block marker
with no special case, and a file — or a whole vhost — removed from the repo disappears from the
host instead of lingering as a route nobody can find in a diff.

**Adopting a route that already exists is the hard half.** These were hand-written on the host long
before the repo owned any of them, so taking one over means deleting the existing copy in the same
pass that adds the managed one. nginx refuses a duplicate `location`, so a half-done takeover does
not serve the old route — it serves nothing. The strip is scoped to the one server block being
adopted: every vhost in that file has a `location /`, and a strip without that scope would take
over one vhost by breaking the rest. `nginx -t` with restore-on-failure is the backstop, so a
signature that fails to match leaves both copies, the config is rejected, and the previous one goes
back.

That superseded the special case written for the retired `ensure-nginx-payment-route.sh`, which is
now deleted. Keeping it would have been worse than redundant: it brace-counted forward from its own
marker comment, so once the generic strip had removed the block underneath it, it would have eaten
whatever came next.

**`api-stage.clutchprotocol.io` is repo-owned as of 2026-09-13** — `/graphql`, `/graphql/ws`,
`/health` and `/`, copied verbatim from the live host and read back afterwards to confirm. The
`clutch_api` upstream they proxy to is still outside the repo; an `upstream` block in a route file
would be a duplicate, so the upstream remains part of what this item has left to solve. The server
block's `add_header` directives sit outside any location and were left alone.

Two post-reload gates were added, because the deploy's existing health gate runs *before* the block
is written and says nothing about the config the reload has since installed:

- `/health` must answer 200 — the vhost still proxies to the Hub API at all
- `/graphql/ws` must answer **101** to a real WebSocket handshake

The second is the one that earns its place. Lose the `Upgrade` headers and the request falls
through to `location /`, which has none: the handshake degrades to a plain 200, every page still
loads, and every subscription silently never fires. A `POST /graphql` gate was written and then
dropped — `location /` proxies everything to the same upstream with the path preserved, so losing
the `/graphql` block entirely would still answer correctly, and the typo it would have caught is
one `nginx -t` rejects at config load.

**A failed gate now restores the previous config.** Reverting the repo change would not undo a bad
outcome: the hand-written locations are stripped in the same pass that adds the managed ones, so a
later deploy without the route file would leave the vhost with no routes at all. The backup the
block script writes before every edit is the only thing that puts the previous edge back.

The script has ten checks in CI against a fixture shaped like the host's config. They found three
real defects before any of this reached the host: a blank line printed before the block survived
the strip, so the file would have grown one line per deploy; the strip count was read off the same
stderr `awk` was using to warn about escaped dots; and the eight-space indent applied to a blank
separator left eight spaces of nothing.

#### All six vhosts, 2026-09-13

The explorer, the three nodes and finally `app-stage` followed `api-stage`, each read off the live
host and diffed against the file committed for it. **24 locations across six vhosts, every one
inside a managed block, none outside** — read back from the host afterwards rather than inferred
from a green deploy.

`app-stage` went last on purpose. Every other vhost degrades to one broken path if a migration goes
wrong; its `location /` is the whole site.

Two things the migration itself taught:

**The strip had to learn about comments.** node1's block carried
`# WebSocket endpoint (use wss://...)` directly above its `/ws` location. Removing the location and
leaving the comment produces config describing a directive that is no longer there — the same drift
this item exists to end, and it had already happened once here. A comment directly above an adopted
location now goes with it; comments above locations nobody adopts are left alone.

**Each node was read separately rather than derived from node1.** They differ only in hostname,
upstream and ports, which is now a measured fact rather than an assumption. "Presumably identical"
is how a hand-maintained difference gets erased.

Seventeen gates now run after the reload, because the deploy's own health gate runs *before* the
block is written and says nothing about the config the reload has since installed. Every repo-owned
vhost is checked: the SPA, `/health`, a prefix-stripping `/api/health`, real WebSocket handshakes
expecting 101, and the nodes' `location /` expecting **404** — a route rather than an accident, so
asserting it is how one quietly turning into a proxy would be noticed. Any failure restores the
backup and fails the deploy.

Both mechanisms were proved by failing. The first gate run rejected a correct config because the
generalised helper had dropped the `graphql-transport-ws` subprotocol the Hub API requires; the
restore fired and every surface stayed up. The second hung on a node's `/ws`, because a successful
upgrade leaves the socket open and an unbounded `curl` never returns — and that timeout landed
*after* the reload, so the restore never ran and the change shipped under a failed deploy. Both are
fixed, and the second is the more instructive: a gate that can hang is a gate that can let a change
through while reporting failure.

#### The upstreams too, later the same day

`clutch_api`, `clutch_web`, `clutch_explorer_api` and `clutch_explorer_web` were the recorded
remainder: every route was owned, but the names those routes resolve to were not, so a change to one
appeared in no diff anywhere.

They could not live in a route file. A `location` lands inside a `server`; an `upstream` belongs to
`http`, one level up — so they have their own flat directory, `config/nginx/clutch.upstreams/`,
their own marker and their own anchor at `http {`. Flat rather than per-vhost because an upstream is
not owned by a vhost and several vhosts name the same one. All four are single-server blocks: one
host, one container each, existing so routes have a name to point at rather than for load balancing.

One thing would have broken it. The strip that removes old managed blocks was keyed on the route
marker alone, which would have left the upstream block in place and inserted a second — a duplicate
`upstream`, and a config nginx refuses outright. It now keys on the prefix both kinds share.

**The upstream guard is deliberately weaker than the `server_name` one.** That one refuses any
change, because this repo has no business declaring a vhost. This one refuses only names that
*disappear*, because adding an upstream is what adding a service looks like, while an upstream
vanishing means every route naming it 502s with the config still passing `nginx -t`. The first
version was the strict kind and the test suite caught it by refusing a legitimate addition.

Read back off the host afterwards, once the probe was taught to show it — its managed-block dump
matched only the route marker, so the thing that had just been migrated was the one thing it could
not display.

**Verification:** met. Every clutch route is present in `config/nginx/clutch.d/`, and the probe
shows no clutch route outside a managed block in any vhost.

### G2. Single host — **Required**

Everything runs on one VPS: nodes, Hub API, treasury services, both databases. That host is a
single point of failure for consensus, custody, and the ledger simultaneously.

**Verification:** a topology where losing any one host loses neither block production nor the
ledger, and a documented recovery time for each component.

### G3. Someone else can operate it — **Blocker** (the prerequisites now exist, 2026-09-11)

`treasury-service` has a manual halt (`minting_halted` in `breaker_state`), a per-transaction cap, a
daily cap and four-eyes approval. That machinery is worth very little if one person knows it exists.
A money system needs a second person who can stop it.

Two things were missing before a second operator was even possible, and both are now in place.

**There was no way to halt.** `POST /internal/halt` existed in the service and nothing could reach
it: resuming had a workflow and a script, halting had neither. An operator who decided something
was wrong had to SSH in and curl with the Approver token by hand, which is the worst possible
moment to be assembling a command. Found by writing the runbook below and noticing it instructed
the reader to use a control that did not exist.

`clutch-deploy` now has *Halt minting (stage)*, mirroring the resume pair, with the asymmetry kept
deliberate: **resuming is gated and halting is not.** Resume refuses while the latest reconciliation
is still a mismatch; halt has no such guard, because halting is cheap and fully reversible — deposits
keep being credited, the reserve total stays correct, only new issuance stops — while being slow to
halt is not reversible at all. It refuses to overwrite an existing `halt_reason`, since the first one
is the one that explains why minting stopped.

**There was no runbook.** `clutch-deploy/docs/ON-CALL.md` now covers what can break, what each of the
ten D3 alerts means and what to do about it, what is safe to do alone, and what needs two people. It
leads with the two rules that matter more than the rest combined:

1. Never clear the breaker to make an alert go away.
2. Never retry a payout whose outcome is unknown.

Both trade a recoverable problem for an unrecoverable one, and both are the tempting move at 3am.
It also names what only two people can do — the four-eyes mint is two separate dispatches so one run
cannot be both roles — and says plainly that if you are on call alone and a mint needs approving, it
waits.

**What remains is a person.** No document closes this item. It needs a named second operator with
access, who has read that runbook and done the three things in its "Before your first shift"
section — including halting and resuming once on a quiet day, because rehearsing the control you are
least likely to use is the point of rehearsing at all.

**Verification:** a named second operator, and a rehearsal in which that person halts minting and
resumes it without the maintainer's help.

### G4. A deploy ships the commit it says it does — **Closed 2026-09-13**

Every workflow that touches the stage host begins by SSHing in and running `git pull --ff-only` in
the checkout, then running scripts from that checkout. On 2026-09-13 that pull started failing:

```
fatal: Cannot fast-forward to multiple branches.
```

A bare `git pull --ff-only` resolves `FETCH_HEAD` against every branch it has just fetched. Pushing
two feature branches was enough to stop the host updating — the first fetch that brings new remote
branches is the one that breaks, so it recurs on any branch push and looks like nothing in the repo.

**The failure was survivable; swallowing it was not.** `deploy-stage.yml` ran
`git pull --ff-only || echo "Note: git pull failed or not a git repo — continuing"`, so a deploy
whose pull had failed went on to `compose pull`, recreate containers with whatever compose files and
scripts the host already had, pass its health gate, and report success. The one line admitting the
repo had not moved was three hundred lines up a log nobody reads on a green run. For a stack whose
deploy also rewrites the edge nginx config and restarts the services that mint, that is a deploy
reporting a state it did not produce.

Found by accident: a read-only probe shipped the same morning did not run, and the only clue was
that its new output was missing.

Fixed in `clutch-deploy` on 2026-09-13:

- every workflow now names `git pull --ff-only origin main`, which has exactly one thing to merge
- `deploy-stage` no longer swallows the failure — a deploy that cannot update the checkout stops
- the probe still continues on a failed pull, because a probe of a stale checkout still answers most
  questions, but it says `WARNING` and says what it means for everything below it
- `PROBE=git` reports branch, upstream, branch/remote config and how many commits behind
  `origin/main` the checkout is. It previously reported status, HEAD and `core.fileMode` — none of
  which would have shown this

**Verification:** met. The host reports `0 commit(s) behind origin/main` on a clean tree with
`branch.main.merge refs/heads/main`, and a failed pull now fails the deploy rather than continuing
past it.
---

## H. Market operations

These are product gaps, already listed honestly in the org README. They do not endanger the
reserve; they decide whether a real ride market is usable.

### H1. Dispute resolution — **Built 2026-09-12; live on the testnet 2026-09-14**

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

**The blocking half now has a design proposal**, not an implementation:
`docs/superpowers/specs/2026-09-11-dispute-resolution-design.md`. Writing it changed the framing of
this item, and the correction matters more than the proposal.

**The gap is asymmetric in the opposite direction to the one this item described.** The protocol
already holds the full fare from `RideAcceptance` and releases it in rider-initiated `RidePay`
installments, with the unpaid remainder returning to the rider on cancel. So a rider who is wronged
mid-trip already has recourse — they stop paying and cancel, losing only what they released. That
was never documented as recourse, which is why it read as absent; the disclosure landed the same day.

The party with **no** protection is the driver, against a rider who takes the ride and simply
declines to release the held fare. The driver has performed, cancels, and the money goes back to the
rider. Any design that only adds rider protection makes the real imbalance worse, which is what would
have happened if this had been built from the item as originally written.

The spec sets out four options against the three questions, and recommends **timed auto-release of
the held remainder**: after a window, the unreleased fare settles to the driver unless the rider has
cancelled. It answers "who decides" with nobody, "what can a decision do" with settlement timing
rather than reversal, and "what stops the mechanism being the attack" by making inaction favour the
driver. It is one node-side rule and no new transaction type — against a juror protocol or an
operator arbitration role, either of which is a subsystem or a liability question.

**One decision inside it has a deadline attached to C1.** If the release window is a consensus
parameter it belongs in `ChainInit`, and genesis-committed values cannot be added later without a new
chain. Deciding whether the window is genesis-committed or per-acceptance has to happen before the
mainnet genesis is fixed, not after.

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

**Built, merged and verified on 2026-09-12** as option B of
`docs/superpowers/specs/2026-09-11-dispute-resolution-design.md`
([clutch-node#12](https://github.com/clutchprotocol/clutch-node/pull/12), 204 tests).

Once `ride_auto_release_secs` has elapsed since a `RideAcceptance`, the same `RideCancel` pays the
unpaid remainder to the **driver** instead of refunding the rider. Inaction used to favour whoever
owed money and now favours whoever is owed. No new transaction type, and no settlement pass
scanning open trips each block.

The four decisions the design left open, settled:

| Question | Decision | Why |
|---|---|---|
| Units | Seconds | Block cadence is `60 / authority_count`, so a block-denominated window would change whenever the validator set does |
| Window | **7200 (2 hours)**, decided 2026-09-12 | Long enough for a rider to notice, short enough that a driver is not financing them |
| Cancel inside the window | Stays free | Charging would punish the legitimately wronged rider, whose protection this is |
| Where configured | `ChainInit`, genesis-committed | It decides who receives money, so a node with a different value computes a different balance from the same block |
| Window start | Acceptance | There is no completion signal, so a long ride and a short one get the same window |

Per-acceptance was rejected because a rider choosing their own window chooses one that never
expires. `clutch-deploy/scripts/check-genesis.sh` now enforces 7200 under `MAINNET=1`, as an exact
value rather than "non-zero", because a typo that shortens the window is the failure that looks
fine.

Every uncertain case settles exactly as before: the rule disabled, an acceptance with no recorded
timestamp, an unreadable driver address, or the clock not yet past the window. `accepted_at` is
written even when the rule is off, so a chain enabling it later does not find trips it cannot date.

**Scope, honestly.** This closes *silent* non-payment. A bad-faith rider who actively cancels every
time still escapes, which is visible on chain and belongs to H2. That is why cancelling was left
free rather than made costly: the alternative punishes the rider this protects.

**Still open:** the mechanism ships disabled and is switched on by the mainnet genesis, so it is
part of C1 rather than a separate deployment.

#### Enabled on stage, 2026-09-14

The mechanism had never executed. `ride_auto_release_secs` is genesis-committed and was absent from
all three node configs, so it defaulted to `0` and every `RideCancel` on the testnet refunded the
passenger — the code was tested, the behaviour had never happened on a chain.

Stage was reset with the parameter set. Confirmed from committed state rather than the config file:
all three nodes answer `ride_auto_release_secs: 300` over `get_chain_info`, at the same height, with
`total_supply: "0"`.

**300 seconds on stage, against the 7200 of the decision of record.** Two hours cannot be exercised
on a development chain — nobody waits that long to test a cancel — so at 7200 the rule would reach
mainnet having never run against a real ride. `check-genesis.sh` was verified to refuse 300 under
`MAINNET=1`, so the short window cannot leak into a mainnet genesis.

Exposing the value was part of this. `get_chain_info` did not carry it, so a genesis parameter that
decides **who receives money** was unobservable from outside the node: an operator could only read
the host's config file, which is the config rather than the committed state, and a client had no way
to tell a rider how long a cancel still refunds them. It is in the response now.

**Still not verified end to end.** A fresh chain has no CLT — `faucet_allocation` is 0 and the faucet
is gone — so funding a passenger and a driver means a real Nile USDT deposit, and the Nile faucet
has a human check. The rule is live and observable; it has still never fired against an actual ride.

### H2. Reputation — **Required** (re-read after H1 shipped, 2026-09-13)

No driver or rider scoring exists, so nothing distinguishes a first-time counterparty from a
repeatedly bad one.

`docs/superpowers/specs/2026-09-11-reputation-design.md` proposes one. The constraint that decides
everything is that **an account is a free keypair**, so any score attached to an address is
discardable — an actor with a bad score makes a new key. Designs that are not answers to that are
worse than nothing, because they look like protection while a new keypair defeats them for the price
of one transaction. A `RideRating` transaction type is the specific thing to avoid: it puts a
scoring rule into consensus, where it cannot be changed without a new chain, and still does not
solve the clean-slate problem.

Two things already costly in this system do answer it, and both are already on chain: deposit
history, since CLT enters circulation only against verified USDT, and completed rides, which need a
funded counterparty. The naive version of that fails to self-dealing — ride with yourself and the
fare returns to you, so a fabricated ride costs only five transaction fees, about **$0.005** — which
is why the measure has to be **distinct counterparties weighted by whether they have their own
deposit history.** Funding is the cost a keypair cannot dodge.

The recommendation is therefore to **derive reputation off chain from public history and keep
scoring out of consensus**: the explorer aggregates facts per address, the Hub API exposes them, and
an application decides what to make of them. No new transaction type, no new state, and no new
attack surface in the node — which is what this item's own verification asks for. It deliberately
produces facts rather than a single number, because a single number is what invites gaming.

**Ordering:** build H1 first. Its timed auto-release means a rider who takes a ride and does not pay
ends up paying anyway, which removes most of the incentive this design guards against. That is why
this item is Required rather than a blocker, and with H1 shipped reputation becomes information for
declining a counterparty in advance rather than the primary defence.

**Verification:** a reputation design that does not become a new attack surface, implemented. The
proposal satisfies the first clause by construction; the second is unbuilt.

**Re-read on 2026-09-13**, as the design asked once H1 shipped. The recommendation holds and the
metric gets sharper.

H1 closed silent non-payment, so reputation no longer has to cover it. The only remaining way to
take a ride without paying is a rider cancelling before the deadline with fare unpaid — which is
narrower than "cancellations" and is what should be surfaced.

That distinction matters because the naive metric punishes the innocent. A driver cancelling a
no-show and a rider cancelling to avoid paying are both cancellations, and the chain already tells
them apart: a cancel records who sent it, and state records how much was paid and when the trip was
accepted. So the metric is cancels initiated by the rider, with fare unpaid, before the
auto-release deadline. No new on-chain data, no blame attribution.

That answers the first of the three open decisions outright. The second — whether "has deposited"
is public — still needs answering before anything is built, because it publishes a fact about a
person rather than about a ride.

### H3. Matching — **Recommended**

Matching is simple, with no surge, pricing engine, or geospatial optimisation. Acceptable at small
scale; it becomes an operational problem before it becomes a technical one.

**Verification:** a recorded decision on the ride volume or city count past which this needs work,
so the limit is chosen rather than discovered.

---

## I. External review

### I1. Security audit — **Blocker** (brief written 2026-09-11)

No external audit has been done. The areas that most need outside eyes are the signing and encoding
path, the four-eyes mint flow, the bounds on the payout endpoint, and the reconciliation arithmetic.

`docs/AUDIT-BRIEF.md` is now written to be handed to a firm as-is. Sending that rather than a
repository URL is the difference between paying an auditor to discover the architecture and paying
them to attack it, and it is the cheapest thing that can be done about this item before money
changes hands.

It states the **seven invariants the design claims**, so a report can say which one a finding breaks
— and so a firm that finds none of them broken has said something useful rather than nothing. It
names where to look ordered by what a finding would cost rather than by lines of code, and it lists
the known gaps explicitly, with the note that a finding restating one of them is not useful while a
finding showing one is *worse than recorded* is.

It also points at the live testnet as a legitimate target, since there is no mainnet to protect, and
explains that test funds need no wallet.

**Verification:** an audit report with every critical and high finding either fixed or accepted in
writing.

**Worth starting early.** Audits have long lead times and this one gates a real-funds launch, so
commissioning it while the KMS and validator work is in flight costs nothing and saves the calendar.
It is also cheaper to run against a system whose known gaps are already documented, which they now
are.

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

### J1. Get advice before accepting a dollar — **Blocker** (brief written 2026-09-11)

Not engineering, and not something anyone on this repo can sign off. A fully-reserved token that is
redeemable for USDT, issued and custodied by an identifiable operator, is money transmission or
e-money in most jurisdictions, with registration, customer due diligence, safeguarding and reporting
consequences. The reserve model being honest does not exempt it.

`docs/LEGAL-BRIEF.md` is now written to hand to counsel: what the system actually does, stated as
facts, with the questions listed and deliberately **not** answered. It contains no legal analysis,
because nobody here is qualified to provide any and a wrong guess costs more than the advice.

Assembling it clarified which facts are likely to dominate, and one of them is not technical:

1. **The operator holds user funds.** USDT is swept to a custody address the operator controls, so
   the token is a claim on the operator's own holdings rather than on a third party. This is the
   central fact and everything else is a detail beside it.
2. **There is no identity verification and no geographic restriction.** An account is a keypair; the
   operator never learns who anyone is. If due diligence turns out to be required, that is an
   architectural change and not a policy one — the system currently *cannot* identify anyone.
3. **The operator is one individual**, with no company, no second signatory, and no segregation
   between operating and user funds beyond the reserve address itself.

The brief also states the question the operator most wants answered: whether a lawful configuration
exists at small scale — low caps, restricted jurisdictions, clear disclosure — that permits a limited
real-funds pilot, and what it would have to be. If the answer is that any real-funds operation needs
a licence first, that is a useful answer and better had before building further.

**Verification:** written advice from a qualified lawyer in the operating jurisdiction, and whatever
registrations that advice names, in place.

**Worth starting early**, alongside the audit rather than after it. Legal advice can invalidate
assumptions underneath the rest of this document, and it is cheaper to learn that before the
remaining engineering is built on them.
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
