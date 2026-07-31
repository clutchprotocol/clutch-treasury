-- Per-intent deposit addresses: one derived Tron address each, from the account xpub.
--
-- The index is allocated from a SEQUENCE, and that choice is the load-bearing part.
--
-- `max(derivation_index) + 1` would be wrong in two ways that both end in two depositors sharing
-- one address. It races: two concurrent creates read the same max and both take it. And it reuses:
-- delete or roll back the highest row and the next create hands its index — and therefore its
-- address — to a different user. Sharing a deposit address is exactly the cross-user
-- misattribution the old `uq_active_pay_amount` index existed to prevent, arrived at from the
-- other direction.
--
-- A sequence gives the one property that actually matters: `nextval()` NEVER returns the same
-- value twice, even across concurrent transactions and even when the calling transaction rolls
-- back. Sequences are deliberately non-transactional for precisely this reason.
--
-- GAPS ARE FINE, REUSE IS NOT. A failed insert burns an index; nothing depends on the sequence
-- being dense. Do not "fix" a gap by lowering the sequence — the signer derives keys from the
-- index, so re-issuing one silently points two intents at the same funds.
--
-- MAXVALUE is 2^31-1 because BIP32 non-hardened derivation only spans that range. Bounding it here
-- makes exhaustion fail at allocation with a plain sequence error, rather than later inside
-- `derive.rs`, which refuses hardened indices — the same refusal, but at a point where an intent
-- row may already exist.
CREATE SEQUENCE deposit_derivation_index_seq
    AS BIGINT
    START WITH 0
    MINVALUE 0
    MAXVALUE 2147483647
    NO CYCLE;

-- Nullable, for now. Every row created from here on carries both, but the discriminator-era rows
-- predate the scheme and no index was ever allocated for them — there is nothing truthful to
-- backfill. These become NOT NULL when the discriminator columns are retired and the old rows go
-- with them.
ALTER TABLE deposit_intents
    ADD COLUMN derivation_index BIGINT,
    ADD COLUMN deposit_address  TEXT;

-- The invariant, enforced by the database rather than trusted to the allocator: no two intents may
-- ever claim one index or one address. Postgres permits multiple NULLs in a unique index, so the
-- legacy rows coexist without weakening this for new ones.
CREATE UNIQUE INDEX uq_deposit_derivation_index ON deposit_intents (derivation_index);
CREATE UNIQUE INDEX uq_deposit_address ON deposit_intents (deposit_address);

-- The watcher matches observed transfers by destination address, so this is its lookup path.
-- Partial: only intents that can still take a payment are ever matched against.
CREATE INDEX ix_deposit_address_open ON deposit_intents (deposit_address)
    WHERE status IN ('created', 'invoiced', 'paying', 'expired');
