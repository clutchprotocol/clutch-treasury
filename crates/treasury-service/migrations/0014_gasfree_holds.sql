-- GasFree deposits (docs/superpowers/specs/2026-09-24-gasfree-transfer-rail-design.md §2).
--
-- A GasFree sweep pays the relay out of the USDT it moves, so a deposit to a GasFree account mints
-- less than arrived. This is what was held back. NULL for a deposit to a plain address, which pays
-- no fee out of the USDT; the sweeper tells the two kinds of deposit apart by it.
ALTER TABLE mint_intents
    ADD COLUMN fee_held_usdt BIGINT CHECK (fee_held_usdt IS NULL OR fee_held_usdt >= 0);

-- One row per GasFree account the treasury has verified a deposit at.
CREATE TABLE gasfree_accounts (
    derivation_index  BIGINT PRIMARY KEY,
    -- G, where the user pays. Counted in the reserve for as long as the row exists: a sweep leaves
    -- the relay's unused margin there.
    gasfree_address   TEXT NOT NULL UNIQUE,
    -- D, the plain address of the same index: the permit's `user`. Its nonce on the GasFree
    -- controller says whether a permit ran.
    owner_address     TEXT NOT NULL,
    -- When the treasury saw ITS OWN first sweep of this account run. Until then every deposit here
    -- holds back the activation fee as well: a sweep asked for some other way can activate the
    -- account before a deposit it moved was credited.
    first_transfer_at TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
