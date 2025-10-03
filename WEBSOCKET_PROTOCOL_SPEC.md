# External Agent WebSocket Protocol - AUTHORITATIVE SPEC

**THIS IS THE ONE TRUE PROTOCOL SPECIFICATION**

All external agents (including Helix) MUST implement this protocol exactly as specified.

**Location:** `/home/luke/pm/zed/WEBSOCKET_PROTOCOL_SPEC.md`

---

## Core Concepts

### External System Side (e.g., Helix)
- **Session**: A conversation thread (e.g., `ses_01k6jg...`)
- **Interaction**: A single user request + AI response pair within a session  
- **Interaction States**: `waiting` → `complete` or `error`

### Zed Side
- **Context**: A conversation thread with the AI assistant (e.g., `8405cd2a-24ae-...`)
- **Message**: Individual messages within a context (user or assistant)
- **Context is the source of truth** for the conversation

### Key Mapping Principle
- **One External Session ↔ One ACP Thread** (1:1 relationship)
- **Only external system maintains the mapping**: `session_id → acp_thread_id`
- **Zed is stateless** - doesn't maintain any session mapping
- **All messages include BOTH IDs** (whichever are known at the time)

---

## Flow 1: New Session - First Message

### Scenario
User creates a new session in external system, sends first message to Zed agent.

### Message Flow

**1. External System → Zed: chat_message**
```json
{
  "type": "chat_message",
  "data": {
    "helix_session_id": "ses_01k6abc...",
    "acp_thread_id": null,
    "message": "Hello, can you help me?",
    "request_id": "req_1234567890"
  }
}
```

**2. Zed Processing**
- Sees `acp_thread_id` is `null` → creates new context
- Creates ACP thread with UUID: `"8405cd2a-24ae-..."`
- **No mapping stored** (Zed is stateless)
- Adds user message to the context
- Starts AI completion

**3. Zed → External System: context_created**
```json
{
  "session_id": "ses_01k6abc...",
  "event_type": "context_created",
  "data": {
    "acp_thread_id": "8405cd2a-24ae-...",
    "helix_session_id": "ses_01k6abc..."
  }
}
```

**4. External System Processing**
- Stores mapping: `session["ses_01k6abc..."].acp_thread_id = "8405cd2a-24ae-..."`
- Does NOT create new session (already exists)
- Does NOT mark interaction complete yet

**5. Zed → External System: message_added** (streaming)
```json
{
  "session_id": "ses_01k6abc...",
  "event_type": "message_added",
  "data": {
    "acp_thread_id": "8405cd2a-24ae-...",
    "message_id": "ai_msg_1759410084",
    "role": "assistant",
    "content": "Hello! How can I",
    "timestamp": 1759410084
  }
}
```

**6. Zed → External System: message_added** (continues streaming)
```json
{
  "session_id": "ses_01k6abc...",
  "event_type": "message_added",
  "data": {
    "acp_thread_id": "8405cd2a-24ae-...",
    "message_id": "ai_msg_1759410084",
    "role": "assistant",
    "content": "Hello! How can I help you today?",
    "timestamp": 1759410085
  }
}
```

**Note**: Same `message_id` with progressively longer content = streaming

**7. Zed → External System: message_completed**
```json
{
  "session_id": "ses_01k6abc...",
  "event_type": "message_completed",
  "data": {
    "acp_thread_id": "8405cd2a-24ae-...",
    "message_id": "ai_msg_1759410084",
    "request_id": "req_1234567890"
  }
}
```

**8. External System Processing**
- Finds waiting interaction for session `"ses_01k6abc..."`
- Marks interaction as `complete`
- Response content already stored from `message_added` events

---

## Flow 2: Follow-up Message in Existing Session

### Scenario
User sends another message in same session (context already exists).

### Message Flow

**1. External System → Zed: chat_message**
```json
{
  "type": "chat_message",
  "data": {
    "helix_session_id": "ses_01k6abc...",
    "acp_thread_id": "8405cd2a-24ae-...",
    "message": "Can you explain more?",
    "request_id": "req_9876543210"
  }
}
```

**2. Zed Processing**
- Sees `acp_thread_id` is provided → uses existing context
- Finds existing context: `"8405cd2a-24ae-..."`
- Adds user message to EXISTING context
- Starts AI completion
- **Does NOT send `context_created`** (already exists)

**3. Zed → External System: message_added** (streaming)
```json
{
  "session_id": "ses_01k6abc...",
  "event_type": "message_added",
  "data": {
    "acp_thread_id": "8405cd2a-24ae-...",
    "message_id": "ai_msg_1759420000",
    "role": "assistant",
    "content": "Sure! Let me explain...",
    "timestamp": 1759420000
  }
}
```

**4. Zed → External System: message_completed**
```json
{
  "session_id": "ses_01k6abc...",
  "event_type": "message_completed",
  "data": {
    "acp_thread_id": "8405cd2a-24ae-...",
    "message_id": "ai_msg_1759420000",
    "request_id": "req_9876543210"
  }
}
```

---

## Message Types Reference

### External System → Zed

#### chat_message
Send user message (new or follow-up).

**Fields:**
- `type`: `"chat_message"`
- `data.helix_session_id`: External system's session ID (required)
- `data.acp_thread_id`: ACP thread ID if known, `null` for first message
- `data.message`: User's message text (required)
- `data.request_id`: Unique request identifier (required)

---

### Zed → External System

#### context_created
Sent ONCE when Zed creates a new context.

**Fields:**
- `session_id`: External session ID (echo back)
- `event_type`: `"context_created"`
- `data.acp_thread_id`: Zed's context UUID
- `data.helix_session_id`: External session ID (echo back)

#### message_added
Streaming AI response (sent multiple times with same `message_id`).

**Fields:**
- `session_id`: External session ID
- `event_type`: `"message_added"`
- `data.acp_thread_id`: Zed's context UUID
- `data.message_id`: Message identifier (same across streaming updates)
- `data.role`: `"assistant"`
- `data.content`: AI response text (progressively longer)
- `data.timestamp`: Unix timestamp

#### message_completed
Sent when AI finishes responding.

**Fields:**
- `session_id`: External session ID
- `event_type`: `"message_completed"`
- `data.acp_thread_id`: Zed's context UUID
- `data.message_id`: Message identifier
- `data.request_id`: Request ID from original chat_message

---

## Critical Implementation Rules

### Zed Side

1. **Stateless Design**
   - Zed does NOT store external session IDs
   - Zed does NOT maintain session-to-context mapping
   - All session routing is external system's responsibility

2. **Context Creation**
   ```rust
   if data.acp_thread_id.is_null() {
       // Create new context
       let context_id = create_new_context();
       send_context_created(session_id, context_id);
   } else {
       // Use existing context
       add_message_to_context(data.acp_thread_id, message);
   }
   ```

3. **Only Send context_created Once**
   - Only when Zed actually creates a new context
   - NOT for follow-up messages
   - Include both `acp_thread_id` and `helix_session_id`

4. **Stream with Same message_id**
   - As content arrives, send `message_added` with progressively longer content
   - Keep `message_id` constant for same assistant message
   - External system overwrites previous content

5. **Always Send message_completed**
   - After AI stops generating
   - Include `request_id` so external system knows which request finished

### External System Side

1. **Store Mapping on context_created**
   ```go
   session.ZedContextID = event.data.acp_thread_id
   UpdateSession(session)
   ```

2. **Include acp_thread_id in Subsequent Messages**
   ```go
   command := ExternalAgentCommand{
       Type: "chat_message",
       Data: {
           "helix_session_id": session.ID,
           "acp_thread_id":   session.ZedContextID,  // null on first message
           "message":          userMessage,
           "request_id":       requestID,
       },
   }
   ```

3. **Update Response on message_added**
   - Find waiting interaction for session
   - Update `interaction.response_message` with latest content
   - Keep state as `waiting` (don't mark complete yet)

4. **Mark Complete on message_completed**
   - Find waiting interaction for session
   - Mark `interaction.state = "complete"`
   - Set completion timestamp

5. **Never Mark Complete Before message_completed**

---

## Why This Design Works

### Separation of Concerns
- **External system** owns session lifecycle and routing
- **Zed** owns context/conversation content
- Clean boundary at the WebSocket protocol

### Stateless Zed
- Zed can restart without losing session mappings
- External system maintains authoritative state
- No synchronization issues

### Streaming Support
- Multiple `message_added` events build response incrementally
- External system sees real-time progress
- Only `message_completed` triggers state transition

### Simple Protocol
- Two message types in, three out
- Easy to implement in any language
- Self-documenting with explicit IDs

---

## Implementation Architecture (Zed Side)

### Problem: WebSocket Runs Without GPUI Context

**Current Issue:**
- WebSocket connection runs in `std::thread` spawned from `init()` (no GPUI context)
- Cannot call `agent_panel.new_acp_thread_with_message()` (requires Window context)
- Cannot use `cx.spawn()` or emit GPUI events
- Creates fake context IDs instead of real ACP threads

**Solution: Callback Channel Pattern**

```rust
// 1. agent_panel initialization - create callback channel
let (thread_creation_callback_tx, mut thread_creation_callback_rx) = mpsc::unbounded_channel();

// Store sender in global for websocket_sync to access
cx.set_global(ThreadCreationCallback {
    sender: thread_creation_callback_tx,
});

// 2. agent_panel spawns background task to process requests
cx.spawn(|panel, mut cx| async move {
    while let Some(request) = thread_creation_callback_rx.recv().await {
        // Execute in GPUI context with window access
        cx.update(|cx| {
            panel.update(cx, |panel, cx| {
                // Get window handle (from workspace or store it)
                let window = ...; // TODO: Need window reference

                // Create REAL ACP thread
                panel.new_acp_thread_with_message(
                    &request.message,
                    request.helix_session_id.clone(),
                    window,
                    cx
                );

                // Real context_id will be sent via ACP event subscription
            })
        }).ok();
    }
}).detach();

// 3. websocket_sync calls callback when chat_message arrives
if let Some(callback) = cx.try_global::<ThreadCreationCallback>() {
    callback.sender.send(ThreadCreationRequest {
        helix_session_id,
        message,
        request_id,
    })?;
}
```

### Open Questions

1. **Window Reference**: How does background task get Window handle?
   - Store weak reference during agent_panel creation?
   - Get from workspace?
   - Create headless window context?

2. **ACP Thread Persistence**: Do ACP threads reliably persist without UI open?
   - Concern: Threads may not save to database without panel visible
   - Need to verify thread_store works headlessly

3. **Existing Assumptions**: Does other Zed code assume agent panel is open?
   - Tool execution?
   - Message handling?
   - Event subscriptions?

### Alternative: GPUI-Free Layer

If GPUI context is too problematic, consider:
- Create standalone `AcpThreadManager` that doesn't require Window
- Uses async context only
- agent_panel just observes and renders
- More refactoring but cleaner architecture

---

## Testing Checklist

- [ ] New session creates REAL ACP thread (not fake ID)
- [ ] `context_created` sent with REAL acp_thread_id from created thread
- [ ] Follow-up message reuses existing context
- [ ] NO second `context_created` for follow-ups
- [ ] Streaming works (multiple `message_added` with same `message_id`)
- [ ] `message_completed` sent at end
- [ ] `request_id` correctly echoed back
- [ ] External session ID always preserved in responses
- [ ] Works headlessly (no UI/panel required)
- [ ] ACP threads persist to database correctly
