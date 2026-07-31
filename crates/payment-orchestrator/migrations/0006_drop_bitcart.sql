-- Bitcart is gone from the deposit path; `bitcart_terminal` would now be a lie.
--
-- Bitcart's TRX daemon attributes a payment by the SENDER's address (`tx.from_addr in
-- request_addresses`, only populated by `set_request_address`), so a request is detectable only
-- once the payer's Tron address is registered against it in advance. Our model is one shared
-- custody address with payers unknown until they pay and the amount discriminator as the identity.
-- Nothing configurable reconciles those, and a per-invoice address is not available for Tron
-- either (TRX_ACCOUNT_PATH is a fixed single-address derivation path).
--
-- The COLUMN's invariant survives unchanged and is still the one migration 0003 describes: a
-- discriminator slot must stay reserved for as long as a payment could still be credited to that
-- amount, or a later user could be allocated an amount someone else's in-flight payment is about
-- to land on. What changed is only who decides when that moment has passed — a clock we own
-- (poller::close_stale_payment_windows, SLOT_HOLD_HOURS) instead of a third party's opinion about
-- an invoice we can no longer even create.
--
-- Renamed rather than repurposed in place so the name cannot mislead the next reader, and so
-- anything still writing the old name fails loudly at compile/query time instead of silently
-- setting a column nothing reads.
ALTER TABLE deposit_intents RENAME COLUMN bitcart_terminal TO payment_window_closed;

-- The index has to be rebuilt because its predicate names the column.
DROP INDEX uq_active_pay_amount;
CREATE UNIQUE INDEX uq_active_pay_amount ON deposit_intents (pay_amount_usdt)
    WHERE status IN ('created', 'invoiced', 'paying', 'expired', 'needs_manual', 'failed')
      AND NOT payment_window_closed;

-- webhook_events existed solely for idempotency-layer-2 dedup of Bitcart's unsigned, never-retried
-- IPN deliveries. There is no webhook any more, so the table can only accumulate nothing.
DROP TABLE IF EXISTS webhook_events;
