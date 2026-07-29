-- Plan C T5: deposit-backed mint intents. client_ref is the orchestrator's idempotency key
-- (its deposit_intent id) — the treasury-side half of "duplicate client_ref replays instead
-- of duplicating" (spec §6). deposit_tx_id is the Tron transfer the tron_verifier checks
-- before auto-approving; it can start NULL (Bitcart's response sometimes lacks the hash —
-- adapter.rs tolerates that) and get backfilled by the verifier's fallback match.
ALTER TABLE mint_intents ADD COLUMN client_ref TEXT UNIQUE;
ALTER TABLE mint_intents ADD COLUMN deposit_tx_id TEXT;
ALTER TABLE mint_intents ADD COLUMN verified_at TIMESTAMPTZ;

-- The gap the plan left open: without this, one real on-chain USDT transfer could be
-- presented as evidence (deposit_tx_id) for two different mint intents, and each would
-- verify independently and approve — a double mint against a single deposit. Partial so
-- manually-created intents (deposit_tx_id IS NULL, Plan B's human flow) are unaffected.
--
-- The fallback-match backfill path in tron_verifier.rs must treat losing this race as a
-- hard mismatch for the losing intent (another intent already claimed that transfer), not
-- a transient error — retrying it would eventually re-observe the same claimed transfer and
-- approve a second mint against it.
CREATE UNIQUE INDEX uq_mint_intents_deposit_tx ON mint_intents (deposit_tx_id)
    WHERE deposit_tx_id IS NOT NULL;
