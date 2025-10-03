# External WebSocket Thread Sync for Zed Editor

**Status**: ✅ FULLY IMPLEMENTED AND TESTED

This crate provides a WebSocket protocol for external systems to control Zed's AI agent threads. The protocol is stateless - Zed only knows about `acp_thread_id`, and external systems maintain all session mapping.

**See**: `/WEBSOCKET_PROTOCOL_SPEC.md` for the complete authoritative specification.

This crate provides a generic system for synchronizing Zed editor conversation threads with external services via WebSocket connections, enabling real-time collaboration and integration with AI platforms and other external tools.

## Features

- **Real-time Synchronization**: Bidirectional sync of conversation contexts between Zed and Helix
- **WebSocket Communication**: Live updates and command streaming
- **HTTP API Server**: RESTful endpoints for external agent integration
- **MCP Tool Integration**: Support for Model Context Protocol tools
- **Settings Integration**: Full Zed settings system integration

## Configuration

Add the following to your Zed settings file (`~/.config/zed/settings.json`):

```json
{
  "external_websocket_sync": {
    "enabled": true,
    "websocket_sync": {
      "enabled": true,
      "external_url": "localhost:8080",
      "auth_token": "your_external_service_token",
      "use_tls": false,
      "auto_reconnect": true,
      "reconnect_delay_seconds": 5,
      "max_reconnect_attempts": 10
    },
    "server": {
      "enabled": true,
      "host": "127.0.0.1",
      "port": 3030,
      "auth_token": "your_external_api_token",
      "enable_cors": true,
      "request_timeout_seconds": 30
    },
    "mcp": {
      "enabled": true,
      "servers": [
        {
          "name": "filesystem",
          "command": "npx",
          "args": ["@modelcontextprotocol/server-filesystem", "/path/to/workspace"],
          "env": {},
          "working_directory": null,
          "auto_restart": true
        }
      ],
      "tool_call_timeout_seconds": 30
    }
  }
}
```

## API Endpoints

When the HTTP server is enabled, the following endpoints are available:

### Health Check
- `GET /health` - Service health and status

### Session Management
- `GET /api/v1/session` - Get current session information

### Context Management
- `GET /api/v1/contexts` - List all conversation contexts
- `POST /api/v1/contexts` - Create a new context
- `GET /api/v1/contexts/:id` - Get context details
- `DELETE /api/v1/contexts/:id` - Delete a context

### Message Management
- `GET /api/v1/contexts/:id/messages` - Get messages from a context
- `POST /api/v1/contexts/:id/messages` - Add a message to a context

### Real-time Sync
- `GET /api/v1/ws` - WebSocket endpoint for real-time updates

### MCP Integration
- `GET /api/v1/mcp/tools` - List available MCP tools
- `POST /api/v1/mcp/tools/:tool_name/call` - Call an MCP tool

### Sync Status
- `GET /api/v1/sync/status` - Get synchronization status
- `GET /api/v1/sync/events` - Get sync events (Server-Sent Events)

## WebSocket Protocol

The WebSocket endpoint supports the following message types:

### Client to Server (Zed to External Service)
```json
{
  "type": "sync_event",
  "session_id": "session_id",
  "event_type": "context_created|context_deleted|message_added|message_updated|message_deleted|context_title_changed",
  "data": { /* event-specific data */ },
  "timestamp": "2024-01-01T00:00:00Z"
}
```

### Server to Client (External Service to Zed)
```json
{
  "type": "add_message|update_message|delete_message|update_context",
  "data": {
    "context_id": "context_id",
    "content": "message content",
    "role": "user|assistant|system"
  }
}
```

## MCP Tool Integration

The integration supports Model Context Protocol (MCP) servers for extending functionality with external tools.

### Pre-configured MCP Servers

#### Filesystem
```json
{
  "name": "filesystem",
  "command": "npx",
  "args": ["@modelcontextprotocol/server-filesystem", "/workspace/path"]
}
```

#### Git
```json
{
  "name": "git", 
  "command": "npx",
  "args": ["@modelcontextprotocol/server-git", "--repository", "/repo/path"]
}
```

#### SQLite
```json
{
  "name": "sqlite",
  "command": "npx", 
  "args": ["@modelcontextprotocol/server-sqlite", "/database.db"]
}
```

## Environment Variables

For quick testing without settings, you can use environment variables:

- `ZED_EXTERNAL_SYNC_ENABLED=1` - Enable external sync
- `ZED_EXTERNAL_WEBSOCKET_ENABLED=1` - Enable WebSocket sync
- `ZED_EXTERNAL_URL=localhost:8080` - External service URL
- `ZED_EXTERNAL_AUTH_TOKEN=token` - Authentication token
- `ZED_EXTERNAL_USE_TLS=1` - Use TLS/SSL
- `ZED_EXTERNAL_MCP_ENABLED=1` - Enable MCP integration

## Development

### Building

```bash
cargo build -p external_websocket_sync
```

### Testing

```bash
cargo test -p external_websocket_sync
```

### Running with Debug Logging

```bash
RUST_LOG=external_websocket_sync=debug zed
```

## Architecture

The integration consists of several key components:

### ExternalWebSocketSync
Main service that coordinates all sync functionality. Manages contexts, WebSocket connections, and MCP servers.

### WebSocketSync
Handles real-time bidirectional communication with external services using WebSocket protocol.

### HTTP Server
Provides RESTful API endpoints for external agents and browser-based clients.

### MCP Manager
Manages Model Context Protocol servers, handles tool discovery and execution.

### Settings
Comprehensive settings system integrated with Zed's configuration.

## Troubleshooting

### WebSocket Connection Issues
- Check that the external service is running and accessible
- Verify the `external_url` setting is correct
- Ensure authentication token is valid
- Check firewall settings

### MCP Tool Errors
- Verify MCP server commands are available (e.g., `npx` is installed)
- Check working directories and permissions
- Review server logs for startup errors
- Ensure required environment variables are set

### Context Sync Problems
- Check that both Zed and the external service are using the same session ID
- Verify authentication tokens match
- Look for network connectivity issues
- Review sync event logs

## Contributing

1. Fork the repository
2. Create a feature branch
3. Implement your changes
4. Add tests for new functionality
5. Ensure all tests pass
6. Submit a pull request

## License

This project is licensed under the GPL-3.0-or-later license.

## Usage Examples

### Basic Setup

1. **Enable the sync in your Zed settings:**

```json
{
  "external_websocket_sync": {
    "enabled": true,
    "websocket_sync": {
      "enabled": true,
      "external_url": "localhost:8080",
      "auth_token": "your_external_service_token_here"
    }
  }
}
```

2. **Start your external service** (make sure it's running on the configured URL)

3. **Open a project in Zed** - the sync will automatically initialize

### HTTP API Usage

With the HTTP server enabled, you can interact with Zed externally:

```bash
# Get session information
curl http://localhost:3030/api/v1/session

# List all conversation contexts
curl http://localhost:3030/api/v1/contexts

# Create a new context
curl -X POST http://localhost:3030/api/v1/contexts \
  -H "Content-Type: application/json" \
  -d '{"title": "My New Conversation"}'

# Add a message to a context
curl -X POST http://localhost:3030/api/v1/contexts/CONTEXT_ID/messages \
  -H "Content-Type: application/json" \
  -d '{"content": "Hello from external agent!", "role": "user"}'
```

### WebSocket Real-time Sync

Connect to the WebSocket endpoint for real-time updates:

```javascript
const ws = new WebSocket('ws://localhost:3030/api/v1/ws');

ws.onopen = () => {
  console.log('Connected to Zed External Sync');
};

ws.onmessage = (event) => {
  const data = JSON.parse(event.data);
  console.log('Received from Zed:', data);
};

// Send a command to Zed
ws.send(JSON.stringify({
  type: 'add_message',
  data: {
    context_id: 'context-id-here',
    content: 'Hello from WebSocket!',
    role: 'user'
  }
}));
```

### MCP Tool Integration

Configure MCP servers to extend functionality:

```json
{
  "external_websocket_sync": {
    "mcp": {
      "enabled": true,
      "servers": [
        {
          "name": "filesystem",
          "command": "npx",
          "args": ["@modelcontextprotocol/server-filesystem", "/workspace"],
          "auto_restart": true
        },
        {
          "name": "git",
          "command": "npx", 
          "args": ["@modelcontextprotocol/server-git", "--repository", "/repo"],
          "auto_restart": true
        }
      ]
    }
  }
}
```

Then call MCP tools via the API:

```bash
# List available tools
curl http://localhost:3030/api/v1/mcp/tools

# Call a filesystem tool
curl -X POST http://localhost:3030/api/v1/mcp/tools/read_file/call \
  -H "Content-Type: application/json" \
  -d '{"arguments": {"path": "/workspace/README.md"}}'
```

### Environment Variable Configuration

For quick testing without modifying settings:

```bash
export ZED_EXTERNAL_SYNC_ENABLED=1
export ZED_EXTERNAL_WEBSOCKET_ENABLED=1
export ZED_EXTERNAL_URL=localhost:8080
export ZED_EXTERNAL_AUTH_TOKEN=your_token_here

# Start Zed with environment variables
zed
```

### Development and Debugging

Enable debug logging to see sync activity:

```bash
RUST_LOG=external_websocket_sync=debug,websocket=debug zed
```

Monitor the sync status:

```bash
# Check health
curl http://localhost:3030/health

# Get sync status  
curl http://localhost:3030/api/v1/sync/status
```