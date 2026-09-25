-- A redemption paid by GasFree permit (spec §4). The trace id leads to the permit's transaction;
-- the nonce and the deadline say, once the deadline has passed, whether the permit can have run.
-- NULL for a TRX payout.
ALTER TABLE redemption_intents
    ADD COLUMN payout_trace_id        TEXT,
    ADD COLUMN payout_permit_nonce    BIGINT,
    ADD COLUMN payout_permit_deadline BIGINT;
