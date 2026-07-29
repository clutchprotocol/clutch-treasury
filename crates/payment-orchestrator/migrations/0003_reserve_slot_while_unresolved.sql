-- uq_active_pay_amount's whole job is the migration-0001 invariant: on the shared static Tron
-- custody address, the AMOUNT is the only thing Bitcart can match a payment by, so a
-- discriminator slot must stay reserved until Bitcart itself can no longer match a payment to
-- that invoice — which is exactly what `bitcart_terminal` records.
--
-- The original status list left `needs_manual` and `failed` out, so reaching either freed the
-- slot on status alone, ignoring bitcart_terminal. That is wrong in the one case it matters
-- most: `paid_partial` (a user who underpaid) sends the intent to `needs_manual` while
-- Bitcart's invoice is STILL LIVE and still able to take the remainder at that amount. Freeing
-- the slot there lets a later, DIFFERENT user be allocated the amount a stranger's live
-- partially-paid invoice is waiting on — the cross-user misattribution this index exists to
-- prevent, and the same defect class as commit a32a101.
--
-- `failed` is only ever set alongside bitcart_terminal = TRUE today (Invalid/Refunded), so
-- including it changes nothing now; it's here so the index expresses the invariant on its own
-- terms rather than depending on every future writer remembering to pair the two.
--
-- Deliberately NOT included: confirmed / mint_requested / credited. Those mean Bitcart's
-- invoice completed, and a completed invoice can't match a new payment — holding their slots
-- would leak one per successful deposit, forever.
DROP INDEX uq_active_pay_amount;
CREATE UNIQUE INDEX uq_active_pay_amount ON deposit_intents (pay_amount_usdt)
    WHERE status IN ('created', 'invoiced', 'paying', 'expired', 'needs_manual', 'failed')
      AND NOT bitcart_terminal;
