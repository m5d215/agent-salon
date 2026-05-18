use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CustomNotification, ErrorData, Implementation, PingRequest, ServerCapabilities, ServerInfo,
    ServerNotification, ServerRequest,
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
use crate::jsonl::JsonlLogger;
use crate::metrics::{
    LivenessLabels, MessageLabels, Metrics, SendFailureLabels, SessionEventLabels, normalise_kind,
};

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
/// `id` is a salon-local identity used for eviction bookkeeping in
/// `deliver_notification`: when a send fails on a specific peer, we need to
/// remove _that exact_ session from the registered list even though
/// `Peer<RoleServer>` itself exposes no identity (its internal channel is
/// private), so we tag every session at registration time.
pub struct Session {
    pub id: Uuid,
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
    pub metrics: Arc<Metrics>,
    pub jsonl: Arc<JsonlLogger>,
}

impl SalonState {
    pub fn new(
        port: u16,
        db: SqlitePool,
        aliases: HashMap<String, String>,
        metrics: Arc<Metrics>,
        jsonl: Arc<JsonlLogger>,
    ) -> Self {
        Self {
            port,
            message_count: AtomicU64::new(0),
            sessions: Mutex::new(Vec::new()),
            db,
            aliases,
            metrics,
            jsonl,
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
        let started = Instant::now();
        deliver_notification(&self.state, &payload, ctx).await;
        self.state
            .metrics
            .send_message_duration
            .observe(started.elapsed().as_secs_f64());

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
        // Treat label as identity: a new connection with the same label evicts
        // any prior session holding it. Reconnects (e.g. Claude Code's /clear)
        // would otherwise pile up ghost entries that only get pruned on a
        // failed send. Unlabeled sessions are left alone — they can't be
        // targeted, so duplicates do no harm.
        let evicted = if let Some(new_label) = label.as_deref() {
            let before = sessions.len();
            sessions.retain(|s| s.label.as_deref() != Some(new_label));
            before - sessions.len()
        } else {
            0
        };
        sessions.push(Session {
            id: Uuid::now_v7(),
            peer: ctx.peer,
            label: label.clone(),
        });
        let active = sessions.len();
        log!(
            "agent-salon: session initialized (label={}, {} active, evicted {})",
            label.as_deref().unwrap_or("<unlabeled>"),
            active,
            evicted,
        );

        let metrics = &self.state.metrics;
        metrics
            .session_events
            .get_or_create(&SessionEventLabels {
                event: "connected".to_string(),
            })
            .inc();
        if evicted > 0 {
            metrics
                .session_events
                .get_or_create(&SessionEventLabels {
                    event: "evicted".to_string(),
                })
                .inc_by(evicted as u64);
        }
        metrics.active_sessions.set(active as i64);

        self.state.jsonl.event(
            "session_connected",
            serde_json::json!({
                "label": label,
                "active": active,
                "evicted": evicted,
                "session_id": session_id,
            }),
        );
        if evicted > 0 {
            self.state.jsonl.event(
                "session_evicted",
                serde_json::json!({
                    "label": label,
                    "count": evicted,
                }),
            );
        }
    }
}

fn extract_label(query: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == "label")
        .map(|(_, v)| v.to_string())
}

/// Per-attempt timeout for the `ping` round-trip when probing whether a
/// session's SSE channel is alive. rmcp's streamable-HTTP transport silently
/// swallows send errors when the client's GET /mcp stream has died (the
/// message goes into the resume cache instead), so `send_notification` alone
/// cannot tell us that a delivery failed. A `ping` request expects a response,
/// so its outcome is observable.
///
/// 5s is short enough to retry within the budget below but generous enough
/// for an idle Claude Code session on a Tailnet hop. A truly busy client
/// (mid-turn) gets multiple chances via the retry loop.
const LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of probe attempts for a targeted send before giving up and
/// recording `liveness_timeout`. Three attempts at 5s each plus inter-attempt
/// backoff keeps total wall-clock around the previous 15s single-shot budget
/// while letting transient transport failures (Claude Code's hard-coded 306s
/// MCP reconnect, brief Wi-Fi blips) heal mid-probe. ADR 0004 §3.
const LIVENESS_PROBE_MAX_ATTEMPTS: u32 = 3;

/// Delay between probe attempts. Gives the receiver's MCP transport room to
/// finish reconnecting (Claude Code's exponential backoff is 1/2/4/8/16s, so
/// even one short pause often lets the new session register) before we
/// re-resolve the label and try again.
const LIVENESS_PROBE_RETRY_BACKOFF: Duration = Duration::from_millis(750);

/// Probe a peer once with a `ping` request, bounded by `LIVENESS_PROBE_TIMEOUT`.
/// Both transport errors and timeouts collapse to `false`.
async fn probe_alive_once(peer: &Peer<RoleServer>) -> bool {
    let req = ServerRequest::PingRequest(PingRequest::default());
    matches!(
        tokio::time::timeout(LIVENESS_PROBE_TIMEOUT, peer.send_request(req)).await,
        Ok(Ok(_))
    )
}

/// Probe a target-label peer with bounded retry. Between attempts, the
/// salon's session list is re-resolved so a session that was evicted and
/// replaced (Claude Code's 306s reconnect cycle is the common cause) lets the
/// next attempt land on the fresh peer rather than the one whose transport
/// just died. Returns the peer that ultimately responded along with the
/// 1-based attempt count, or `None` if every attempt failed.
async fn probe_alive_with_retry(
    state: &SalonState,
    target_label: &str,
    initial: Peer<RoleServer>,
) -> ProbeOutcome {
    let mut current = initial;
    for attempt in 1..=LIVENESS_PROBE_MAX_ATTEMPTS {
        if probe_alive_once(&current).await {
            return ProbeOutcome::Alive {
                peer: current,
                attempts: attempt,
            };
        }
        if attempt >= LIVENESS_PROBE_MAX_ATTEMPTS {
            return ProbeOutcome::Dead {
                attempts: attempt,
            };
        }
        tokio::time::sleep(LIVENESS_PROBE_RETRY_BACKOFF).await;
        let sessions = state.sessions.lock().await;
        match sessions.iter().find(|s| s.label.as_deref() == Some(target_label)) {
            Some(s) => current = s.peer.clone(),
            None => {
                return ProbeOutcome::Dead {
                    attempts: attempt,
                };
            }
        }
    }
    ProbeOutcome::Dead {
        attempts: LIVENESS_PROBE_MAX_ATTEMPTS,
    }
}

enum ProbeOutcome {
    Alive {
        peer: Peer<RoleServer>,
        attempts: u32,
    },
    Dead {
        attempts: u32,
    },
}

/// Deliver a notification to matching sessions and persist it to the DB.
///
/// - If `payload.target` is set, only sessions whose label equals it receive
///   the notification. The probe runs with bounded retry that re-resolves the
///   target label between attempts (ADR 0004 §3) so a session evicted and
///   replaced by Claude Code's 306s MCP reconnect can still be reached.
/// - If `payload.target` is None, every connected session receives it
///   (broadcast). Each candidate is probed once; no retry, since there is no
///   single target label to re-resolve against.
/// - A session whose probe ultimately fails is skipped for this message and
///   recorded under `delivery_errors`, but it stays registered — a transient
///   busy client must not be evicted on one missed probe. Only a session
///   whose `send_notification` itself errors is pruned from the session list.
///
/// Lock discipline: the `state.sessions` mutex is held only briefly, to
/// snapshot the candidate peers at the start and to apply post-send eviction
/// at the end. Probes and notification sends happen lock-free so new sessions
/// can register mid-flight (this is exactly what enables retry to pick up the
/// post-eviction replacement).
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

    let kind_label = normalise_kind(meta.get("kind").and_then(|v| v.as_str()));

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

    let metrics = &state.metrics;
    let target_label_for_metrics = resolved_target
        .clone()
        .unwrap_or_else(|| "<broadcast>".to_string());

    // Snapshot the candidates under a brief lock, then drop it so probes and
    // sends below run lock-free. Each candidate carries the session's salon
    // id so the post-send eviction pass can remove the exact session even if
    // the same label has been replaced in the meantime.
    let candidates: Vec<(Uuid, Peer<RoleServer>, String)> = {
        let sessions = state.sessions.lock().await;
        sessions
            .iter()
            .filter(|s| match &resolved_target {
                None => true,
                Some(target) => s.label.as_deref() == Some(target.as_str()),
            })
            .map(|s| {
                let label = s.label.clone().unwrap_or_else(|| "<unlabeled>".into());
                (s.id, s.peer.clone(), label)
            })
            .collect()
    };

    let mut to_evict: Vec<Uuid> = Vec::new();

    for (session_id, peer, label_for_log) in candidates {
        let outcome = match &resolved_target {
            Some(target) => probe_alive_with_retry(state, target, peer).await,
            None => {
                if probe_alive_once(&peer).await {
                    ProbeOutcome::Alive { peer, attempts: 1 }
                } else {
                    ProbeOutcome::Dead { attempts: 1 }
                }
            }
        };

        let (alive_peer, attempts) = match outcome {
            ProbeOutcome::Alive { peer, attempts } => (peer, attempts),
            ProbeOutcome::Dead { attempts } => {
                log!(
                    "agent-salon: skipping delivery (ping failed after {attempts} attempt(s)), \
                     keeping session: {label_for_log}"
                );
                metrics
                    .liveness
                    .get_or_create(&LivenessLabels {
                        target: label_for_log.clone(),
                        result: "timeout".to_string(),
                        attempt: attempts.to_string(),
                    })
                    .inc();
                metrics
                    .send_failures
                    .get_or_create(&SendFailureLabels {
                        reason: "liveness_timeout".to_string(),
                    })
                    .inc();
                state.jsonl.event(
                    "delivery_skipped",
                    serde_json::json!({
                        "target": label_for_log,
                        "reason": "liveness_timeout",
                        "attempts": attempts,
                        "message_id": id.to_string(),
                    }),
                );
                delivery_errors.push(label_for_log);
                continue;
            }
        };

        metrics
            .liveness
            .get_or_create(&LivenessLabels {
                target: label_for_log.clone(),
                result: "ok".to_string(),
                attempt: attempts.to_string(),
            })
            .inc();
        match alive_peer.send_notification(notification.clone()).await {
            Ok(()) => {
                metrics
                    .messages
                    .get_or_create(&MessageLabels {
                        source: payload.source.clone().unwrap_or_default(),
                        target: target_label_for_metrics.clone(),
                        kind: kind_label.clone(),
                    })
                    .inc();
                delivered_to.push(label_for_log);
            }
            Err(e) => {
                log!("agent-salon: dropping session (send failed): {e}");
                metrics
                    .send_failures
                    .get_or_create(&SendFailureLabels {
                        reason: "delivery_error".to_string(),
                    })
                    .inc();
                state.jsonl.event(
                    "send_failed",
                    serde_json::json!({
                        "target": label_for_log,
                        "error": e.to_string(),
                        "message_id": id.to_string(),
                    }),
                );
                delivery_errors.push(label_for_log);
                to_evict.push(session_id);
            }
        }
    }

    // A targeted send that found no matching live session is a target_unknown
    // failure. Broadcasts don't count — landing in nobody's mailbox is allowed
    // (there might just be nobody listening).
    if resolved_target.is_some() && delivered_to.is_empty() && delivery_errors.is_empty() {
        metrics
            .send_failures
            .get_or_create(&SendFailureLabels {
                reason: "target_unknown".to_string(),
            })
            .inc();
    }

    // Apply post-send eviction: remove sessions whose `send_notification`
    // returned Err. Match by salon-local session id so we don't accidentally
    // remove a different same-label session that has registered in the
    // meantime (e.g. mid-probe reconnect that the retry loop just picked up).
    let active_after = {
        let mut sessions = state.sessions.lock().await;
        if !to_evict.is_empty() {
            sessions.retain(|s| !to_evict.contains(&s.id));
        }
        sessions.len()
    };
    metrics.active_sessions.set(active_after as i64);

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
    match db::insert_message(&state.db, &row).await {
        Ok(()) => {
            metrics.messages_stored.inc();
        }
        Err(e) => {
            log!("agent-salon: failed to persist message {}: {e}", row.id);
        }
    }

    // Emit the message body to the JSONL log regardless of DB outcome — the
    // shipping path is independent of persistence, and the log line is the
    // most useful record either way (DB failures are rare but rare enough to
    // matter).
    state.jsonl.message(&row);
}
