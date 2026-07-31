-- The address a deposit was expected at, per intent.
--
-- Deposits no longer share one custody address: the orchestrator derives one per intent from an
-- account xpub, so "was this deposit paid?" is a question about THAT address, not a global one. The
-- verifier is the approver in the four-eyes split, so it must check the address the intent names
-- rather than a value from its own config — otherwise it approves on evidence gathered at an
-- address that no longer receives anything.
--
-- Same shape and reasoning as 0003's expected_amount_usdt: nullable, because Plan B's
-- human-created intents (client_ref IS NULL) have no deposit to point at, but MANDATORY for
-- deposit-backed ones. A deposit-backed intent with no address is unverifiable, and an unverifiable
-- intent must never be approvable.
ALTER TABLE mint_intents ADD COLUMN deposit_address TEXT;

-- NOT VALID, deliberately, exactly as 0003 did: the constraint binds every future write without
-- demanding a rewrite of history that cannot satisfy it. Pre-existing deposit-backed rows were
-- created when one shared custody address was the only answer, and there is nothing truthful to
-- backfill — the verifier refuses them explicitly instead.
ALTER TABLE mint_intents
    ADD CONSTRAINT deposit_backed_needs_address
    CHECK (client_ref IS NULL OR deposit_address IS NOT NULL) NOT VALID;

-- One intent per address. A second intent naming an address that already backs another would let
-- one transfer be presented as evidence twice — the same double-credit hazard 0002's
-- uq_mint_intents_deposit_tx closes for tx ids, arrived at from the address side.
CREATE UNIQUE INDEX uq_mint_intents_deposit_address ON mint_intents (deposit_address)
    WHERE deposit_address IS NOT NULL;
