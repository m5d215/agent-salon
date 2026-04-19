use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

use crate::mcp::{self, RelayState};

/// Body of a `/notify` request.
///
/// The sender identifier lives in the URL (`?label=<name>`), not in the body.
/// This makes the transport — not the payload — responsible for declaring
/// identity, which prevents a compromised or misbehaving payload (e.g. an
/// LLM-generated body) from spoofing `source`.
#[derive(Debug, Clone, Deserialize)]
pub struct NotifyPayload {
    pub content: String,
    /// Label of the target session. If omitted, the notification is broadcast
    /// to every connected session.
    pub target: Option<String>,
    pub meta: Option<HashMap<String, serde_json::Value>>,
    /// Populated by the handler from the `?label=` query parameter.
    /// Not accepted from the request body.
    #[serde(skip_deserializing)]
    pub source: Option<String>,
}

/// Query parameters for `/notify`. `label` is the sender's self-declared name
/// and is required.
#[derive(Debug, Deserialize)]
pub struct NotifyQuery {
    pub label: String,
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
    payload.source = Some(query.label);
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
