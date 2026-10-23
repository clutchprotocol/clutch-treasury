-- BEP-20 USDT on BNB Smart Chain, phase 1 (docs/superpowers/specs/2026-10-23-bsc-usdt-design.md):
-- which chain a deposit-backed intent's USDT arrived on. Every row before this one is TRON.
--
-- A BSC deposit is a direct top-up from the user's own wallet: `direct_from` is the sender, which is
-- the beneficiary itself (a BSC address and a CLT address are the same 20 bytes in the same
-- spelling), and `deposit_tx_id` is the BSC transaction hash (0x + 64 hex), which can never equal a
-- TRON id (64 hex), so uq_mint_intents_deposit_tx covers both chains.
ALTER TABLE mint_intents ADD COLUMN network TEXT NOT NULL DEFAULT 'tron' CHECK (network IN ('tron', 'bsc'));

ALTER TABLE mint_intents
    ADD CONSTRAINT bsc_is_direct
    CHECK (network <> 'bsc' OR (direct_from IS NOT NULL AND lower(direct_from) = lower(beneficiary)));
