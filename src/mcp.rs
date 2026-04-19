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

/// Shared state between HTTP handler and MCP server.
pub struct RelayState {
    pub port: u16,
    pub message_count: AtomicU64,
    pub sessions: Mutex<Vec<Peer<RoleServer>>>,
}

impl RelayState {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            message_count: AtomicU64::new(0),
            sessions: Mutex::new(Vec::new()),
        }
    }

    pub async fn session_count(&self) -> usize {
        self.sessions.lock().await.len()
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
        let sessions = self.state.session_count().await;
        let port = self.state.port;
        format!(
            "Notify endpoint:  http://127.0.0.1:{port}/notify\n\
             MCP endpoint:     http://127.0.0.1:{port}/mcp\n\
             Active sessions:  {sessions}\n\
             Messages relayed: {count}"
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
                 and messages are forwarded as notifications to this session.",
            )
    }

    async fn on_initialized(&self, ctx: NotificationContext<RoleServer>) {
        let mut sessions = self.state.sessions.lock().await;
        sessions.push(ctx.peer);
        eprintln!("relay-mcp: session initialized ({} active)", sessions.len());
    }
}

/// Broadcast a notification to every connected session.
/// Sessions whose channel has closed are removed.
pub async fn broadcast_notification(state: &RelayState, payload: &NotifyPayload) {
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
    for peer in sessions.drain(..) {
        match peer.send_notification(notification.clone()).await {
            Ok(()) => alive.push(peer),
            Err(e) => eprintln!("relay-mcp: dropping session (send failed): {e}"),
        }
    }
    *sessions = alive;
}
