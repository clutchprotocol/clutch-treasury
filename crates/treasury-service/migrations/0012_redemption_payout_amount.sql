-- What a redemption actually pays out, quoted once at creation and never recomputed.
--
-- Until now a redemption was par by construction: the payout worker passed `amount_clt` straight
-- to the signer as micro-USDT. A redemption fee makes the two different numbers, and they must be
-- stored separately rather than derived at payout time from config. Deriving it later would let an
-- operator change the fee between the moment a user is quoted and the moment they are paid — and
-- the burn in between is irreversible, so the user would have no way back out of a worse deal than
-- the one they accepted.
--
-- `amount_clt` keeps its meaning exactly: the CLT the user must burn, matched against the on-chain
-- burn amount before anything pays. This column is only ever the USDT leg.
ALTER TABLE redemption_intents ADD COLUMN payout_amount_usdt BIGINT;

-- Every existing row was created under par, so par is its true quote, not a guess.
UPDATE redemption_intents SET payout_amount_usdt = amount_clt WHERE payout_amount_usdt IS NULL;

ALTER TABLE redemption_intents ALTER COLUMN payout_amount_usdt SET NOT NULL;

-- A payout above the burn would mint reserve out of nowhere; one at or below zero is not a
-- redemption at all. Both are enforced here rather than only in Rust, because this column is the
-- number that leaves the float.
ALTER TABLE redemption_intents ADD CONSTRAINT payout_within_burn
    CHECK (payout_amount_usdt > 0 AND payout_amount_usdt <= amount_clt);
