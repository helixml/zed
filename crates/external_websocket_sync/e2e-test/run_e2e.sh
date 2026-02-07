#!/usr/bin/env bash
set -euo pipefail

# E2E test for Zed WebSocket sync
# Tests the full flow: Zed connects → sends agent_ready → mock server sends
# chat_message → Zed creates thread → streams response → sends message_completed
#
# Environment variables:
#   ZED_BINARY     - Path to Zed binary (default: /usr/local/bin/zed)
#   TEST_TIMEOUT   - Timeout in seconds (default: 120)

echo "============================================"
echo "  Zed WebSocket Sync E2E Test"
echo "============================================"
echo ""

ZED_BINARY="${ZED_BINARY:-/usr/local/bin/zed}"
TEST_TIMEOUT="${TEST_TIMEOUT:-120}"
PROJECT_DIR="/test/project"
MOCK_PORT_FILE="/tmp/mock_helix_port"

# Cleanup function
cleanup() {
    echo "[cleanup] Shutting down..."
    [ -n "${ZED_PID:-}" ] && kill "$ZED_PID" 2>/dev/null || true
    [ -n "${MOCK_PID:-}" ] && kill "$MOCK_PID" 2>/dev/null || true
    [ -n "${XVFB_PID:-}" ] && kill "$XVFB_PID" 2>/dev/null || true
    rm -f "$MOCK_PORT_FILE"
}
trap cleanup EXIT

# Start D-Bus session (required by Zed for GPU init / portal notifications)
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    export DBUS_SESSION_BUS_ADDRESS=$(dbus-daemon --session --fork --print-address 2>/dev/null || true)
    echo "[setup] D-Bus session: ${DBUS_SESSION_BUS_ADDRESS:-none}"
fi

# Start virtual framebuffer if no display
if ! xdpyinfo -display "${DISPLAY:-}" >/dev/null 2>&1; then
    echo "[setup] Starting Xvfb on :99..."
    Xvfb :99 -screen 0 1280x720x24 -ac +extension GLX &
    XVFB_PID=$!
    export DISPLAY=:99
    sleep 1
    if ! kill -0 "$XVFB_PID" 2>/dev/null; then
        echo "[error] Xvfb failed to start"
        exit 1
    fi
    echo "[setup] Xvfb started (PID $XVFB_PID)"
fi

# Verify Zed binary exists
if [ ! -f "$ZED_BINARY" ]; then
    echo "[error] Zed binary not found at $ZED_BINARY"
    exit 1
fi

echo "[setup] Zed binary: $ZED_BINARY"
echo "[setup] Timeout: ${TEST_TIMEOUT}s"
echo ""

# ---- Mock Helix WebSocket Server ----
echo "[mock-server] Starting mock Helix WebSocket server..."

cat > /tmp/mock_helix_server.py << 'PYEOF'
#!/usr/bin/env python3
"""Mock Helix WebSocket server for E2E testing.

Implements the Helix side of the WebSocket sync protocol:
1. Accepts connections on /api/v1/external-agents/sync
2. Waits for agent_ready event from Zed
3. Sends a chat_message command
4. Records all received events and validates the protocol flow
"""
import asyncio
import json
import sys
from http import HTTPStatus

import websockets
from websockets.asyncio.server import serve

received_events = []
test_done = asyncio.Event()
chat_sent = False  # Only send chat_message once
TEST_MESSAGE = "What is 2 + 2? Reply with just the number."
REQUEST_ID = "e2e-test-req-001"

async def handle_connection(websocket):
    """Handle a single WebSocket connection from Zed."""
    global chat_sent
    print(f"[mock-server] Zed connected from {websocket.remote_address}", flush=True)

    async for message in websocket:
        try:
            data = json.loads(message)
            event_type = data.get("event_type", "unknown")
            print(f"[mock-server] <- Received: {event_type}", flush=True)
            received_events.append(data)

            if event_type == "agent_ready" and not chat_sent:
                chat_sent = True
                print("[mock-server] Agent ready! Sending chat_message...", flush=True)
                command = {
                    "type": "chat_message",
                    "data": {
                        "message": TEST_MESSAGE,
                        "request_id": REQUEST_ID,
                        "agent_name": "zed-agent"
                    }
                }
                await websocket.send(json.dumps(command))
                print("[mock-server] -> Sent: chat_message", flush=True)

            elif event_type == "agent_ready":
                print("[mock-server] Agent ready (ignoring, chat already sent)", flush=True)

            elif event_type == "thread_created":
                tid = data.get("data", {}).get("acp_thread_id", "?")
                print(f"[mock-server] Thread created: {tid}", flush=True)

            elif event_type == "message_added":
                role = data.get("data", {}).get("role", "?")
                content = data.get("data", {}).get("content", "")
                preview = (content[:80] + "...") if len(content) > 80 else content
                print(f"[mock-server] Message ({role}): {preview}", flush=True)

            elif event_type == "message_completed":
                print("[mock-server] Message completed!", flush=True)
                test_done.set()

        except json.JSONDecodeError:
            print(f"[mock-server] Invalid JSON: {message[:100]}", flush=True)


async def process_request(connection, request):
    """Validate the WebSocket request path and auth."""
    # request.path includes query string in websockets 15.x
    path = request.path.split("?")[0]
    if path != "/api/v1/external-agents/sync":
        return connection.respond(HTTPStatus.NOT_FOUND, f"Not found: {path}\n")
    return None


async def run_server(port):
    """Run the mock server and validate results."""
    async with serve(
        handle_connection,
        "127.0.0.1",
        port,
        process_request=process_request,
    ) as server:
        actual_port = server.sockets[0].getsockname()[1]
        print(f"[mock-server] Listening on ws://127.0.0.1:{actual_port}", flush=True)

        with open("/tmp/mock_helix_port", "w") as f:
            f.write(str(actual_port))

        try:
            await asyncio.wait_for(test_done.wait(), timeout=120)
        except asyncio.TimeoutError:
            event_types = [e.get("event_type") for e in received_events]
            print(f"[mock-server] TIMEOUT. Events received: {event_types}", flush=True)
            sys.exit(1)

    # ---- Validate protocol flow ----
    event_types = [e.get("event_type") for e in received_events]
    print(f"[mock-server] Event sequence: {event_types}", flush=True)

    errors = []
    for expected in ["agent_ready", "thread_created", "message_added", "message_completed"]:
        if expected not in event_types:
            errors.append(f"Missing expected event: {expected}")

    assistant_msgs = [
        e for e in received_events
        if e.get("event_type") == "message_added"
        and e.get("data", {}).get("role") == "assistant"
    ]
    if not assistant_msgs:
        errors.append("No assistant messages received")
    else:
        last_content = assistant_msgs[-1].get("data", {}).get("content", "")
        print(f"[mock-server] Last assistant content: {last_content[:200]}", flush=True)

    if errors:
        for err in errors:
            print(f"[mock-server] FAIL: {err}", flush=True)
        sys.exit(1)

    print("[mock-server] ALL PROTOCOL CHECKS PASSED", flush=True)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    asyncio.run(run_server(port))
PYEOF

python3 /tmp/mock_helix_server.py 0 &
MOCK_PID=$!
sleep 2

if [ ! -f "$MOCK_PORT_FILE" ]; then
    echo "[error] Mock server failed to start"
    exit 1
fi
MOCK_PORT=$(cat "$MOCK_PORT_FILE")
echo "[mock-server] Running on port $MOCK_PORT"
echo ""

# ---- Configure Zed via environment variables ----
# ExternalSyncSettings reads from env vars, not settings.json
export ZED_EXTERNAL_SYNC_ENABLED=true
export ZED_WEBSOCKET_SYNC_ENABLED=true
export ZED_HELIX_URL="127.0.0.1:${MOCK_PORT}"
export ZED_HELIX_TOKEN="test-token"
export ZED_HELIX_TLS=false
export ZED_HELIX_SKIP_TLS_VERIFY=false
export HELIX_SESSION_ID="e2e-test-session-001"

# ---- Write Zed settings.json for LLM provider ----
ZED_CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/zed"
mkdir -p "$ZED_CONFIG_DIR"
cat > "$ZED_CONFIG_DIR/settings.json" << JSONEOF
{
  "agent": {
    "default_model": {
      "provider": "anthropic",
      "model": "claude-sonnet-4-20250514"
    },
    "always_allow_tool_actions": true,
    "show_onboarding": false,
    "auto_open_panel": true
  }
}
JSONEOF
echo "[zed] Wrote settings to $ZED_CONFIG_DIR/settings.json"

echo "[zed] Starting Zed with WebSocket sync..."
echo "[zed]   ZED_HELIX_URL=$ZED_HELIX_URL"
echo "[zed]   ZED_EXTERNAL_SYNC_ENABLED=$ZED_EXTERNAL_SYNC_ENABLED"
echo "[zed]   ZED_STATELESS=${ZED_STATELESS:-not set}"
echo "[zed]   ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY:+set (${#ANTHROPIC_API_KEY} chars)}"
echo ""

# Start Zed
"$ZED_BINARY" \
    --allow-multiple-instances \
    "$PROJECT_DIR" \
    &
ZED_PID=$!

echo "[zed] Started (PID $ZED_PID)"
echo ""
echo "[test] Waiting for protocol flow to complete (timeout: ${TEST_TIMEOUT}s)..."

# ---- Wait for test to complete ----
ELAPSED=0
while [ "$ELAPSED" -lt "$TEST_TIMEOUT" ]; do
    # Check if Zed crashed
    if ! kill -0 "$ZED_PID" 2>/dev/null; then
        wait "$ZED_PID" || ZED_EXIT=$?
        echo "[error] Zed exited early with code ${ZED_EXIT:-0}"
        # Still check if mock server got enough events
        if ! kill -0 "$MOCK_PID" 2>/dev/null; then
            wait "$MOCK_PID"
            MOCK_EXIT=$?
            if [ "$MOCK_EXIT" -eq 0 ]; then
                echo ""
                echo "============================================"
                echo "  E2E TEST PASSED (Zed exited but protocol completed)"
                echo "============================================"
                exit 0
            fi
        fi
        echo ""
        echo "============================================"
        echo "  E2E TEST FAILED (Zed crashed)"
        echo "============================================"
        exit 1
    fi

    # Check if mock server completed successfully
    if ! kill -0 "$MOCK_PID" 2>/dev/null; then
        wait "$MOCK_PID"
        MOCK_EXIT=$?
        if [ "$MOCK_EXIT" -eq 0 ]; then
            echo ""
            echo "============================================"
            echo "  E2E TEST PASSED"
            echo "============================================"
            exit 0
        else
            echo ""
            echo "============================================"
            echo "  E2E TEST FAILED (mock server exit: $MOCK_EXIT)"
            echo "============================================"
            exit 1
        fi
    fi

    sleep 2
    ELAPSED=$((ELAPSED + 2))
done

echo ""
echo "============================================"
echo "  E2E TEST TIMED OUT after ${TEST_TIMEOUT}s"
echo "============================================"
exit 1
