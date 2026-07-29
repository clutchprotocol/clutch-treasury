use axum::{routing::get, Json, Router};
use serde_json::json;

use crate::configuration::OrchConfig;

#[derive(Clone)]
pub struct AppState {
    pub config: OrchConfig,
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

pub fn router(config: OrchConfig) -> Router {
    let state = AppState { config };
    Router::new().route("/health", get(health)).with_state(state)
}
