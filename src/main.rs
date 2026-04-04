mod http;
mod mcp;

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use mcp::{RelayHandler, RelayState};
use rmcp::ServiceExt;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::var("RELAY_MCP_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // Bind HTTP listener early to resolve the actual port.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let actual_port = listener.local_addr()?.port();

    let state = Arc::new(RelayState {
        port: actual_port,
        message_count: AtomicU64::new(0),
    });

    // Channel: HTTP handler -> MCP notification forwarder.
    let (tx, mut rx) = mpsc::channel::<http::NotifyPayload>(256);

    // Start MCP server on stdio.
    let handler = RelayHandler::new(state.clone());
    let service = handler.serve(rmcp::transport::io::stdio()).await?;
    let peer = service.peer().clone();

    eprintln!("relay-mcp HTTP listening on http://127.0.0.1:{actual_port}");

    // Start HTTP server.
    let app = http::router(http::AppState {
        relay: state.clone(),
        tx,
    });
    let http_handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    // Forward HTTP payloads as MCP notifications.
    let notify_handle = tokio::spawn(async move {
        while let Some(payload) = rx.recv().await {
            if let Err(e) = mcp::send_notification(&peer, &payload).await {
                eprintln!("relay-mcp: notification error: {e}");
            }
        }
    });

    // Block until MCP session ends (Claude Code closes stdin).
    service
        .waiting()
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?;

    http_handle.abort();
    notify_handle.abort();

    Ok(())
}
