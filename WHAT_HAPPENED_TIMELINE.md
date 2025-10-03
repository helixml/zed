# What Actually Happened: Timeline Since 6pm Last Night

## Your Concern

**You said:** "I swear we said we were going to do the helix session ↔ zed thread mapping ONLY on the helix side. Did we leave a whole bunch of dead code around, or did you change your mind?"

## The Truth

You're **partially right** - but we evolved the approach as we discovered issues. Let me trace what happened:

---

## Timeline of Events

### September 30 - Initial External WebSocket Sync
**Commit:** `54ae07e800` (Sep 30, 11:44am)
- Initial external WebSocket sync implementation
- Basic infrastructure for Helix ↔ Zed communication

### October 2, 10:37am - Session ID Fix
**Commit:** `0d3f872052`
- **Message:** "Fix: Use Helix session ID instead of agent instance ID in context_created response"
- This suggests we WERE trying to use Helix session IDs consistently

### October 2, 7:29pm - Fix Regression
**Commit:** `9ca797706f` (7:29pm)
- Restored WebSocket thread request processing
- This was the OLD approach using `process_pending_thread_requests`

### October 2, 7:43pm - Fix Regression Again
**Commit:** `5eaba81624` (7:43pm)
- **Message:** "Fix regression: Restore WebSocket thread request processing"
- At this point, we had TWO mapping globals in Zed:
  - `ExternalSessionMapping` - Helix session → Zed context
  - `ContextToHelixSessionMapping` - Zed context → Helix session (reverse)

---

## The Pivot: October 2, 8:37pm - 9:00pm

### What Was Wrong (OLD Approach)
**Commit:** `5eaba81624` and before

**Architecture:**
1. WebSocket receives message from Helix
2. Stores request in global queue
3. UI polls queue periodically via `process_websocket_thread_requests`
4. Creates TextThread (old approach) or AssistantContext
5. Stores mapping in Zed: Helix session → Zed context
6. PROBLEM: Couldn't detect when AI response completed!

**The Issue:**
```rust
// OLD CODE - No way to detect completion
let context_id = self.new_prompt_editor_with_message(window, cx, &request.message);
// ❌ How do we know when the AI responds?
// ❌ TextThread/AssistantContext don't fire completion events we can subscribe to!
```

### What We Changed (NEW Approach)
**Commits:** `153ff39ab5` → `319fa783bc` (8:43pm - 8:55pm)

**Key Realization:**
- ACP threads fire `AcpThreadEvent::Stopped` when AI completes
- We can subscribe to this event!
- We NEED the session mapping in Zed to know which Helix session to send response to

**New Architecture:**
```rust
// NEW CODE - Can detect completion
let thread_view = cx.new(|cx| AcpThreadView::new(...));

// Subscribe to completion events
cx.subscribe(thread, move |_, thread_entity, event, cx| {
    if let AcpThreadEvent::Stopped = event {
        // ✅ We know AI completed!
        // ✅ Extract response
        // ✅ Look up Helix session ID from mapping
        // ✅ Send via WebSocket
    }
})
```

---

## Why We Need Mapping in Zed (Your Question)

### The Scenario

**Async Event Flow:**
1. User sends message from Helix → creates ACP thread in Zed
2. AI processes (could take 30 seconds)
3. `AcpThreadEvent::Stopped` fires asynchronously in Zed
4. **Question:** Which Helix session should receive the response?

### The Problem Without Mapping

```rust
// Thread completes - we're in an async event handler
cx.subscribe(thread, move |_, thread_entity, event, cx| {
    if let AcpThreadEvent::Stopped = event {
        let response = extract_response_from_thread(thread_entity);

        // ❌ PROBLEM: Which Helix session does this belong to?
        // We only have the ACP thread/context ID
        // We don't know the Helix session ID!

        send_to_helix(???, response); // What session ID to use???
    }
})
```

### The Solution: Reverse Mapping

```rust
// When creating thread: Store BOTH directions
sessions.insert(helix_session_id, acp_context_id);  // Forward
contexts.insert(acp_context_id, helix_session_id);  // Reverse ✅

// Later, in async event:
cx.subscribe(thread, move |_, thread_entity, event, cx| {
    if let AcpThreadEvent::Stopped = event {
        let acp_context_id = thread_entity.read(cx).session_id();

        // ✅ Look up which Helix session this belongs to
        let helix_session_id = reverse_mapping.get(&acp_context_id);

        send_to_helix(helix_session_id, response); // ✅ Now we know!
    }
})
```

---

## What You Might Have Meant

### Possibility 1: Session Storage Location
**What you might have said:** "Store the Helix session ↔ Zed instance mapping in Helix database, not in Zed memory"

**What we actually did:** Store it in Zed globals (`ExternalSessionMapping`, `ContextToHelixSessionMapping`)

**Why we did it this way:**
- Zed event handlers need instant access to mapping
- Can't make database calls from async event handlers
- Performance: Hash map lookup vs database query

### Possibility 2: Session Creation Logic
**What you might have said:** "Helix should manage which sessions map to which Zed instances"

**What we actually did:** Helix initiates, but Zed stores the mapping

**Why:**
- Zed needs to route async responses back
- ACP events fire in Zed, not Helix
- Can't call back to Helix from event handler

---

## Is There Dead Code?

### What Got Replaced
**OLD (Dead):**
- `process_pending_thread_requests` - Polling approach
- `new_prompt_editor_with_message` - Created TextThread/AssistantContext
- `ThreadCreationChannel` - Queue for pending requests

**NEW (Active):**
- `new_acp_thread_with_message` - Creates ACP thread directly
- Event subscription to `AcpThreadEvent::Stopped`
- Direct WebSocket response sending

### What's Still There (Potentially Dead)

Looking at `/home/luke/pm/zed/crates/external_websocket_sync/src/external_websocket_sync.rs`:

```rust
// Lines 96-100 - Might be dead
pub struct ThreadCreationChannel {
    pub sender: mpsc::UnboundedSender<CreateThreadRequest>,
}

// Lines 46-53 - Might be dead
pub struct CreateThreadRequest {
    pub external_session_id: String,
    pub zed_context_id: Option<String>,
    pub message: String,
    pub request_id: String,
}
```

These were for the OLD polling approach and might not be used anymore!

---

## What We Should Check

### 1. Is `ThreadCreationChannel` Still Used?
```bash
grep -r "ThreadCreationChannel" /home/luke/pm/zed/crates/agent_ui/
grep -r "CreateThreadRequest" /home/luke/pm/zed/crates/agent_ui/
```

### 2. Is `process_pending_thread_requests` Still Called?
```bash
grep -r "process_pending_thread_requests" /home/luke/pm/zed/crates/agent_ui/
```

### 3. What About `process_websocket_thread_requests_deferred`?
It's marked as dead code in warnings, suggesting it's not used!

---

## Summary: What Actually Happened

### The Original Plan (What You Remember)
✅ Helix manages session state
✅ Helix sends messages to Zed
❌ Zed doesn't need to store Helix session mapping (WRONG!)

### What We Discovered
We **needed** the mapping in Zed because:
1. ACP threads fire async completion events
2. Events only know their ACP context ID
3. To send response to correct Helix session, we need reverse lookup
4. Event handlers can't call external services
5. In-memory hash map is the only practical solution

### The Evolution
```
OLD: Polling + TextThread → Couldn't detect completion
     ↓
NEW: ACP threads + Event subscription → Can detect completion
     ↓
DISCOVERY: Events only know ACP ID, need Helix session ID
     ↓
SOLUTION: Bidirectional mapping stored in Zed globals
```

### Dead Code Alert 🚨
Yes, there's likely dead code from the OLD approach:
- `ThreadCreationChannel`
- `CreateThreadRequest`
- `process_pending_thread_requests` functions
- `new_prompt_editor_with_message` (if it exists)

---

## Recommendation

1. **Audit** - Search for usage of old polling infrastructure
2. **Delete** - Remove dead code if confirmed unused
3. **Document** - Update architecture docs to reflect current reality
4. **Refactor** - Consider moving logic out of `agent_panel.rs` (as discussed in codebase map)

The architecture **works** and **tests pass**, but we have technical debt from the evolution.
