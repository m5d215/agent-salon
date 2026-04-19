mod admin;
mod db;
mod http;
mod mcp;

use std::net::SocketAddr;
use std::sync::Arc;

use mcp::{SalonHandler, SalonState};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::var("AGENT_SALON_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9315);
    let bind = std::env::var("AGENT_SALON_BIND").unwrap_or_else(|_| "127.0.0.1".to_string());
    let db_path =
        std::env::var("AGENT_SALON_DB").unwrap_or_else(|_| "./agent-salon.db".to_string());

    let pool = db::open(&db_path).await?;
    eprintln!("agent-salon: db at {db_path}");

    let listener = tokio::net::TcpListener::bind((bind.as_str(), port)).await?;
    let actual_addr = listener.local_addr()?;
    let actual_port = actual_addr.port();

    let state = Arc::new(SalonState::new(actual_port, pool));

    // MCP service: stateful streamable HTTP, fresh handler per session.
    let mcp_service = StreamableHttpService::new(
        {
            let state = state.clone();
            move || Ok(SalonHandler::new(state.clone()))
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let app = http::router(http::AppState {
        salon: state.clone(),
    })
    .nest_service("/mcp", mcp_service);

    eprintln!("agent-salon listening on http://{actual_addr}");
    eprintln!("  notify: POST http://{actual_addr}/notify");
    eprintln!("  mcp:         http://{actual_addr}/mcp");
    eprintln!("  admin UI:    http://{actual_addr}/admin");

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;

    Ok(())
}
