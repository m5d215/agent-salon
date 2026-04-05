# relay-mcp

A lightweight HTTP-to-MCP notification bridge for Claude Code. External processes POST to a local HTTP endpoint, and messages are forwarded as channel notifications to the active Claude Code session.

## Requirements

- Rust 1.70+
- Claude Code with `channelsEnabled` setting

## Build

```bash
cargo build --release
```

## Setup

### 1. Register as MCP server

```bash
# Project scope (recommended)
claude mcp add --scope project relay-mcp -- /path/to/relay-mcp/target/release/relay-mcp

# Or user scope (all projects)
claude mcp add --scope user relay-mcp -- /path/to/relay-mcp/target/release/relay-mcp
```

### 2. Enable channel notifications

**Both are required.** Channel notifications are off by default in Claude Code.

Add to your settings file (`~/.claude/settings.json` or `.claude/settings.local.json`):

```json
{
  "channelsEnabled": true
}
```

### 3. Start Claude Code with channel flags

```bash
claude --dangerously-load-development-channels server:relay-mcp
```

The `--dangerously-load-development-channels` flag is needed for non-plugin MCP servers. Without it, the server will be rejected as "not on the approved channels allowlist".

## Usage

With the session running, send notifications via HTTP:

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

- `202 Accepted` -- notification sent
- `422 Unprocessable Entity` -- missing or invalid body

### Port configuration

| Method | Description |
|--------|-------------|
| `RELAY_MCP_PORT=9315` | Fixed port via environment variable |
| (unset) | OS assigns an available port |

## Local testing

Without Claude Code, you can test the MCP server standalone:

```bash
./scripts/test-server.sh
```

Then POST from another terminal:

```bash
jq -nc '{ content: "hello", source: "test"}' \
  | curl -fsS http://127.0.0.1:9315/notify \
    -H 'Content-Type: application/json' -d @-
```

The MCP JSON-RPC notification will appear on stdout.

## Architecture

```
External Process                relay-mcp                    Claude Code
     |                            |                              |
     |  POST /notify (HTTP)       |                              |
     |--------------------------->|                              |
     |  202 Accepted              |                              |
     |<---------------------------|                              |
     |                            |  notifications/claude/channel (stdio)
     |                            |----------------------------->|
     |                            |                              |  (wakes session)
```

## Tech Stack

- **Rust** with `rmcp` (official MCP SDK), `axum`, `tokio`
- Single binary, ~4 MB, ~2-5 MB memory at idle
