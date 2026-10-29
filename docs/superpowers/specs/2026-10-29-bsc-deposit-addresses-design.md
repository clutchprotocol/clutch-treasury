# BNB Chain deposit addresses — design

Status: being built, stage first. Phase 3 of `2026-10-23-bsc-usdt-design.md`.
Date: 2026-10-29
Asked for by: Mehran ("why does BNB Chain need an amount, why can't it work like Mpay": a QR code and an
address the user sends any amount to, from any wallet or exchange). He chose this over a QR code for
the shared pay-in address, which would still credit only the user's own wallet.
Scope: `tron-signer`, `payment-orchestrator`, `treasury-service`, the `bsc` crate; `clutch-hub` (the
deposit panel); `clutch-deploy` (secret, xpub, compose, probe). Not touched: `clutch-node`, the CLT
genesis, the mint key, the TRON rail.

## What the user sees

Wallet → Top up → USDT → BNB Chain shows **their own BSC address and its QR code**. They send any
amount of BEP-20 USDT to it, from any wallet or any exchange. A minute or two later it shows under
Recent deposits, then as CLT. No amount to type, no button to press first, no Clutch fee. The address
never changes.

## Shape

The TRON rail's permanent deposit addresses (`2026-08-30-permanent-deposit-addresses-design.md`),
copied to BSC, with the TRX rail's "fund the address, then sweep it" pattern for gas:

| Piece | TRON today | BSC |
|---|---|---|
| Secret | `DEPOSIT_MNEMONIC`, signer only | **`BSC_DEPOSIT_MNEMONIC`**, a second, separate mnemonic, signer only |
| User address | `m/44'/195'/0'/0/i` | `m/44'/60'/0'/0/i` (MetaMask's own path, so the mnemonic opens in MetaMask) |
| Index `i` | `deposit_addresses.derivation_index` | **the same `i`**: one index per user, two chains |
| Orchestrator holds | the account xpub | `BSC_DEPOSIT_ACCOUNT_XPUB` (`m/44'/60'/0'`), no key |
| Detect | TronGrid, per address | `eth_getLogs` for USDT `Transfer(*, addr)`, all addresses in one filter (chunks), `confirmations` deep |
| Credit | to the address's owner, any sender | the same |
| Verify (treasury) | its own TronGrid read + signer's address for `i` | its own BSC node + signer's BSC address for `i` |
| Sweep to | the float (one wallet) | **`BSC_PAY_IN_ADDRESS`**, Mehran's wallet, key **off** the host |
| Gas | GasFree relay fee | BNB from a gas account `m/44'/60'/0'/1/0`, sent to the address just before its sweep |

### Why a separate mnemonic

`DEPOSIT_MNEMONIC` already holds the whole TRON reserve (one wallet). Deriving BSC keys from it would
mean one leak takes both chains. A separate one keeps them apart, and using the standard Ethereum path
means a human can recover any deposit address by importing it into MetaMask. It is a second secret to
back up: losing it strands whatever is unswept, and every address users were shown keeps receiving
money nobody can move.

### Why the same index

A user already has an index the day they open the deposit panel. Reusing it needs no new sequence and
no new allocation race; the BSC address is derived from it and stored next to the TRON one
(`deposit_addresses.bsc_address`).

### Why sweep to the pay-in address

It is Mehran's own wallet and its key is not on the host. So the most a full compromise of the host can
take on BSC is what sits at deposit addresses between arrival and sweep (seconds to minutes) plus the
gas account's BNB. Every BSC dollar that has been swept is out of reach. The pay-in address is also
what phase 1 already counts in the reserve, so reconciliation does not change shape.

## The money path, step by step

1. **Address.** `POST /api/v1/deposits` answers `bsc_address` next to the TRON address while BSC
   deposit addresses are on (`APP_BSC_DEPOSIT_ACCOUNT_XPUB` set and BSC on). It is derived from the
   xpub at `0/i` and stored once.
2. **Detect.** Each watcher pass reads the same confirmed block range it reads for the pay-in address,
   and also `Transfer(*, a)` for every stored BSC deposit address `a` (100 per `eth_getLogs`). A
   transaction's Transfers to one deposit address are summed. One deposit row per transaction:
   `network = 'bsc'`, the address and index, `direct_from` empty, the BSC hash as its id. A transfer
   from the pay-in address, or of less than a micro-USDT, is ignored. A transaction that pays two of
   our users (an exchange batch) credits the first; the second is recorded as unattributed with a p1,
   because the transaction hash is the one idempotency key both services have.
3. **Verify.** The treasury asks the signer for the BSC address of index `i` and refuses unless it is
   the intent's address (an address no sweep can move must never back CLT). Then, through its own BSC
   node on the configured chain: the receipt succeeded, it is `confirmations` deep, and its USDT
   Transfers to that address add up to at least what was proposed. It mints what arrived. Below the
   minimum deposit it holds for a human, like every other path.
4. **Sweep.** The treasury's sweeper sends `POST /internal/bsc/sweep {index}` for each credited,
   unswept BSC address (any balance: a sweep costs a fraction of a cent). The signer:
   - reads the address's USDT; none → `nothing_to_sweep`;
   - prices the transfer: `eth_gasPrice` (refused above 10 gwei) × `eth_estimateGas` + 25% (refused
     above 150,000 gas);
   - if the address lacks that much BNB, sends exactly the difference from the gas account and waits
     for it (or answers `gas_account_dry` with the gas address and the amounts);
   - sends `transfer(pay_in, whole balance)` from the address, signed EIP-155 for the configured
     chain, and waits up to 30 s for the receipt: success → `swept`; reverted → `failed`; no receipt
     yet → `failed` ("not mined yet"), and the next pass finds the address empty and records it swept.
   - refuses while the address or the gas account has a transaction pending (`busy`).
   Like the TRON sweep, the request carries **only an index**: the destination, the token and the
   chain are the signer's own settings.
5. **Reserve.** Reconciliation adds the USDT still at unswept BSC deposit addresses to the pay-in
   balance. An unreadable balance fails the run, as on TRON.

## Costs

BSC gas is about 0.1 gwei today. A deposit costs two transactions, the BNB top-up (21,000 gas) and the
USDT transfer (about 35,000–55,000): roughly 0.00001 BNB, under a cent. The gas account needs a manual
top-up: 0.01 BNB covers about a thousand deposits. `PROBE=bsc` prints its address and balance; a dry
account raises one warning an hour, deposits are still credited and counted, and sweeps resume when it
is topped up. Nothing is charged to the user.

## Settings

Signer: `APP_BSC_DEPOSIT_MNEMONIC` (optional; with it, BSC sweeps are on and the `APP_BSC_*` settings
are required) and the shared `APP_BSC_*`. Orchestrator: `APP_BSC_DEPOSIT_ACCOUNT_XPUB` (optional; with
it, users get BSC addresses). Treasury: nothing new; it verifies and sweeps a BSC deposit-address
intent whenever one exists. The signer prints the xpub and the gas address at start and at
`GET /internal/bsc/xpub`; clutch-deploy's provisioning writes the xpub, as it does for TRON.

## Database

- orchestrator: `deposit_addresses.bsc_address TEXT UNIQUE`.
- treasury: the `bsc_is_direct` check is widened: a BSC intent is either a direct top-up (as before)
  or a deposit-address one (`direct_from` empty, `deposit_address` and `derivation_index` set).

## What stays as it is

Phase 1 (pay from your own wallet to the pay-in address, credited by sender) keeps working for anyone
who registered, so nothing in flight is stranded. Withdrawals are still paid on TRON only. Mainnet
stays on phase 1 until Mehran says go: it needs the mainnet mnemonic, its xpub, and BNB in the
mainnet gas account.

## Risks

1. **A second hot secret.** Bounded by the sweep: deposit addresses hold money only until the next
   pass. Back the mnemonic up off the host like `DEPOSIT_MNEMONIC`.
2. **Gas account empty.** Deposits are credited and counted but not swept; one alert an hour.
3. **Wrong token or chain.** Anything other than USDT on BSC sent to a deposit address is not
   credited. It is recoverable with the mnemonic (same address on every EVM chain), by hand.
4. **BNB left behind.** The estimate's margin stays at the address, a few hundred-thousandths of a
   BNB per user. It is spent by that user's next sweep, because only the difference is ever sent.
