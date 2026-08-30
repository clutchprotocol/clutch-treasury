-- daily_payout_total (payout.rs) needs an immutable "when did this leave the queue" timestamp.
-- `updated_at` looked like it would do, but it is touched by every later write to the row --
-- pay_intent's eventual confirmation UPDATE re-enters a day-old payout into TODAY's budget the
-- moment it confirms, while an Ambiguous row that nothing ever touches again just sits at its
-- claim time and ages out of the 24h window despite possibly having spent real float capacity.
--
-- Set by drain_once's claim UPDATE and touched by nothing else -- confirmation and an Ambiguous
-- outcome both leave it alone. (A refused intent later RE-claimed on a subsequent pass DOES get
-- re-stamped -- correctly: a new claim spends new budget, not the same claim again.) What
-- actually mirrors how breakers::daily_mint_total keys its window on the immutable
-- mint_intents.created_at is that no write other than the claim itself ever moves it.
ALTER TABLE redemption_intents ADD COLUMN payout_submitted_at TIMESTAMPTZ;

-- Backfill: any row already sitting in payout_submitted/paid predates this column and would
-- otherwise read NULL forever, falling out of daily_payout_total's window on every future pass --
-- a money cap reading LOW is the same class of quiet wrongness as an uncounted reserve, and this
-- project already lost a day to one of those (see chain_sync.rs). `updated_at` is only an
-- APPROXIMATION of the true claim time (drain_once's claim UPDATE also touches it going forward,
-- per the column doc above -- this is the one place that overlap is actually fine, since it is a
-- one-time backfill, not the ongoing window logic), but an approximate timestamp that eventually
-- ages out normally is strictly better than NULL, which excludes the row from the cap forever.
UPDATE redemption_intents SET payout_submitted_at = updated_at
 WHERE status IN ('payout_submitted','paid') AND payout_submitted_at IS NULL;
