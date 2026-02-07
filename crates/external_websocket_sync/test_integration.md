# Helix Integration Testing Guide

This guide provides step-by-step instructions for testing the Helix integration in Zed.

## Prerequisites

1. **Build Zed with Helix Integration**
   ```bash
   # From the zed repository root
   cargo build --release
   ```

2. **Check Integration is Enabled**
   - The integration should be included automatically since it's in the workspace
   - Look for "Initializing Helix integration module" in Zed logs

## Test 1: Basic Integration Initialization

### Step 1: Enable Debug Logging
```bash
RUST_LOG=helix_integration=debug,workspace=debug ./target/release/zed
```

### Expected Log Output
```
[INFO  helix_integration] Initializing Helix integration module
[INFO  helix_integration] Helix integration module ready, waiting for assistant context initialization
```

### Step 2: Open a Project
1. Open Zed
2. Open any folder/project (File → Open... or Cmd+O)
3. Check logs for:
```
[INFO  helix_integration] Initializing Helix integration with project
[INFO  helix_integration] Starting Helix integration service
```

## Test 2: Settings Integration

### Step 1: Configure Helix Integration
Edit your Zed settings file (`~/.config/zed/settings.json`):

```json
{
  "helix_integration": {
    "enabled": true,
    "server": {
      "enabled": true,
      "host": "127.0.0.1",
      "port": 3030
    },
    "websocket_sync": {
      "enabled": true,
      "helix_url": "localhost:8080",
      "use_tls": false
    }
  }
}
```

### Step 2: Restart Zed and Check Logs
Expected output:
```
[INFO  helix_integration] Starting Helix integration service
[INFO  helix_integration] HTTP server started on 127.0.0.1:3030
```

## Test 3: HTTP Server Testing

### Prerequisites
- Helix integration enabled with server settings as above
- Zed running with a project open

### Step 1: Health Check
```bash
curl http://localhost:3030/health
```

Expected response:
```json
{
  "status": "ok",
  "version": "0.1.0",
  "session_id": "zed-session",
  "uptime_seconds": 123,
  "active_contexts": 0,
  "sync_clients": 0
}
```

### Step 2: Session Information
```bash
curl http://localhost:3030/api/v1/session
```

Expected response:
```json
{
  "session_id": "zed-session",
  "last_session_id": null,
  "active_contexts": 0,
  "websocket_connected": false,
  "sync_clients": 0
}
```

### Step 3: List Contexts
```bash
curl http://localhost:3030/api/v1/contexts
```

Expected response:
```json
[
  {
    "id": "example-context",
    "title": "Example Context",
    "message_count": 0,
    "last_message_at": "2024-01-01T00:00:00Z",
    "status": "active"
  }
]
```

### Step 4: Create Context
```bash
curl -X POST http://localhost:3030/api/v1/contexts \
  -H "Content-Type: application/json" \
  -d '{"title": "Test Context"}'
```

Expected response:
```json
{
  "context_id": "uuid-string-here",
  "title": "Test Context",
  "created_at": "2024-01-01T00:00:00Z"
}
```

### Step 5: Add Message to Context
```bash
curl -X POST http://localhost:3030/api/v1/contexts/CONTEXT_ID/messages \
  -H "Content-Type: application/json" \
  -d '{
    "content": "Hello from test!",
    "role": "user"
  }'
```

Expected response:
```json
{
  "message_id": 1234567890,
  "context_id": "CONTEXT_ID",
  "created_at": "2024-01-01T00:00:00Z"
}
```

## Test 4: WebSocket Connection Testing

### Using WebSocket Client (wscat)
If you have `wscat` installed:

```bash
# Install wscat if needed
npm install -g wscat

# Connect to WebSocket endpoint
wscat -c ws://localhost:3030/api/v1/ws
```

### Using Browser Console
Open your browser's developer console and run:

```javascript
const ws = new WebSocket('ws://localhost:3030/api/v1/ws');

ws.onopen = () => {
  console.log('Connected to Zed Helix integration');
};

ws.onmessage = (event) => {
  console.log('Received:', JSON.parse(event.data));
};

ws.onerror = (error) => {
  console.error('WebSocket error:', error);
};

// Test sending a message
ws.send(JSON.stringify({
  type: 'add_message',
  data: {
    context_id: 'test-context',
    content: 'Hello from WebSocket!',
    role: 'user'
  }
}));
```

Expected behavior:
- Connection should establish successfully
- Messages should be logged on both client and server

## Test 5: Environment Variable Override

### Step 1: Test with Environment Variables
```bash
export ZED_HELIX_INTEGRATION_ENABLED=1
export ZED_HELIX_WEBSOCKET_ENABLED=1
export ZED_HELIX_URL=localhost:9999
export ZED_HELIX_AUTH_TOKEN=test-token

RUST_LOG=helix_integration=debug ./target/release/zed
```

### Expected Behavior
- Integration should start even without settings file configuration
- Should attempt to connect to localhost:9999 instead of default

## Test 6: Error Handling

### Test Invalid Server Port
1. Set server port to an already occupied port in settings
2. Restart Zed
3. Check logs for proper error handling:
```
[ERROR helix_integration] Failed to start HTTP server: Address already in use
```

### Test Invalid WebSocket URL
1. Configure invalid Helix URL in settings
2. Restart Zed
3. Check logs for connection errors:
```
[ERROR helix_integration] Failed to initialize WebSocket sync: Invalid URL
```

## Test 7: Integration with Zed Assistant

### Prerequisites
- Zed built with assistant features enabled
- Project with some code files open

### Step 1: Open Assistant Panel
1. Open Zed
2. Use Cmd+Shift+A (or View → Assistant) to open assistant
3. Check logs for integration activity

### Step 2: Create Assistant Conversation
1. Start a new conversation in the assistant
2. Check logs for context creation:
```
[INFO  helix_integration] Created new conversation context: uuid (Title)
```

### Step 3: Add Messages
1. Send messages in the assistant
2. Check logs for message tracking:
```
[INFO  helix_integration] HTTP API: Adding message to context uuid: content (role: user)
```

## Troubleshooting

### Common Issues

1. **Integration Not Starting**
   - Check that helix_integration is in Cargo.toml workspace members
   - Verify Zed was built with the integration included
   - Check for initialization errors in logs

2. **HTTP Server Not Accessible**
   - Verify port is not in use: `netstat -an | grep 3030`
   - Check firewall settings
   - Ensure correct host/port in settings

3. **WebSocket Connection Fails**
   - Check that Helix server is actually running
   - Verify URL format and port
   - Check for authentication issues

4. **No Context Creation**
   - Ensure assistant system is properly initialized
   - Check project is loaded correctly
   - Verify context store initialization in logs

### Debug Commands

```bash
# Check if integration is loaded
ps aux | grep zed
lsof -i :3030  # Check if server is listening

# Monitor logs in real-time
tail -f ~/.local/state/zed/logs/Zed.log | grep helix

# Test network connectivity
curl -I http://localhost:3030/health
nmap -p 3030 localhost
```

## Success Criteria

The integration is working correctly if:

1. ✅ Zed starts without errors with integration enabled
2. ✅ HTTP server starts and responds to health check
3. ✅ Settings are loaded and applied correctly  
4. ✅ WebSocket endpoint accepts connections
5. ✅ API endpoints return expected JSON responses
6. ✅ Context creation works (even if placeholder)
7. ✅ Error handling works gracefully
8. ✅ Integration initializes when projects are opened

## Next Steps

Once basic testing passes:

1. **Mock Helix Server**: Create a simple mock server for full testing
2. **Load Testing**: Test with multiple concurrent connections
3. **Persistence Testing**: Test context/message persistence
4. **Real Helix Integration**: Connect to actual Helix instance
5. **User Acceptance Testing**: Test real workflows

## Mock Helix Server

For complete testing, create a simple mock server:

```javascript
// mock-helix-server.js
const WebSocket = require('ws');
const express = require('express');

const app = express();
app.use(express.json());

// HTTP endpoints
app.get('/api/v1/external-agents/sync', (req, res) => {
  res.json({ status: 'ok' });
});

// WebSocket server
const wss = new WebSocket.Server({ port: 8080 });

wss.on('connection', (ws) => {
  console.log('Zed connected');
  
  ws.on('message', (data) => {
    const message = JSON.parse(data);
    console.log('Received from Zed:', message);
    
    // Echo back or send test commands
    ws.send(JSON.stringify({
      type: 'add_message',
      data: {
        context_id: 'test',
        content: 'Hello from mock Helix!',
        role: 'assistant'
      }
    }));
  });
});

app.listen(8081, () => {
  console.log('Mock Helix server running on port 8081');
  console.log('WebSocket server running on port 8080');
});
```

Run with: `node mock-helix-server.js`
