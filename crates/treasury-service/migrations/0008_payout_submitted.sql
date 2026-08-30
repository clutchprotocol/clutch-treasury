-- The payout worker claims an intent BEFORE calling the signer, so a crash mid-call lands in a
-- state that is visibly ambiguous rather than one that looks retryable.
--
-- Without this status the only options after a lost response are "retry and maybe pay twice" or
-- "give up and orphan the burn". This is the third option: recorded as in-flight, resolvable by
-- looking at the float's outbound transfers.
ALTER TABLE redemption_intents DROP CONSTRAINT redemption_intents_status_check;
ALTER TABLE redemption_intents ADD CONSTRAINT redemption_intents_status_check CHECK (status IN
    ('created','burn_confirmed','payout_pending','payout_submitted','paid','expired','failed'));
