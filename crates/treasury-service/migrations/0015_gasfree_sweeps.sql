-- A GasFree sweep in flight (spec §3, §5). A permit returns a trace id, not a finished transfer. The
-- sweep is done when the controller's nonces(owner) moves past the permit's nonce — the chain, not
-- the relay — and after its deadline it can no longer run. All NULL, or all set.
ALTER TABLE gasfree_accounts
    ADD COLUMN pending_trace_id     TEXT,
    ADD COLUMN pending_nonce        BIGINT,
    ADD COLUMN pending_deadline     BIGINT,
    ADD COLUMN pending_value_usdt   BIGINT,
    ADD COLUMN pending_requested_at TIMESTAMPTZ,
    ADD CONSTRAINT gasfree_pending_all_or_nothing CHECK (
        (pending_nonce IS NULL) = (pending_trace_id IS NULL)
        AND (pending_nonce IS NULL) = (pending_deadline IS NULL)
        AND (pending_nonce IS NULL) = (pending_value_usdt IS NULL)
        AND (pending_nonce IS NULL) = (pending_requested_at IS NULL));
