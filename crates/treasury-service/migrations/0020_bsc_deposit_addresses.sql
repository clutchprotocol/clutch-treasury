-- BNB Smart Chain deposit addresses (docs/superpowers/specs/2026-10-29-bsc-deposit-addresses-design.md):
-- a BSC intent is now one of two shapes. Either a direct top-up from the beneficiary's own wallet to the
-- pay-in address (as before: `direct_from` is the beneficiary, no address), or a deposit at the user's
-- own BSC address (no `direct_from`; the address and its index, which the verifier checks with the
-- signer and the sweeper sweeps by).
ALTER TABLE mint_intents DROP CONSTRAINT bsc_is_direct;

ALTER TABLE mint_intents
    ADD CONSTRAINT bsc_shape
    CHECK (
        network <> 'bsc'
        OR (direct_from IS NOT NULL AND lower(direct_from) = lower(beneficiary) AND deposit_address IS NULL)
        OR (direct_from IS NULL AND deposit_address IS NOT NULL AND derivation_index IS NOT NULL)
    );
