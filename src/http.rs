use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::mcp::RelayState;

#[derive(Debug, Clone, Deserialize)]
pub struct NotifyPayload {
    pub content: String,
    pub source: Option<String>,
    pub meta: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Clone)]
pub struct AppState {
    pub relay: Arc<RelayState>,
    pub tx: mpsc::Sender<NotifyPayload>,
}

pub async fn handle_notify(
    State(state): State<AppState>,
    Json(payload): Json<NotifyPayload>,
) -> StatusCode {
    state.relay.message_count.fetch_add(1, Ordering::Relaxed);
    let _ = state.tx.try_send(payload);
    StatusCode::ACCEPTED
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/notify", post(handle_notify))
        .with_state(state)
}
