-- payment-orchestrator's own tables, own migrations directory, own migrate! invocation
-- (tests/db_deposits.rs), all independent of treasury-service's. In real dev/prod the
-- two services already run against separate Postgres databases (.env.example), so plain
-- sequential 0001, 0002, ... numbering here is fine — sqlx's _sqlx_migrations bookkeeping
-- table only needs to be unique WITHIN a database, never across services. (The test rig's
-- shared single database was the one place that assumption briefly broke; the fixture in
-- tests/db_deposits.rs gives this crate its own database there too, for the same reason.)
CREATE TABLE deposit_intents (
    id UUID PRIMARY KEY,
    user_pk TEXT NOT NULL,
    clt_address TEXT NOT NULL,
    amount_usdt BIGINT NOT NULL CHECK (amount_usdt > 0),      -- what the user asked to deposit
    pay_amount_usdt BIGINT NOT NULL,                          -- amount + discriminator
    amount_clt BIGINT NOT NULL,                               -- == amount_usdt (par); credited on confirm
    status TEXT NOT NULL DEFAULT 'created' CHECK (status IN
        ('created','invoiced','paying','confirmed','mint_requested','credited',
         'expired','failed','needs_manual')),
    client_key TEXT NOT NULL,
    invoice_id TEXT,
    tron_tx_id TEXT,
    response_status SMALLINT,          -- original HTTP status for idempotent replay
    response_body JSONB,               -- stored response for idempotent replay
    bitcart_terminal BOOLEAN NOT NULL DEFAULT FALSE,  -- invoice terminal on Bitcart's side
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_pk, client_key)
);
-- Discriminator slot frees ONLY when the Bitcart invoice is terminal — our soft
-- 'expired' still honours late payments, so the amount must stay reserved until
-- Bitcart itself can no longer match a payment to it (cross-user misattribution
-- on the static address otherwise).
CREATE UNIQUE INDEX uq_active_pay_amount ON deposit_intents (pay_amount_usdt)
    WHERE status IN ('created','invoiced','paying','expired') AND NOT bitcart_terminal;

CREATE TABLE webhook_events (
    id BIGSERIAL PRIMARY KEY,
    provider TEXT NOT NULL,
    event_key TEXT NOT NULL UNIQUE,    -- bitcart: "{invoice_id}:{status}"
    payload JSONB NOT NULL,
    processed BOOLEAN NOT NULL DEFAULT FALSE,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
