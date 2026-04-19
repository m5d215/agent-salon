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

There are two ways to send a notification:

1. **From inside a Claude Code session** — call the `send_message` MCP tool. Schema-validated, no URL construction, and the sender identity is bound to the session's own label (no spoofing possible).
2. **From an external process** (CI hook, shell script, webhook) — POST to `/notify` with a `?label=` query parameter.

### MCP tool: `send_message`

Any connected session that was initialized with `?label=<name>` can call:

```jsonc
// tools/call arguments
{
  "content": "Build finished",           // required
  "target":  "laptop-a",                 // optional; omit to broadcast
  "meta":    { "commit": "abc123" }      // optional; each key becomes a <channel> attribute
}
```

The sender (`source`) is taken from the calling session's own `?label=` and cannot be overridden from the tool arguments. A session without a label receives `-32602 Invalid Params` if it tries to call `send_message`.

### POST /notify?label=&lt;name&gt;

The sender's identity lives in the URL (`?label=<name>`), **not in the body**. This is deliberate: the body is usually produced by an LLM or an automated process, and a body-declared `source` would let the payload spoof its own identity. Putting the label on the URL pushes identification into the transport layer, which is controlled by the calling environment (shell config, `.mcp.json`, CI secrets, etc.).

| Location | Field | Type | Required | Description |
|----------|-------|------|----------|-------------|
| query | `label` | string | **yes** | Sender identifier. Surfaced to the receiver as `<channel source="...">`. |
| body | `content` | string | yes | Message body |
| body | `target` | string | no | Session label to deliver to. If omitted, the notification is broadcast to every connected session. |
| body | `meta` | object | no | Arbitrary key-value metadata. Every key is passed through to the channel tag as an attribute. |

`source` in the body is **ignored** (silently stripped). Use the query parameter.

**Responses:**

- `202 Accepted` — notification queued for delivery
- `400 Bad Request` — `?label=` missing
- `422 Unprocessable Entity` — missing or invalid body

If no session matches the target (or no session is connected at all), the message is dropped silently.

```bash
# Minimal example — send to "laptop-a", claim to be "ci".
curl -X POST 'http://127.0.0.1:9315/notify?label=ci' \
  -H 'Content-Type: application/json' \
  -d '{"content":"Build finished","target":"laptop-a"}'
```

### Labelling sessions

Each Claude Code session can identify itself with a label via a `?label=<name>` query parameter on the `/mcp` URL. `POST /notify` with a matching `target` then fans out only to sessions wearing that label.

```json
{
  "mcpServers": {
    "relay-mcp": {
      "type": "http",
      "url": "http://127.0.0.1:9315/mcp?label=laptop-a"
    }
  }
}
```

```bash
# Only the "laptop-a" session(s) receive this:
curl -X POST http://127.0.0.1:9315/notify \
  -H 'Content-Type: application/json' \
  -d '{"content":"Build finished","target":"laptop-a"}'
```

Multiple sessions can share a label — they form a group and every targeted notification fans out to all of them. Unlabeled sessions only receive broadcasts (notifications without `target`).

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
