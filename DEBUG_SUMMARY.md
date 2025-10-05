# Debugging Summary - Comprehensive Logging Added

## Changes Made:

### 1. Fixed WebSocket URL  
- Updated `../helix/zed-config/settings.json`
- Changed from `localhost:8080` to `localhost:8080/api/v1/external-agents/sync`

### 2. Added Comprehensive Logging

**websocket_sync.rs**:
- `init_websocket_service()` - Thread spawn, Tokio runtime creation
- `WebSocketSync::start()` - URL parsing, connection attempt, stream split
- Outgoing task - Every event send attempt
- Incoming task - Every message received
- `handle_incoming_message()` - JSON parsing, command type
- Every step shows success (✅) or failure (❌)

**external_websocket_sync.rs**:
- `request_thread_creation()` - Callback lookup and channel send
- `init_thread_creation_callback()` - Global registration

**zed.rs**:
- Workspace creation setup
- Settings check
- WebSocket service startup

### 3. Created Automated Test Script
`../helix/test-zed-websocket-integration.sh` - Full end-to-end test

## Log Markers To Watch For:

### Startup Sequence (should see ALL of these):
```
🔧 [ZED] Setting up WebSocket integration...
✅ [ZED] WebSocket thread handler initialized
🔧 [ZED] Checking WebSocket settings...
🔧 [ZED] Settings: enabled=true, websocket.enabled=true, url=...
🔌 [ZED] WebSocket sync ENABLED - starting service  
🔌 [ZED] Calling init_websocket_service()...
🔧 [WEBSOCKET] init_websocket_service() called with URL: ...
✅ [WEBSOCKET] WebSocket thread spawned
🧵 [WEBSOCKET] Spawned dedicated thread for WebSocket
✅ [WEBSOCKET] Created Tokio runtime
🔌 [WEBSOCKET] Starting WebSocket service with Tokio runtime
🔗 [WEBSOCKET] Attempting connection to ws://...
✅ [WEBSOCKET] WebSocket connected! Response status: ...
✅ [WEBSOCKET] Outgoing task spawned
📥 [WEBSOCKET-IN] Incoming task started, waiting for messages
✅ [WEBSOCKET] WebSocketSync fully initialized
🔧 [CALLBACK] init_thread_creation_callback() called
✅ [CALLBACK] Global thread creation callback registered
🔧 [THREAD_SERVICE] Handler task started, waiting for requests...
```

### When Message Received:
```
📥 [WEBSOCKET-IN] Received WebSocket message
📥 [WEBSOCKET-IN] Received text: {"type":"chat_message",...}
🔧 [WEBSOCKET-IN] handle_incoming_message() called
✅ [WEBSOCKET-IN] Parsed command type: chat_message
💬 [WEBSOCKET-IN] Processing chat_message: ...
🎯 [WEBSOCKET-IN] Calling request_thread_creation()...
🔧 [CALLBACK] request_thread_creation() called
✅ [CALLBACK] Found global callback sender
✅ [CALLBACK] Request sent to callback channel
✅ [WEBSOCKET-IN] request_thread_creation() succeeded
📨 [THREAD_SERVICE] Received thread creation request
🆕 [THREAD_SERVICE] Creating new ACP thread
```

## Known Issues to Fix:

1. **Settings might not load if container doesn't mount config**
2. **WebSocket URL must include full path** `/api/v1/external-agents/sync`
3. **Initialization order critical**: setup_thread_handler() BEFORE init_websocket_service()

## Next Test:

1. Launch new Zed session via Helix UI
2. Check container logs:
   ```bash
   docker logs <container-name> 2>&1 | grep -E "ZED|WEBSOCKET|THREAD_SERVICE|CALLBACK"
   ```
3. Send message from Helix
4. Watch logs for message flow

If you don't see the startup logs, settings aren't loaded or feature isn't enabled.
If WebSocket doesn't connect, check URL and Helix endpoint.
If callback fails, check initialization order.

