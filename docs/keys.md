# Key custody status

- mint authority: ENV-VAR STUB (testnet only; user decision 2026-07-27).
  `ChainSigner` trait in crates/clutch-chain/src/signer.rs is the swap boundary.
- MAINNET BLOCKER: implement `KmsSigner` (AWS KMS ECC_SECG_P256K1, alloy-signer-aws
  pattern) + key ceremony + tested recovery BEFORE any real-funds deployment.
- Keys per function (spec §5): mint / reserve custody / payout initiation are THREE
  SEPARATE keys, none interchangeable. Only the mint key exists in this service.
  `PayoutRail` (crates/treasury-service/src/payout.rs) is the trait boundary for the
  payout key exactly as `ChainSigner` is for the mint key, but `StubRail` — its only
  implementor — signs nothing and holds no key material; it logs and returns a fake
  `stub:{uuid}` reference. The custody key and the payout key BOTH arrive with the real
  Tron rail (Plan C follow-on): the payout key signs outbound TRC-20 transfers, the
  custody key is a separate hot/cold reserve-custody key never used for payouts. Neither
  exists yet.
