use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use prometheus_client::encoding::text::encode;
use serde::Deserialize;

use crate::mcp::{self, DeliveryContext, SalonState};

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
    pub salon: Arc<SalonState>,
}

pub async fn handle_notify(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(query): Query<NotifyQuery>,
    Json(mut payload): Json<NotifyPayload>,
) -> StatusCode {
    state.salon.message_count.fetch_add(1, Ordering::Relaxed);
    payload.source = Some(query.label);
    let ctx = DeliveryContext {
        via: Some(crate::db::Via::Notify),
        sender_addr: Some(addr.to_string()),
        sender_session_id: None,
    };
    // Spawn so the HTTP response returns immediately.
    let salon = state.salon.clone();
    tokio::spawn(async move {
        mcp::deliver_notification(&salon, &payload, ctx).await;
    });
    StatusCode::ACCEPTED
}

/// Render the Prometheus / OpenMetrics text exposition for the shared
/// registry. Returns a `text/plain; version=0.0.4` body so generic
/// Prometheus scrapers (and Alloy's `prometheus.scrape`) accept it as-is.
/// The endpoint is intentionally unauthenticated; the network perimeter
/// (loopback / Tailnet ACLs) is the access control here.
pub async fn handle_metrics(State(state): State<AppState>) -> Response {
    let mut buf = String::new();
    let render = {
        let registry = state
            .salon
            .metrics
            .registry
            .lock()
            .expect("metrics registry poisoned");
        encode(&mut buf, &registry)
    };
    match render {
        Ok(()) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "text/plain; version=0.0.4")],
            buf,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("metrics encode failed: {e}"),
        )
            .into_response(),
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/notify", post(handle_notify))
        .route("/metrics", get(handle_metrics))
        .route("/admin", get(crate::admin::list_page))
        .route("/admin/messages/{id}", get(crate::admin::detail_page))
        .with_state(state)
}
