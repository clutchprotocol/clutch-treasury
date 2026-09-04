-- An intent over the per-transaction mint cap cannot pass on retry: the cap is a property of the
-- amount, not of the moment. Retrying such an intent ten times and then failing it stranded a real
-- 1,000 USDT deposit on 2026-09-03 -- a `failed` row leaves the reserve sum and the sweeper skips
-- it, while the money sat untouched at its address.
--
-- `needs_manual` is the honest state: the money is real and still counted, a human has to act
-- (raise the cap, then approve the intent again), and nothing is retried in the meantime.
ALTER TABLE mint_intents DROP CONSTRAINT mint_intents_status_check;
ALTER TABLE mint_intents ADD CONSTRAINT mint_intents_status_check CHECK (status IN
    ('created','approved','submitted','credited','failed','rejected','needs_manual'));
