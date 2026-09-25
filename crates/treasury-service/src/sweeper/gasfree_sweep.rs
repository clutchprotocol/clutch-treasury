//! Sweeping deposits out of GasFree accounts (spec §3), and deciding from the chain when a sweep ran
//! (spec §5).
//!
//! A GasFree sweep ends as a permit with the relay, not a finished transfer. The account is swept
//! when the controller's `nonces(owner)` moves past the permit's nonce. The account's balance never
//! says it: the relay's unused margin stays there, so it never falls to zero.

use sqlx::PgPool;

use super::SweepSigner;
use crate::configuration::AppConfig;
use crate::tron_verifier::TronClient;

/// The tripwire, read once per pass (spec §5). `false`: no GasFree sweep is asked for this pass.
pub(super) async fn code_unchanged(pool: &PgPool, settings: &gasfree::Settings, client: &TronClient) -> bool {
    todo!("Task 3 Step 5")
}

/// Every permit in flight: did it run, and can it still run?
pub(super) async fn settle(pool: &PgPool, settings: &gasfree::Settings, client: &TronClient, signer: &dyn SweepSigner) {
    todo!("Task 3 Step 5")
}

/// One GasFree account holding credited deposits: ask for a permit if one could move anything.
pub(super) async fn sweep_account(
    pool: &PgPool,
    config: &AppConfig,
    settings: &gasfree::Settings,
    client: &TronClient,
    signer: &dyn SweepSigner,
    index: i64,
    address: &str,
) {
    todo!("Task 3 Step 5")
}
