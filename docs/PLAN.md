# relay-mcp Design Plan

## Overview

Generic HTTP-to-MCP notification bridge.
External processes POST messages to a local HTTP endpoint, and the MCP server forwards them as `notifications/claude/channel` to the connected Claude Code session.

```
┌─────────────┐  HTTP POST   ┌─────────────┐  stdio/MCP notification  ┌──────────────┐
│ jira-watcher │─────────────▶│             │──────────────────────────▶│              │
│ slack-poller │              │  relay-mcp  │                           │ Claude Code  │
│ curl / etc.  │              │             │◀─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ │  Session     │
└─────────────┘              └─────────────┘  stdio (MCP transport)    └──────────────┘
```

## Motivation

Claude Code's Discord plugin proves that `notifications/claude/channel` works for pushing external messages into a session. But the pattern is tightly coupled to Discord. relay-mcp extracts the generic part — HTTP in, MCP notification out — so any external process can push messages.

## Architecture

### MCP Server (core)

- **Transport**: stdio (standard MCP server, spawned by Claude Code)
- **HTTP endpoint**: localhost, configurable port
- **Single responsibility**: receive HTTP POST, emit MCP notification

### HTTP Interface

```
POST /notify
Content-Type: application/json

{
  "content": "string (required) — message body",
  "source": "string (optional) — e.g. 'jira', 'slack', 'cron'",
  "meta": {
    // arbitrary key-value pairs, forwarded as-is
  }
}
```

Response:
- `202 Accepted` — notification sent
- `400 Bad Request` — missing content

### MCP Notification

```typescript
mcp.notification({
  method: 'notifications/claude/channel',
  params: {
    content,       // from POST body
    meta: {
      source,      // from POST body
      ts,          // ISO 8601 timestamp (server-generated)
      ...meta      // from POST body
    }
  }
})
```

### MCP Tools (optional, for session-side control)

| Tool | Description |
|------|-------------|
| `relay_status` | Show HTTP endpoint URL, port, message count |

## Configuration

Port selection strategy (in order):
1. `RELAY_MCP_PORT` environment variable
2. Auto-select available port

The server prints the listening port to stderr on startup so both Claude Code logs and external scripts can discover it. Also writes `~/.relay-mcp/port` for programmatic discovery.

## Tech Stack

- **Language**: Rust
- **MCP SDK**: `rmcp` (MCP公式 Rust SDK, v1.3.0+)
- **HTTP**: `axum` (tokio-based)
- **Async Runtime**: `tokio`
- **Serialization**: `serde` + `serde_json`

## File Structure

```
relay-mcp/
├── src/
│   ├── main.rs             # entrypoint
│   ├── mcp.rs              # MCP server handler + notification
│   └── http.rs             # axum HTTP endpoint
├── docs/
│   └── PLAN.md
├── Cargo.toml
└── README.md
```

## Example Usage

### 1. Register as MCP server

```json
// ~/.claude/settings.json (mcpServers)
{
  "relay-mcp": {
    "command": "bun",
    "args": ["run", "/path/to/relay-mcp/src/server.ts"],
    "env": {
      "RELAY_MCP_PORT": "9315"
    }
  }
}
```

### 2. Push from external process

```bash
curl -X POST http://localhost:9315/notify \
  -H 'Content-Type: application/json' \
  -d '{"content": "PROJ-123 assigned to you", "source": "jira"}'
```

### 3. Claude Code session receives

```xml
<channel source="relay-mcp" ts="2026-04-03T10:00:00Z">
PROJ-123 assigned to you
</channel>
```

## Out of Scope (for now)

- Authentication / token-based access control
- Queue / persistence (if session is not connected, messages are dropped)
- WebSocket or SSE transport
- Built-in pollers (Jira, Slack, etc.) — these are separate CLIs that POST to relay-mcp

## Reference

- Discord plugin implementation: `~/.claude/plugins/cache/claude-plugins-official/discord/0.0.4/server.ts`
  - `handleInbound()` (line 802-884): message processing + notification emission
  - `mcp.notification()` (line 868-883): the exact notification format Claude Code consumes
