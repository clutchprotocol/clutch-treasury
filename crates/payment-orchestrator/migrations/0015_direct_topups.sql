-- TronLink direct top-ups (src/direct.rs): a user pays USDT from their own TRON address straight to
-- the treasury's pay-in address, and is credited by the SENDER.
--
-- A TRON address and a CLT address made from the same key carry the same 20 bytes, so the sender of
-- a transfer from tron(X) can only be whoever holds the key of CLT address X. That is why no proof
-- beyond the login is needed. A payer row exists only once the user has asked for a direct top-up in
-- the app: an exchange's hot wallet, or anyone else who never asked, is not credited automatically.
CREATE TABLE direct_payers (
    user_pk      TEXT PRIMARY KEY,
    -- base58check(0x41 || the 20 bytes of clt_address). One user, one sender.
    tron_address TEXT NOT NULL UNIQUE,
    clt_address  TEXT NOT NULL,
    -- Only transfers that landed after this are credited. One that arrived before the user asked was
    -- reported as unattributed and may already have been refunded by hand.
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The read watermark of the pay-in address, the same idea as deposit_addresses.last_polled_at: it
-- only advances after a successful read.
CREATE TABLE direct_watch (
    pay_to         TEXT PRIMARY KEY,
    last_polled_at TIMESTAMPTZ
);

-- Transfers to the pay-in address that no payer row claims, recorded once so the alert fires once
-- and so they are never credited later: a human decides what happens to that money.
CREATE TABLE direct_unattributed (
    tron_tx_id   TEXT PRIMARY KEY,
    from_address TEXT NOT NULL,
    amount_usdt  BIGINT NOT NULL,
    seen_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Set on a deposit that was a direct top-up: the TRON address that paid. Such a row has no deposit
-- address and no derivation index, because nothing is swept: the money is already at the pay-in
-- address.
ALTER TABLE deposit_intents ADD COLUMN direct_from TEXT;
