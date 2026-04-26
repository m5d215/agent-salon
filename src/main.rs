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
    let config = load_config_file();

    let port: u16 = cfg_var(&config, "AGENT_SALON_PORT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(9315);
    let bind = cfg_var(&config, "AGENT_SALON_BIND").unwrap_or_else(|| "0.0.0.0".to_string());
    let db_path =
        cfg_var(&config, "AGENT_SALON_DB").unwrap_or_else(|| "./agent-salon.db".to_string());
    let aliases = cfg_var(&config, "AGENT_SALON_ALIASES")
        .map(|s| parse_aliases(&s))
        .unwrap_or_default();
    let allowed_hosts = cfg_var(&config, "AGENT_SALON_ALLOWED_HOSTS")
        .map(|s| parse_allowed_hosts(&s))
        .unwrap_or_default();

    let pool = db::open(&db_path).await?;
    eprintln!("agent-salon: db at {db_path}");
    if !aliases.is_empty() {
        // Count only — do not log the alias → real mapping, to avoid leaving
        // the real labels in plain-text logs that may be shipped elsewhere.
        eprintln!("agent-salon: {} target alias(es) loaded", aliases.len());
    }
    if !allowed_hosts.is_empty() {
        eprintln!("agent-salon: allowed hosts: {}", allowed_hosts.join(", "));
    }

    let listener = tokio::net::TcpListener::bind((bind.as_str(), port)).await?;
    let actual_addr = listener.local_addr()?;
    let actual_port = actual_addr.port();

    let state = Arc::new(SalonState::new(actual_port, pool, aliases));

    // MCP service: stateful streamable HTTP, fresh handler per session.
    let mut mcp_config = StreamableHttpServerConfig::default();
    if !allowed_hosts.is_empty() {
        mcp_config = mcp_config.with_allowed_hosts(allowed_hosts);
    }
    let mcp_service = StreamableHttpService::new(
        {
            let state = state.clone();
            move || Ok(SalonHandler::new(state.clone()))
        },
        Arc::new(LocalSessionManager::default()),
        mcp_config,
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

/// Resolve a config value, preferring the live process environment over
/// any value loaded from the config file. Empty environment values are
/// treated as "set" (returned as-is) — same behavior as `std::env::var`.
fn cfg_var(config: &HashMap<String, String>, key: &str) -> Option<String> {
    std::env::var(key).ok().or_else(|| config.get(key).cloned())
}

/// Read `AGENT_SALON_CONFIG` and parse the file at that path. Returns an
/// empty map when the env var is unset (no config file used) or when the
/// file is missing (warning logged). The path is exposed via env so that
/// platform installers (e.g. the Homebrew formula) can point at
/// `${HOMEBREW_PREFIX}/etc/agent-salon.conf` without code changes here.
fn load_config_file() -> HashMap<String, String> {
    let Ok(path) = std::env::var("AGENT_SALON_CONFIG") else {
        return HashMap::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let map = parse_config(&s);
            eprintln!("agent-salon: loaded {} setting(s) from {path}", map.len());
            map
        }
        Err(e) => {
            eprintln!("agent-salon: skipping config {path}: {e}");
            HashMap::new()
        }
    }
}

/// Parse a `KEY=VALUE` config file. Lines starting with `#` and blank
/// lines are skipped. Keys with no `=` are skipped. Surrounding double
/// quotes around the value (`KEY="value"`) are stripped. Whitespace
/// around the key and around the value (outside the quotes) is trimmed.
fn parse_config(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        let v = v.trim();
        let v = if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
            &v[1..v.len() - 1]
        } else {
            v
        };
        out.insert(k.to_string(), v.to_string());
    }
    out
}

/// Parse `AGENT_SALON_ALLOWED_HOSTS` of the form `host,host:port,...`.
/// Whitespace around entries is trimmed; empty entries are skipped. The
/// returned list is fed into `StreamableHttpServerConfig::with_allowed_hosts`,
/// which performs the actual authority parsing.
fn parse_allowed_hosts(s: &str) -> Vec<String> {
    s.split(',')
        .filter_map(|h| {
            let h = h.trim();
            if h.is_empty() {
                None
            } else {
                Some(h.to_string())
            }
        })
        .collect()
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
    use super::{parse_aliases, parse_allowed_hosts, parse_config};

    #[test]
    fn parses_config_basic() {
        let s = "
            # comment line, ignored
            AGENT_SALON_PORT=9315
            AGENT_SALON_BIND = 127.0.0.1
            AGENT_SALON_ALLOWED_HOSTS=\"host.example.com,localhost\"
        ";
        let m = parse_config(s);
        assert_eq!(m.get("AGENT_SALON_PORT").map(String::as_str), Some("9315"));
        assert_eq!(
            m.get("AGENT_SALON_BIND").map(String::as_str),
            Some("127.0.0.1")
        );
        assert_eq!(
            m.get("AGENT_SALON_ALLOWED_HOSTS").map(String::as_str),
            Some("host.example.com,localhost")
        );
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn parses_config_skips_blank_and_malformed() {
        let s = "\n\n  \n=novalue\nNOEQUALS\nKEY=value\n";
        let m = parse_config(s);
        assert_eq!(m.get("KEY").map(String::as_str), Some("value"));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn parses_config_preserves_inner_quotes_and_equals() {
        let s = "KEY=value=with=equals\nQUOTED=\"a=b,c=d\"";
        let m = parse_config(s);
        assert_eq!(m.get("KEY").map(String::as_str), Some("value=with=equals"));
        assert_eq!(m.get("QUOTED").map(String::as_str), Some("a=b,c=d"));
    }

    #[test]
    fn parses_allowed_hosts_csv() {
        let h = parse_allowed_hosts("localhost, 127.0.0.1, example.com:8080");
        assert_eq!(h, vec!["localhost", "127.0.0.1", "example.com:8080"]);
    }

    #[test]
    fn skips_empty_allowed_hosts_entries() {
        let h = parse_allowed_hosts(" , localhost ,, ");
        assert_eq!(h, vec!["localhost"]);
    }

    #[test]
    fn empty_allowed_hosts_input_yields_empty_vec() {
        assert!(parse_allowed_hosts("").is_empty());
        assert!(parse_allowed_hosts("   ,  ").is_empty());
    }

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
