# ⚠️ OUT OF DATE - DO NOT USE ⚠️

**THIS DOCUMENT IS OUTDATED AND KEPT FOR HISTORICAL REFERENCE ONLY**

**See the authoritative spec at:** `/home/luke/pm/zed/WEBSOCKET_PROTOCOL_SPEC.md`

---

# WebSocket Remote Control Design - Generic Zed Integration

## Design Principle

**Goal:** Make Zed's WebSocket integration completely generic and reusable for ANY remote control project, not just Helix.

**Key Constraint:** Zed should have ZERO knowledge of Helix. All Helix-specific logic should live in Helix.

---

## Current Problem

### What We Have Now (WRONG)

**In Zed:**
```rust
// ❌ Helix-specific globals in Zed
pub struct ExternalSessionMapping {
    pub sessions: Arc<RwLock<HashMap<String, ContextId>>>,  // Helix ID → ACP ID
}

pub struct ContextToHelixSessionMapping {
    pub contexts: Arc<RwLock<HashMap<String, String>>>,  // ACP ID → Helix ID
}

// ❌ Helix-specific response logic in Zed
if let AcpThreadEvent::Stopped = event {
    let helix_session_id = reverse_mapping.get(&acp_context_id);  // COUPLING!
    send_to_helix(helix_session_id, response);
}
```

**Problems:**
1. Zed knows about "Helix sessions" - tight coupling
2. Zed maintains mapping state - not Zed's responsibility
3. Can't reuse for other remote control projects
4. Violates separation of concerns

---

## Proposed Solution

### Core Insight

**The client (Helix) should track which ACP thread IDs belong to which sessions.**

When Helix receives a response from Zed, the response includes the ACP thread/context ID. Helix looks it up in its own database to know which session it belongs to.

---

## Generic WebSocket Protocol

### Messages FROM Client to Zed

#### 1. Create Thread
```json
{
    "type": "create_thread",
    "message": "Hello, how are you?",
    "metadata": {
        "any_client_data": "client can put anything here",
        "not_interpreted_by_zed": true
    }
}
```

**Zed's Response:**
```json
{
    "type": "thread_created",
    "acp_thread_id": "550e8400-e29b-41d4-a716-446655440000",
    "acp_session_id": "abc123def456"  // ACP's session identifier
}
```

**Client's Responsibility:**
- Store mapping: `my_session_id` → `acp_thread_id` / `acp_session_id`
- Track this in client's database
- Use for all future lookups

---

#### 2. Send Message to Existing Thread
```json
{
    "type": "send_message",
    "acp_thread_id": "550e8400-e29b-41d4-a716-446655440000",
    "message": "What's the weather?"
}
```

**Client knows** which ACP thread to send to (from its own mapping).

---

### Messages FROM Zed to Client

#### 1. Thread Created Confirmation
```json
{
    "type": "thread_created",
    "acp_thread_id": "550e8400-e29b-41d4-a716-446655440000",
    "acp_session_id": "abc123def456"
}
```

---

#### 2. Message Completed (AI Response)
```json
{
    "type": "message_completed",
    "acp_thread_id": "550e8400-e29b-41d4-a716-446655440000",
    "acp_session_id": "abc123def456",
    "content": "The AI's response text here..."
}
```

**Client's Responsibility:**
- Look up which client session corresponds to `acp_thread_id`
- Route response to correct user/session
- All mapping logic stays in client

---

#### 3. Thread Updated
```json
{
    "type": "thread_updated",
    "acp_thread_id": "550e8400-e29b-41d4-a716-446655440000",
    "entry_index": 5
}
```

---

#### 4. Error
```json
{
    "type": "error",
    "error": "Thread not found",
    "acp_thread_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

---

## Zed Implementation (Generic)

### What Zed Needs to Do

**On WebSocket Message Received:**
1. Parse message type
2. If `create_thread`: Create new ACP thread
3. Return `acp_thread_id` and `acp_session_id` to client
4. Subscribe to thread events
5. **IGNORE** any client metadata - don't store it, don't interpret it

**On ACP Thread Event:**
1. Detect `AcpThreadEvent::Stopped`
2. Extract AI response
3. Send WebSocket message with:
   - `acp_thread_id` - Client can look this up
   - `acp_session_id` - Client can look this up
   - `content` - The actual response

**What Zed Does NOT Do:**
- ❌ Store any client session IDs
- ❌ Maintain any client → ACP mappings
- ❌ Know about "Helix" or any specific client
- ❌ Route based on client session IDs

---

## Helix Implementation

### What Helix Needs to Do

**Database Schema:**
```sql
CREATE TABLE external_agent_sessions (
    helix_session_id UUID PRIMARY KEY,
    acp_thread_id UUID NOT NULL,
    acp_session_id VARCHAR(255),
    created_at TIMESTAMP,
    last_activity TIMESTAMP
);

CREATE INDEX idx_acp_thread_id ON external_agent_sessions(acp_thread_id);
CREATE INDEX idx_acp_session_id ON external_agent_sessions(acp_session_id);
```

**On User Creates Session:**
1. Send `create_thread` message to Zed via WebSocket
2. Receive `thread_created` response with `acp_thread_id` and `acp_session_id`
3. Store in database:
   ```sql
   INSERT INTO external_agent_sessions
   (helix_session_id, acp_thread_id, acp_session_id)
   VALUES ($1, $2, $3);
   ```

**On Receiving Response from Zed:**
1. Parse WebSocket message
2. Extract `acp_thread_id` or `acp_session_id`
3. Look up in database:
   ```sql
   SELECT helix_session_id
   FROM external_agent_sessions
   WHERE acp_thread_id = $1 OR acp_session_id = $2;
   ```
4. Route response to correct Helix session
5. All routing logic in Helix!

---

## Benefits of This Design

### 1. Generic and Reusable
- Any project can use Zed's WebSocket remote control
- No Helix-specific code in Zed
- Easy to document and maintain

### 2. Separation of Concerns
- Zed: Creates threads, fires events, sends ACP IDs
- Client: Maps ACP IDs to client sessions, routes responses
- Clear boundaries

### 3. Minimal Zed Changes
- Remove: `ExternalSessionMapping`, `ContextToHelixSessionMapping`
- Add: Generic WebSocket message handlers
- Keep: ACP thread creation and event subscription
- **Much smaller change surface**

### 4. Stateless Zed
- Zed doesn't store client session state
- Zed can restart without losing client mappings
- Client maintains authoritative session state

### 5. Scalability
- Client database can handle complex session logic
- Zed stays simple and focused
- Easy to add multiple clients

---

## Implementation Changes Needed

### In Zed: REMOVE

```rust
// ❌ DELETE - Helix-specific
pub struct ExternalSessionMapping { ... }
pub struct ContextToHelixSessionMapping { ... }

// ❌ DELETE - Helix-specific parameter
fn new_acp_thread_with_message(
    &mut self,
    initial_message: &str,
    helix_session_id: String,  // ❌ DELETE THIS
    window: &mut Window,
    cx: &mut Context<Self>,
)
```

### In Zed: CHANGE TO

```rust
// ✅ Generic WebSocket handler
fn handle_websocket_message(
    &mut self,
    message: WebSocketMessage,
    window: &mut Window,
    cx: &mut Context<Self>,
) {
    match message.message_type.as_str() {
        "create_thread" => {
            let thread_view = cx.new(|cx| {
                AcpThreadView::new(...)
            });

            // Get ACP identifiers
            let (acp_thread_id, acp_session_id) = extract_ids(thread_view, cx);

            // Send back to client (client stores the mapping!)
            send_response(WebSocketResponse {
                type: "thread_created",
                acp_thread_id,
                acp_session_id,
            });

            // Subscribe to events
            cx.subscribe(thread, move |_, thread, event, cx| {
                if let AcpThreadEvent::Stopped = event {
                    let response = extract_response(thread, cx);

                    // Send with ACP IDs - client does the mapping!
                    send_response(WebSocketResponse {
                        type: "message_completed",
                        acp_thread_id,      // ✅ Client knows this
                        acp_session_id,     // ✅ Client knows this
                        content: response,
                    });
                }
            });
        }

        "send_message" => {
            // Look up by ACP thread ID (Zed knows its own threads)
            let thread = find_thread_by_acp_id(message.acp_thread_id);
            thread.send_message(message.content);
        }

        _ => { /* unknown message type */ }
    }
}
```

### In Helix: ADD

```go
// Helix API - Store mapping when thread created
func (s *ExternalAgentService) CreateSession(ctx context.Context, helixSessionID string) error {
    // Send WebSocket message to Zed
    response := sendToZed(WebSocketMessage{
        Type: "create_thread",
        Message: "...",
    })

    // Store mapping in Helix database
    _, err := s.db.Exec(ctx,
        "INSERT INTO external_agent_sessions (helix_session_id, acp_thread_id, acp_session_id) VALUES ($1, $2, $3)",
        helixSessionID,
        response.AcpThreadID,
        response.AcpSessionID,
    )

    return err
}

// Helix API - Route response when received from Zed
func (s *ExternalAgentService) HandleZedResponse(response WebSocketMessage) error {
    // Look up which Helix session this belongs to
    var helixSessionID string
    err := s.db.QueryRow(ctx,
        "SELECT helix_session_id FROM external_agent_sessions WHERE acp_thread_id = $1",
        response.AcpThreadID,
    ).Scan(&helixSessionID)

    // Route to correct Helix session
    return s.routeToSession(helixSessionID, response.Content)
}
```

---

## Migration Path

### Phase 1: Update Zed (Make Generic)
1. Remove `ExternalSessionMapping` and `ContextToHelixSessionMapping`
2. Change `new_acp_thread_with_message` to not take Helix session ID
3. Update WebSocket responses to include `acp_thread_id` and `acp_session_id`
4. Remove all Helix-specific code

### Phase 2: Update Helix (Add Mapping)
1. Add database table for session mappings
2. Store mapping when receiving `thread_created` from Zed
3. Look up mapping when receiving `message_completed` from Zed
4. Update routing logic to use database

### Phase 3: Test
1. Verify end-to-end flow works
2. Verify Helix correctly routes responses
3. Verify no Helix knowledge remains in Zed

### Phase 4: Document
1. Update API documentation with generic protocol
2. Provide example client implementations
3. Mark as reusable for other projects

---

## Example: Other Clients

### VSCode Extension
```typescript
// VSCode extension using same protocol
class ZedRemoteControl {
    private mappings = new Map<string, string>(); // vscode session → acp thread

    async createThread(message: string, vscodeSessionId: string) {
        const response = await this.sendToZed({
            type: "create_thread",
            message: message
        });

        // Store mapping locally
        this.mappings.set(vscodeSessionId, response.acp_thread_id);
    }

    handleZedResponse(response: any) {
        // Find which VSCode session this belongs to
        for (let [vscodeSession, acpThread] of this.mappings) {
            if (acpThread === response.acp_thread_id) {
                this.routeToVSCode(vscodeSession, response.content);
                return;
            }
        }
    }
}
```

### Slack Bot
```python
# Slack bot using same protocol
class ZedSlackBot:
    def __init__(self):
        self.mappings = {}  # slack_thread_id → acp_thread_id

    async def handle_slack_message(self, slack_thread_id, message):
        response = await send_to_zed({
            "type": "create_thread",
            "message": message
        })

        # Store mapping in Redis/database
        self.mappings[slack_thread_id] = response["acp_thread_id"]

    async def handle_zed_response(self, response):
        # Look up which Slack thread
        for slack_id, acp_id in self.mappings.items():
            if acp_id == response["acp_thread_id"]:
                await post_to_slack(slack_id, response["content"])
```

**Both work with zero Zed changes!**

---

## Technical Details

### ACP Thread/Session IDs

**Question:** What's the difference between `acp_thread_id` and `acp_session_id`?

**Answer:**
- `acp_thread_id` - Entity ID of the AcpThreadView
- `acp_session_id` - Session ID from AcpThread (the actual conversation)
- Both are available in Zed and can be returned to client
- Client can use either (or both) for lookup

**Recommendation:**
- Return both in responses
- Client decides which to use for mapping
- Provides flexibility for different client architectures

---

## Why This is Better

### Current Approach
```
Client sends: helix_session_id
Zed stores:   helix_session_id → acp_id (in memory)
Zed stores:   acp_id → helix_session_id (in memory)
Zed uses:     helix_session_id when sending response
```
**Problem:** Zed coupled to client's session model

### New Approach
```
Client sends: (nothing - just message)
Zed returns:  acp_thread_id, acp_session_id
Client stores: client_session_id → acp_thread_id (in database)
Client uses:   acp_thread_id to look up client_session_id
```
**Benefit:** Zed doesn't know or care about client sessions

---

## Compatibility

### Breaking Changes
- Client must implement mapping logic
- Client must handle `thread_created` response
- Client must look up sessions from `acp_thread_id`

### Migration Support
Could provide compatibility layer:
```rust
// Temporary compatibility - deprecated
fn new_acp_thread_with_message_legacy(
    helix_session_id: String,  // Deprecated!
    ...
) {
    // Warn that this is deprecated
    log::warn!("Using deprecated API with client session tracking");

    // Call new generic version
    handle_websocket_message(...)
}
```

---

## Documentation Required

1. **Generic Protocol Spec** - WebSocket message formats
2. **Client Integration Guide** - How to implement mapping
3. **Example Clients** - Reference implementations
4. **Migration Guide** - How to update existing Helix integration

---

## Decision

**Recommendation:** Implement the generic approach

**Rationale:**
1. ✅ Removes Helix coupling from Zed
2. ✅ Makes Zed reusable for other projects
3. ✅ Moves complexity to the right place (client controls its sessions)
4. ✅ Simpler Zed implementation (stateless)
5. ✅ Better separation of concerns
6. ✅ Client can use proper database for mapping (not in-memory)

**Trade-off:**
- Requires Helix to implement database mapping
- But this is the RIGHT place for it!

---

## Next Steps

1. **Review this design** - Does it meet your requirements?
2. **Update Zed code** - Remove Helix-specific parts
3. **Update Helix code** - Add mapping table and logic
4. **Update tests** - Test new generic protocol
5. **Document** - Write generic protocol specification

**Goal:** Zed should have ZERO mention of "Helix" in its WebSocket code!
