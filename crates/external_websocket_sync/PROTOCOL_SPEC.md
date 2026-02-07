# Zed <-> Helix WebSocket Sync Protocol Specification

## Overview

This document defines the WebSocket protocol used for real-time synchronization between Zed IDE and the Helix platform. The protocol enables Helix to remotely control Zed's AI agent threads: sending chat messages, opening existing threads, and receiving streaming responses.

**Key principle**: Zed is stateless with respect to Helix sessions. Zed only knows about `acp_thread_id` (its internal thread identifier). Helix maintains all session-to-thread mapping on its side.

## Connection

### Endpoint

```
ws://{host}/api/v1/external-agents/sync?session_id={session_id}
```

- `host`: The Helix API server address (e.g., `localhost:8080`)
- `session_id`: The Helix session ID that this Zed instance belongs to

### Authentication

Authentication is performed via the HTTP `Authorization` header during the WebSocket upgrade:

```
Authorization: Bearer {token}
```

The token is the Helix API key (starts with `hl-`).

### TLS

- Use `wss://` for TLS connections, `ws://` for plaintext
- Enterprise deployments may set `skip_tls_verify: true` for internal CAs (not recommended for production)

## Message Formats

### Zed -> Helix (SyncMessage)

All messages from Zed to Helix use the `SyncMessage` format:

```json
{
  "session_id": "<helix_session_id>",
  "event_type": "<event_type>",
  "data": { ... },
  "timestamp": "<ISO 8601 timestamp>"
}
```

The `session_id` field is set from the `HELIX_SESSION_ID` environment variable in Zed's container.

### Helix -> Zed (ExternalAgentCommand)

All messages from Helix to Zed use the `ExternalAgentCommand` format:

```json
{
  "type": "<command_type>",
  "data": { ... }
}
```

## Zed -> Helix Event Types

### `agent_ready`

Sent when the ACP agent (e.g., qwen-code, zed-agent) has finished initialization and is ready to receive commands. This prevents race conditions where Helix sends prompts before the agent is loaded.

```json
{
  "event_type": "agent_ready",
  "data": {
    "agent_name": "qwen",
    "thread_id": null
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `agent_name` | string | yes | Name of the agent that became ready |
| `thread_id` | string or null | no | Thread ID if a thread was loaded from a previous session |

### `thread_created`

Sent when Zed creates a new ACP thread in response to a `chat_message` command from Helix.

```json
{
  "event_type": "thread_created",
  "data": {
    "acp_thread_id": "thread-uuid-here",
    "request_id": "req-001"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `acp_thread_id` | string | yes | Zed's internal thread identifier |
| `request_id` | string | yes | Echoed from the originating `chat_message` for correlation |

### `user_created_thread`

Sent when the user creates a new thread directly in the Zed UI (not via Helix). Helix can use this to create a corresponding session.

```json
{
  "event_type": "user_created_thread",
  "data": {
    "acp_thread_id": "thread-uuid-here",
    "title": "My New Thread"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `acp_thread_id` | string | yes | Zed's internal thread identifier |
| `title` | string or null | no | Thread title if available |

### `thread_title_changed`

Sent when a thread's title is updated in Zed (e.g., auto-generated from content).

```json
{
  "event_type": "thread_title_changed",
  "data": {
    "acp_thread_id": "thread-uuid-here",
    "title": "Updated Title"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `acp_thread_id` | string | yes | Zed's internal thread identifier |
| `title` | string | yes | New thread title |

### `message_added`

Sent while the AI agent is streaming its response. Multiple `message_added` events are sent for the same `message_id`, each with progressively longer `content` (accumulated, not delta).

```json
{
  "event_type": "message_added",
  "data": {
    "acp_thread_id": "thread-uuid-here",
    "message_id": "msg-123",
    "role": "assistant",
    "content": "The answer is 42",
    "timestamp": 1706000000
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `acp_thread_id` | string | yes | Thread this message belongs to |
| `message_id` | string | yes | Unique message identifier (stable across streaming chunks) |
| `role` | string | yes | Message role: `"user"`, `"assistant"`, or `"system"` |
| `content` | string | yes | Full accumulated message content so far |
| `timestamp` | integer | yes | Unix timestamp |

### `message_completed`

Sent when the AI agent finishes its response for a given request.

```json
{
  "event_type": "message_completed",
  "data": {
    "acp_thread_id": "thread-uuid-here",
    "message_id": "msg-123",
    "request_id": "req-001"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `acp_thread_id` | string | yes | Thread the message belongs to |
| `message_id` | string | yes | The completed message's identifier |
| `request_id` | string | yes | Echoed from the originating `chat_message` for correlation |

### `thread_load_error`

Sent when Zed fails to load a thread (e.g., thread is already active in the UI, or database error).

```json
{
  "event_type": "thread_load_error",
  "data": {
    "acp_thread_id": "thread-uuid-here",
    "request_id": "req-001",
    "error": "Thread is already active in another panel"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `acp_thread_id` | string | yes | Thread that failed to load |
| `request_id` | string | yes | Echoed from the originating command |
| `error` | string | yes | Human-readable error description |

## Helix -> Zed Command Types

### `chat_message`

Send a message to an AI agent thread. If `acp_thread_id` is null, Zed creates a new thread. If `acp_thread_id` is provided, Zed sends the message to the existing thread.

```json
{
  "type": "chat_message",
  "data": {
    "message": "What is the meaning of life?",
    "request_id": "req-001",
    "acp_thread_id": null,
    "agent_name": "qwen"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `message` | string | yes | The chat message content |
| `request_id` | string | yes | Unique request ID for response correlation |
| `acp_thread_id` | string or null | no | null = create new thread, string = use existing |
| `agent_name` | string or null | no | Which agent to use (e.g., "qwen", "zed-agent"). Defaults to Zed's native agent |

### `open_thread`

Open/focus an existing thread in Zed's UI. This loads the thread from the database and displays it in the agent panel.

```json
{
  "type": "open_thread",
  "data": {
    "acp_thread_id": "thread-uuid-here",
    "agent_name": "qwen"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `acp_thread_id` | string | yes | The thread to open |
| `agent_name` | string or null | no | Which agent to use when opening the thread |

## Readiness Protocol

The readiness protocol prevents race conditions where Helix sends commands before Zed's agent is fully initialized.

### Flow

1. **Zed connects** to the WebSocket endpoint
2. **Zed sends `agent_ready`** when the ACP agent process has initialized
3. **Helix queues commands** received before `agent_ready` (e.g., the initial `chat_message`)
4. **On `agent_ready`**, Helix flushes the queued commands to Zed
5. **Timeout fallback**: If `agent_ready` is not received within 60 seconds, Helix proceeds anyway

### Sequence Diagram

```
Zed                          Helix
 |                             |
 |--- WebSocket Connect ------>|
 |                             |
 |                             |--- (queues initial chat_message)
 |                             |
 |--- agent_ready ----------->|
 |                             |--- (flushes queue)
 |<-- chat_message ------------|
 |                             |
 |--- thread_created -------->|
 |--- message_added --------->|  (streaming, multiple)
 |--- message_added --------->|
 |--- message_completed ----->|
 |                             |
```

## Typical Message Flows

### Flow 1: Helix Initiates New Thread

```
Helix -> Zed:  chat_message { message: "...", request_id: "req-1", acp_thread_id: null }
Zed -> Helix:  thread_created { acp_thread_id: "thread-1", request_id: "req-1" }
Zed -> Helix:  message_added { acp_thread_id: "thread-1", message_id: "msg-1", content: "The" }
Zed -> Helix:  message_added { acp_thread_id: "thread-1", message_id: "msg-1", content: "The answer" }
Zed -> Helix:  message_added { acp_thread_id: "thread-1", message_id: "msg-1", content: "The answer is 42" }
Zed -> Helix:  message_completed { acp_thread_id: "thread-1", message_id: "msg-1", request_id: "req-1" }
```

### Flow 2: Follow-up Message to Existing Thread

```
Helix -> Zed:  chat_message { message: "...", request_id: "req-2", acp_thread_id: "thread-1" }
Zed -> Helix:  message_added { acp_thread_id: "thread-1", message_id: "msg-2", content: "..." }
Zed -> Helix:  message_completed { acp_thread_id: "thread-1", message_id: "msg-2", request_id: "req-2" }
```

Note: No `thread_created` event for follow-up messages since the thread already exists.

### Flow 3: Open Existing Thread

```
Helix -> Zed:  open_thread { acp_thread_id: "thread-1" }
(Zed loads thread from database and displays in UI)
```

### Flow 4: User Creates Thread in Zed UI

```
(User clicks "New Thread" in Zed)
Zed -> Helix:  user_created_thread { acp_thread_id: "thread-2", title: "My Thread" }
(Helix creates corresponding session)
```

### Flow 5: Error During Thread Load

```
Helix -> Zed:  chat_message { message: "...", request_id: "req-3", acp_thread_id: "thread-1" }
Zed -> Helix:  thread_load_error { acp_thread_id: "thread-1", request_id: "req-3", error: "Thread already active" }
```

## Reconnection

Zed automatically reconnects when the WebSocket connection drops (e.g., API restart, network issues):

- **Exponential backoff**: 1s, 2s, 4s, 8s, ..., capped at 30s
- **On reconnection**: Zed sends `agent_ready` again
- **Queued outgoing events**: Events queued during disconnection may be lost (known limitation)

## Wire Format Notes

- All messages are JSON text frames (not binary)
- Zed sends `OutgoingMessage` format: `{"event_type": "...", "data": {...}}`
- Helix sends `ExternalAgentCommand` format: `{"type": "...", "data": {...}}`
- Ping/pong: Standard WebSocket ping/pong frames are supported
- Echoed user messages (with `role: "user"`) from Helix are ignored by Zed to prevent loops
