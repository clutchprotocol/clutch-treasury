-- A mint the ledger recorded that the chain no longer holds.
--
-- treasury_events is append-only on purpose, so the original mint_executed row cannot be edited
-- away: it is a true record of something that did happen. What the ledger could not express is the
-- chain LOSING it afterwards -- which stage did, when nodes running developer_mode erased their
-- databases on shutdown and took a $10 mint with them. Liability then counted CLT the chain does
-- not hold, exceeded custody, and tripped the breaker.
--
-- Correcting that with burn_redeemed would assert someone redeemed CLT for USDT, which is a
-- different event with a payout attached. This is its own kind: no USDT moves, no payout is owed,
-- and the pair (original mint_executed + mint_reversed) stays legible to an auditor as
-- "recorded, then lost" rather than vanishing.
--
-- NOT a general-purpose adjustment. It says one specific thing.

ALTER TABLE treasury_events DROP CONSTRAINT treasury_events_kind_check;
ALTER TABLE treasury_events ADD CONSTRAINT treasury_events_kind_check CHECK (kind IN
    ('mint_executed','burn_redeemed','mint_reversed','custody_deposit','custody_withdrawal','buffer_topup'));

-- Subtracts alongside burn_redeemed. The ::BIGINT cast is required for the same reason as the
-- original view: SUM(BIGINT) returns NUMERIC, which sqlx cannot decode into i64 here.
CREATE OR REPLACE VIEW ledger_balances AS SELECT
    (COALESCE(SUM(amount_clt) FILTER (WHERE kind = 'mint_executed'), 0)
   - COALESCE(SUM(amount_clt) FILTER (WHERE kind = 'burn_redeemed'), 0)
   - COALESCE(SUM(amount_clt) FILTER (WHERE kind = 'mint_reversed'), 0))::BIGINT AS clt_liability,
    (COALESCE(SUM(amount_usdt) FILTER (WHERE kind IN ('custody_deposit','buffer_topup')), 0)
   - COALESCE(SUM(amount_usdt) FILTER (WHERE kind = 'custody_withdrawal'), 0))::BIGINT AS custody_usdt
FROM treasury_events;

-- uq_events_intent_kind already makes (intent_id, kind) unique, so one intent cannot be reversed
-- twice -- the second append fails on the index rather than silently halving liability again.
