#!/usr/bin/env bash
# End-to-end smoke test without Claude Code.
#
# Starts agent-salon, performs the MCP Streamable HTTP initialize handshake,
# opens an SSE GET stream, POSTs a notification to /notify, and prints the
# resulting notifications/claude/channel event that the stream receives.

set -euo pipefail

PORT=${AGENT_SALON_PORT:-9315}
BIN=./target/release/agent-salon
MCP="http://127.0.0.1:$PORT/mcp?label=smoke"
NOTIFY="http://127.0.0.1:$PORT/notify?label=smoke"

if [[ ! -x "$BIN" ]]; then
  cargo build --release
fi

STREAM_LOG=$(mktemp)
cleanup() {
  [[ -n "${SERVER_PID:-}" ]] && kill "$SERVER_PID" 2>/dev/null || true
  [[ -n "${STREAM_PID:-}" ]] && kill "$STREAM_PID" 2>/dev/null || true
  rm -f "$STREAM_LOG"
}
trap cleanup EXIT

# 1. Start agent-salon.
AGENT_SALON_PORT="$PORT" "$BIN" &
SERVER_PID=$!

for _ in $(seq 1 20); do
  if nc -z 127.0.0.1 "$PORT" 2>/dev/null; then
    break
  fi
  sleep 0.1
done

echo "# agent-salon up on $PORT"

# 2. Initialize and capture session id.
HEADERS=$(mktemp)
curl -fsS -D "$HEADERS" -X POST "$MCP" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0.1"}}}' \
  >/dev/null
SESSION=$(awk 'BEGIN{IGNORECASE=1}/^mcp-session-id:/{gsub(/\r/,""); print $2}' "$HEADERS")
rm -f "$HEADERS"
echo "# session=$SESSION"

# 3. notifications/initialized.
curl -fsS -X POST "$MCP" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H "mcp-session-id: $SESSION" \
  -H 'MCP-Protocol-Version: 2025-06-18' \
  --data '{"jsonrpc":"2.0","method":"notifications/initialized"}' >/dev/null

# 4. Open standalone GET stream to receive server-initiated notifications.
curl -sS -N -X GET "$MCP" \
  -H 'Accept: text/event-stream' \
  -H "mcp-session-id: $SESSION" \
  -H 'MCP-Protocol-Version: 2025-06-18' \
  >"$STREAM_LOG" &
STREAM_PID=$!

sleep 0.3

# 5. POST a sample notification.
echo "# POST /notify"
jq -nc '{content: "hello", meta: {example: true}}' \
  | curl -fsS -X POST "$NOTIFY" \
      -H 'Content-Type: application/json' -d @- \
  | cat
echo

# 6. Wait briefly then dump what the stream saw.
sleep 0.5
echo
echo "# --- SSE stream ---"
cat "$STREAM_LOG"
