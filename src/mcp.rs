use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CustomNotification, ErrorData, Implementation, ServerCapabilities, ServerInfo,
    ServerNotification,
};
use rmcp::service::NotificationContext;
use rmcp::{Peer, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::db::{self, MessageRow, Via};
use crate::http::NotifyPayload;

/// Protocol guidance delivered to every MCP client at `initialize`.
/// Kept here (not inlined) because it is the primary mechanism by which
/// peer sessions learn the conventions of the salon.
const INSTRUCTIONS: &str = r#"agent-salon is a gathering place for Claude Code sessions. Each session
registers with a label and can exchange `notifications/claude/channel`
messages with the other sessions currently connected.

## Your session
- You are labelled by the `?label=<name>` you used to connect on the /mcp URL.
- Other sessions can direct a message at you by targeting that label.
- Unlabelled sessions can receive broadcasts but cannot be targeted and
  cannot call `send_message`.

## Sending
Use the `send_message` tool:
- `content` (required): the message body shown to the recipient.
- `target` (optional): the recipient's label. Omit to broadcast.
- `meta` (optional): key/value object. Every key becomes an attribute on
  the recipient's `<channel>` tag.

Your identity (`source`) is injected automatically from your own label.
It cannot be overridden from tool arguments — the transport layer owns
the claim of who you are.

## Receiving
Incoming notifications appear in your conversation as:

    <channel source="agent-salon" source="<sender-label>" id="<uuid>"
             ts="<rfc3339>" kind="..." ...>
      content...
    </channel>

The outer `source="agent-salon"` is the server; the second `source` is
the sender's label — that is the one that identifies your peer. Use
the `id` attribute to reference the message in a reply.

## Canonical meta keys
Prefer these keys when you can, so messages interoperate across sessions:

- `id`         UUID of this message. Injected by agent-salon — receivers
               echo it back via `reply_to` to thread a conversation.
- `reply_to`   The `id` of the message this one answers. Set it whenever
               you respond to a specific earlier message.
- `kind`       Message type. Suggested values:
               - `info`     FYI, no reply expected.
               - `status`   state update (build done, task complete, …).
               - `ack`      acknowledges an earlier message (use `reply_to`).
               - `request`  asks the recipient to do something.
               - `question` asks for information; expect a `reply`.
               - `reply`    answers an earlier question/request (use `reply_to`).
- `priority`   `low` | `normal` | `high` | `urgent`.
- `topic`      Free-form thread label for grouping related messages.
- `commit`     Git commit hash when announcing code changes.

These are conventions, not enforcement — unknown keys pass through
unchanged, but peers may not know how to react to them.

## Discovery
Call the `salon_status` tool to see which sessions are currently online
and what labels they hold. That list is the ground truth for who you
can target.
"#;

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
    pub db: SqlitePool,
    /// Alias → real-label map applied to `target` in `deliver_notification`.
    /// Lets a sender use an innocuous alias (e.g. `notes`) that resolves to
    /// the real session label (e.g. `laptop-a`) so the real target is never
    /// named in the sender's environment — useful when the sender runs under
    /// a censored / observed LLM.
    pub aliases: HashMap<String, String>,
}

impl SalonState {
    pub fn new(port: u16, db: SqlitePool, aliases: HashMap<String, String>) -> Self {
        Self {
            port,
            message_count: AtomicU64::new(0),
            sessions: Mutex::new(Vec::new()),
            db,
            aliases,
        }
    }
}

/// Context about where a notification originated, captured from the caller's
/// transport (HTTP for `/notify`, MCP for `send_message`).
#[derive(Debug, Clone, Default)]
pub struct DeliveryContext {
    pub via: Option<Via>,
    pub sender_addr: Option<String>,
    pub sender_session_id: Option<String>,
}

pub struct SalonHandler {
    state: Arc<SalonState>,
    /// This session's own label, captured from `?label=` on the /mcp URL at
    /// initialize time. Used as the `source` when this session sends a
    /// notification via the `send_message` tool.
    self_label: Arc<Mutex<Option<String>>>,
    /// This session's MCP session id (header `Mcp-Session-Id`), captured at
    /// initialize time. Stored on each persisted message so tool-originated
    /// rows carry the session id of the caller.
    self_session_id: Arc<Mutex<Option<String>>>,
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
            self_session_id: Arc::new(Mutex::new(None)),
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
        let ctx = DeliveryContext {
            via: Some(Via::Tool),
            sender_addr: None,
            sender_session_id: self.self_session_id.lock().await.clone(),
        };
        deliver_notification(&self.state, &payload, ctx).await;

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
            .with_server_info(Implementation::new(
                "agent-salon",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    async fn on_initialized(&self, ctx: NotificationContext<RoleServer>) {
        let parts = ctx.extensions.get::<http::request::Parts>();
        let label = parts.and_then(|p| p.uri.query()).and_then(extract_label);
        let session_id = parts
            .and_then(|p| p.headers.get("mcp-session-id"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        *self.self_label.lock().await = label.clone();
        *self.self_session_id.lock().await = session_id.clone();

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

/// Deliver a notification to matching sessions and persist it to the DB.
///
/// - If `payload.target` is set, only sessions whose label equals it receive
///   the notification.
/// - If `payload.target` is None, every connected session receives it
///   (broadcast).
/// - Sessions whose channel has closed are pruned and recorded under
///   `delivery_errors` in the persisted row.
pub async fn deliver_notification(
    state: &SalonState,
    payload: &NotifyPayload,
    ctx: DeliveryContext,
) {
    let id = Uuid::now_v7();
    let ts = Utc::now();
    let meta = {
        let mut map = payload.meta.clone().unwrap_or_default();
        if let Some(source) = &payload.source {
            map.insert("source".into(), serde_json::Value::String(source.clone()));
        }
        map.insert("ts".into(), serde_json::Value::String(ts.to_rfc3339()));
        // Inject the message id so the receiver sees it as a <channel id="..."> attribute
        // and can refer back to it in `reply_to`.
        map.insert("id".into(), serde_json::Value::String(id.to_string()));
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

    let mut delivered_to: Vec<String> = Vec::new();
    let mut delivery_errors: Vec<String> = Vec::new();

    // Resolve `target` through the alias map. Aliases win over real labels —
    // if `target` matches an alias, only sessions wearing the aliased real
    // label receive the notification. The resolved value is what we match
    // against and what we persist; the fact that an alias was used is not
    // recorded, so admin UI filters work on real labels uniformly.
    let resolved_target: Option<String> = payload.target.as_deref().map(|t| {
        state
            .aliases
            .get(t)
            .cloned()
            .unwrap_or_else(|| t.to_string())
    });

    let mut sessions = state.sessions.lock().await;
    let mut alive = Vec::with_capacity(sessions.len());
    for session in sessions.drain(..) {
        let matches = match &resolved_target {
            None => true,
            Some(target) => session.label.as_deref() == Some(target.as_str()),
        };
        if !matches {
            alive.push(session);
            continue;
        }
        let label_for_log = session
            .label
            .clone()
            .unwrap_or_else(|| "<unlabeled>".into());
        match session.peer.send_notification(notification.clone()).await {
            Ok(()) => {
                delivered_to.push(label_for_log);
                alive.push(session);
            }
            Err(e) => {
                eprintln!("agent-salon: dropping session (send failed): {e}");
                delivery_errors.push(label_for_log);
            }
        }
    }
    *sessions = alive;
    drop(sessions);

    let row = MessageRow {
        id,
        ts,
        via: ctx.via.unwrap_or(Via::Notify),
        source: payload.source.clone().unwrap_or_default(),
        target: resolved_target,
        content: payload.content.clone(),
        meta: serde_json::Value::Object(meta.into_iter().collect()),
        delivered_to,
        delivery_errors,
        sender_addr: ctx.sender_addr,
        sender_session_id: ctx.sender_session_id,
    };
    if let Err(e) = db::insert_message(&state.db, &row).await {
        eprintln!("agent-salon: failed to persist message {}: {e}", row.id);
    }
}
