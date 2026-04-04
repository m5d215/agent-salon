#!/usr/bin/env bash

set -Ceuo pipefail

PORT=${RELAY_MCP_PORT:-9315}
BIN=./target/release/relay-mcp

if [[ ! -x "$BIN" ]]; then
  cargo build --release
fi

cat <<EOT >&2
Starting relay-mcp on port $PORT ...
Press Ctrl+C to stop.
---
Example:
  jq -nc '{ content: "hello", source: "test"}' | curl -fsS http://127.0.0.1:$PORT/notify -H 'Content-Type: application/json' -d @-
---

EOT

{
    # init
    jq -nc '{
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
            protocolVersion: "2025-03-26",
            capabilities: {},
            clientInfo: {
                name: "test",
                version: "0.1.0",
            }
        }
    }'

    # initialized
    jq -nc '{
        jsonrpc: "2.0",
        method: "notifications/initialized"
    }'

    cat -
} | RELAY_MCP_PORT="$PORT" "$BIN"
