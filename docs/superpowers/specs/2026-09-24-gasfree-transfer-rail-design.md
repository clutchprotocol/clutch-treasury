# GasFree as a transfer rail for sweeps and payouts — design

Status: approved design, not yet implemented
Date: 2026-09-24
Scope: `clutch-treasury` (a new `gasfree` crate, `tron-signer`, `treasury-service`,
`payment-orchestrator`), plus configuration, a probe and invariants in `clutch-deploy`

## Problem

Every USDT transfer on TRON costs energy, and without staked TRX the network burns TRX to pay for
it. Today that cost is carried by the operator:

- A **sweep** moves a deposit from its derived address to custody. A freshly derived address holds
  no TRX — receiving tokens does not create a balance — so `tron-signer` first sends it 10 TRX
  from the fee account at `1/0`. Measured on mainnet on 2026-09-10 at 64,285 energy, 6.43 TRX.
- A **payout** sends USDT out of the float at `2/0`, which likewise needs TRX to move.

With no TRX in the fee account, every sweep answers `fee_account_dry`, and nothing consolidates.
The mainnet chain has been live since 2026-09-19, but the operator has no budget to fund TRX, so
mainnet cannot accept deposits or pay redemptions. And mainnet has `total_supply` 0 with
`tx_fee` 1000, so until something is minted, no transaction of any kind is possible on it.

## Decision

Add **GasFree** as a second transfer rail, used for both sweeps and payouts. GasFree lets a TRC-20
transfer pay its network cost **in the token being moved** instead of in TRX: the owner signs a
permit, a relay (a "service provider") submits it and pays the gas, and its fee is taken from the
transferred token. No TRX is ever needed, and the operator needs no capital.

- **Selectable per deployment**, `TRANSFER_RAIL=trx|gasfree`, defaulting to `trx`. The TRX rail
  stays, so the choice can be reversed when there is budget (with the limit described in
  "Switching back" below).
- **Sweep each deposit as soon as it confirms**, to keep the time user money spends in a GasFree
  contract to minutes.
- **Mint only what will reach custody.** CLT stays fully reserved.

Chosen by the maintainer on 2026-09-24, over waiting for TRX and over GasFree-only.

### The trade-off that was accepted

A GasFree address is a **beacon proxy**. Its creation code calls `implementation()` (selector
`5c60da1b`) on a beacon, so the code running at every user's GasFree address is whatever the beacon
points to, and whoever controls the beacon can change it for all of them at once — including, in
principle, to code that moves the money out.

Read on-chain on 2026-09-24:

| | Mainnet |
|---|---|
| GasFreeController | `TFFAMQLZybALaLb4uxHA9RBE7pxhUAjF3U` |
| Beacon | `TSP9UW6FQhT76XD2jWA6ipGMx3yGbjDffP` |
| `implementation()` | `0xa3b0edffa1b94e93d297dcc9b6860175e9b537ec` |
| `owner()` | **reverts** — the upgrade authority is not readable by the standard call |

So it was not possible to confirm whether the upgrade authority is a multisig, a timelock or one
key. Today's deposit addresses are plain accounts that only the mnemonic can move; this rail adds a
third party who can. That weakens the property the custody design protects, and it was accepted
knowingly, for lack of budget, with two mitigations: sweeping immediately (section 3) and a
tripwire on the beacon (section 5).

### Fees, measured

`PROBE=gasfree` in `clutch-deploy`, on 2026-09-24, against the Nile relay:

| Nile USDT (`TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf`) | Smallest unit | USDT |
|---|---|---|
| `activateFee` | 1000000 | 1.00, once per GasFree address |
| `transferFee` | 300000 | 0.30, every transfer |

The GasFree specification's own examples give 10 USDT for **each**. The live Nile values are what
was measured and are what this design is sized on. **Mainnet fees are not yet known**: the Nile API
key answered `Apikey not found.` against mainnet, and mainnet needs its own key and its own reading
before its rail is switched on.

## 1. Addresses

**What a user is shown.** In the `gasfree` rail, each new user is given `G = gasfree(D)`, where `D`
is the address `address_at(index)` gives them today. `G` is computed from `D` and three public
constants — the controller, the beacon, and the creation code — so no private key is involved and
the orchestrator stays key-free.

The derivation, from the official SDK (`gasfreeio/gasfree-sdk-js`, `src/GasFree.ts`):

```
salt          = D as 20 bytes, left-padded to 32
initData      = selector("initialize(address)") ++ salt
bytecodeHash  = keccak256(creationCode ++ abi.encode(address beacon, bytes initData))
G             = keccak256(0x41 ++ controller ++ salt ++ bytecodeHash)[12..32]
```

The CREATE2 prefix is `0x41` on TRON (`TronGasFree.getCreate2PrefixByte`), not Ethereum's `0xff`.
Getting it wrong produces a valid-looking address nobody controls.

**For addresses, the change is one line.** `address_for_user` stores whatever it computes in
`deposit_addresses.address`, and everything downstream keys off that column: the poller watches it,
the treasury verifies the transfer landed there, and the reserve reads its balance. Storing `G`
instead of `D` carries all three across unchanged.

**Existing users keep their address.** The insert is `ON CONFLICT (user_pk) DO NOTHING`, so anyone
who already has a row keeps their plain `D`. Only users created after the switch get `G`. No
migration.

**One derivation, in a new shared crate.** The orchestrator and the signer each derive `D`
independently today, deliberately, because one holds only the xpub and the other the mnemonic.
GasFree derivation involves no key, and here duplication is a hazard: two copies that drifted
would have the app show a user one address while the signer swept another. So it lives in one small
`gasfree` crate, depending only on `sha3` and `bs58`, used by both.

## 2. Minting and the reserve rule

**The rule:** CLT minted must never exceed the USDT that will reach custody. Today it holds because
the operator pays the sweep fee separately, in TRX, so the whole deposit reaches custody. On this
rail the fee is taken out of the USDT, so the mint must be smaller.

**The treasury decides the fee, not the orchestrator.** Today the orchestrator sends
`"amount_clt": intent.received_usdt` (`treasury_bridge.rs`) and the treasury verifies the on-chain
transfer covers it. The orchestrator is the public-facing service; if it chose the fee, a
compromised orchestrator could set it to zero, mint the whole amount, and leave the reserve short
after the sweep. So the treasury mints

```
amount_clt = observed_usdt − fee_reserved
```

from its own configuration and an on-chain check, capping whatever the orchestrator proposes.

**How much is held back:**

```
fee_reserved = TRANSFER_MAX                    if G is already activated
             = ACTIVATE_MAX + TRANSFER_MAX     otherwise
```

**Activation has one source of truth: whether `G` has contract code on-chain**, read through
TronGrid, which both services already use. The relay's API also reports an `active` field; it is
informational only and is never used to size a fee. This matters because the treasury's
`fee_reserved` and the signer's `maxFee` must be the same number — if the two services read
activation from different sources and disagreed, the signer could sign a `maxFee` larger than what
was held back, which is exactly an under-reserve. Reading it from one place makes them agree by
construction.

Both maxima are configured **above** the live fee — for Nile's 1.00 and 0.30, for example 1.50 and
0.50.

**The relay cannot take more, by signature rather than trust.** The signer signs each permit with
`maxFee` equal to the same amount, and the relay is limited by the permit to charging at most
`maxFee`. The signer computes it from the same configuration and the same on-chain check, so
nothing new is passed to `sweep(index)`.

**Why the reserve holds in every branch:**

- Before the sweep, `G` holds `observed`, and the reserve counts it; supply rose by
  `observed − fee_reserved`. The reserve leads supply by `fee_reserved`.
- After the sweep, the receiver — the float or custody, per section 4 — gets `observed − maxFee`,
  which is what was minted (section 3), and the relay's unused margin stays at `G`, still counted.
  Both destinations are counted in the reserve, so the reserve still leads supply.
- Two deposits credited before either is swept each hold back a full fee, but one sweep pays one
  fee. The reserve then leads by the extra fee. That over-reserves — safe — and is uncommon,
  because sweeps run on confirmation.

**If the live fee rises above the maximum,** the relay refuses the permit and the deposit stays at
`G`, still fully backing its CLT in the reserve count. A human is paged. **Never sign a higher
`maxFee` than was held back**: that is the one action that would turn a stuck deposit into an
under-reserve.

**Minimum deposit.** A deposit where `observed − fee_reserved` is below `MIN_DEPOSIT_USDT` mints
nothing and is held for a human (`needs_manual`). The deposit panel shows the minimum and the fee
**before** the user pays.

**The cost of this safety, stated plainly:** users pay the configured maximum, not the live fee. At
1.50 + 0.50 against a live 1.00 + 0.30, a first deposit pays 0.70 more than strictly needed. The
difference stays behind as extra backing, and it is what later pays for the float's activation
(section 4). It must be presented to users as "fee up to", never as a fixed fee.

## 3. Sweeping

Inside `tron-signer`, following the GasFree specification:

1. Read the GasFree account for `D` (`GET /api/v1/address/{D}`): activation, current `nonce`,
   balance at `G`.
2. Build the permit: `token`, `serviceProvider`, `user = D`, `receiver`, `value`, `maxFee`,
   `deadline`, `version = 1`, `nonce`.
3. Sign it (TIP-712) with `D`'s key.
4. Submit it (`POST /api/v1/gasfree/submit`); the relay returns a `traceId`.
5. Follow that `traceId` (`GET /api/v1/gasfree/{traceId}`) until the transfer is confirmed.

**INDEX-only, unchanged.** Every field comes from configuration or the chain: `token` is USDT from
config, `receiver` from section 4, `maxFee` from section 2, and `nonce` and the balance from the
account read. The caller supplies an index and nothing else, so it cannot redirect a micro-USDT.

**The signer picks the method from the chain.** `sweep(index)` derives both `D` and `G` from the
index, reads both balances, and sweeps each by its own method — a permit for `G`, today's TRX path
for `D`. So a user created before the switch keeps working, and a user who pays the wrong one of
their two addresses is still swept.

**The receiver gets exactly what was minted.** The permit sends `value = balance − maxFee`. The
relay's real fee is at most `maxFee`; whatever it did not need stays at `G`, counted in the reserve,
and is swept with the next deposit. `maxFee` here is computed exactly as `fee_reserved` in section 2
— from the same configuration and the same on-chain activation check — never from the relay's
`active` field.

**Asynchronous, like funding already is.** Submission returns a `traceId`, not a finished transfer,
so a sweep can end as submitted-and-pending. That matches the two-pass shape the TRX rail already
has — funded on one pass, swept on the next.

**Hygiene:**

- one relay, **pinned** in `GASFREE_SERVICE_PROVIDER` rather than chosen at runtime, because the
  permit names it;
- a short `deadline`, minutes, so a signed permit that was never submitted cannot be used much
  later;
- never two sweeps of one address at once, because they would collide on one `nonce`.

**New outcomes:** `Pending { trace_id }` and `Rejected { reason }`. `Rejected` is what a fee above
`maxFee` produces, and it pages per section 2.

## 4. Payouts and the float

**The float moves to a GasFree address too:** `F = gasfree(the 2/0 address)`. A payout becomes a
permit from `F`, its fee taken from the float. The payout API still takes `to` and `amount` — the
documented exception — and is still bounded by the float balance and the per-transaction cap.

**Every payout pays for itself already.** A redemption keeps back `REDEMPTION_FEE_USDT`. With
`X` redeemed, supply falls by `X` and the float by `X − fee + relay_fee`, so the reserve's lead over
supply changes by `fee − relay_fee`. That is never negative while
`REDEMPTION_FEE_USDT ≥ TRANSFER_MAX`, which becomes an invariant (section 7).

**The float fills itself from deposits.** Topping it up from custody by hand needs TRX in the
operator's wallet, which is the very thing this rail avoids. So a sweep's `receiver` is

```
receiver = F         if F's balance is below PAYOUT_FLOAT_TARGET_USDT
         = custody   otherwise
```

Both destinations are fixed — the float derived, custody configured — so the caller still chooses
nothing. The float fills to its target and stops, which keeps it small, as readiness item B4
requires. **This changes the most sensitive function in the system** and is named here rather than
left implicit.

**The float's one-time activation is paid from surplus, never by a user.** The float's first
outgoing transfer costs `ACTIVATE + TRANSFER`. If a redemption triggered it, the redemption fee
would not cover it and the reserve would drop below supply. So activation is its own one-time
operator step — a typed-confirmation workflow in `clutch-deploy`, like `fund-float.yml` — which
refuses unless `reserve − supply ≥ ACTIVATE_MAX + TRANSFER_MAX`. It makes the float's first
outgoing transfer — the smallest amount the relay accepts, from the float to custody — which is what
causes the relay to deploy the float's contract. Custody gains that amount back; the reserve loses
only the relay's fee, which the surplus already covers, so supply is never left uncovered. Until the
float is activated, redemptions answer "not available yet" rather than failing partway.

**The float address is derived, and provisioning writes it.** The signer derives `F`; the treasury
needs `F` to count it. `provision-treasury-secrets.sh` already reads public material from a
throwaway signer, and gains `F`, writing `PAYOUT_FLOAT_ADDRESS` so the two cannot disagree. A
disagreement would under-count the reserve and trip the breaker — the safe direction — but should
never reach that point.

## 5. Failures, the tripwire, and switching back

**The chain is the truth, not the relay.** A relay reporting pending or done is never trusted alone;
the sweeper checks `G`'s balance on-chain. After a permit's `deadline` it can no longer execute, so
the balance settles it: swept means done, not swept means a fresh permit with the next `nonce`.

**Relay down or slow:** the deposit stays at `G`, credited and counted, is retried on the next pass,
and pages after a threshold — the same shape as an empty fee account today.

**Fee above `maxFee`:** refused, held, paged. See section 2.

**A relay cannot take its fee and skip the transfer.** The permit executes on-chain as one step, or
not at all.

**The tripwire on the accepted risk.** Each pass, the treasury reads the beacon's
`implementation()`. If it differs from `GASFREE_EXPECTED_IMPLEMENTATION`, GasFree has changed the
code holding users' money, and the treasury:

1. stops the orchestrator issuing GasFree addresses to new depositors;
2. stops signing permits;
3. pages.

Money already at a GasFree address is exposed either way; the purpose is to put no more in.

**Switching back to the TRX rail is only half a switch.** New users get plain addresses again, but a
user who already has a GasFree address keeps it — deposit addresses are permanent — so their
deposits still sweep through GasFree, and the third party does not fully leave. Leaving GasFree
entirely means moving those users to new plain addresses, the way per-intent addresses were retired
in `2026-08-30-permanent-deposit-addresses-design.md`. Possible, not free, and out of scope here.

## 6. Configuration

All in the one env file of each deployment (`.env` for the testnet, `.env.mainnet` for mainnet),
so the three services read the same values:

| Variable | Read by | Meaning |
|---|---|---|
| `TRANSFER_RAIL` | orchestrator, treasury, signer | `trx` (default) or `gasfree` |
| `GASFREE_API_KEY`, `GASFREE_API_SECRET` | signer | Relay credentials, from developer.gasfree.io. Per network. |
| `GASFREE_SERVICE_PROVIDER` | signer | The pinned relay |
| `GASFREE_ACTIVATE_FEE_MAX_USDT` | treasury, signer | Held back once per address |
| `GASFREE_TRANSFER_FEE_MAX_USDT` | treasury, signer | Held back per transfer |
| `GASFREE_EXPECTED_IMPLEMENTATION` | treasury | The beacon's `implementation()`, recorded at switch-on |
| `MIN_DEPOSIT_USDT` | treasury | Below this after the fee, nothing is minted |
| `PAYOUT_FLOAT_TARGET_USDT` | signer | Sweeps go to the float until it holds this |

The relay's API authenticates each request with `HMAC-SHA256(METHOD + PATH + TIMESTAMP)` under the
API secret, base64-encoded, sent as `Authorization: ApiKey {key}:{signature}` with a `Timestamp`
header. Base URLs: `https://open.gasfree.io/tron/` and `https://open-test.gasfree.io/nile/`.

The API key cannot move funds — only a permit signed by a deposit key can — so a leaked key costs
rate limit and relay reputation, not money. It still lives only in the host's env file.

## 7. Invariants

`clutch-deploy/scripts/check-cap-invariants.sh` gains, when `TRANSFER_RAIL=gasfree`:

1. `REDEMPTION_FEE_USDT ≥ GASFREE_TRANSFER_FEE_MAX_USDT` — otherwise a payout's relay fee exceeds
   what the redemption kept back, and every redemption lowers the reserve below supply.
2. `MIN_DEPOSIT_USDT > 0` and the maxima positive — a zero maximum would sign permits the relay
   refuses, silently stopping every sweep.

And `PROBE=gasfree` reports whether the **live** fee is at or below each configured maximum. That
comparison uses live data and so belongs in the probe, not in the static script.

## 8. Testing

All in CI; nothing is built on the operator's machine.

- **Address derivation** against the five official vectors in
  `gasfree-sdk-js/src/tests/constant.ts`, asserting the salt, the bytecode hash and the final
  address separately, so a mismatch names the step that is wrong.
- **Permit hash**: a CI job runs the official JavaScript SDK on fixed inputs, and the Rust
  implementation must produce the identical TIP-712 hash. Two independent implementations
  agreeing is a stronger check than either tested alone. A wrong hash fails safe — every permit is
  refused — but makes the rail useless.
- **The reserve rule, directly.** For each case — first deposit, later deposit, two deposits swept
  together, fee above `maxFee`, below the minimum, a redemption, float activation — assert
  `reserve ≥ supply` after the step. It is the property this design exists for.
- **The dangerous directions fail safe:** an orchestrator proposing too much is capped by the
  treasury; the signer refuses a `maxFee` above what was held back; `sweep` still accepts only an
  index.
- **The tripwire fires**, given a wrong `implementation()`.

## 9. Rollout on Nile

In order, with reconciliation reading OK after **every** step:

1. Merge everything with `TRANSFER_RAIL=trx`. Nothing changes; all tests green.
2. Switch the testnet to `gasfree`; record the Nile beacon's `implementation()` in
   `GASFREE_EXPECTED_IMPLEMENTATION`.
3. **One real deposit** to a new GasFree address. This resolves the first open question below
   before anything else depends on it.
4. Activate the float, once, from surplus.
5. **One real redemption.**

**Done means** a deposit swept to custody with **zero TRX** in the fee account, custody rising by
exactly the minted amount, a redemption paid from the GasFree float, and reconciliation OK
throughout.

Mainnet follows only after that, with its own API key and its own fee reading.

## Open questions, resolved on Nile before code depends on them

1. **What `value` means.** Whether it is what the receiver gets with the fee charged on top, or the
   total with the fee taken out of it. The reserve rule holds either way; the number written into
   the permit differs. Section 3 assumes the first and must be corrected if the measurement says the
   second.
2. **The Nile beacon's `implementation()`**, which has not been read yet.
3. **Mainnet fees**, which need a mainnet API key.
4. **Who controls the beacon upgrade.** `owner()` reverts; the authority may be readable through
   another interface. Worth knowing; not a blocker, since the tripwire does not depend on it.

## Out of scope

- Moving existing users off GasFree addresses, if the rail is ever switched back.
- Choosing among several relays at runtime, or failing over between them.
- Batching several addresses into one permit.
- Any change to how custody itself is spent. Nothing in this stack can move custody, and this
  design adds nothing that can.
