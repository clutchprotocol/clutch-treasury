-- The verifier had no way to know what amount to actually expect on-chain.
--
-- `amount_clt` is what the user asked to deposit (say 10_000_000). What they were told to PAY is
-- that plus a per-intent discriminator (10_000_123) — and on the shared static Tron custody
-- address the discriminated amount is the ONLY thing distinguishing one user's payment from
-- another's. That value lived solely in payment-orchestrator's deposit_intents.pay_amount_usdt
-- and was never sent here, so tron_verifier's fallback match compared against amount_clt: "any
-- confirmed transfer to custody of AT LEAST the intended amount".
--
-- That matches a different user's larger deposit. The first deposit-backed intent with a NULL
-- deposit_tx_id would claim whichever qualifying transfer TronGrid listed first, be approved on a
-- stranger's money, and (since the verifier ledgers the OBSERVED amount) record that stranger's
-- full transfer as this intent's custody deposit. The rightful depositor is then locked out of
-- their own transfer by uq_mint_intents_deposit_tx.
--
-- So the expected pay amount has to travel with the intent. NOT NULL is enforced for
-- deposit-backed intents specifically, so the verifier can never face a NULL and be tempted to
-- fall back to amount_clt again; Plan B's manual intents (client_ref IS NULL) are unaffected.
ALTER TABLE mint_intents ADD COLUMN expected_amount_usdt BIGINT;

-- NOT VALID deliberately, and it is not a weakening: Postgres still enforces this on every
-- INSERT and UPDATE from here on, which is the entire point (stop a bridge from creating an
-- unverifiable intent). It only skips the one-time scan of rows that predate the column.
--
-- That scan is what we must skip. Deposit-backed intents were introduced by 0002 in this same
-- unreleased batch, so any existing row with a client_ref and no expected amount is a
-- pre-release artifact — and there is no safe value to backfill it with. Setting it to
-- amount_clt would be actively harmful: that is the undiscriminated figure whose use as a match
-- key is the exact bug this migration exists to fix. Rejecting or deleting such rows from a
-- migration is worse still.
--
-- Leaving them NULL is safe because tron_verifier fail-closes on a NULL expected amount: it
-- returns Transient (never approve, never reject) and the stuck-intent sweep escalates to a
-- human. Unverifiable rows therefore sit still and get looked at, which is the correct outcome.
ALTER TABLE mint_intents ADD CONSTRAINT deposit_backed_needs_expected_amount
    CHECK (client_ref IS NULL OR expected_amount_usdt IS NOT NULL) NOT VALID;
