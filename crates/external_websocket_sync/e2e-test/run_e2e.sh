#!/usr/bin/env bash
set -euo pipefail

# E2E test for Zed WebSocket sync
# Tests the full flow: Zed connects → sends agent_ready → mock server sends
# chat_message → Zed creates thread → streams response → sends message_completed
#
# The mock server is a Go binary (helix-ws-test-server) that imports the same
# wsprotocol package used by the production Helix API server — same message
# parsing, routing, and accumulation code runs in both tests and production.
#
# Environment variables:
#   ZED_BINARY              - Path to Zed binary (default: /usr/local/bin/zed)
#   TEST_TIMEOUT            - Timeout in seconds (default: 300)
#   HELIX_WS_TEST_SERVER    - Path to Go test server binary (default: /usr/local/bin/helix-ws-test-server)

echo "============================================"
echo "  Zed WebSocket Sync E2E Test"
echo "============================================"
echo ""

ZED_BINARY="${ZED_BINARY:-/usr/local/bin/zed}"
# Default timeout scales with number of agent rounds (each round takes ~150s
# including Phase 15's long-form prose streaming).
AGENT_COUNT=$(echo "${E2E_AGENTS:-zed-agent}" | tr ',' '\n' | wc -l)
DEFAULT_TIMEOUT=$((300 * AGENT_COUNT))
TEST_TIMEOUT="${TEST_TIMEOUT:-$DEFAULT_TIMEOUT}"
MOCK_SERVER="${HELIX_WS_TEST_SERVER:-/usr/local/bin/helix-ws-test-server}"
PROJECT_DIR="/test/project"
MOCK_PORT_FILE="/tmp/mock_helix_port"

# Screenshot capture settings
SCREENSHOT_DIR="${SCREENSHOT_DIR:-/test/screenshots}"
SCREENSHOT_INTERVAL="${SCREENSHOT_INTERVAL:-3}"

# Cleanup function
cleanup() {
    echo "[cleanup] Shutting down..."
    [ -n "${SCREENSHOT_PID:-}" ] && kill "$SCREENSHOT_PID" 2>/dev/null || true
    [ -n "${ZED_PID:-}" ] && kill "$ZED_PID" 2>/dev/null || true
    [ -n "${MOCK_PID:-}" ] && kill "$MOCK_PID" 2>/dev/null || true
    [ -n "${XVFB_PID:-}" ] && kill "$XVFB_PID" 2>/dev/null || true
    rm -f "$MOCK_PORT_FILE"

    # Dump Zed errors/panics (full log available at ZED_LOG_FILE)
    if [ -f "${ZED_LOG_FILE:-}" ]; then
        # NB: `grep -c` PRINTS the count and EXITS NON-ZERO when the count is 0, so
        # `$(grep -c ... || echo 0)` yields the two-line string "0\n0" and every
        # subsequent `[ "$X" -gt 0 ]` dies with "integer expression expected".
        # Put the fallback on the assignment, not inside the substitution.
        ZED_ERRORS=$(grep -ciE "panic|error|fatal" "$ZED_LOG_FILE" 2>/dev/null) || ZED_ERRORS=0
        if [ "$ZED_ERRORS" -gt 0 ]; then
            echo ""
            echo "=================================================="
            echo "  ZED PROCESS ERRORS ($ZED_ERRORS lines)"
            echo "=================================================="
            grep -iE "panic|error|fatal" "$ZED_LOG_FILE" | tail -50 || true
            echo "  (full log: $ZED_LOG_FILE)"
        fi
        # ACP_SPAWN/ACP_DEDUP are at log::info level — surface them explicitly
        ACP_LINES=$(grep -cE "ACP_SPAWN|ACP_DEDUP" "$ZED_LOG_FILE" 2>/dev/null) || ACP_LINES=0
        if [ "$ACP_LINES" -gt 0 ]; then
            echo ""
            echo "=================================================="
            echo "  ACP_SPAWN / ACP_DEDUP ($ACP_LINES lines)"
            echo "=================================================="
            grep -E "ACP_SPAWN|ACP_DEDUP" "$ZED_LOG_FILE" || true
        fi

        # Turn lifecycle: cancel / interrupt / silence-watchdog decisions.
        #
        # These are the events needed to tell the three failure shapes apart when
        # a round fails, and reading them from the Helix side alone is impossible
        # (Helix sees completions, not the ordering that produced them):
        #   - cancel landing on the wrong turn  -> CANCEL_TASK vs THREAD_SERVICE order
        #   - a stale cancel correctly dropped  -> "Stale cancel ignored"
        #   - agent accepted a prompt then died -> helix_silent_prompt_wedge
        # Without this block the harness reported only "phase N timed out", which
        # is not enough to attribute a failure.
        LIFECYCLE_RE="CANCEL_TASK|Interrupt flag set|Stale cancel ignored|helix_silent_prompt_wedge|THREAD_SERVICE\] (Sending follow-up|Updated request_id|Sending to existing)"
        LIFECYCLE_LINES=$(grep -cE "$LIFECYCLE_RE" "$ZED_LOG_FILE" 2>/dev/null) || LIFECYCLE_LINES=0
        if [ "$LIFECYCLE_LINES" -gt 0 ]; then
            echo ""
            echo "=================================================="
            echo "  TURN LIFECYCLE / CANCEL ORDERING ($LIFECYCLE_LINES lines)"
            echo "=================================================="
            grep -E "$LIFECYCLE_RE" "$ZED_LOG_FILE" | tail -60 || true
        fi
        # Persist the full zed log into the mounted screenshots dir for offline inspection
        if [ -d "$SCREENSHOT_DIR" ]; then
            cp "$ZED_LOG_FILE" "$SCREENSHOT_DIR/zed-e2e.log" 2>/dev/null || true
        fi
    fi

    # Report screenshots
    if [ -d "$SCREENSHOT_DIR" ]; then
        # `ls` exits non-zero when the glob matches nothing, and `set -o pipefail`
        # propagates that through `| wc -l` to the ASSIGNMENT — which `set -e`
        # then treats as fatal, aborting this trap mid-way. Put the fallback on
        # the assignment (same shape as the grep -c cases above).
        SHOT_COUNT=$(ls -1 "$SCREENSHOT_DIR"/*.png 2>/dev/null | wc -l) || SHOT_COUNT=0
        echo "[screenshots] Captured $SHOT_COUNT screenshots in $SCREENSHOT_DIR"
    fi

    # The script runs under `set -e`, and a non-zero status from the LAST command
    # in an EXIT trap replaces the script's own exit status. Every command above
    # is diagnostics — a failing grep/cp/test must never turn a PASSING run into
    # a failure (headless runs captured no screenshots and exited 2 this way).
    # `return 0` here leaves a real `exit 1` from the test intact; verified.
    return 0
}
trap cleanup EXIT

# Start D-Bus session (required by Zed for GPU init / portal notifications)
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    export DBUS_SESSION_BUS_ADDRESS=$(dbus-daemon --session --fork --print-address 2>/dev/null || true)
    echo "[setup] D-Bus session: ${DBUS_SESSION_BUS_ADDRESS:-none}"
fi

# In E2E_HEADLESS=1 mode we skip Xvfb entirely and run Zed with --headless.
# This validates that the WebSocket sync + agent backend works with no display server.
if [ "${E2E_HEADLESS:-0}" = "1" ]; then
    echo "[setup] E2E_HEADLESS=1: skipping Xvfb; Zed will be launched with --headless"
    unset DISPLAY
    SCREENSHOT_PID=""
else
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

    # Start background screenshot capture
    mkdir -p "$SCREENSHOT_DIR"
    (
        SHOT_NUM=0
        while true; do
            sleep "$SCREENSHOT_INTERVAL"
            SHOT_NUM=$((SHOT_NUM + 1))
            FILENAME=$(printf "%s/screenshot-%04d.png" "$SCREENSHOT_DIR" "$SHOT_NUM")
            import -window root "$FILENAME" 2>/dev/null || true
        done
    ) &
    SCREENSHOT_PID=$!
    echo "[screenshots] Background capture started (every ${SCREENSHOT_INTERVAL}s → $SCREENSHOT_DIR)"
fi

# Verify binaries exist
if [ ! -f "$ZED_BINARY" ]; then
    echo "[error] Zed binary not found at $ZED_BINARY"
    exit 1
fi

if [ ! -f "$MOCK_SERVER" ]; then
    echo "[error] Go test server binary not found at $MOCK_SERVER"
    echo "[error] Build it with: cd helix-ws-test-server && CGO_ENABLED=0 go build -o $MOCK_SERVER ."
    exit 1
fi

echo ""
echo "============================================"
echo "  BINARY VERSIONS"
echo "============================================"
echo "  Zed --system-specs:"
"$ZED_BINARY" --system-specs 2>/dev/null | sed 's/^/    /' || echo "    (failed to run --system-specs)"
echo "  Go test server md5: $(md5sum "$MOCK_SERVER" 2>/dev/null | cut -c1-32)"
echo "============================================"
echo ""

echo "[setup] Zed binary: $ZED_BINARY"
echo "[setup] Mock server: $MOCK_SERVER"
echo "[setup] Timeout: ${TEST_TIMEOUT}s"
echo ""

# ---- Smoke mode ----
# E2E_SMOKE=1 runs a single tool-call phase instead of the full suite: the agent
# reads magic-number.txt and echoes the value. A deterministic local model drives
# the native agent, so this can gate every CI build without external credentials.
export E2E_SMOKE="${E2E_SMOKE:-0}"
export E2E_PLAN="${E2E_PLAN:-0}"
if [ "$E2E_SMOKE" = "1" ]; then
    if [ -z "${E2E_SMOKE_FILE:-}" ]; then
        export E2E_SMOKE_FILE="$PROJECT_DIR/magic-number.txt"
        echo "4242" > "$E2E_SMOKE_FILE"
        echo "[setup] E2E_SMOKE=1: wrote $E2E_SMOKE_FILE (single tool-call phase)"
    else
        # Caller pointed the smoke test at their own file — don't create it.
        # Pointing it at a path that does not exist is how you verify the gate
        # can actually fail.
        echo "[setup] E2E_SMOKE=1: using caller-provided E2E_SMOKE_FILE=$E2E_SMOKE_FILE"
    fi
fi

# ---- Start Go WebSocket Test Server ----
echo "[mock-server] Starting Go test server (shares wsprotocol with production Helix)..."

"$MOCK_SERVER" &
MOCK_PID=$!
sleep 2

if [ ! -f "$MOCK_PORT_FILE" ]; then
    echo "[error] Mock server failed to start"
    exit 1
fi
MOCK_PORT=$(cat "$MOCK_PORT_FILE")
echo "[mock-server] Running on port $MOCK_PORT"
echo ""

if [ "$E2E_SMOKE" = "1" ]; then
    export E2E_MODEL_PROVIDER=openai
    export E2E_MODEL=e2e-smoke-model
    export OPENAI_API_KEY=e2e-smoke-key
    export OPENAI_BASE_URL="http://127.0.0.1:${MOCK_PORT}/v1"
    echo "[setup] Smoke mode: using deterministic local model at $OPENAI_BASE_URL"
fi

# ---- Configure Zed via environment variables ----
# ExternalSyncSettings reads from env vars, not settings.json
export ZED_EXTERNAL_SYNC_ENABLED=true
export ZED_WEBSOCKET_SYNC_ENABLED=true
export ZED_HELIX_URL="127.0.0.1:${MOCK_PORT}"
export ZED_HELIX_TOKEN="test-token"
export ZED_HELIX_TLS=false
export ZED_HELIX_SKIP_TLS_VERIFY=false
export HELIX_SESSION_ID="ses_e2e-test-session-001"

# ---- Determine which agents to test ----
# E2E_AGENTS controls which agent rounds to run. Default: zed-agent only (fastest).
# Add `claude` or `codex` for live ACP-backed rounds. Recommended CI matrix:
# zed-agent in headful mode and each external agent in E2E_HEADLESS=1 mode.
export E2E_AGENTS="${E2E_AGENTS:-zed-agent}"
echo "[setup] E2E_AGENTS=$E2E_AGENTS"

# ---- Write Zed settings.json for LLM provider ----
ZED_CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/zed"
mkdir -p "$ZED_CONFIG_DIR"

# Build agent-server settings for the selected external agents.
AGENT_SERVERS_JSON=""
AGENT_SERVER_ENTRIES=""
if echo "$E2E_AGENTS" | grep -q "claude"; then
    # Claude Code needs ANTHROPIC_API_KEY passed through settings (Zed clears env vars)
    CLAUDE_KEY="${ANTHROPIC_API_KEY:-}"
    if [ -z "$CLAUDE_KEY" ]; then
        echo "[error] ANTHROPIC_API_KEY is required when testing claude agent"
        exit 1
    fi
    # Use local claude-agent-acp build if mounted, otherwise let Zed auto-install from npm
    CLAUDE_PATH_JSON=""
    if [ -f "/opt/claude-agent-acp/dist/index.js" ]; then
        CLAUDE_PATH_JSON="\"path\": \"node\", \"args\": [\"/opt/claude-agent-acp/dist/index.js\"],"
        LOCAL_VERSION=$(node -e "console.log(require('/opt/claude-agent-acp/package.json').version)" 2>/dev/null || echo "unknown")
        echo "[setup] Using LOCAL claude-agent-acp v$LOCAL_VERSION from /opt/claude-agent-acp"
    else
        # Resolve latest once so the logged version is the version this run executes.
        #
        # The scope matters and has been wrong before: Zed installs
        # @agentclientprotocol/claude-agent-acp (see crates/agent_servers/), NOT
        # @anthropic-ai/... . Querying the wrong scope silently yields "unknown",
        # which quietly disables the one signal that distinguishes "we regressed"
        # from "the agent package changed under us" when the claude round fails.
        CLAUDE_ACP_PKG="@agentclientprotocol/claude-agent-acp"
        CLAUDE_ACP_VERSION=$(npm view "$CLAUDE_ACP_PKG" version 2>/dev/null || echo "")
        if [ -z "$CLAUDE_ACP_VERSION" ]; then
            echo "[error] Could not resolve $CLAUDE_ACP_PKG@latest"
            exit 1
        fi
        CLAUDE_PATH_JSON="\"path\": \"npx\", \"args\": [\"-y\", \"$CLAUDE_ACP_PKG@$CLAUDE_ACP_VERSION\"],"
        echo "[setup] Latest-provider lane: $CLAUDE_ACP_PKG@$CLAUDE_ACP_VERSION (latest resolved at run start)"
    fi
    AGENT_SERVER_ENTRIES=$(cat << AGENTEOF
    "claude": {
      ${CLAUDE_PATH_JSON}
      "env": {
        "ANTHROPIC_API_KEY": "${CLAUDE_KEY}"
      }
    }
AGENTEOF
)
    echo "[setup] Claude Code agent configured with API key"
fi

if echo "$E2E_AGENTS" | grep -q "codex"; then
    CODEX_KEY="${OPENAI_API_KEY:-}"
    if [ -z "$CODEX_KEY" ]; then
        echo "[error] OPENAI_API_KEY is required when testing codex agent"
        exit 1
    fi
    if [ -n "$AGENT_SERVER_ENTRIES" ]; then
        AGENT_SERVER_ENTRIES="${AGENT_SERVER_ENTRIES},"
    fi
    CODEX_ACP_PKG="@agentclientprotocol/codex-acp"
    CODEX_ACP_VERSION=$(npm view "$CODEX_ACP_PKG" version 2>/dev/null || echo "unknown")
    echo "[setup] Using npm-installed codex-acp $CODEX_ACP_PKG (auto-install, latest=$CODEX_ACP_VERSION)"
    # codex-acp model ids are "model[effort]" (ModelId.fromString in the package).
    # E2E_CODEX_MODEL lets CI pick a cheaper model/effort than the local default.
    CODEX_MODEL="${E2E_CODEX_MODEL:-gpt-5.6-terra}"
    echo "[setup] Codex model: $CODEX_MODEL"
    AGENT_SERVER_ENTRIES="${AGENT_SERVER_ENTRIES}
    \"codex-acp\": {
      \"type\": \"registry\",
      \"default_mode\": \"agent-full-access\",
      \"default_model\": \"${CODEX_MODEL}\",
      \"env\": {
        \"OPENAI_API_KEY\": \"${CODEX_KEY}\"
      }
    }"
    echo "[setup] Codex agent configured with API key"
fi

if echo "$E2E_AGENTS" | grep -q "plan-test-agent"; then
    if [ -n "$AGENT_SERVER_ENTRIES" ]; then
        AGENT_SERVER_ENTRIES="${AGENT_SERVER_ENTRIES},"
    fi
    AGENT_SERVER_ENTRIES="${AGENT_SERVER_ENTRIES}
    \"plan-test-agent\": {
      \"type\": \"custom\",
      \"command\": \"/usr/local/bin/plan-test-agent\"
    }"
    echo "[setup] Deterministic plan test agent configured"
fi

if echo "$E2E_AGENTS" | grep -q "lifecycle-test-agent"; then
    if [ -n "$AGENT_SERVER_ENTRIES" ]; then
        AGENT_SERVER_ENTRIES="${AGENT_SERVER_ENTRIES},"
    fi
    AGENT_SERVER_ENTRIES="${AGENT_SERVER_ENTRIES}
    \"lifecycle-test-agent\": {
      \"type\": \"custom\",
      \"command\": \"/usr/local/bin/plan-test-agent\",
      \"env\": { \"E2E_SCRIPTED_LIFECYCLE\": \"1\" }
    }"
    echo "[setup] Deterministic lifecycle test agent configured"
fi

if [ -n "$AGENT_SERVER_ENTRIES" ]; then
    AGENT_SERVERS_JSON=$(cat << AGENTEOF
  "agent_servers": {
${AGENT_SERVER_ENTRIES}
  },
AGENTEOF
)
fi

# ---- Native (zed-agent) model selection ----
# Defaults keep the historical Anthropic config. E2E_MODEL_PROVIDER=openai lets
# CI drive the native agent from an OpenAI model instead — the OpenAI provider
# accepts an explicit available_models entry, so new model ids work without a
# Zed release.
#
# reasoning_effort defaults to "none" because Zed's OpenAI provider talks to
# /v1/chat/completions, and that endpoint rejects any other effort when the
# request carries function tools ("use /v1/responses or set reasoning_effort to
# 'none'"). The smoke test needs tools, so "none" is the only working value —
# and the cheapest.
E2E_MODEL_PROVIDER="${E2E_MODEL_PROVIDER:-anthropic}"
if [ "$E2E_MODEL_PROVIDER" = "openai" ]; then
    E2E_MODEL="${E2E_MODEL:-gpt-5.6-luna}"
    LANGUAGE_MODELS_JSON=$(cat << MODELEOF
    "openai": {
      "api_url": "${OPENAI_BASE_URL:-https://api.openai.com/v1}",
      "available_models": [
        {
          "name": "${E2E_MODEL}",
          "display_name": "${E2E_MODEL}",
          "max_tokens": 128000,
          "reasoning_effort": "${E2E_REASONING_EFFORT:-none}"
        }
      ]
    }
MODELEOF
)
else
    E2E_MODEL="${E2E_MODEL:-claude-sonnet-4-6}"
    LANGUAGE_MODELS_JSON=$(cat << MODELEOF
    "anthropic": {
      "api_url": "${ANTHROPIC_BASE_URL:-https://api.anthropic.com}"
    }
MODELEOF
)
fi
echo "[setup] Native agent model: ${E2E_MODEL_PROVIDER}/${E2E_MODEL}"

cat > "$ZED_CONFIG_DIR/settings.json" << JSONEOF
{
${AGENT_SERVERS_JSON}
  "language_models": {
${LANGUAGE_MODELS_JSON}
  },
  "agent": {
    "default_model": {
      "provider": "${E2E_MODEL_PROVIDER}",
      "model": "${E2E_MODEL}"
    },
    "always_allow_tool_actions": true,
    "show_onboarding": false,
    "auto_open_panel": true
  },
  "context_servers": {
    "slow-mcp-test": {
      "enabled": true,
      "command": "/usr/local/bin/slow-mcp-server",
      "args": []
    }
  }
}
JSONEOF
echo "[zed] Wrote settings to $ZED_CONFIG_DIR/settings.json"

echo "[zed] Starting Zed with WebSocket sync..."
echo "[zed]   ZED_HELIX_URL=$ZED_HELIX_URL"
echo "[zed]   ZED_EXTERNAL_SYNC_ENABLED=$ZED_EXTERNAL_SYNC_ENABLED"
echo "[zed]   ZED_STATELESS=${ZED_STATELESS:-not set}"
echo "[zed]   ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY:+set (${#ANTHROPIC_API_KEY} chars)}"
echo "[zed]   OPENAI_API_KEY=${OPENAI_API_KEY:+set (${#OPENAI_API_KEY} chars)}"
echo "[zed]   HELIX_ACP_SILENCE_TIMEOUT_SECS=${HELIX_ACP_SILENCE_TIMEOUT_SECS:-default}"
echo "[zed]   E2E_AGENTS=$E2E_AGENTS"
echo "[zed]   E2E_HEADLESS=${E2E_HEADLESS:-0}"
echo ""

ZED_HEADLESS_ARGS=()
if [ "${E2E_HEADLESS:-0}" = "1" ]; then
    ZED_HEADLESS_ARGS=(--headless)
fi

# Start Zed (capture logs for debugging)
ZED_LOG_FILE="/tmp/zed-e2e.log"
"$ZED_BINARY" \
    --allow-multiple-instances \
    "${ZED_HEADLESS_ARGS[@]}" \
    "$PROJECT_DIR" \
    > "$ZED_LOG_FILE" 2>&1 &
ZED_PID=$!
echo "[zed] Logs: $ZED_LOG_FILE"

echo "[zed] Started (PID $ZED_PID)"
echo ""
echo "[test] Waiting for protocol flow to complete (timeout: ${TEST_TIMEOUT}s)..."

# ---- Wait for test to complete ----
ELAPSED=0
while [ "$ELAPSED" -lt "$TEST_TIMEOUT" ]; do
    # Check if test server requested Zed restart (Phase 12: reconnect test)
    if [ -f "/tmp/zed-restart-requested" ]; then
        echo "[zed] Restart requested by test server (Phase 12: reconnect test)"
        rm -f /tmp/zed-restart-requested
        kill "$ZED_PID" 2>/dev/null || true
        wait "$ZED_PID" 2>/dev/null || true
        echo "[zed] Restarting Zed..."
        "$ZED_BINARY" \
            --allow-multiple-instances \
            "${ZED_HEADLESS_ARGS[@]}" \
            "$PROJECT_DIR" \
            >> "$ZED_LOG_FILE" 2>&1 &
        ZED_PID=$!
        echo "[zed] Restarted (new PID $ZED_PID)"
    fi

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
