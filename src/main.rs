mod http;
mod mcp;

use std::sync::Arc;

use mcp::{RelayHandler, RelayState};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::var("RELAY_MCP_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9315);
    let bind = std::env::var("RELAY_MCP_BIND").unwrap_or_else(|_| "127.0.0.1".to_string());

    let listener = tokio::net::TcpListener::bind((bind.as_str(), port)).await?;
    let actual_addr = listener.local_addr()?;
    let actual_port = actual_addr.port();

    let state = Arc::new(RelayState::new(actual_port));

    // MCP service: stateful streamable HTTP, fresh handler per session.
    let mcp_service = StreamableHttpService::new(
        {
            let state = state.clone();
            move || Ok(RelayHandler::new(state.clone()))
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let app = http::router(http::AppState {
        relay: state.clone(),
    })
    .nest_service("/mcp", mcp_service);

    eprintln!("relay-mcp listening on http://{actual_addr}");
    eprintln!("  notify: POST http://{actual_addr}/notify");
    eprintln!("  mcp:         http://{actual_addr}/mcp");

    axum::serve(listener, app).await?;

    Ok(())
}
