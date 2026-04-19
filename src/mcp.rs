use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CustomNotification, ErrorData, Implementation, ServerCapabilities, ServerInfo,
    ServerNotification,
};
use rmcp::service::NotificationContext;
use rmcp::{Peer, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::http::NotifyPayload;

/// A connected MCP session and its user-supplied label (from `?label=` query).
pub struct Session {
    pub peer: Peer<RoleServer>,
    pub label: Option<String>,
}

/// Shared state of the salon.
pub struct SalonState {
    pub port: u16,
    pub message_count: AtomicU64,
    pub sessions: Mutex<Vec<Session>>,
}

impl SalonState {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            message_count: AtomicU64::new(0),
            sessions: Mutex::new(Vec::new()),
        }
    }
}

pub struct SalonHandler {
    state: Arc<SalonState>,
    /// This session's own label, captured from `?label=` on the /mcp URL at
    /// initialize time. Used as the `source` when this session sends a
    /// notification via the `send_message` tool.
    self_label: Arc<Mutex<Option<String>>>,
    tool_router: ToolRouter<Self>,
}

/// Tool parameters for `send_message`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMessageParams {
    /// Message body. Shown to the receiver inside the <channel> tag.
    pub content: String,
    /// Target session label. If omitted, the notification is broadcast to
    /// every connected session.
    #[serde(default)]
    pub target: Option<String>,
    /// Optional metadata. Every key becomes an attribute on the receiver's
    /// <channel> tag (e.g. `{"kind": "ack"}` -> `<channel kind="ack">`).
    #[serde(default)]
    pub meta: Option<HashMap<String, serde_json::Value>>,
}

#[tool_router]
impl SalonHandler {
    pub fn new(state: Arc<SalonState>) -> Self {
        Self {
            state,
            self_label: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Show the salon's HTTP endpoints, active sessions, and message count")]
    async fn salon_status(&self) -> String {
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

    #[tool(
        description = "Send a channel notification to another session (or broadcast). \
                       The sender identity (source) is taken from this session's own label \
                       and cannot be overridden. Requires this session to have been \
                       initialized with ?label=<name> on the /mcp URL."
    )]
    async fn send_message(
        &self,
        Parameters(params): Parameters<SendMessageParams>,
    ) -> Result<String, ErrorData> {
        let source = self.self_label.lock().await.clone();
        let Some(source) = source else {
            return Err(ErrorData::invalid_params(
                "This session has no label. Reconnect with ?label=<name> on the /mcp URL \
                 before calling send_message.",
                None,
            ));
        };

        self.state.message_count.fetch_add(1, Ordering::Relaxed);
        let target_for_reply = params.target.clone();
        let payload = NotifyPayload {
            content: params.content,
            target: params.target,
            meta: params.meta,
            source: Some(source),
        };
        deliver_notification(&self.state, &payload).await;

        Ok(match target_for_reply {
            Some(t) => format!("delivered to sessions labelled '{t}'"),
            None => "broadcast to all connected sessions".to_string(),
        })
    }
}

#[tool_handler]
impl ServerHandler for SalonHandler {
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
            .with_server_info(Implementation::new("agent-salon", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "agent-salon: a gathering place for Claude Code sessions. \
                 Connect with ?label=<name> on the /mcp URL to register your session. \
                 Use the `send_message` tool to deliver notifications to other labelled \
                 sessions (or broadcast). External processes can also POST to the \
                 /notify?label=<name> HTTP endpoint.",
            )
    }

    async fn on_initialized(&self, ctx: NotificationContext<RoleServer>) {
        let label = ctx
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.uri.query())
            .and_then(extract_label);

        *self.self_label.lock().await = label.clone();

        let mut sessions = self.state.sessions.lock().await;
        sessions.push(Session {
            peer: ctx.peer,
            label: label.clone(),
        });
        eprintln!(
            "agent-salon: session initialized (label={}, {} active)",
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
pub async fn deliver_notification(state: &SalonState, payload: &NotifyPayload) {
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
            Err(e) => eprintln!("agent-salon: dropping session (send failed): {e}"),
        }
    }
    *sessions = alive;
}
