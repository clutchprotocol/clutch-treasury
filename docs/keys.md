# Key custody status

- mint authority: ENV-VAR STUB (testnet only; user decision 2026-07-27).
  `ChainSigner` trait in crates/clutch-chain/src/signer.rs is the swap boundary.
- MAINNET BLOCKER: implement `KmsSigner` (AWS KMS ECC_SECG_P256K1, alloy-signer-aws
  pattern) + key ceremony + tested recovery BEFORE any real-funds deployment.
- Keys per function (spec §5): mint / reserve custody / payout initiation are SEPARATE
  keys. Only the mint key exists in this service; custody and payout keys arrive with
  the real Tron rail (Plan C follow-on).
