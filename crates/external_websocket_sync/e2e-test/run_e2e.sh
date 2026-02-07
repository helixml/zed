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
TEST_TIMEOUT="${TEST_TIMEOUT:-240}"
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

Multi-phase test covering:
  Phase 1: Basic thread - send chat_message, get response
  Phase 2: Follow-up on same thread - send chat_message with acp_thread_id
  Phase 3: New thread via WebSocket - send chat_message without acp_thread_id
  Phase 4: Verify all threads tracked correctly

This simulates what Helix spectasks need: a single session spanning
multiple Zed threads (e.g., when context fills up and a new thread starts).
"""
import asyncio
import json
import sys
from http import HTTPStatus

import websockets
from websockets.asyncio.server import serve

received_events = []
all_tests_done = asyncio.Event()
websocket_ref = None  # Store websocket for sending commands from phases

# Track state across phases
phase = 0
thread_ids = []  # Collect all thread IDs we see
completed_threads = {}  # thread_id -> list of completed request_ids


async def handle_connection(websocket):
    """Handle a single WebSocket connection from Zed."""
    global phase, websocket_ref
    websocket_ref = websocket
    print(f"[mock-server] Zed connected from {websocket.remote_address}", flush=True)

    async for message in websocket:
        try:
            data = json.loads(message)
            event_type = data.get("event_type", "unknown")
            event_data = data.get("data", {})
            print(f"[mock-server] <- {event_type}: {json.dumps(event_data)[:120]}", flush=True)
            received_events.append(data)

            if event_type == "agent_ready" and phase == 0:
                phase = 1
                await run_phase_1(websocket)

            elif event_type == "thread_created":
                tid = event_data.get("acp_thread_id", "?")
                thread_ids.append(tid)
                print(f"[mock-server] 📋 Thread #{len(thread_ids)}: {tid}", flush=True)

            elif event_type == "message_completed":
                tid = event_data.get("acp_thread_id", "?")
                rid = event_data.get("request_id", "?")
                completed_threads.setdefault(tid, []).append(rid)
                print(f"[mock-server] ✅ Completed: thread={tid[:12]}... req={rid}", flush=True)

                # Advance to next phase after completion
                if phase == 1:
                    phase = 2
                    await asyncio.sleep(1)
                    await run_phase_2(websocket)
                elif phase == 2:
                    phase = 3
                    await asyncio.sleep(1)
                    await run_phase_3(websocket)
                elif phase == 3:
                    phase = 4
                    await asyncio.sleep(0.5)
                    all_tests_done.set()

        except json.JSONDecodeError:
            print(f"[mock-server] Invalid JSON: {message[:100]}", flush=True)


async def run_phase_1(ws):
    """Phase 1: Basic thread creation - send chat_message, no thread ID."""
    print("\n" + "="*50, flush=True)
    print("  PHASE 1: Basic thread creation", flush=True)
    print("="*50, flush=True)
    command = {
        "type": "chat_message",
        "data": {
            "message": "What is 2 + 2? Reply with just the number.",
            "request_id": "req-phase1",
            "agent_name": "zed-agent"
        }
    }
    await ws.send(json.dumps(command))
    print("[mock-server] -> Sent: chat_message (phase 1, new thread)", flush=True)


async def run_phase_2(ws):
    """Phase 2: Follow-up on existing thread - send to same thread ID."""
    print("\n" + "="*50, flush=True)
    print("  PHASE 2: Follow-up on existing thread", flush=True)
    print("="*50, flush=True)
    if not thread_ids:
        print("[mock-server] ERROR: No thread IDs captured from phase 1!", flush=True)
        sys.exit(1)

    first_thread_id = thread_ids[0]
    print(f"[mock-server] Using thread from phase 1: {first_thread_id}", flush=True)
    command = {
        "type": "chat_message",
        "data": {
            "message": "What is 3 + 3? Reply with just the number.",
            "request_id": "req-phase2",
            "acp_thread_id": first_thread_id,
            "agent_name": "zed-agent"
        }
    }
    await ws.send(json.dumps(command))
    print("[mock-server] -> Sent: chat_message (phase 2, follow-up)", flush=True)


async def run_phase_3(ws):
    """Phase 3: New thread via WebSocket - simulates thread transition
    (e.g., context ran out, Helix starts fresh thread for same task)."""
    print("\n" + "="*50, flush=True)
    print("  PHASE 3: New thread (simulating thread transition)", flush=True)
    print("="*50, flush=True)
    command = {
        "type": "chat_message",
        "data": {
            "message": "What is 10 + 10? Reply with just the number.",
            "request_id": "req-phase3",
            "agent_name": "zed-agent"
        }
    }
    await ws.send(json.dumps(command))
    print("[mock-server] -> Sent: chat_message (phase 3, new thread)", flush=True)


async def process_request(connection, request):
    """Validate the WebSocket request path."""
    path = request.path.split("?")[0]
    if path != "/api/v1/external-agents/sync":
        return connection.respond(HTTPStatus.NOT_FOUND, f"Not found: {path}\n")
    return None


def validate_results():
    """Validate all test phases passed."""
    print("\n" + "="*50, flush=True)
    print("  VALIDATION", flush=True)
    print("="*50, flush=True)

    event_types = [e.get("event_type") for e in received_events]
    print(f"[mock-server] Total events: {len(received_events)}", flush=True)
    print(f"[mock-server] Thread IDs seen: {thread_ids}", flush=True)
    print(f"[mock-server] Completed threads: {completed_threads}", flush=True)

    errors = []

    # --- Phase 1: Basic thread creation ---
    thread_created_events = [e for e in received_events if e.get("event_type") == "thread_created"]
    if len(thread_created_events) < 1:
        errors.append("Phase 1: No thread_created event")

    phase1_completions = [e for e in received_events
        if e.get("event_type") == "message_completed"
        and e.get("data", {}).get("request_id") == "req-phase1"]
    if not phase1_completions:
        errors.append("Phase 1: No message_completed for req-phase1")

    # --- Phase 2: Follow-up on existing thread ---
    # Should NOT create a new thread (follow-up to existing)
    phase2_completions = [e for e in received_events
        if e.get("event_type") == "message_completed"
        and e.get("data", {}).get("request_id") == "req-phase2"]
    if not phase2_completions:
        errors.append("Phase 2: No message_completed for req-phase2")
    else:
        # Follow-up should use the SAME thread_id as phase 1
        p2_thread = phase2_completions[0].get("data", {}).get("acp_thread_id", "")
        if thread_ids and p2_thread != thread_ids[0]:
            errors.append(f"Phase 2: Follow-up used different thread! Expected {thread_ids[0][:12]}..., got {p2_thread[:12]}...")
        else:
            print(f"[mock-server] ✅ Phase 2: Follow-up used same thread: {p2_thread[:12]}...", flush=True)

    # --- Phase 3: New thread creation ---
    if len(thread_ids) < 2:
        errors.append(f"Phase 3: Expected at least 2 thread_created events, got {len(thread_ids)}")
    else:
        if thread_ids[0] == thread_ids[1]:
            errors.append("Phase 3: New thread has same ID as first thread!")
        else:
            print(f"[mock-server] ✅ Phase 3: New thread created: {thread_ids[1][:12]}...", flush=True)

    phase3_completions = [e for e in received_events
        if e.get("event_type") == "message_completed"
        and e.get("data", {}).get("request_id") == "req-phase3"]
    if not phase3_completions:
        errors.append("Phase 3: No message_completed for req-phase3")

    # --- Overall: assistant responses ---
    assistant_msgs = [e for e in received_events
        if e.get("event_type") == "message_added"
        and e.get("data", {}).get("role") == "assistant"]
    if len(assistant_msgs) < 3:
        errors.append(f"Expected at least 3 assistant messages, got {len(assistant_msgs)}")

    for i, msg in enumerate(assistant_msgs):
        content = msg.get("data", {}).get("content", "")[:80]
        print(f"[mock-server] Assistant msg {i+1}: {content}", flush=True)

    # --- Phase 2 should NOT have created a thread_created event ---
    # Count thread_created events: phase 1 creates one, phase 3 creates one = 2
    # Phase 2 (follow-up) should NOT create a thread
    if len(thread_created_events) > 2:
        # More than 2 thread_created = phase 2 incorrectly created a thread
        errors.append(f"Too many thread_created events ({len(thread_created_events)}). Phase 2 follow-up should not create new thread.")

    # --- Summary ---
    if errors:
        print("", flush=True)
        for err in errors:
            print(f"[mock-server] ❌ FAIL: {err}", flush=True)
        return False

    print("", flush=True)
    print("[mock-server] ✅ Phase 1: Basic thread creation - PASSED", flush=True)
    print("[mock-server] ✅ Phase 2: Follow-up on existing thread - PASSED", flush=True)
    print("[mock-server] ✅ Phase 3: New thread via WebSocket - PASSED", flush=True)
    print(f"[mock-server] ✅ Total threads: {len(thread_ids)}, Total completions: {sum(len(v) for v in completed_threads.values())}", flush=True)
    return True


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
            await asyncio.wait_for(all_tests_done.wait(), timeout=180)
        except asyncio.TimeoutError:
            event_types = [e.get("event_type") for e in received_events]
            print(f"[mock-server] TIMEOUT at phase {phase}. Events: {event_types}", flush=True)
            sys.exit(1)

    if validate_results():
        print("\n[mock-server] ALL MULTI-THREAD PROTOCOL CHECKS PASSED", flush=True)
    else:
        sys.exit(1)


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
  "language_models": {
    "anthropic": {
      "api_url": "https://api.anthropic.com"
    }
  },
  "agent": {
    "default_model": {
      "provider": "anthropic",
      "model": "claude-sonnet-4-5-latest"
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
