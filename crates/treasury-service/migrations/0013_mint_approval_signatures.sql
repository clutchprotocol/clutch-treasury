-- Approval signatures for M-of-N minting.
--
-- On a chain with mint_threshold > 1 the node requires signatures from several distinct
-- authorities over the mint approval digest (see clutch-node
-- docs/superpowers/specs/2026-09-11-m-of-n-mint-authority-design.md). This table is where an
-- approver's signature waits between approving and the outbox assembling the transaction.
--
-- treasury-service never holds an approver's key. The signature arrives already made, on the
-- approve call, and this service only verifies that it recovers to a configured authority and
-- relays it. That is the whole point: if this service could produce the second signature, the
-- second signature would not mean anything.
--
-- The primary key enforces one signature per signer per intent, which is the same rule the node
-- enforces when it refuses a repeated authority. Enforced in both places on purpose -- the node
-- is authoritative, and this stops a duplicate ever reaching it.
CREATE TABLE IF NOT EXISTS mint_approval_signatures (
    intent_id      UUID        NOT NULL REFERENCES mint_intents(id) ON DELETE CASCADE,
    signer_address TEXT        NOT NULL,
    sig_r          TEXT        NOT NULL,
    sig_s          TEXT        NOT NULL,
    sig_v          BIGINT      NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (intent_id, signer_address)
);

CREATE INDEX IF NOT EXISTS mint_approval_signatures_intent_idx
    ON mint_approval_signatures (intent_id);
