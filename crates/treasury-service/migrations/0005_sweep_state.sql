-- Sweep state, and with it the ability to know where the reserve actually sits.
--
-- Deposits now land on one derived address per intent and are moved to the main treasury address by
-- a later sweep. Between those two moments the reserve is spread across N addresses, so "how much
-- USDT do we hold" stops being a single balance read.
--
-- This does NOT affect the breaker. `judge` halts on `custody_reported < ledger_liability`, and
-- `custody_reported` is the LEDGER's figure, which records what arrived regardless of which address
-- it arrived at. `trongrid_balance` is explicitly a cross-check column that "plays no part in any
-- branch" — so unswept funds could never have halted minting.
--
-- What they would have done is quietly break the fourth source. Reading only the main address while
-- deposits sit elsewhere reports a reserve near zero against a growing liability: a cross-check
-- that is permanently and visibly wrong, which is worse than one that is absent, because people
-- learn to ignore it and then disbelieve it on the day it is right. The same shape as
-- get_custody_balance silently returning Ok(0) for an unactivated account — a reserve read that
-- lies in the safe-looking direction.
ALTER TABLE mint_intents ADD COLUMN swept_at TIMESTAMPTZ;

-- The reserve sum walks unswept deposit addresses on every reconciliation run, so it needs to find
-- them without scanning history. Partial: swept rows hold nothing and are never of interest.
CREATE INDEX ix_mint_intents_unswept ON mint_intents (deposit_address)
    WHERE deposit_address IS NOT NULL AND swept_at IS NULL;
