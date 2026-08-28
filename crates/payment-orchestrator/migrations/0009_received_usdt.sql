-- Record what actually arrived, not only what was asked for.
--
-- `evaluate_payment` has always returned the observed total, and a test in custody.rs states the
-- intent plainly: "Overpayment records what ARRIVED, not what was intended — the reconciliation
-- cross-check compares the ledger against custody, so recording the intended figure builds in a
-- permanent discrepancy." Nothing carried that figure anywhere. The poller logged it, alerted
-- "credited what arrived", and dropped it; the bridge sent `amount_usdt`, the requested amount.
--
-- The first real deposit on stage was 1,000 USDT against a $10 intent. $10 of CLT was minted, the
-- full 1,000 was swept into the treasury, and the ledger recorded neither the excess nor a debt for
-- it. The reserve covered it — the error ran in the safe direction — but the depositor was owed
-- ~$990 that nothing in the system knew about.
--
-- NULL, not 0, for rows that predate this: 0 would read as "nothing arrived" for deposits that were
-- paid and credited long before the column existed.
ALTER TABLE deposit_intents ADD COLUMN received_usdt BIGINT CHECK (received_usdt > 0);

COMMENT ON COLUMN deposit_intents.received_usdt IS
  'Observed on-chain total paid to this intent''s derived address. NULL until settled, and on rows
   predating the column. This is what gets credited — amount_usdt is only what the user asked for.';
