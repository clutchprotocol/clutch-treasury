//! One permanent deposit address per user.
//!
//! Replaces per-intent derivation. The index still comes from `deposit_derivation_index_seq` but is
//! consumed once per user and stored, so the address a depositor was given keeps working for every
//! later deposit. The sequence is never reset — legacy per-intent addresses hold issued indexes,
//! and reusing one would hand a new user an address someone else already paid into.
use sqlx::PgPool;

use crate::derive::AddressDeriver;

/// The user's deposit address, deriving and storing it on first call.
///
/// Idempotent by construction: the INSERT is `ON CONFLICT (user_pk) DO NOTHING` followed by a read,
/// so two concurrent first-calls settle on whichever row won rather than deriving twice.
pub async fn address_for_user(
    pool: &PgPool,
    deriver: &AddressDeriver,
    user_pk: &str,
    clt_address: &str,
) -> Result<String, String> {
    if let Some(addr) = existing(pool, user_pk).await? {
        return Ok(addr);
    }

    let index = crate::deposits::allocate_derivation_index(pool)
        .await
        .map_err(|e| format!("allocating a derivation index: {e}"))?;

    let index_u32 =
        u32::try_from(index).map_err(|_| format!("derivation index {index} is out of range"))?;
    let address = deriver.address_at(index_u32)?;

    sqlx::query(
        "INSERT INTO deposit_addresses (user_pk, derivation_index, address, clt_address)
         VALUES ($1, $2, $3, $4) ON CONFLICT (user_pk) DO NOTHING",
    )
    .bind(user_pk)
    .bind(index)
    .bind(&address)
    .bind(clt_address)
    .execute(pool)
    .await
    .map_err(|e| format!("storing the deposit address: {e}"))?;

    // Re-read rather than returning `address`: if a concurrent call won the race, the stored row is
    // the one the poller will watch, and handing back the losing derivation would tell a user to
    // pay an address nothing polls. The burned index is simply skipped — cheaper than a lock.
    existing(pool, user_pk)
        .await?
        .ok_or_else(|| "deposit address vanished immediately after insert".to_string())
}

async fn existing(pool: &PgPool, user_pk: &str) -> Result<Option<String>, String> {
    sqlx::query_scalar("SELECT address FROM deposit_addresses WHERE user_pk = $1")
        .bind(user_pk)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("reading the deposit address: {e}"))
}

/// Put a user's address on the fast poll tier, because they are about to send.
pub async fn mark_hot(pool: &PgPool, user_pk: &str, window_hours: i64) -> Result<(), String> {
    sqlx::query(
        "UPDATE deposit_addresses SET hot_until = now() + make_interval(hours => $2::int)
         WHERE user_pk = $1",
    )
    .bind(user_pk)
    .bind(window_hours)
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|e| format!("marking the deposit address hot: {e}"))
}
