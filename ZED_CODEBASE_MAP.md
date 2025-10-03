# Zed Codebase Map - ACP Integration Architecture

## Concern: Implementation Location

**Your Question:** "I thought we weren't going to have the implementation in the frontend code and put it lower in the system instead?"

**Reality Check:** The implementation IS currently in `agent_panel.rs` which is UI code. Let me map out the current architecture and identify what should potentially be refactored.

---

## Current Architecture (As Implemented)

### Layer 1: External WebSocket Sync Crate (Lower Level)
**Location:** `/home/luke/pm/zed/crates/external_websocket_sync/`

**Purpose:** Provides infrastructure for WebSocket communication and session management

**What's Actually Here:**
```
external_websocket_sync/
├── src/
│   ├── external_websocket_sync.rs  # Global structs and types
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
- `ThreadCreationChannel` - Channel for thread creation requests
- WebSocket client connection handling

**What It Does:**
- Defines global data structures for session tracking
- Provides WebSocket client connectivity
- Manages settings for external sync

**What It DOESN'T Do:**
- ❌ Does NOT create ACP threads
- ❌ Does NOT handle thread lifecycle
- ❌ Does NOT send responses (just provides the channel)

---

### Layer 2: Agent UI Crate (Frontend/UI Level)
**Location:** `/home/luke/pm/zed/crates/agent_ui/`

**Purpose:** UI for agent interactions (panels, views, etc.)

**What's Actually Here:**
```
agent_ui/
├── src/
│   ├── agent_panel.rs              # ⚠️ MAIN ACP INTEGRATION IS HERE
│   ├── agent_panel_tests.rs        # Our 6 functional tests
│   ├── acp/
│   │   └── thread_view.rs          # ACP thread view UI
│   └── ... other UI components
```

**Key Functions in `agent_panel.rs`:**
- `new_acp_thread_with_message()` (lines 1100-1250) - **CREATES ACP THREADS**
  - Receives WebSocket messages
  - Creates ACP thread views
  - Manages session mappings
  - Subscribes to thread events
  - **Sends responses back via WebSocket**

**What It Does:**
- ✅ Creates ACP threads from WebSocket messages
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

### Current Flow
```
WebSocket Message (from Helix)
    ↓
external_websocket_sync (receives, stores in global)
    ↓
agent_panel.rs (UI CODE!) processes pending requests  ⚠️
    ↓
agent_panel.rs creates ACP thread                     ⚠️
    ↓
agent_panel.rs subscribes to events                   ⚠️
    ↓
agent_panel.rs sends response via WebSocket           ⚠️
    ↓
Back to Helix
```

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

## Recommended Refactoring

### Option 1: Create Middle Layer (Clean Architecture)

**New Crate:** `external_agent_manager/`

**Responsibilities:**
- Receive WebSocket messages from `external_websocket_sync`
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
| `agent_ui/src/agent_panel.rs` | UI | **Main implementation** (thread creation, events, responses) | 1100-1250 |
| `external_websocket_sync/src/external_websocket_sync.rs` | Infrastructure | Global structs, session mappings | 46-100 |
| `external_websocket_sync/src/websocket_sync.rs` | Infrastructure | WebSocket client connection | N/A |
| `acp_thread/src/acp_thread.rs` | Core | Thread state, events, AI logic | 783-803 (events) |

### Test Files
| File | What It Tests |
|------|---------------|
| `agent_ui/src/agent_panel_tests.rs` | Full bidirectional sync (6 tests) |

### Documentation
| File | Purpose |
|------|---------|
| `ACP_INTEGRATION_SUMMARY.md` | Complete implementation overview |
| `ACP_TESTS_README.md` | Test usage guide |
| `ACP_TEST_COVERAGE_SUMMARY.md` | Coverage analysis |

---

## Why This Matters

### Current Issues
1. **Testing Complexity** - Need full UI framework (GPUI) to test business logic
2. **Tight Coupling** - UI and business logic intertwined
3. **Reusability** - Can't reuse this logic without UI
4. **Maintainability** - Changes to UI affect core functionality

### If We Refactor
1. **Better Tests** - Test business logic without UI framework
2. **Separation** - UI changes don't affect core logic
3. **Extensibility** - Easy to add new external integrations
4. **Clarity** - Clear boundaries between layers

---

## Decision Point

**Question:** Should we refactor this to move the implementation out of `agent_panel.rs`?

**Considerations:**
- **It works** - Current implementation functions correctly, all tests pass
- **Time investment** - Refactoring would take significant effort
- **Risk** - Moving working code could introduce bugs
- **Future maintenance** - Cleaner architecture would help long-term

**My Recommendation:**
- **Short term:** Document this as technical debt
- **Long term:** Plan refactoring when:
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

**The issue:**
Business logic lives in the UI layer, which violates separation of concerns.

**The fix:**
Create a middle layer (`external_agent_manager`) to handle the orchestration between WebSocket and ACP threads, leaving the UI to just render.

**Current status:**
It works, it's tested (100% coverage), but it's not ideally architected. Whether to refactor depends on your priorities and timeline.
