-- BNB Smart Chain deposit addresses (docs/superpowers/specs/2026-10-29-bsc-deposit-addresses-design.md):
-- each user's own BSC address, derived from the BSC account xpub at the same index as their TRON
-- address (`derivation_index`), so one index serves both chains. Set the first time the deposit panel
-- asks while BSC deposit addresses are on; never changed after. 0x + 40 lowercase hex.
ALTER TABLE deposit_addresses ADD COLUMN bsc_address TEXT UNIQUE;
