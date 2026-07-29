use axum::http::{HeaderMap, StatusCode};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

use crate::configuration::OrchConfig;

/// Byte-identical to clutch-hub-api's JWT claims (`src/hub/auth.rs`, `src/hub/graphql/handler.rs`)
/// — this service validates tokens minted by the hub, it never mints its own, so the struct and
/// the decode call below must match what the hub actually issues, not a plausible equivalent.
#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    pub pk: String, // public key
    pub exp: usize, // expiration time
}

/// Validate `Authorization: Bearer <JWT>` (HS256, claims `{pk, exp}`) and return the caller's
/// public key. The orchestrator holds no chain keys and no treasury approver token — this is
/// the only identity check it performs; everything downstream trusts `pk` as the caller.
pub fn authenticated_pk(headers: &HeaderMap, config: &OrchConfig) -> Result<String, StatusCode> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s).trim())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    decode::<Claims>(
        token,
        &DecodingKey::from_secret(config.jwt_secret.as_bytes()),
        &Validation::new(Algorithm::HS256),
    )
    .map(|data| data.claims.pk)
    .map_err(|_| StatusCode::UNAUTHORIZED)
}
