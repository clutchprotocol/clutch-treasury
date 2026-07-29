-- Append-only reserve ledger. ponytail: event log + views, not double-entry postings;
-- upgrade path = Cala/Formance if auditors require. Invariants live in triggers/uniques.
CREATE TABLE treasury_events (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN
        ('mint_executed','burn_redeemed','custody_deposit','custody_withdrawal','buffer_topup')),
    amount_clt BIGINT NOT NULL DEFAULT 0 CHECK (amount_clt >= 0),
    amount_usdt BIGINT NOT NULL DEFAULT 0 CHECK (amount_usdt >= 0),
    intent_id UUID,
    chain_tx_hash TEXT,
    description TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX uq_events_intent_kind ON treasury_events (intent_id, kind)
    WHERE intent_id IS NOT NULL;

CREATE FUNCTION forbid_mutation() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'treasury_events is append-only';
END $$ LANGUAGE plpgsql;
CREATE TRIGGER treasury_events_append_only
    BEFORE UPDATE OR DELETE ON treasury_events
    FOR EACH ROW EXECUTE FUNCTION forbid_mutation();

-- ::BIGINT casts required: SUM(BIGINT) returns NUMERIC, which sqlx cannot decode
-- into i64 without the bigdecimal feature.
CREATE VIEW ledger_balances AS SELECT
    (COALESCE(SUM(amount_clt) FILTER (WHERE kind = 'mint_executed'), 0)
   - COALESCE(SUM(amount_clt) FILTER (WHERE kind = 'burn_redeemed'), 0))::BIGINT AS clt_liability,
    (COALESCE(SUM(amount_usdt) FILTER (WHERE kind IN ('custody_deposit','buffer_topup')), 0)
   - COALESCE(SUM(amount_usdt) FILTER (WHERE kind = 'custody_withdrawal'), 0))::BIGINT AS custody_usdt
FROM treasury_events;

CREATE TABLE mint_intents (
    id UUID PRIMARY KEY,
    beneficiary TEXT NOT NULL,
    amount_clt BIGINT NOT NULL CHECK (amount_clt > 0),
    status TEXT NOT NULL DEFAULT 'created' CHECK (status IN
        ('created','approved','submitted','credited','failed','rejected')),
    credit_ref CHAR(64) NOT NULL UNIQUE,
    created_by TEXT NOT NULL,
    approved_by TEXT,
    chain_tx_hash TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Four-eyes in the schema, not just policy (spec §5).
    CONSTRAINT four_eyes CHECK (approved_by IS NULL OR approved_by <> created_by)
);

CREATE TABLE redemption_intents (
    id UUID PRIMARY KEY,
    redeemer_address TEXT NOT NULL,
    payout_address TEXT NOT NULL,
    amount_clt BIGINT NOT NULL CHECK (amount_clt > 0),
    status TEXT NOT NULL DEFAULT 'created' CHECK (status IN
        ('created','burn_confirmed','payout_pending','paid','expired','failed')),
    redemption_ref CHAR(64) NOT NULL UNIQUE,
    burn_tx_hash TEXT,
    payout_ref TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE chain_outbox (
    id BIGSERIAL PRIMARY KEY,
    intent_id UUID NOT NULL UNIQUE REFERENCES mint_intents(id),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN
        ('pending','submitted','confirmed','failed')),
    attempts INT NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE reconciliation_runs (
    id BIGSERIAL PRIMARY KEY,
    run_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    onchain_supply BIGINT NOT NULL,
    genesis_allocation BIGINT NOT NULL,
    ledger_liability BIGINT NOT NULL,
    custody_reported BIGINT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('ok','over_backed_drift','mismatch','error')),
    detail JSONB NOT NULL DEFAULT '{}'
);

-- Single-row tables: id must equal TRUE.
CREATE TABLE breaker_state (
    id BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    minting_halted BOOLEAN NOT NULL DEFAULT FALSE,
    halt_reason TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO breaker_state (id) VALUES (TRUE);

CREATE TABLE chain_cursor (
    id BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    last_processed_height BIGINT NOT NULL DEFAULT 0
);
INSERT INTO chain_cursor (id) VALUES (TRUE);

-- Every manual intervention and breaker trip lands here. No silent overrides (spec §5).
CREATE TABLE alerts (
    id BIGSERIAL PRIMARY KEY,
    severity TEXT NOT NULL CHECK (severity IN ('info','warn','p1')),
    source TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
