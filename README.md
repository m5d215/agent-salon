# relay-mcp

A lightweight HTTP-to-MCP notification bridge for Claude Code. External processes POST to a local HTTP endpoint, and messages are forwarded as channel notifications to every connected Claude Code session.

relay-mcp runs as a standalone long-running daemon that serves both the external webhook (`POST /notify`) and the MCP Streamable HTTP transport (`/mcp`) on a single port. Claude Code connects over HTTP, so multiple concurrent sessions can share the same relay.

## Requirements

- Rust 1.70+
- Claude Code with `channelsEnabled` setting

## Build

```bash
cargo build --release
```

## Setup

### 1. Start relay-mcp (daemon)

```bash
./target/release/relay-mcp
# → listening on http://127.0.0.1:9315
```

Keep it running in a separate terminal / tmux pane / launchd job.

### 2. Register as MCP server (HTTP transport)

```bash
claude mcp add --scope project --transport http relay-mcp http://127.0.0.1:9315/mcp
```

Or write `.mcp.json` directly:

```json
{
  "mcpServers": {
    "relay-mcp": {
      "type": "http",
      "url": "http://127.0.0.1:9315/mcp"
    }
  }
}
```

### 3. Enable channel notifications

**Both are required.** Channel notifications are off by default in Claude Code.

Add to your settings file (`~/.claude/settings.json` or `.claude/settings.local.json`):

```json
{
  "channelsEnabled": true
}
```

### 4. Start Claude Code with channel flags

```bash
claude --dangerously-load-development-channels server:relay-mcp
```

The `--dangerously-load-development-channels` flag is needed for non-plugin MCP servers. Without it, the server will be rejected as "not on the approved channels allowlist".

## Usage

With relay-mcp running and a Claude Code session connected, send notifications via HTTP:

```bash
curl -X POST http://127.0.0.1:9315/notify \
  -H 'Content-Type: application/json' \
  -d '{"content": "Build finished", "source": "ci"}'
```

### POST /notify

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `content` | string | yes | Message body |
| `source` | string | no | Sender identifier (e.g. "ci", "webhook") |
| `meta` | object | no | Arbitrary key-value metadata |

**Responses:**

- `202 Accepted` -- notification queued for broadcast
- `422 Unprocessable Entity` -- missing or invalid body

Messages are broadcast to **every** connected Claude Code session. If no session is connected, the message is dropped.

### Configuration

| Env var | Default | Description |
|---------|---------|-------------|
| `RELAY_MCP_PORT` | `9315` | TCP port the daemon binds to |
| `RELAY_MCP_BIND` | `127.0.0.1` | Bind address. Set to `0.0.0.0` (or a specific interface IP) to accept connections from other machines — e.g. over a Tailscale / VPN network. |

## Local testing

Without Claude Code, you can exercise the full pipeline standalone:

```bash
./scripts/test-server.sh
```

The script spins up relay-mcp, runs through initialize / initialized / GET stream, POSTs a sample notification, and prints the resulting `notifications/claude/channel` event.

## Architecture

```
External Process                  relay-mcp (daemon)                 Claude Code
     |                                  |                                 |
     |  POST /notify (HTTP)             |                                 |
     |--------------------------------->|                                 |
     |  202 Accepted                    |                                 |
     |<---------------------------------|                                 |
     |                                  |  notifications/claude/channel   |
     |                                  |  (MCP Streamable HTTP / SSE)    |
     |                                  |-------------------------------->|
     |                                  |                                 |  (wakes session)
```

Internally, each connected Claude Code session is tracked as an `rmcp::Peer`. `POST /notify` broadcasts to every peer; peers that fail to send are pruned.

## Tech Stack

- **Rust** with `rmcp` (official MCP SDK), `axum`, `tokio`
- MCP Streamable HTTP server (`rmcp::transport::streamable_http_server`)
- Single binary, long-running daemon
