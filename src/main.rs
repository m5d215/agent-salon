mod admin;
mod db;
mod http;
mod mcp;

use std::collections::HashMap;
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
    let bind = std::env::var("AGENT_SALON_BIND").unwrap_or_else(|_| "0.0.0.0".to_string());
    let db_path =
        std::env::var("AGENT_SALON_DB").unwrap_or_else(|_| "./agent-salon.db".to_string());
    let aliases = std::env::var("AGENT_SALON_ALIASES")
        .ok()
        .map(|s| parse_aliases(&s))
        .unwrap_or_default();

    let pool = db::open(&db_path).await?;
    eprintln!("agent-salon: db at {db_path}");
    if !aliases.is_empty() {
        // Count only — do not log the alias → real mapping, to avoid leaving
        // the real labels in plain-text logs that may be shipped elsewhere.
        eprintln!("agent-salon: {} target alias(es) loaded", aliases.len());
    }

    let listener = tokio::net::TcpListener::bind((bind.as_str(), port)).await?;
    let actual_addr = listener.local_addr()?;
    let actual_port = actual_addr.port();

    let state = Arc::new(SalonState::new(actual_port, pool, aliases));

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

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

/// Parse `AGENT_SALON_ALIASES` of the form `alias:real,alias2:real2`.
/// Whitespace around entries and around the colon is trimmed. Empty pairs
/// and malformed entries (no colon, empty side) are skipped silently.
fn parse_aliases(s: &str) -> HashMap<String, String> {
    s.split(',')
        .filter_map(|pair| {
            let pair = pair.trim();
            if pair.is_empty() {
                return None;
            }
            let (k, v) = pair.split_once(':')?;
            let k = k.trim();
            let v = v.trim();
            if k.is_empty() || v.is_empty() {
                return None;
            }
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_aliases;

    #[test]
    fn parses_simple_pairs() {
        let m = parse_aliases("notes:laptop-a,drafts:home-mac");
        assert_eq!(m.get("notes").map(String::as_str), Some("laptop-a"));
        assert_eq!(m.get("drafts").map(String::as_str), Some("home-mac"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn trims_whitespace_and_skips_malformed() {
        let m = parse_aliases(" notes : laptop-a , , bad , :empty, key: ,drafts:home-mac");
        assert_eq!(m.get("notes").map(String::as_str), Some("laptop-a"));
        assert_eq!(m.get("drafts").map(String::as_str), Some("home-mac"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn empty_input_yields_empty_map() {
        assert!(parse_aliases("").is_empty());
    }
}
