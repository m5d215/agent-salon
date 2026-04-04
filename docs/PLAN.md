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
- **Capabilities**: `tools` + `experimental: { "claude/channel": {} }` (required for Claude Code to accept channel notifications)

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
- `422 Unprocessable Entity` — missing or invalid body

### MCP Notification

```json
{
  "method": "notifications/claude/channel",
  "params": {
    "content": "...",
    "meta": {
      "source": "...",
      "ts": "2026-04-03T10:00:00Z",
      "...": "..."
    }
  }
}
```

### MCP Tools

| Tool | Description |
|------|-------------|
| `relay_status` | Show HTTP endpoint URL, port, message count |

## Configuration

Port selection strategy (in order):
1. `RELAY_MCP_PORT` environment variable
2. Auto-select available port

The server prints the listening port to stderr on startup.

### Claude Code settings

Channel notifications require two settings:

1. **`channelsEnabled: true`** in `~/.claude/settings.json` or `.claude/settings.local.json`
2. **`--dangerously-load-development-channels server:relay-mcp`** flag at startup (required for non-plugin MCP servers)

## Tech Stack

- **Language**: Rust
- **MCP SDK**: `rmcp` (MCP official Rust SDK, v1.3.0+)
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
├── scripts/
│   └── test-server.sh      # standalone test without Claude Code
├── docs/
│   └── PLAN.md
├── Cargo.toml
└── README.md
```

## Example Usage

### 1. Register as MCP server

```bash
claude mcp add --scope project relay-mcp \
  -e RELAY_MCP_PORT=9315 \
  -- /path/to/relay-mcp/target/release/relay-mcp
```

### 2. Start Claude Code

```bash
claude --dangerously-load-development-channels server:relay-mcp
```

### 3. Push from external process

```bash
curl -X POST http://localhost:9315/notify \
  -H 'Content-Type: application/json' \
  -d '{"content": "PROJ-123 assigned to you", "source": "jira"}'
```

### 4. Claude Code session receives

```xml
<channel source="relay-mcp" source="jira" ts="2026-04-03T10:00:00Z">
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
