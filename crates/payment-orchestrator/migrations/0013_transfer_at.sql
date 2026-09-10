-- When the money actually moved, as opposed to when we noticed. The deposit history in the app
-- showed created_at, which is the row's age, and a deposit whose transfer predated the row by
-- six days rendered as "7m ago" -- indistinguishable from a deposit the user had just made.
-- Permanent addresses make that gap ordinary rather than exotic: any old transfer at an address
-- becomes a new row the first time that address is polled after a reset.
--
-- Nullable, and it stays nullable: rows that settled before this column existed have no honest
-- value to backfill, and inventing created_at for them would rebuild the exact confusion this
-- column exists to remove. Readers fall back and say so.
ALTER TABLE deposit_intents ADD COLUMN transfer_at TIMESTAMPTZ;
