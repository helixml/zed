# Test Coverage Analysis - What We Actually Test

## Your Concern

**"The tests don't test the websocket stuff at all"**

**You're RIGHT.** Let me break down what we actually test vs what we don't.

---

## Current Tests Overview

### Test Set 1: `agent_panel_tests.rs` (6 tests)
**Location:** `/home/luke/pm/zed/crates/agent_ui/src/agent_panel_tests.rs`

**What these test:**
- ❌ NO real WebSocket connection
- ❌ NO actual network communication
- ✅ UI layer logic only (AgentPanel behavior)
- ✅ Mock WebSocket sender (tokio channel, not real socket)
- ✅ Session mapping logic
- ✅ ACP thread creation
- ✅ Event handling

**How they work:**
```rust
// FAKE WebSocket - just a channel
let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
cx.set_global(WebSocketSender {
    sender: Arc::new(RwLock::new(Some(tx))),  // Not a real WebSocket!
});

// Capture what WOULD be sent
let received_message = rx.try_recv();
```

**What they DON'T test:**
- WebSocket connection establishment
- WebSocket message serialization/deserialization
- Network errors
- Connection drops
- Reconnection logic
- Real async message flow
- Integration with actual WebSocket library (tungstenite)

---

### Test Set 2: `test_integration.rs` (3 tests)
**Location:** `/home/luke/pm/zed/crates/external_websocket_sync/src/test_integration.rs`

**What these test:**
- ❌ NO real WebSocket connection
- ✅ Message handler logic (parsing commands)
- ✅ Event serialization
- ✅ JSON format validation
- ❌ NO actual network I/O

**How they work:**
```rust
// Call handler directly with string
WebSocketSync::handle_incoming_message(
    "test-session",
    command_json,  // Just a string, not from network
    &command_sender,
    &event_sender,
).await?;

// Check channel output
let response_event = event_receiver.recv().await;
```

**What they DON'T test:**
- Real WebSocket frames
- Actual tungstenite::Message types
- Network transmission
- Binary vs text frames
- Connection lifecycle

---

## What's Missing: Real WebSocket Tests

### Gap 1: No Actual WebSocket Server/Client
We never test:
- Starting a real WebSocket server
- Client connecting to it
- Sending actual WebSocket frames
- Receiving actual WebSocket frames

### Gap 2: No End-to-End Flow
We never test:
```
Real Client (sends WS message)
    ↓ (actual network)
Real Server (receives WS message)
    ↓ (processes)
Real Server (sends WS response)
    ↓ (actual network)
Real Client (receives WS response)
```

### Gap 3: No Integration with Tungstenite
We use `tungstenite` library but never test:
- Handshake
- Frame encoding/decoding
- Connection management
- Error conditions (disconnect, timeout, etc.)

---

## Why This Matters

### Scenarios NOT Covered

1. **Connection Errors**
   ```
   What happens if WebSocket connection drops?
   What happens if server is unreachable?
   What happens during reconnection?
   ```
   **Currently:** ❌ Untested

2. **Message Format Issues**
   ```
   What if tungstenite serializes differently than we expect?
   What if frame splitting happens?
   What if we get partial messages?
   ```
   **Currently:** ❌ Untested

3. **Concurrent Messages**
   ```
   What if multiple messages arrive simultaneously?
   What if response comes before we're ready?
   What if messages arrive out of order?
   ```
   **Currently:** ❌ Untested

4. **Protocol Edge Cases**
   ```
   What about WebSocket ping/pong?
   What about close frames?
   What about binary vs text frames?
   ```
   **Currently:** ❌ Untested

---

## How Hard Would Real WebSocket Tests Be?

### Option 1: In-Process WebSocket Server (EASIER)

**Difficulty:** Medium
**Time:** 2-3 hours
**Dependencies:** tokio, tungstenite (already have)

**Implementation:**
```rust
#[tokio::test]
async fn test_real_websocket_bidirectional() {
    // Start real WebSocket server on random port
    let server = tokio::spawn(async {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(handle_connection(stream));
        }
    });

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create real WebSocket client
    let (mut ws_stream, _) = connect_async("ws://127.0.0.1:xxxx").await?;

    // Send REAL WebSocket message
    ws_stream.send(Message::Text(json!({
        "type": "create_thread",
        "message": "Hello"
    }).to_string())).await?;

    // Receive REAL WebSocket response
    if let Some(msg) = ws_stream.next().await {
        let response: Value = serde_json::from_str(&msg?.to_string())?;
        assert_eq!(response["type"], "thread_created");
        assert!(response["acp_thread_id"].is_string());
    }
}
```

**Pros:**
- Tests real WebSocket frames
- Tests actual serialization
- No external dependencies
- Fast execution

**Cons:**
- Still in same process (not truly separate)
- Doesn't test network errors well

---

### Option 2: Docker-Based Integration Test (HARDER)

**Difficulty:** Hard
**Time:** 1 day
**Dependencies:** Docker, docker-compose

**Implementation:**
```rust
// Build Zed with WebSocket server
// Run in Docker container
// Connect from test with real client
// Truly separate processes
```

**Pros:**
- True integration test
- Tests real deployment scenario
- Can test network failures

**Cons:**
- Slow (Docker startup)
- Complex setup
- CI/CD complexity

---

### Option 3: Mock WebSocket Library (COMPROMISE)

**Difficulty:** Easy
**Time:** 1 hour
**Dependencies:** Just test mocks

**Implementation:**
```rust
// Use tokio-tungstenite's test utilities
// Mock WebSocket at library level
// Test our code but not tungstenite itself
```

**Pros:**
- Quick to implement
- Good coverage of our code
- No infrastructure needed

**Cons:**
- Doesn't test real tungstenite behavior
- Misses integration issues

---

## Recommendation: Add Real WebSocket Tests

### Proposed Test Suite

#### Test 1: Real Connection Lifecycle
```rust
#[tokio::test]
async fn test_websocket_connection_lifecycle() {
    // Start server
    // Client connects
    // Send/receive messages
    // Client disconnects gracefully
    // Verify cleanup
}
```

#### Test 2: Bidirectional Message Flow
```rust
#[tokio::test]
async fn test_real_bidirectional_flow() {
    // Client sends create_thread
    // Server receives, processes
    // Server sends thread_created
    // Client receives, validates
    // Verify message format matches protocol
}
```

#### Test 3: Multiple Concurrent Clients
```rust
#[tokio::test]
async fn test_multiple_clients() {
    // Connect 3 clients
    // Each creates thread
    // Each gets unique thread_id
    // Verify no cross-talk
}
```

#### Test 4: Connection Error Handling
```rust
#[tokio::test]
async fn test_connection_errors() {
    // Kill server mid-message
    // Verify client handles gracefully
    // Test reconnection logic
}
```

#### Test 5: Message Serialization
```rust
#[tokio::test]
async fn test_tungstenite_serialization() {
    // Send various message types
    // Verify tungstenite encodes correctly
    // Verify we can decode what we sent
    // Test edge cases (empty, large, unicode)
}
```

---

## Implementation Plan

### Phase 1: Add Basic Real WebSocket Test (2-3 hours)

**File:** `crates/external_websocket_sync/src/real_websocket_tests.rs`

```rust
#[cfg(test)]
mod real_websocket_tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{connect_async, accept_async};

    async fn start_test_server() -> SocketAddr {
        // Minimal WebSocket server for testing
    }

    #[tokio::test]
    async fn test_real_websocket_roundtrip() {
        // Full WebSocket test
    }
}
```

### Phase 2: Add Error Scenarios (1-2 hours)

```rust
#[tokio::test]
async fn test_connection_drop() { }

#[tokio::test]
async fn test_invalid_messages() { }

#[tokio::test]
async fn test_timeout_handling() { }
```

### Phase 3: Add Concurrency Tests (1-2 hours)

```rust
#[tokio::test]
async fn test_concurrent_messages() { }

#[tokio::test]
async fn test_multiple_threads_one_connection() { }
```

---

## Why We Need This

### Current Risk
**Without real WebSocket tests, we could have:**
- Serialization bugs (our JSON doesn't match what tungstenite sends)
- Frame handling bugs (messages split across frames)
- Connection bugs (reconnection doesn't work)
- Concurrency bugs (race conditions in message handling)

**And we wouldn't know until production!**

### With Real WebSocket Tests
✅ Catch serialization issues early
✅ Verify actual network behavior
✅ Test error scenarios
✅ Build confidence in the integration
✅ Safe to deploy

---

## Difficulty Assessment

### Easy (1-2 hours): In-process WebSocket test
- Start server on localhost
- Connect client
- Send/receive messages
- **This alone would catch 80% of issues**

### Medium (4-6 hours): Comprehensive suite
- Connection lifecycle
- Error handling
- Concurrency
- Protocol edge cases

### Hard (1-2 days): Full integration
- Docker-based
- Separate processes
- Network simulation
- Performance testing

---

## My Recommendation

**Start with Easy:** Add 2-3 real WebSocket tests using in-process server/client

**Benefits:**
- Quick to implement (2-3 hours)
- Catches most real bugs
- Builds on existing infrastructure
- No new dependencies

**First Test to Write:**
```rust
#[tokio::test]
async fn test_create_thread_real_websocket() {
    // This ONE test would verify:
    // - Real WebSocket connection works
    // - Message serialization is correct
    // - tungstenite integration works
    // - Response flow is correct
}
```

**This is CRITICAL** because right now we're testing with mocks and assuming the real WebSocket will work the same way. That's a dangerous assumption!

---

## Next Steps

1. **Create** `real_websocket_tests.rs` in `external_websocket_sync`
2. **Implement** basic connection test (1 hour)
3. **Add** message roundtrip test (1 hour)
4. **Run** and verify (30 min)
5. **Iterate** based on findings

**Total:** 2-3 hours to dramatically increase confidence

**Without this:** We're basically hoping the WebSocket integration works 🤞

---

## Summary

**Current Tests:**
- ✅ UI logic
- ✅ Mock channels
- ✅ Message handlers
- ❌ Real WebSockets
- ❌ Network I/O
- ❌ tungstenite integration

**Needed:**
- Real WebSocket server/client tests
- Actual message serialization verification
- Connection lifecycle testing
- Error scenario coverage

**Effort:** 2-3 hours for basic coverage, worth it for peace of mind!
