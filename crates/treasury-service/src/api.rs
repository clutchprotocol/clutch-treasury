use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::get,
    Json, Router,
};
use serde_json::json;
use sqlx::PgPool;

use crate::configuration::AppConfig;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: AppConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Approver,
    ReadOnly,
}

/// Static bearer tokens, one per role — disjoint on purpose (four-eyes, spec §5).
/// Returns the caller's role; the handler decides which roles it accepts.
pub fn caller_role(headers: &HeaderMap, config: &AppConfig) -> Result<Role, StatusCode> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s).trim())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if token == config.initiator_token {
        Ok(Role::Initiator)
    } else if token == config.approver_token {
        Ok(Role::Approver)
    } else if token == config.readonly_token {
        Ok(Role::ReadOnly)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

pub fn router(pool: PgPool, config: AppConfig) -> Router {
    let state = AppState { pool, config };
    Router::new()
        .route("/health", get(health))
        .with_state(state)
}
