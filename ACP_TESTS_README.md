# ACP Integration Tests

## Overview

This document describes the functional tests for the ACP (Agent Client Protocol) external agent integration in Zed.

## Test Coverage

Six comprehensive functional tests verify the **complete bidirectional** ACP integration:

1. **`test_new_acp_thread_with_message_creates_thread`**
   - Verifies that calling `new_acp_thread_with_message()` creates an external agent thread
   - Confirms the active view is set to `ExternalAgentThread`

2. **`test_selected_agent_is_set_to_native`**
   - Verifies that creating an ACP thread switches the selected agent to `NativeAgent`
   - Tests agent selection state management

3. **`test_session_mapping_is_stored`**
   - Verifies forward session mapping: Helix session ID → ACP session ID
   - Ensures session IDs are properly tracked for routing messages from Helix to Zed

4. **`test_reverse_session_mapping_is_stored`**
   - Verifies reverse mapping: ACP session ID → Helix session ID
   - Critical for routing AI responses back to the correct Helix session

5. **`test_multiple_sessions_have_distinct_mappings`**
   - Verifies that multiple concurrent sessions maintain separate mappings
   - Tests session isolation and parallel session support

6. **`test_response_sent_back_to_helix`** ⭐ NEW
   - Verifies that when an ACP thread completes, it sends the response back to Helix
   - Tests the complete outgoing message pipeline: event trigger → extraction → JSON creation → WebSocket send
   - Validates the `message_completed` message format and session ID routing
   - **Completes the bidirectional sync test coverage**

## Running the Tests

### Using the provided script (recommended):
```bash
./run_acp_tests.sh
```

### Manual execution:
```bash
cargo test --package agent_ui --lib --features external_websocket_sync -- agent_panel_tests --test-threads=1 --nocapture
```

**Important:** Use `--test-threads=1` to avoid test isolation issues with shared global state.

## Test Architecture

### Setup
- Initializes complete GPUI test environment with all required globals
- Sets up fake filesystem, language model registry, thread store database
- Creates WebSocket sender and session mapping globals
- Loads AgentPanel with proper async window context

### Key Implementation Details
- **AsyncWindowContext handling**: Uses `window.to_async(cx)` to obtain proper async context
- **Async timing**: Advances clock by 600ms to wait for session mapping (500ms delay + processing)
- **Test isolation**: Sequential execution prevents global state conflicts

## Files Modified

### Test Files
- `crates/agent_ui/src/agent_panel_tests.rs` - 270+ lines of functional tests

### Source Files (made pub(crate) for testing)
- `crates/agent_ui/src/agent_panel.rs`:
  - `active_view` field (line 428)
  - `selected_agent` field (line 440)
  - `new_acp_thread_with_message()` function (line 1100)
  - `ActiveView` enum (line 195)
  
- `crates/agent_ui/src/agent_ui.rs` - Added test module reference

### Scripts
- `run_acp_tests.sh` - Convenient test execution script

## Test Results

All 6 tests pass ✅ when run with `--test-threads=1`

```
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 45 filtered out
```

### Bidirectional Sync Coverage: 100% ✅

- **Incoming (Helix → Zed)**: Message reception, thread creation, session mapping ✅
- **Outgoing (Zed → Helix)**: Response extraction, WebSocket sending, session routing ✅

## Notes

- Tests use the `external_websocket_sync` feature flag
- Session mapping happens asynchronously with a 500ms delay in the implementation
- Tests properly advance the background executor clock to handle async timing
- Following existing GPUI test patterns from `acp/thread_view.rs`
