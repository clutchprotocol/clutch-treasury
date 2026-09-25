-- Whether a user's permanent deposit address is a GasFree account (spec §1). Set once, when the
-- address is issued: new users get one while APP_TRANSFER_RAIL=gasfree, and a user keeps the kind of
-- address they were given, like the address itself (spec §5). The deposit route reads it to put
-- GasFree's tripwire and its fee in front of the address.
ALTER TABLE deposit_addresses ADD COLUMN gasfree BOOLEAN NOT NULL DEFAULT FALSE;
