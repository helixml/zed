# Dead Code Cleanup - ACP Integration

## Summary

**Yes, you were right!** There IS dead code left over from the evolution of the implementation.

---

## What You Remember vs. What Happened

### What You Likely Said
> "Let's handle the Helix session ↔ Zed mapping on the Helix side only"

### What Actually Happened
We **tried** that, but discovered we **need** the mapping in Zed for async event handling. However, we left the OLD implementation around as dead code.

---

## Dead Code Identified

### In `agent_panel.rs`

#### Function: `process_websocket_thread_requests_with_window` (line 450)
**Status:** ⚠️ DEAD - Called from line 2899 but uses OLD polling approach

**What it does:**
- Polls for pending WebSocket thread creation requests
- Creates threads using OLD TextThread/AssistantContext approach
- Stores session mapping

**Why it's dead:**
- We replaced this with `new_acp_thread_with_message` which creates ACP threads directly
- The NEW approach doesn't use the request queue pattern

---

#### Function: `process_websocket_thread_requests_deferred` (line 487)
**Status:** 💀 COMPLETELY DEAD - Marked in compiler warnings as unused

**What it does:**
- Same as above but for deferred processing

**Why it's dead:**
- Never called anywhere
- Compiler warns about it being dead code

---

### In `external_websocket_sync` crate

#### Struct: `CreateThreadRequest`
**File:** `external_websocket_sync/src/external_websocket_sync.rs` (lines 46-53)
**Status:** ⚠️ POTENTIALLY DEAD

```rust
pub struct CreateThreadRequest {
    pub external_session_id: String,
    pub zed_context_id: Option<String>,
    pub message: String,
    pub request_id: String,
}
```

**Usage:** Only used by `process_pending_thread_requests` which is called by dead functions

---

#### Struct: `ThreadCreationChannel`
**File:** `external_websocket_sync/src/external_websocket_sync.rs` (lines 96-100)
**Status:** ⚠️ POTENTIALLY DEAD

```rust
pub struct ThreadCreationChannel {
    pub sender: mpsc::UnboundedSender<CreateThreadRequest>,
}
```

**Usage:** Channel for queuing thread creation requests (OLD polling pattern)

---

#### Function: `process_pending_thread_requests`
**File:** `external_websocket_sync/src/external_websocket_sync.rs` (needs verification)
**Status:** ⚠️ DEAD - Called only by dead functions in agent_panel.rs

---

## Why We Have Both Old and New

### The Evolution

**Phase 1: OLD Approach (Still in code)**
```
WebSocket message arrives
    ↓
Stored in ThreadCreationChannel queue
    ↓
UI polls via process_pending_thread_requests
    ↓
Creates TextThread/AssistantContext
    ↓
❌ PROBLEM: No way to detect when AI completes!
```

**Phase 2: NEW Approach (What actually runs)**
```
WebSocket message arrives
    ↓
Directly calls new_acp_thread_with_message
    ↓
Creates ACP thread with event subscription
    ↓
Fires AcpThreadEvent::Stopped when done
    ↓
✅ Can extract response and send back to Helix!
```

### Why Both Exist

During rapid iteration last night (8pm-9pm), we:
1. Implemented NEW approach (ACP threads)
2. Got it working
3. Wrote comprehensive tests
4. **FORGOT to delete OLD approach**

The dead functions are still being called from one place (line 2899) but they find no pending requests because nothing uses the queue anymore!

---

## The Mapping Question

### You Were Right About
❌ Having duplicate/redundant mapping storage

### You Were Wrong About (Or We Discovered)
We **do** need the mapping in Zed because:

**The Async Event Problem:**
```rust
// This fires LATER, asynchronously
cx.subscribe(thread, move |_, thread, event, cx| {
    if let AcpThreadEvent::Stopped = event {
        // We're here with NO context about which Helix session this is!
        // We only know the ACP thread/context ID
        // We MUST look it up to know where to send the response

        let helix_session = mapping.get(thread.session_id());  // NEED THIS!
        send_to_helix(helix_session, response);
    }
});
```

**Why we can't "just ask Helix":**
- Event handler is async, fires seconds later
- Can't make blocking calls to Helix database
- Can't make HTTP requests from event handler
- Need instant in-memory lookup

---

## What Should Be Cleaned Up

### Immediate Cleanup (Safe)

1. **Delete** `process_websocket_thread_requests_deferred` - Completely unused
2. **Delete** `process_websocket_thread_requests_with_window` - Uses old pattern
3. **Remove** call to it from line 2899

### Investigate Then Clean

4. **Check** if `CreateThreadRequest` is used elsewhere
5. **Check** if `ThreadCreationChannel` is used elsewhere
6. **Check** if `process_pending_thread_requests` is used by anything else
7. **Delete** if confirmed unused

### Consider (Bigger Refactor)

8. Move business logic out of `agent_panel.rs` into separate layer (per codebase map)
9. Consolidate session mapping globals (two structs doing similar things)

---

## Verification Commands

```bash
# Check CreateThreadRequest usage
grep -r "CreateThreadRequest" /home/luke/pm/zed/crates/ --include="*.rs" | grep -v test

# Check ThreadCreationChannel usage
grep -r "ThreadCreationChannel" /home/luke/pm/zed/crates/ --include="*.rs" | grep -v test

# Check process_pending_thread_requests usage
grep -r "process_pending_thread_requests" /home/luke/pm/zed/crates/ --include="*.rs" | grep -v test

# Check if new_acp_thread_with_message is what actually runs
grep -r "new_acp_thread_with_message" /home/luke/pm/zed/crates/ --include="*.rs" | grep -v test
```

---

## Recommendation

**Short Term:**
1. Delete confirmed dead code (functions marked as unused)
2. Keep the session mapping globals (they're needed for async events)
3. Document why we need mapping in Zed

**Long Term:**
1. Clean up remaining old infrastructure after verification
2. Refactor per codebase map (move logic out of UI layer)
3. Consolidate mapping structures if possible

---

## Bottom Line

**You were right:**
- ✅ There IS dead code from the old approach
- ✅ We should clean it up

**But also:**
- The bidirectional mapping in Zed IS necessary (not dead)
- It's there because of async event handling requirements
- We couldn't do it "only on Helix side" due to technical constraints

The implementation **works** but yes, needs cleanup!
