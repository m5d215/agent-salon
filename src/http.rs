use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

use crate::mcp::{self, RelayState};

#[derive(Debug, Clone, Deserialize)]
pub struct NotifyPayload {
    pub content: String,
    pub source: Option<String>,
    /// Label of the target session. If omitted, the notification is broadcast
    /// to every connected session.
    pub target: Option<String>,
    pub meta: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Clone)]
pub struct AppState {
    pub relay: Arc<RelayState>,
}

pub async fn handle_notify(
    State(state): State<AppState>,
    Json(payload): Json<NotifyPayload>,
) -> StatusCode {
    state.relay.message_count.fetch_add(1, Ordering::Relaxed);
    // Spawn so the HTTP response returns immediately.
    let relay = state.relay.clone();
    tokio::spawn(async move {
        mcp::deliver_notification(&relay, &payload).await;
    });
    StatusCode::ACCEPTED
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/notify", post(handle_notify))
        .with_state(state)
}
