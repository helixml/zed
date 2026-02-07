#!/usr/bin/env bash
set -euo pipefail

# E2E test for Zed WebSocket sync
# Tests the full flow: Zed → ACP agent → WebSocket → Mock Helix Server
#
# Prerequisites:
#   - Zed binary built with --features external_websocket_sync
#   - ACP agent (claude or qwen) available in PATH
#   - xvfb for headless display (or a real display)
#
# Environment variables:
#   ANTHROPIC_API_KEY - Required for Claude Code agent
#   ZED_BINARY        - Path to Zed binary (default: zed in PATH)
#   TEST_AGENT        - Which agent to test: "claude", "qwen", or "both" (default: both)
#   TEST_TIMEOUT      - Timeout in seconds (default: 120)

echo "============================================"
echo "  Zed WebSocket Sync E2E Test"
echo "============================================"
echo ""

ZED_BINARY="${ZED_BINARY:-$(which zed 2>/dev/null || echo /usr/local/bin/zed)}"
TEST_AGENT="${TEST_AGENT:-both}"
TEST_TIMEOUT="${TEST_TIMEOUT:-120}"
MOCK_SERVER_PORT="${MOCK_SERVER_PORT:-0}"  # 0 = random
PROJECT_DIR="${PROJECT_DIR:-/tmp/zed-e2e-project}"

# Create test project
mkdir -p "$PROJECT_DIR"
echo "# E2E Test Project" > "$PROJECT_DIR/README.md"
echo 'fn main() { println!("hello"); }' > "$PROJECT_DIR/main.rs"

# Start virtual framebuffer if no display
if [ -z "${DISPLAY:-}" ]; then
    echo "[setup] Starting Xvfb on :99..."
    Xvfb :99 -screen 0 1280x720x24 &
    XVFB_PID=$!
    export DISPLAY=:99
    sleep 1
    echo "[setup] Xvfb started (PID $XVFB_PID)"
fi

# Verify Zed binary exists
if [ ! -f "$ZED_BINARY" ]; then
    echo "[error] Zed binary not found at $ZED_BINARY"
    echo "[error] Build with: cargo build --release -p zed --features external_websocket_sync"
    exit 1
fi

echo "[setup] Zed binary: $ZED_BINARY"
echo "[setup] Test agent: $TEST_AGENT"
echo "[setup] Timeout: ${TEST_TIMEOUT}s"
echo ""

# Start the mock Helix WebSocket server
# This is a standalone Rust binary that listens for WebSocket connections
# and validates the protocol flow
echo "[mock-server] Starting mock Helix WebSocket server..."

# The mock server is built as part of the test infrastructure
# For now, use a simple Python WebSocket server as a placeholder
# TODO: Build a standalone mock server binary from mock_helix_server.rs

cat > /tmp/mock_helix_server.py << 'PYEOF'
#!/usr/bin/env python3
"""Minimal mock Helix WebSocket server for E2E testing.

Implements the Helix side of the WebSocket sync protocol:
1. Accepts connections on /api/v1/external-agents/sync
2. Waits for agent_ready event
3. Sends a chat_message command
4. Records all received events
5. Validates the protocol flow
"""
import asyncio
import json
import sys
import signal
from datetime import datetime

try:
    import websockets
    from websockets.server import serve
except ImportError:
    print("[mock-server] Installing websockets...")
    import subprocess
    subprocess.check_call([sys.executable, "-m", "pip", "install", "websockets", "-q"])
    import websockets
    from websockets.server import serve

# State
received_events = []
agent_ready = asyncio.Event()
message_completed = asyncio.Event()
test_passed = asyncio.Event()

TEST_MESSAGE = "What is 2 + 2? Reply with just the number."
REQUEST_ID = "e2e-test-req-001"

async def handle_connection(websocket):
    """Handle a single WebSocket connection from Zed."""
    print(f"[mock-server] Zed connected from {websocket.remote_address}")

    async def reader():
        """Read events from Zed."""
        async for message in websocket:
            try:
                data = json.loads(message)
                event_type = data.get("event_type", "unknown")
                print(f"[mock-server] ← Received: {event_type}")
                received_events.append(data)

                if event_type == "agent_ready":
                    print(f"[mock-server] Agent ready! Sending chat message...")
                    agent_ready.set()

                    # Send chat_message command
                    command = {
                        "type": "chat_message",
                        "data": {
                            "message": TEST_MESSAGE,
                            "request_id": REQUEST_ID,
                            "agent_name": "zed-agent"
                        }
                    }
                    await websocket.send(json.dumps(command))
                    print(f"[mock-server] → Sent: chat_message")

                elif event_type == "thread_created":
                    thread_id = data.get("data", {}).get("acp_thread_id", "?")
                    print(f"[mock-server] Thread created: {thread_id}")

                elif event_type == "message_added":
                    role = data.get("data", {}).get("role", "?")
                    content = data.get("data", {}).get("content", "")
                    preview = content[:80] + "..." if len(content) > 80 else content
                    print(f"[mock-server] Message ({role}): {preview}")

                elif event_type == "message_completed":
                    print(f"[mock-server] Message completed!")
                    message_completed.set()

            except json.JSONDecodeError:
                print(f"[mock-server] Invalid JSON: {message[:100]}")

    await reader()

async def run_server(port):
    """Run the mock server."""
    async with serve(handle_connection, "127.0.0.1", port) as server:
        actual_port = server.sockets[0].getsockname()[1]
        print(f"[mock-server] Listening on ws://127.0.0.1:{actual_port}/api/v1/external-agents/sync")

        # Write port to file so the test script can find it
        with open("/tmp/mock_helix_port", "w") as f:
            f.write(str(actual_port))

        # Wait for test completion or timeout
        try:
            await asyncio.wait_for(message_completed.wait(), timeout=120)
            print("[mock-server] ✅ Test flow completed successfully!")

            # Validate protocol flow
            event_types = [e.get("event_type") for e in received_events]
            print(f"[mock-server] Event sequence: {event_types}")

            expected_flow = ["agent_ready", "thread_created"]
            for expected in expected_flow:
                if expected not in event_types:
                    print(f"[mock-server] ❌ Missing expected event: {expected}")
                    sys.exit(1)

            if "message_added" not in event_types:
                print("[mock-server] ❌ No message_added events received")
                sys.exit(1)

            if "message_completed" not in event_types:
                print("[mock-server] ❌ No message_completed event received")
                sys.exit(1)

            # Check that we got assistant content
            assistant_messages = [
                e for e in received_events
                if e.get("event_type") == "message_added"
                and e.get("data", {}).get("role") == "assistant"
            ]
            if not assistant_messages:
                print("[mock-server] ❌ No assistant messages received")
                sys.exit(1)

            last_content = assistant_messages[-1].get("data", {}).get("content", "")
            print(f"[mock-server] Last assistant content: {last_content[:200]}")

            if "4" in last_content:
                print("[mock-server] ✅ Assistant correctly answered '4'")
            else:
                print(f"[mock-server] ⚠ Assistant response doesn't contain '4': {last_content[:100]}")

            print("[mock-server] ✅ ALL PROTOCOL CHECKS PASSED")
            test_passed.set()

        except asyncio.TimeoutError:
            print("[mock-server] ❌ Timeout waiting for message_completed")
            event_types = [e.get("event_type") for e in received_events]
            print(f"[mock-server] Events received so far: {event_types}")
            sys.exit(1)

if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    asyncio.run(run_server(port))
PYEOF

python3 /tmp/mock_helix_server.py 0 &
MOCK_PID=$!
sleep 2

# Read the actual port
if [ ! -f /tmp/mock_helix_port ]; then
    echo "[error] Mock server failed to start"
    kill $MOCK_PID 2>/dev/null || true
    exit 1
fi
MOCK_PORT=$(cat /tmp/mock_helix_port)
echo "[mock-server] Running on port $MOCK_PORT"

# Configure Zed settings for WebSocket sync
ZED_CONFIG_DIR="/tmp/zed-e2e-config"
mkdir -p "$ZED_CONFIG_DIR"
cat > "$ZED_CONFIG_DIR/settings.json" << JSONEOF
{
  "agent": {
    "auto_open_panel": true,
    "show_onboarding": false
  },
  "external_sync": {
    "enabled": true,
    "websocket_sync": {
      "enabled": true,
      "external_url": "ws://127.0.0.1:${MOCK_PORT}/api/v1/external-agents/sync",
      "auth_token": "test-token",
      "use_tls": false,
      "skip_tls_verify": true
    }
  }
}
JSONEOF

echo "[zed] Starting Zed with WebSocket sync..."
echo "[zed] Config: $ZED_CONFIG_DIR/settings.json"

# Start Zed
"$ZED_BINARY" \
    --allow-multiple-instances \
    "$PROJECT_DIR" \
    &
ZED_PID=$!

echo "[zed] Started (PID $ZED_PID)"

# Wait for test to complete
echo ""
echo "[test] Waiting for protocol flow to complete (timeout: ${TEST_TIMEOUT}s)..."

ELAPSED=0
while [ $ELAPSED -lt "$TEST_TIMEOUT" ]; do
    if ! kill -0 $MOCK_PID 2>/dev/null; then
        # Mock server exited
        wait $MOCK_PID
        MOCK_EXIT=$?
        if [ $MOCK_EXIT -eq 0 ]; then
            echo ""
            echo "============================================"
            echo "  ✅ E2E TEST PASSED"
            echo "============================================"
            kill $ZED_PID 2>/dev/null || true
            exit 0
        else
            echo ""
            echo "============================================"
            echo "  ❌ E2E TEST FAILED (mock server exit: $MOCK_EXIT)"
            echo "============================================"
            kill $ZED_PID 2>/dev/null || true
            exit 1
        fi
    fi

    sleep 2
    ELAPSED=$((ELAPSED + 2))
done

echo ""
echo "============================================"
echo "  ❌ E2E TEST TIMED OUT after ${TEST_TIMEOUT}s"
echo "============================================"
kill $ZED_PID 2>/dev/null || true
kill $MOCK_PID 2>/dev/null || true
exit 1
