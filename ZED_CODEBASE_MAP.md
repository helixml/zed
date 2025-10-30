# Zed Codebase Map - ACP Integration Architecture

## Concern: Implementation Location

**Your Question:** "I thought we weren't going to have the implementation in the frontend code and put it lower in the system instead?"

**Reality Check:** The implementation IS currently in `agent_panel.rs` which is UI code. Let me map out the current architecture and identify what should potentially be refactored.

---

## Current Architecture (As Implemented - Post Cleanup)

### Layer 1: External WebSocket Sync Crate (Lower Level)
**Location:** `/home/luke/pm/zed/crates/external_websocket_sync/`

**Purpose:** Provides infrastructure for WebSocket communication and session management

**What's Actually Here:**
```
external_websocket_sync/
├── src/
│   ├── external_websocket_sync.rs  # Global structs, init, types
│   ├── websocket_sync.rs          # WebSocket connection handling
│   ├── sync.rs                    # Sync client implementation
│   ├── server.rs                  # HTTP server for external connections
│   ├── mcp.rs                     # MCP server integration
│   ├── types.rs                   # Type definitions
│   └── sync_settings/             # Settings management
```

**Key Components:**
- `ExternalSessionMapping` - Global mapping: Helix session ID → ACP context ID
- `ContextToHelixSessionMapping` - Global reverse mapping: ACP context ID → Helix session ID
- `WebSocketSender` - Global WebSocket sender for responses
- `GLOBAL_CONTEXT_TO_HELIX_MAPPING` - Static global for async task access
- WebSocket client connection handling

**What It Does:**
- Defines global data structures for session tracking
- Provides WebSocket client connectivity
- Manages settings for external sync
- **Initializes WebSocket connection during startup** (`init()` function)

**What It DOESN'T Do:**
- ❌ Does NOT create ACP threads
- ❌ Does NOT handle thread lifecycle
- ❌ Does NOT trigger thread creation (no polling, no queue)

---

### Layer 2: Agent UI Crate (Frontend/UI Level)
**Location:** `/home/luke/pm/zed/crates/agent_ui/`

**Purpose:** UI for agent interactions (panels, views, etc.)

**What's Actually Here:**
```
agent_ui/
├── src/
│   ├── agent_panel.rs              # ⚠️ MAIN ACP INTEGRATION IS HERE
│   ├── agent_panel_tests.rs        # Our 6 functional tests (MOCK WebSocket)
│   ├── acp/
│   │   └── thread_view.rs          # ACP thread view UI
│   └── ... other UI components
```

**Key Functions in `agent_panel.rs`:**
- `new_acp_thread_with_message()` (lines 995-1177) - **CREATES ACP THREADS**
  - Called directly (no polling mechanism)
  - Creates ACP thread views synchronously
  - Manages session mappings asynchronously
  - Subscribes to `AcpThreadEvent::Stopped`
  - **Sends responses back via WebSocket when event fires**

**What It Does:**
- ✅ Creates ACP threads from direct function calls
- ✅ Manages thread lifecycle
- ✅ Handles event subscriptions
- ✅ Extracts AI responses
- ✅ Sends responses back to Helix

**Problem:**
This is UI/presentation layer code handling business logic!

---

### Layer 3: ACP Thread Crate (Core Logic)
**Location:** `/home/luke/pm/zed/crates/acp_thread/`

**Purpose:** Core ACP thread implementation (conversation management)

**What's Here:**
```
acp_thread/
├── src/
│   └── acp_thread.rs  # ACP thread implementation, events, entries
```

**Key Components:**
- `AcpThread` - The actual conversation thread
- `AcpThreadEvent` - Events like `Stopped`, `EntryUpdated`, etc.
- Thread entry management
- AI interaction logic

**What It Does:**
- Manages conversation state
- Fires events when AI completes
- Handles tool calls and interactions

**What It DOESN'T Know About:**
- ❌ Helix sessions
- ❌ WebSocket connections
- ❌ External sync

---

## The Problem: Layering Violation

### Current Flow (Post-Cleanup)
```
WebSocket Connection Established
    ↓ (during external_websocket_sync::init())
external_websocket_sync creates globals (mappings, sender)
    ↓
[TRIGGER POINT - Currently unclear how this happens]
    ↓
agent_panel.new_acp_thread_with_message() called directly  ⚠️
    ↓
agent_panel.rs creates ACP thread synchronously           ⚠️
    ↓
agent_panel.rs subscribes to AcpThreadEvent::Stopped      ⚠️
    ↓
When event fires → extract response                       ⚠️
    ↓
agent_panel.rs sends via WebSocket global                 ⚠️
    ↓
Back to Helix
```

**Key Change from Before:**
- ~~No more `ThreadCreationChannel`~~ (REMOVED)
- ~~No more `PendingThreadRequests` queue~~ (REMOVED)
- ~~No more polling in render()~~ (REMOVED)
- Now uses **direct function call** to `new_acp_thread_with_message()`

### What SHOULD Happen (Ideal Architecture)
```
WebSocket Message (from Helix)
    ↓
external_websocket_sync (receives)
    ↓
NEW: external_thread_manager (business logic layer)
    ↓
acp_thread (core thread logic)
    ↓ (events)
NEW: external_thread_manager (handles responses)
    ↓
external_websocket_sync (sends)
    ↓
Back to Helix

UI layer just observes and renders
```

---

## Test Coverage - Where Tests Integrate

### Test Set 1: `agent_panel_tests.rs` (6 tests)
**Location:** `/home/luke/pm/zed/crates/agent_ui/src/agent_panel_tests.rs`

**What Layer Do These Test?**
```
╔═══════════════════════════════════════╗
║  Layer 2: agent_panel.rs (UI LAYER)  ║ ← Tests run HERE
╠═══════════════════════════════════════╣
║  • new_acp_thread_with_message()     ║
║  • Session mapping storage           ║
║  • Event subscription logic          ║
║  • Response extraction & sending     ║
╚═══════════════════════════════════════╝
         ↓ (uses)                ↑
         ↓                       ↑ (mocked)
╔═════════════════════╗  ╔═════════════════════╗
║ Layer 3: ACP Thread ║  ║ Layer 1: WebSocket  ║
║ (REAL)              ║  ║ (MOCKED)            ║
╚═════════════════════╝  ╚═════════════════════╝
```

**What They Actually Test:**
- ✅ **UI layer business logic** (thread creation, mapping, events)
- ✅ **Integration with real ACP thread** (actual thread is created)
- ✅ **Event handling** (subscribe to real AcpThreadEvent::Stopped)
- ❌ **NO WebSocket testing** (uses mock tokio channel)

**How the Mock Works:**
```rust
// Test setup creates FAKE WebSocket sender
let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
cx.set_global(WebSocketSender {
    sender: Arc::new(RwLock::new(Some(tx))),  // NOT a real WebSocket!
});

// Test can capture what WOULD be sent to WebSocket
let received_message = rx.try_recv();  // Gets JSON from channel, not network
```

**What's NOT Tested:**
- ❌ Real WebSocket connection establishment
- ❌ WebSocket frame serialization (tungstenite::Message)
- ❌ Network transmission
- ❌ Connection errors/drops
- ❌ WebSocket protocol handshake

### Test Set 2: `test_integration.rs` (3 tests)
**Location:** `/home/luke/pm/zed/crates/external_websocket_sync/src/test_integration.rs`

**What Layer Do These Test?**
```
╔══════════════════════════════════════════╗
║  Layer 1: external_websocket_sync       ║ ← Tests run HERE
╠══════════════════════════════════════════╣
║  • Message handler logic                ║
║  • Event serialization                  ║
║  • JSON format validation               ║
╚══════════════════════════════════════════╝
```

**What They Actually Test:**
- ✅ **Message handler logic** (parsing JSON commands)
- ✅ **Event serialization** (converting events to JSON)
- ✅ **Protocol format** (JSON structure matches spec)
- ❌ **NO WebSocket testing** (calls handler directly with strings)

**How It Works:**
```rust
// Call handler directly with string (not from network)
WebSocketSync::handle_incoming_message(
    "test-session",
    command_json,  // Just a string, not tungstenite::Message
    &command_sender,
    &event_sender,
).await?;

// Check what event was generated
let response_event = event_receiver.recv().await;
```

**What's NOT Tested:**
- ❌ Real WebSocket frames
- ❌ Network I/O
- ❌ tungstenite integration

---

## The Missing Piece: Real WebSocket Tests

**Neither test set actually tests WebSocket communication!**

Both test sets mock the WebSocket layer using tokio channels. This means:

### What Could Break in Production (Untested):
1. **WebSocket Serialization**
   - Our JSON might not serialize correctly to WebSocket frames
   - tungstenite might encode differently than expected

2. **Connection Issues**
   - WebSocket handshake failures
   - Connection drops mid-message
   - Reconnection logic

3. **Message Format**
   - Frame splitting (large messages)
   - Binary vs text frame handling
   - Partial message handling

4. **Concurrency**
   - Multiple simultaneous messages
   - Out-of-order delivery
   - Race conditions in WebSocket layer

### Recommended: Add Real WebSocket Tests
See `TEST_COVERAGE_ANALYSIS.md` for detailed plan to add in-process WebSocket server tests.

---

## Recommended Refactoring

### Option 1: Create Middle Layer (Clean Architecture)

**New Crate:** `external_agent_manager/`

**Responsibilities:**
- Receive triggers to create threads
- Create and manage ACP threads
- Subscribe to thread events
- Extract responses
- Send responses via `external_websocket_sync`

**Benefits:**
- ✅ Separates business logic from UI
- ✅ UI becomes pure presentation
- ✅ Testable without UI framework
- ✅ Reusable for non-UI scenarios

**agent_panel.rs** becomes:
- Just renders the thread view
- Observes state changes
- Delegates actions to manager

### Option 2: Move Logic to external_websocket_sync (Pragmatic)

**Extend:** `external_websocket_sync/src/thread_manager.rs` (new file)

**Responsibilities:**
- Same as Option 1, but within existing crate

**Benefits:**
- ✅ No new crate needed
- ✅ Still separates concerns
- ✅ Keeps related code together

**Downside:**
- Mixes infrastructure (WebSocket) with business logic (thread management)

---

## Current File Locations Reference

### Implementation Files
| File | Layer | What It Does | Lines of Interest |
|------|-------|--------------|-------------------|
| `agent_ui/src/agent_panel.rs` | UI | **Main implementation** (thread creation, events, responses) | 995-1177 |
| `external_websocket_sync/src/external_websocket_sync.rs` | Infrastructure | Global structs, session mappings, init | 46-85 |
| `external_websocket_sync/src/websocket_sync.rs` | Infrastructure | WebSocket client connection | 83-116 |
| `acp_thread/src/acp_thread.rs` | Core | Thread state, events, AI logic | Events fired here |

### Test Files
| File | What It Tests | What Layer |
|------|---------------|------------|
| `agent_ui/src/agent_panel_tests.rs` | Full flow with MOCK WebSocket (6 tests) | UI Layer + ACP Thread (real) |
| `external_websocket_sync/src/test_integration.rs` | Message handling with MOCK (3 tests) | Infrastructure Layer |

**CRITICAL: No tests actually test real WebSocket communication!**

### Documentation
| File | Purpose |
|------|---------|
| `ACP_INTEGRATION_SUMMARY.md` | Complete implementation overview |
| `ACP_TESTS_README.md` | Test usage guide |
| `TEST_COVERAGE_ANALYSIS.md` | Coverage analysis - what's NOT tested |
| `WEBSOCKET_REMOTE_CONTROL_DESIGN.md` | Generic protocol design |

---

## Why This Matters

### Current Issues
1. **Testing Gap** - No real WebSocket testing means potential bugs in production
2. **Tight Coupling** - UI and business logic intertwined
3. **Unclear Trigger** - How does `new_acp_thread_with_message()` get called?
4. **Maintainability** - Changes to UI affect core functionality

### If We Refactor
1. **Better Tests** - Test business logic without UI framework
2. **Separation** - UI changes don't affect core logic
3. **Extensibility** - Easy to add new external integrations
4. **Clarity** - Clear boundaries between layers
5. **Real WebSocket Tests** - Can test actual network communication

---

## Decision Point

**Question:** Should we refactor this to move the implementation out of `agent_panel.rs`?

**Considerations:**
- **It works** - Current implementation functions correctly, all tests pass
- **But lacks coverage** - No real WebSocket tests = production risk
- **Time investment** - Refactoring would take significant effort
- **Risk** - Moving working code could introduce bugs
- **Future maintenance** - Cleaner architecture would help long-term

**My Recommendation:**
1. **Immediate:** Add real WebSocket tests (2-3 hours) - See TEST_COVERAGE_ANALYSIS.md
2. **Short term:** Document the trigger mechanism (how does thread creation get called?)
3. **Long term:** Plan refactoring when:
   - Adding more external integrations
   - Need to test without UI
   - Performance becomes an issue

---

## Crate Dependency Graph

```
agent_ui (UI/Frontend)
  ├─→ external_websocket_sync (Infrastructure)
  │     └─→ assistant_context (Context management)
  ├─→ acp_thread (Core ACP logic)
  └─→ agent (Agent server abstraction)

Current problem: agent_ui contains too much business logic!
```

**Ideal:**
```
agent_ui (UI/Frontend - pure presentation)
  └─→ external_agent_manager (Business logic) [NEW]
        ├─→ external_websocket_sync (Infrastructure)
        ├─→ acp_thread (Core ACP logic)
        └─→ agent (Agent server abstraction)
```

---

## Summary

**Where the code actually is:**
- **UI Layer** (`agent_panel.rs`) - Thread creation, event handling, response sending ⚠️
- **Infrastructure** (`external_websocket_sync`) - WebSocket, session mappings, globals ✅
- **Core** (`acp_thread`) - Thread state, AI interaction ✅

**What tests actually test:**
- **UI Layer tests** - Business logic with REAL ACP threads but MOCK WebSocket
- **Infrastructure tests** - Message handling logic, no real WebSocket
- **MISSING** - Real WebSocket connection tests ⚠️

**The issues:**
1. Business logic lives in the UI layer (violates separation of concerns)
2. No real WebSocket tests (production risk)
3. Unclear trigger mechanism for thread creation

**The fix:**
1. Add real WebSocket tests (immediate priority)
2. Create a middle layer (`external_agent_manager`) to handle orchestration
3. Leave UI to just render

**Current status:**
It works, it's tested (UI logic + ACP integration), but WebSocket layer is untested and architecture isn't ideal. Whether to refactor depends on priorities and timeline.
