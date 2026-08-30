-- daily_payout_total (payout.rs) needs an immutable "when did this leave the queue" timestamp.
-- `updated_at` looked like it would do, but it is touched by every later write to the row --
-- pay_intent's eventual confirmation UPDATE re-enters a day-old payout into TODAY's budget the
-- moment it confirms, while an Ambiguous row that nothing ever touches again just sits at its
-- claim time and ages out of the 24h window despite possibly having spent real float capacity.
--
-- Set once, by drain_once's claim UPDATE, and never again -- this is what actually mirrors how
-- breakers::daily_mint_total keys its own window on the immutable mint_intents.created_at.
ALTER TABLE redemption_intents ADD COLUMN payout_submitted_at TIMESTAMPTZ;
