-- TronLink direct top-ups (tron_verifier.rs, `evaluate_direct`): a user pays from their own TRON
-- address straight to the GasFree float, so the intent has no deposit address and no derivation
-- index. `direct_from` is the TRON address that paid; the verifier mints only if it is the TRON
-- address of the beneficiary (the same 20 bytes, so the same key) and the transfer reached the float.
ALTER TABLE mint_intents ADD COLUMN direct_from TEXT;

-- A deposit-backed intent needs somewhere its evidence is: a deposit address, or a direct sender.
ALTER TABLE mint_intents DROP CONSTRAINT deposit_backed_needs_address;
ALTER TABLE mint_intents
    ADD CONSTRAINT deposit_backed_needs_address
    CHECK (client_ref IS NULL OR deposit_address IS NOT NULL OR direct_from IS NOT NULL) NOT VALID;

-- A direct top-up is never swept (nothing to sweep: the money is at the float already), and is
-- verified by its transaction alone.
ALTER TABLE mint_intents
    ADD CONSTRAINT direct_topup_shape
    CHECK (direct_from IS NULL OR (deposit_address IS NULL AND derivation_index IS NULL AND deposit_tx_id IS NOT NULL));
