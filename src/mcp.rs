use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{
    CustomNotification, Implementation, ServerCapabilities, ServerInfo, ServerNotification,
};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceError};

use crate::http::NotifyPayload;

/// Shared state between HTTP handler and MCP server.
pub struct RelayState {
    pub port: u16,
    pub message_count: AtomicU64,
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
        format!("HTTP endpoint: http://localhost:{port}/notify\nMessages relayed: {count}")
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
}

/// Send a notification to the connected Claude Code session.
pub async fn send_notification(
    peer: &rmcp::Peer<rmcp::RoleServer>,
    payload: &NotifyPayload,
) -> Result<(), ServiceError> {
    let meta = {
        let mut map = payload.meta.clone().unwrap_or_default();
        if let Some(source) = &payload.source {
            map.insert(
                "source".into(),
                serde_json::Value::String(source.clone()),
            );
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

    peer.send_notification(ServerNotification::CustomNotification(
        CustomNotification::new("notifications/claude/channel", Some(params)),
    ))
    .await
}
