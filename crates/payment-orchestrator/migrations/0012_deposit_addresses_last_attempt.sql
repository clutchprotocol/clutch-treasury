-- Rotation order and the read watermark are two different questions (R23): a failing address must
-- still move to the back of the poll queue, but its watermark must not advance on a read it never got.
ALTER TABLE deposit_addresses ADD COLUMN last_attempt_at TIMESTAMPTZ;
