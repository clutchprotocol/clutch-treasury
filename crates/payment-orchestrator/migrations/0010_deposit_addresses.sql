-- One permanent address per user, replacing one address per deposit.
--
-- The derivation index still comes from deposit_derivation_index_seq, but is consumed once per
-- USER and kept. The sequence is never reset: legacy per-intent addresses already hold issued
-- indexes, and reusing one would hand a new user an address a previous deposit was sent to.
CREATE TABLE deposit_addresses (
    user_pk          TEXT PRIMARY KEY,
    derivation_index BIGINT NOT NULL UNIQUE,
    address          TEXT   NOT NULL UNIQUE,
    -- Where the CLT is minted. Previously the user supplied this per deposit; now the chain creates
    -- the row and nothing would carry it, so it is captured once when the address is issued. Without
    -- it a credited deposit has no destination and the mint cannot be built.
    clt_address      TEXT   NOT NULL,
    -- Set when the user opens the deposit panel: the moment they are about to send. The poller
    -- serves hot addresses first, so the common case stays near-real-time without polling every
    -- address every pass.
    hot_until        TIMESTAMPTZ,
    -- Stamped after each poll. Doubles as the rotation key for cold addresses and as the
    -- min_timestamp bound, so a long-lived address does not re-fetch its whole history.
    last_polled_at   TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Identity moves from address to TRANSACTION. The treasury already enforces the same thing with
-- uq_mint_intents_deposit_tx; this is the orchestrator's half, and it is what makes address reuse
-- safe: two transfers to one address are two credits, and the same transfer seen twice is one.
CREATE UNIQUE INDEX uq_deposit_intents_tron_tx_id
    ON deposit_intents (tron_tx_id) WHERE tron_tx_id IS NOT NULL;
