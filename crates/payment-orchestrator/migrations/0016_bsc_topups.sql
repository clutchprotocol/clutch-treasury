-- BEP-20 USDT top-ups on BNB Smart Chain, phase 1 (src/bsc_watch.rs,
-- docs/superpowers/specs/2026-10-23-bsc-usdt-design.md): a user pays USDT from their own wallet to
-- one BSC pay-in address and is credited by SENDER, like a TronLink direct top-up. On BSC the sending
-- address and the CLT address are the same 20 bytes in the same spelling, so no conversion is needed.

-- Which chain a deposit arrived on. Every row before this one is TRON. A BSC row's `tron_tx_id` holds
-- the BSC transaction hash (0x + 64 hex), which can never equal a TRON id (64 hex), so the unique
-- index on that column keeps one credit per transaction across both chains.
ALTER TABLE deposit_intents ADD COLUMN network TEXT NOT NULL DEFAULT 'tron' CHECK (network IN ('tron', 'bsc'));

-- Users who pressed "Pay with BNB Chain". Only transfers from `address` that landed after
-- `created_at` are credited, for the reason direct_payers gives.
CREATE TABLE bsc_payers (
    user_pk    TEXT PRIMARY KEY,
    -- 0x + 40 lowercase hex: the user's CLT address, which is also their BSC address.
    address    TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The last BSC block read for the pay-in address. It only advances after a whole range was handled.
CREATE TABLE bsc_watch (
    pay_in     TEXT PRIMARY KEY,
    last_block BIGINT NOT NULL
);

-- Transfers to the pay-in address that no payer claims: recorded once so the alert fires once, and
-- never credited later, because a human decides what happens to that money.
CREATE TABLE bsc_unattributed (
    tx_hash      TEXT PRIMARY KEY,
    from_address TEXT NOT NULL,
    amount_usdt  BIGINT NOT NULL,
    seen_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
