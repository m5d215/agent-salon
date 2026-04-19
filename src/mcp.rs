use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{
    CustomNotification, Implementation, ServerCapabilities, ServerInfo, ServerNotification,
};
use rmcp::service::NotificationContext;
use rmcp::{Peer, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::http::NotifyPayload;

/// A connected MCP session and its user-supplied label (from `?label=` query).
pub struct Session {
    pub peer: Peer<RoleServer>,
    pub label: Option<String>,
}

/// Shared state between HTTP handler and MCP server.
pub struct RelayState {
    pub port: u16,
    pub message_count: AtomicU64,
    pub sessions: Mutex<Vec<Session>>,
}

impl RelayState {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            message_count: AtomicU64::new(0),
            sessions: Mutex::new(Vec::new()),
        }
    }
}

pub struct RelayHandler {
    state: Arc<RelayState>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl RelayHandler {
    pub fn new(state: Arc<RelayState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Show HTTP endpoint URL, port, and message count")]
    async fn relay_status(&self) -> String {
        let count = self.state.message_count.load(Ordering::Relaxed);
        let port = self.state.port;
        let sessions = self.state.sessions.lock().await;
        let session_lines = if sessions.is_empty() {
            "  (none)".to_string()
        } else {
            sessions
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let label = s.label.as_deref().unwrap_or("<unlabeled>");
                    format!("  [{i}] label={label}")
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        format!(
            "Notify endpoint:  http://127.0.0.1:{port}/notify\n\
             MCP endpoint:     http://127.0.0.1:{port}/mcp\n\
             Active sessions ({total}):\n{session_lines}\n\
             Messages relayed: {count}",
            total = sessions.len(),
        )
    }
}

#[tool_handler]
impl ServerHandler for RelayHandler {
    fn get_info(&self) -> ServerInfo {
        let mut capabilities = ServerCapabilities::builder().enable_tools().build();
        // Declare claude/channel capability so Claude Code accepts our notifications.
        capabilities.experimental = Some(
            serde_json::from_value(serde_json::json!({
                "claude/channel": {}
            }))
            .unwrap(),
        );
        ServerInfo::new(capabilities)
            .with_server_info(Implementation::new("relay-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "relay-mcp: HTTP-to-MCP notification bridge. \
                 External processes POST to the HTTP endpoint, \
                 and messages are forwarded as notifications to this session. \
                 Set ?label=<name> on the /mcp URL to receive targeted notifications.",
            )
    }

    async fn on_initialized(&self, ctx: NotificationContext<RoleServer>) {
        let label = ctx
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.uri.query())
            .and_then(extract_label);

        let mut sessions = self.state.sessions.lock().await;
        sessions.push(Session {
            peer: ctx.peer,
            label: label.clone(),
        });
        eprintln!(
            "relay-mcp: session initialized (label={}, {} active)",
            label.as_deref().unwrap_or("<unlabeled>"),
            sessions.len()
        );
    }
}

fn extract_label(query: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == "label")
        .map(|(_, v)| v.to_string())
}

/// Deliver a notification to matching sessions.
/// - If `payload.target` is set, only sessions whose label equals it receive the notification.
/// - If `payload.target` is None, every connected session receives it (broadcast).
/// Sessions whose channel has closed are pruned.
pub async fn deliver_notification(state: &RelayState, payload: &NotifyPayload) {
    let meta = {
        let mut map = payload.meta.clone().unwrap_or_default();
        if let Some(source) = &payload.source {
            map.insert("source".into(), serde_json::Value::String(source.clone()));
        }
        map.insert(
            "ts".into(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
        map
    };

    let params = serde_json::json!({
        "content": payload.content,
        "meta": meta,
    });

    let notification = ServerNotification::CustomNotification(CustomNotification::new(
        "notifications/claude/channel",
        Some(params),
    ));

    let mut sessions = state.sessions.lock().await;
    let mut alive = Vec::with_capacity(sessions.len());
    for session in sessions.drain(..) {
        let matches = match &payload.target {
            None => true,
            Some(target) => session.label.as_deref() == Some(target.as_str()),
        };
        if !matches {
            alive.push(session);
            continue;
        }
        match session.peer.send_notification(notification.clone()).await {
            Ok(()) => alive.push(session),
            Err(e) => eprintln!("relay-mcp: dropping session (send failed): {e}"),
        }
    }
    *sessions = alive;
}
