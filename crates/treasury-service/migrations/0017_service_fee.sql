-- The operator's service fee on top-ups (src/service_fee.rs).
--
-- A verified deposit's mint is split in two: the user's intent mints less by `service_fee_clt`,
-- and a second intent, `service_fee_for` naming the deposit's, mints exactly that to the operator.
-- UNIQUE makes one fee per deposit a property of the schema, not of the code path that writes it.
ALTER TABLE mint_intents
    ADD COLUMN service_fee_clt BIGINT CHECK (service_fee_clt IS NULL OR service_fee_clt > 0);
ALTER TABLE mint_intents
    ADD COLUMN service_fee_for UUID UNIQUE REFERENCES mint_intents(id);
