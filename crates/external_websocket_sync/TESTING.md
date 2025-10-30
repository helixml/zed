# Helix Integration Testing Guide

This comprehensive guide provides step-by-step instructions for testing the Helix integration in Zed Editor.

## Quick Start Testing

### 1. Build Zed with Helix Integration

```bash
# From the zed repository root
cargo build --release

# Or for development
cargo build
```

### 2. Configure Helix Integration

Create or edit `~/.config/zed/settings.json`:

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
      "use_tls": false,
      "auto_reconnect": true
    }
  }
}
```

### 3. Start Zed with Debug Logging

```bash
RUST_LOG=helix_integration=debug,workspace=debug ./target/release/zed
```

### 4. Verify Integration Started

Look for these log messages:
```
[INFO  helix_integration] Initializing Helix integration module
[INFO  helix_integration] Helix integration module ready, waiting for assistant context initialization
```

### 5. Open a Project

1. File → Open... (or Cmd+O on macOS)
2. Select any folder
3. Look for:
```
[INFO  helix_integration] Initializing Helix integration with project
[INFO  helix_integration] Starting Helix integration service
[INFO  helix_integration] HTTP server started on 127.0.0.1:3030
```

### 6. Test HTTP API

```bash
# Test health endpoint
curl http://localhost:3030/health

# Expected response:
{
  "status": "ok",
  "version": "0.1.0",
  "session_id": "zed-session",
  "uptime_seconds": 123,
  "active_contexts": 0,
  "sync_clients": 0
}
```

## Detailed Testing Scenarios

### Test 1: Basic Integration Functionality

**Goal**: Verify the integration initializes and HTTP server starts correctly.

**Steps**:
1. Start Zed with debug logging
2. Open any project folder
3. Check logs for successful initialization
4. Test health endpoint: `curl http://localhost:3030/health`
5. Test session endpoint: `curl http://localhost:3030/api/v1/session`

**Expected Results**:
- HTTP server responds on port 3030
- Health check returns status "ok"
- Session endpoint returns valid session data
- No error messages in logs

**Troubleshooting**:
- If port 3030 is occupied, change it in settings
- If no logs appear, check RUST_LOG environment variable
- If server doesn't start, check settings.json syntax

### Test 2: Context Management API

**Goal**: Test context creation, listing, and management.

**Setup**:
```bash
# Set base URL for convenience
BASE_URL="http://localhost:3030/api/v1"
```

**Steps**:

1. **List existing contexts**:
```bash
curl $BASE_URL/contexts
```

2. **Create a new context**:
```bash
curl -X POST $BASE_URL/contexts \
  -H "Content-Type: application/json" \
  -d '{"title": "Test Context", "initial_message": "Hello world"}'
```

3. **Get context details** (use ID from step 2):
```bash
curl $BASE_URL/contexts/YOUR_CONTEXT_ID
```

4. **Add message to context**:
```bash
curl -X POST $BASE_URL/contexts/YOUR_CONTEXT_ID/messages \
  -H "Content-Type: application/json" \
  -d '{"content": "Test message", "role": "user"}'
```

5. **Get context messages**:
```bash
curl $BASE_URL/contexts/YOUR_CONTEXT_ID/messages
```

**Expected Results**:
- Context creation returns valid UUID and timestamp
- Context listing includes newly created context
- Message addition returns message ID
- All endpoints return proper JSON responses

### Test 3: WebSocket Real-time Communication

**Goal**: Test WebSocket connection and message exchange.

**Prerequisites**:
- Install wscat: `npm install -g wscat` (or use browser console)

**Using wscat**:
```bash
# Connect to WebSocket
wscat -c ws://localhost:3030/api/v1/ws

# Send test message
{"type": "ping", "data": {}}
```

**Using Browser Console**:
```javascript
// Connect to WebSocket
const ws = new WebSocket('ws://localhost:3030/api/v1/ws');

ws.onopen = () => console.log('Connected to Zed');
ws.onmessage = (event) => console.log('Received:', JSON.parse(event.data));
ws.onerror = (error) => console.error('WebSocket error:', error);

// Send test message
ws.send(JSON.stringify({
  type: 'add_message',
  data: {
    context_id: 'test-context',
    content: 'Hello from WebSocket!',
    role: 'user'
  }
}));
```

**Expected Results**:
- WebSocket connection establishes successfully
- Server acknowledges messages in logs
- Client receives responses

### Test 4: Mock Helix Server Integration

**Goal**: Test full bidirectional communication with mock Helix server.

**Setup Mock Server**:

1. **Install dependencies**:
```bash
# In the helix_integration directory
npm init -y
npm install express ws cors uuid
```

2. **Start mock server**:
```bash
node crates/helix_integration/mock-helix-server.js
```

3. **Update Zed settings** to point to mock server:
```json
{
  "helix_integration": {
    "enabled": true,
    "websocket_sync": {
      "enabled": true,
      "helix_url": "localhost:8080",
      "auth_token": "test-token"
    }
  }
}
```

4. **Restart Zed** and open a project

**Test Steps**:

1. **Check mock server health**:
```bash
curl http://localhost:8081/health
```

2. **Create thread in mock server**:
```bash
curl -X POST http://localhost:8081/api/v1/threads \
  -H "Content-Type: application/json" \
  -d '{"title": "Test Thread"}'
```

3. **Monitor WebSocket communication** in both server logs and Zed logs

4. **Test bidirectional sync**:
   - Create context in Zed (should appear in mock server)
   - Add message via mock server API (should trigger WebSocket update to Zed)

**Expected Results**:
- WebSocket connection establishes between Zed and mock server
- Context creation in Zed triggers events in mock server
- Mock server can send commands back to Zed
- Both servers log communication activity

### Test 5: Settings and Configuration

**Goal**: Verify settings system integration and configuration loading.

**Test Different Configurations**:

1. **Minimal Configuration**:
```json
{
  "helix_integration": {
    "enabled": true
  }
}
```

2. **Full Configuration**:
```json
{
  "helix_integration": {
    "enabled": true,
    "server": {
      "enabled": true,
      "host": "0.0.0.0",
      "port": 4040,
      "enable_cors": true
    },
    "websocket_sync": {
      "enabled": true,
      "helix_url": "production.helix.example.com",
      "use_tls": true,
      "auto_reconnect": true,
      "reconnect_delay_seconds": 10
    },
    "mcp": {
      "enabled": true,
      "servers": [
        {
          "name": "filesystem",
          "command": "npx",
          "args": ["@modelcontextprotocol/server-filesystem", "/workspace"]
        }
      ]
    }
  }
}
```

3. **Environment Variable Override**:
```bash
export ZED_HELIX_INTEGRATION_ENABLED=1
export ZED_HELIX_URL=localhost:9999
./target/release/zed
```

**Expected Results**:
- Settings are loaded and applied correctly
- Environment variables override settings file
- Invalid settings are handled gracefully
- Server starts on configured port

### Test 6: Error Handling and Edge Cases

**Goal**: Verify robust error handling.

**Test Scenarios**:

1. **Port Already in Use**:
   - Start another service on port 3030
   - Start Zed
   - Should log error and continue without HTTP server

2. **Invalid WebSocket URL**:
   - Set helix_url to invalid URL
   - Should log connection error and continue

3. **Invalid JSON in API Calls**:
```bash
curl -X POST http://localhost:3030/api/v1/contexts \
  -H "Content-Type: application/json" \
  -d 'invalid json'
```

4. **Missing Context ID**:
```bash
curl http://localhost:3030/api/v1/contexts/nonexistent-id
```

**Expected Results**:
- Graceful error handling without crashes
- Meaningful error messages in logs
- Proper HTTP status codes returned
- Integration continues to function after errors

## Automated Testing

### Run Unit Tests

```bash
# Run helix_integration tests
cargo test -p helix_integration

# Run with output
cargo test -p helix_integration -- --nocapture
```

### API Test Script

Use the provided test script:

```bash
# Make executable
chmod +x crates/helix_integration/test_api.sh

# Run tests
./crates/helix_integration/test_api.sh

# Verbose output
./crates/helix_integration/test_api.sh --verbose

# Custom URL
./crates/helix_integration/test_api.sh --url http://localhost:4040
```

### Performance Testing

**Simple Load Test**:
```bash
# Test 100 concurrent requests
for i in {1..100}; do
  curl -s http://localhost:3030/health &
done
wait
```

**WebSocket Load Test**:
```javascript
// Browser console - test multiple connections
const connections = [];
for (let i = 0; i < 10; i++) {
  const ws = new WebSocket('ws://localhost:3030/api/v1/ws');
  ws.onopen = () => console.log(`Connection ${i} opened`);
  connections.push(ws);
}
```

## Integration with Zed Assistant

### Prerequisites
- Zed built with assistant features
- Assistant panel available (Cmd+Shift+A or View → Assistant)

### Test Steps

1. **Open Assistant Panel**:
   - Use keyboard shortcut or menu
   - Should see assistant interface

2. **Create New Conversation**:
   - Click "New Conversation" or similar
   - Monitor logs for context creation:
   ```
   [INFO  helix_integration] Created new conversation context: uuid (title)
   ```

3. **Send Messages**:
   - Type message in assistant
   - Send message
   - Check logs for message tracking

4. **Monitor HTTP API**:
   ```bash
   # Should show new context
   curl http://localhost:3030/api/v1/contexts
   
   # Should show messages
   curl http://localhost:3030/api/v1/contexts/CONTEXT_ID/messages
   ```

**Expected Results**:
- Assistant context creation triggers integration events
- Messages in assistant are tracked by integration
- HTTP API reflects assistant state

## Troubleshooting Guide

### Common Issues

**1. Integration Not Starting**
```
Symptoms: No helix_integration logs appear
Solutions:
- Check workspace Cargo.toml includes helix_integration
- Verify build included the crate
- Check RUST_LOG environment variable
```

**2. HTTP Server Not Starting**
```
Symptoms: Server endpoints not accessible
Solutions:
- Check if port is already in use: netstat -an | grep 3030
- Verify settings.json syntax
- Check file permissions on settings file
- Try different port in settings
```

**3. WebSocket Connection Fails**
```
Symptoms: WebSocket connection refused or timeouts
Solutions:
- Verify WebSocket URL format
- Check firewall settings
- Ensure target server is running
- Check authentication tokens
```

**4. Settings Not Loading**
```
Symptoms: Environment variables work but settings don't
Solutions:
- Check settings file path: ~/.config/zed/settings.json
- Verify JSON syntax
- Check file permissions
- Restart Zed after settings changes
```

**5. No Context Creation**
```
Symptoms: Integration starts but contexts aren't created
Solutions:
- Verify project is loaded
- Check assistant system initialization
- Ensure context store is available
- Check for context store initialization errors
```

### Debug Commands

```bash
# Check if Zed is running
ps aux | grep zed

# Check if server is listening
lsof -i :3030
netstat -an | grep 3030

# Monitor logs in real-time
tail -f ~/.local/state/zed/logs/Zed.log | grep helix

# Test network connectivity
curl -I http://localhost:3030/health
telnet localhost 3030

# Check port availability
nc -z localhost 3030
```

### Log Analysis

**Successful Startup Logs**:
```
[INFO  helix_integration] Initializing Helix integration module
[INFO  helix_integration] Storing session and prompt builder for Helix integration
[INFO  helix_integration] Initializing Helix integration with project
[INFO  helix_integration] Starting Helix integration service
[INFO  helix_integration] HTTP server started on 127.0.0.1:3030
```

**Error Patterns to Look For**:
```
[ERROR helix_integration] Failed to start HTTP server: Address already in use
[ERROR helix_integration] Failed to initialize WebSocket sync: Connection refused
[WARN  helix_integration] Session or prompt builder not available for initialization
```

## Success Criteria

The integration is working correctly when:

- ✅ Zed starts without errors with integration enabled
- ✅ HTTP server starts and responds to health check on configured port
- ✅ Settings are loaded correctly from settings.json
- ✅ WebSocket endpoint accepts connections
- ✅ All API endpoints return expected JSON responses
- ✅ Context management works (creation, listing, deletion)
- ✅ Message operations work (adding, retrieving)
- ✅ Integration initializes when new projects are opened
- ✅ Error handling is graceful and informative
- ✅ Mock Helix server can communicate bidirectionally
- ✅ Settings changes take effect after restart
- ✅ Multiple concurrent connections work correctly

## Next Steps After Testing

Once basic testing passes:

1. **Real Helix Integration**: Connect to actual Helix instance
2. **Production Testing**: Test in real development workflows
3. **Performance Optimization**: Profile and optimize for production use
4. **Feature Enhancement**: Add advanced features based on testing feedback
5. **User Documentation**: Create end-user guides based on testing experience
6. **CI/CD Integration**: Add automated tests to build pipeline

## Reporting Issues

When reporting issues, please include:

1. **Environment Information**:
   - OS and version
   - Zed version and build info
   - Rust version used for build

2. **Configuration**:
   - Full settings.json content
   - Environment variables set
   - Command line used to start Zed

3. **Steps to Reproduce**:
   - Exact steps taken
   - Expected vs actual behavior
   - Screenshots if applicable

4. **Logs**:
   - Full log output with DEBUG level
   - Any error messages
   - Network connection details

5. **Testing Context**:
   - Which test scenarios were attempted
   - Results of API test script
   - Mock server interaction logs