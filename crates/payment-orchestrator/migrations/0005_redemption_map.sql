-- Plan C T6: the redemption proxy's own mapping row. Mirrors deposit_intents' role for the
-- deposit path — this crate's database is the only place that knows which user_pk asked for
-- which treasury redemption intent, so `GET /api/v1/redemptions/:id`'s owner check (same
-- convention as get_deposit_handler) has something to check against.
--
-- No idempotency-key column here on purpose (see redemptions.rs module docs): a duplicate
-- POST is deliberately NOT deduplicated at this layer. A second treasury intent just sits
-- unfulfilled since only one on-chain Burn can ever carry a given redemption_ref — litter,
-- not a double-spend. So this table has no unique constraint analogous to deposit_intents'
-- (user_pk, client_key).
--
-- status + redemption_ref + payout_address + amount_clt are captured from the treasury's
-- CREATE response and read back verbatim by GET — there is no `GET
-- /internal/redemption-intents/:id` on the treasury side to refresh from (checked: api.rs's
-- router only has the POST), so this row reflects the treasury's status AS OF intent
-- creation, not live. Flagged in the task report; out of scope here per the brief (treasury
-- changes are excluded from this task).
CREATE TABLE redemption_map (
    id UUID PRIMARY KEY,
    user_pk TEXT NOT NULL,
    treasury_intent_id UUID NOT NULL,
    payout_tron_address TEXT NOT NULL,
    amount_clt BIGINT NOT NULL CHECK (amount_clt > 0),
    redemption_ref TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The owner check's lookup path (GET /api/v1/redemptions/:id) filters by id already (primary
-- key); this index is for any future per-user listing, cheap to add now while the table is new.
CREATE INDEX idx_redemption_map_user_pk ON redemption_map (user_pk);
