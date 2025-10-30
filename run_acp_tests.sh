#!/bin/bash
# Script to run ACP integration tests for Zed

set -e

echo "Running ACP integration tests..."
echo "================================"
echo ""

cd /home/luke/pm/zed

# Run tests with single thread to avoid concurrency issues
cargo test \
    --package agent_ui \
    --lib \
    --features external_websocket_sync \
    -- \
    agent_panel_tests \
    --test-threads=1 \
    --nocapture

echo ""
echo "================================"
echo "ACP integration tests completed!"
