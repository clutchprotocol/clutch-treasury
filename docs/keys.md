# Key custody status

- mint authority: ENV-VAR STUB (testnet only; user decision 2026-07-27).
  `ChainSigner` trait in crates/clutch-chain/src/signer.rs is the swap boundary.
- MAINNET BLOCKER: implement `KmsSigner` (AWS KMS ECC_SECG_P256K1, alloy-signer-aws
  pattern) + key ceremony + tested recovery BEFORE any real-funds deployment.
- Keys per function (spec §5): mint / reserve custody / payout initiation are THREE
  SEPARATE keys, none interchangeable. Two now exist in some form.
  The PAYOUT key is a hot float derived from the deposit mnemonic at
  `m/44'/195'/0'/2/0`, held only by tron-signer, spendable only via
  `POST /internal/payout`, and bounded by its own balance plus a per-tx cap. It is
  deliberately NOT the custody key: an operator tops it up from custody, and its
  balance is the ceiling on what a compromised treasury-service could move.
  The CUSTODY key still does not exist in this stack — nothing here can spend from
  APP_TREASURY_ADDRESS, which is why the float exists at all.
  `PayoutSigner` (crates/treasury-service/src/payout.rs) is the swap boundary for a
  KMS-backed payout key, exactly as `ChainSigner` is for the mint key.
