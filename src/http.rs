use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

use crate::mcp::{self, RelayState};

#[derive(Debug, Clone, Deserialize)]
pub struct NotifyPayload {
    pub content: String,
    /// Explicit sender identifier. Overrides the `?label=` query parameter.
    pub source: Option<String>,
    /// Label of the target session. If omitted, the notification is broadcast
    /// to every connected session.
    pub target: Option<String>,
    pub meta: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
pub struct NotifyQuery {
    /// Sender self-identification. If `source` is missing from the body,
    /// relay-mcp falls back to this value for `meta.source`.
    pub label: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub relay: Arc<RelayState>,
}

pub async fn handle_notify(
    State(state): State<AppState>,
    Query(query): Query<NotifyQuery>,
    Json(mut payload): Json<NotifyPayload>,
) -> StatusCode {
    state.relay.message_count.fetch_add(1, Ordering::Relaxed);
    if payload.source.is_none() {
        payload.source = query.label;
    }
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
