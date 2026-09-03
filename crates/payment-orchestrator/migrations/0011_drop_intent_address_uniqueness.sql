-- Address reuse is the point: one user, one address, many deposits over its life.
--
-- What these indexes were protecting is not lost, it changes key. uq_deposit_intents_tron_tx_id
-- (migration 0010) makes every credit unique per TRANSACTION, which is a strictly better guarantee:
-- under the old model two transfers to one address were ONE deposit by construction, which is
-- quietly wrong. Under this one they are correctly two.
DROP INDEX IF EXISTS uq_deposit_address;
DROP INDEX IF EXISTS uq_deposit_derivation_index;
