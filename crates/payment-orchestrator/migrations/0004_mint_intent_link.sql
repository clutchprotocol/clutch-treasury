-- Plan C 5b: the bridge worker's own bookkeeping on the deposit row it drives. The `confirmed`
-- row IS the pending-operation row (spec §6 outbox semantics) — no separate queue table, so
-- everything the bridge needs to resume after a crash or a treasury outage lives here.
--
-- treasury_intent_id: set once the POST to /internal/mint-intents succeeds, moving the deposit
-- to mint_requested. Without this column the bridge would have to re-derive the treasury intent
-- id from client_ref on every poll tick (a lookup this row should just hold, per the brief) —
-- and it is what the status-poll step (GET /internal/mint-intents/:id) needs to even know which
-- id to ask about.
ALTER TABLE deposit_intents ADD COLUMN treasury_intent_id UUID;

-- attempts + next_attempt_at: jittered backoff bookkeeping for BOTH the create step (POSTing a
-- confirmed deposit to the treasury) and the poll step (checking a mint_requested deposit's
-- status) — a treasury outage on either step must not lose the deposit or spin the log. Kept as
-- one counter/one timestamp pair rather than two, since only one of the two steps is ever live
-- for a given row at a time (a row is either confirmed-not-yet-requested, or mint_requested and
-- polling; never both), so there is nothing to disambiguate.
ALTER TABLE deposit_intents ADD COLUMN attempts INT NOT NULL DEFAULT 0;
ALTER TABLE deposit_intents ADD COLUMN next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now();
