# BEP-20 USDT on BNB Smart Chain — design

Status: phase 1 (deposits from a wallet, stage only) being built; phases 2 and 3 are proposals
Date: 2026-10-23
Scope: `clutch-treasury` (`treasury-service`, `payment-orchestrator`), `clutch-hub` (the deposit
panel), `clutch-deploy` (settings, probe). Not touched: `clutch-node`, the CLT genesis, the mint key.

## Problem

A user's first top-up on TRON costs $4.00 in relay fees (GasFree's maximum, held back from the
USDT), and $2.00 after that. For a $10 or $20 top-up that is most of the point of the app. On BNB
Smart Chain a USDT transfer costs a few cents of BNB, and the people who already use MetaMask or
Trust Wallet usually hold BSC USDT already.

## Decision

Add BSC as a second network USDT can arrive on, in three phases. Phase 1 is the only one built now.

| Phase | What | Key on the server | Needs Mehran |
|---|---|---|---|
| 1 | **Deposits from the user's own wallet** to one BSC pay-in address, credited by sender | none | a pay-in address (stage: testnet) |
| 2 | Withdrawals in BSC USDT, from the pay-in address | yes: a separate BSC key, ideally KMS | go, key ceremony |
| 3 | Per-user BSC deposit addresses, for exchange withdrawals | yes: a separate mnemonic, plus a BNB gas float | go |

### Why phase 1 is "credit by sender", not per-user addresses

On BSC a user's wallet address **is** their CLT address: the same key, the same 20 bytes, the same
`0x` spelling. A Transfer of BSC USDT *from* `0xX` can only have been signed by whoever holds the key
of CLT address `0xX`, so minting its value to `0xX` pays the person who paid. This is the argument the
TronLink direct top-up (`direct.rs`, clutch-treasury #73) already relies on, with one difference: no
base58 conversion is needed, the addresses are literally equal.

That buys a lot:

- **No per-user addresses, so nothing to sweep.** The money lands at the pay-in address directly.
- **No BNB gas float.** Sweeping per-user addresses would need BNB sent to each one first (there is
  no GasFree on BSC). The user pays their own transfer's gas, a few cents.
- **No key on the server.** Receiving needs only an address. The pay-in address's key stays off the
  host until phase 2 decides where it lives, so phase 1 adds no hot wallet.

The cost: an exchange withdrawal is sent from the exchange's wallet, not the user's, and cannot be
credited this way. That is phase 3. Same "ask first" rule as TronLink: a transfer is credited only
when its sender pressed "Pay with BNB Chain" in the app before sending. Anything else that arrives is
recorded once, raises one p1 (at 1 USDT and above) and is left for a human to refund, never credited
later.

## Phase 1 in detail

### Flow

1. The user, signed in with an Ethereum wallet (MetaMask, Trust Wallet, WalletConnect), opens the
   deposit panel and picks **BNB Chain (BEP-20)**.
2. The app calls `POST /payment/api/v1/bsc-topups`. The orchestrator records `bsc_payers(user_pk,
   address, created_at)` and answers with the network: chain id, pay-in address, USDT contract,
   decimals, minimum.
3. The app asks the wallet to switch to that chain (adding it if missing) and to send
   `transfer(pay_in, amount × 10^decimals)` on the USDT contract. The user signs once.
4. The orchestrator's BSC watcher reads `Transfer(*, pay_in)` logs on the USDT contract with
   `eth_getLogs`, up to `head − confirmations`, and credits each transfer whose sender has a payer row
   older than the transfer's block. A deposit row is written with `network = 'bsc'`, the BSC tx hash as
   its id, and the sender as `direct_from`.
5. The bridge creates the treasury mint intent as for any deposit, adding `"network": "bsc"`.
6. The treasury verifies on its own, through its own RPC (four-eyes: the orchestrator proposes, the
   verifier approves): the receipt exists and succeeded, it is at least `confirmations` deep, and the
   USDT Transfer logs in it from `direct_from` to the pay-in address (both from the treasury's own
   settings and the intent's beneficiary, never from the request) add up to at least what was
   proposed. `direct_from` must equal the beneficiary. It mints what arrived, never more.

### Decimals

BSC USDT has 18 decimals; CLT and TRON USDT have 6. One micro-USDT is 10^12 base units. The credit is
`floor(value / 10^12)`: the remainder, below a millionth of a dollar, stays in the reserve as surplus.
Flooring can only under-credit, which is the safe direction; refusing such a transfer would strand the
whole payment instead. Decimals are a setting (default 18) because testnet tokens differ; the probe
reads `decimals()` from the contract so a wrong setting is visible.

### Confirmations

Default 15 blocks. BSC has fast finality (two blocks, a couple of seconds); 15 is a margin, not a
requirement, and costs about ten seconds.

### Caps and fees

Unchanged and shared with TRON: the same per-deposit and per-day mint caps (they bound CLT, which
does not care where the USDT came from), the same minimum deposit, the same 1% service fee (not on a
user's first top-up). No relay fee, because there is no relay.

### Reserve accounting

Reconciliation adds the pay-in address's BSC USDT balance (floored to micro-USDT) to the reserve. An
unreadable BSC balance fails the run, exactly like an unreadable TRON one: a partial sum would read
as a shortfall. The reserve is then split across two chains, and the treasury has no way to move
USDT between them; that matters only once withdrawals exist (phase 2).

### Settings

Both services, env only, all-or-nothing (a half-set group refuses to start):

| Variable | Example (stage) |
|---|---|
| `APP_BSC_TOPUPS_ENABLED` | `true` |
| `APP_BSC_RPC_URL` | a BSC testnet RPC |
| `APP_BSC_CHAIN_ID` | `97` (mainnet `56`) |
| `APP_BSC_USDT_CONTRACT` | the testnet USDT token; mainnet `0x55d398326f99059ff775485246999027b3197955` |
| `APP_BSC_USDT_DECIMALS` | `18` |
| `APP_BSC_PAY_IN_ADDRESS` | an address whose key is held off the host |
| `APP_BSC_CONFIRMATIONS` | `15` |

### Database

- treasury: `mint_intents.network TEXT NOT NULL DEFAULT 'tron'`, checked to be `tron` or `bsc`.
- orchestrator: `deposit_intents.network` (same), `bsc_payers`, `bsc_unattributed`, and `bsc_watch`
  (the last block read). The BSC tx hash goes in `tron_tx_id`, whose unique index then covers both
  chains; a BSC hash (`0x` + 64 hex) can never equal a TRON id (64 hex).

### What phase 1 does not do

No sweeps, no BSC payouts, no BNB anywhere on the server, no mainnet. Withdrawals stay TRON only, and
the redemption panel says so. A BSC deposit is redeemable in TRON USDT like any other CLT, which is
fine while the TRON float can cover it, and is the main thing phase 2 has to address.

## Phase 2 (proposal): withdrawals on BSC

The user picks the network when withdrawing; payouts leave the BSC pay-in address. Needs:

- A key for the pay-in address on the host. Use a **separate** secret, never a path on
  `DEPOSIT_MNEMONIC`, so one leak does not take both chains. BSC signs Ethereum-style (secp256k1 over
  Keccak), which is what `AzureKmsSigner` already does for the CLT chain, so this key can sit in KMS
  from day one, unlike the TRON float (readiness A2).
- BNB on the pay-in address for gas, about $0.05 a payout. The one manual top-up this rail has.
- Per-chain payout caps and a per-chain float check: a BSC withdrawal can fail while TRON holds
  plenty. Rebalancing between chains is a manual exchange step.

## Phase 3 (proposal): per-user deposit addresses

For users who withdraw from an exchange. One address per user at `m/44'/60'/0'/0/i` of a new
mnemonic, a sweep to the pay-in address after confirmation, and a BNB float that tops up each address
before its sweep (the old TRX rail's pattern), or a CREATE2 forwarder contract that sweeps without
per-address gas. Bigger than phases 1 and 2 together; worth it only if exchange users ask.

## Risks

1. **Second issuer.** BSC USDT is Binance-Peg BSC-USD, backed by Binance, not issued by Tether. The
   reserve then depends on two issuers.
2. **Unattributed transfers.** Users who skip the button, or pay from an exchange, need a refund by
   hand. The p1 says so, and the panel says "send only from this wallet".
3. **Wrong chain.** Someone may send ERC-20 USDT on Ethereum to the same address. Nothing watches
   Ethereum; that money is recoverable only with the pay-in key (it is the same address there). The
   panel names the network and the app switches the wallet itself.
