//! Types for Helix integration

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Information about the current session
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub last_session_id: Option<String>,
    pub active_contexts: usize,
    pub websocket_connected: bool,
    pub sync_clients: usize,
}

/// Information about a conversation context
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextInfo {
    pub id: String,
    pub title: String,
    pub message_count: usize,
    pub last_message_at: DateTime<Utc>,
    pub status: String,
}

/// Information about a message
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageInfo {
    pub id: u64,
    pub context_id: String,
    pub role: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub status: String,
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Request to create a new context
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateContextRequest {
    pub title: Option<String>,
    pub initial_message: Option<String>,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

/// Response when creating a context
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateContextResponse {
    pub context_id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
}

/// Request to add a message to a context
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddMessageRequest {
    pub content: String,
    pub role: String,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

/// Response when adding a message
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddMessageResponse {
    pub message_id: u64,
    pub context_id: String,
    pub created_at: DateTime<Utc>,
}

/// Configuration for sync service
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncConfig {
    pub enabled: bool,
    pub helix_api_url: String,
    pub sync_interval_seconds: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            helix_api_url: "http://localhost:8080".to_string(),
            sync_interval_seconds: 5,
        }
    }
}

/// Configuration for MCP integration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpConfig {
    pub enabled: bool,
    pub server_configs: Vec<McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_configs: Vec::new(),
        }
    }
}

/// Configuration for an MCP server
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// Events that Zed sends to external system via WebSocket
/// Per WEBSOCKET_PROTOCOL_SPEC.md - Zed is stateless and only knows about acp_thread_id
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SyncEvent {
    /// Sent when Zed creates a new ACP thread
    #[serde(rename = "thread_created")]
    ThreadCreated {
        acp_thread_id: String,
        request_id: String,
    },
    /// Sent while AI is streaming response (same message_id, progressively longer content)
    #[serde(rename = "message_added")]
    MessageAdded {
        acp_thread_id: String,
        message_id: String,
        role: String,
        content: String,
        timestamp: i64,
    },
    /// Sent when AI finishes responding
    #[serde(rename = "message_completed")]
    MessageCompleted {
        acp_thread_id: String,
        message_id: String,
        request_id: String,
    },
}

/// Incoming command from external system to Zed
/// Per WEBSOCKET_PROTOCOL_SPEC.md - external system sends chat_message
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IncomingChatMessage {
    pub acp_thread_id: Option<String>,  // null = create new thread, Some(id) = use existing
    pub message: String,
    pub request_id: String,
}

/// Response for health check endpoint
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub session_id: String,
    pub uptime_seconds: u64,
    pub active_contexts: usize,
    pub sync_clients: usize,
}

/// Error response
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
    pub code: Option<String>,
    pub details: Option<HashMap<String, serde_json::Value>>,
}

/// WebSocket message types (DEPRECATED - use SyncEvent directly)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WebSocketMessage {
    /// Sync event from Zed to external system
    SyncEvent(SyncEvent),
    /// Ping message
    Ping { id: String },
    /// Pong response
    Pong { id: String },
    /// Error message
    Error(ErrorResponse),
    /// Subscribe to events
    Subscribe {
        events: Vec<String>,
    },
    /// Unsubscribe from events
    Unsubscribe {
        events: Vec<String>,
    },
}

/// MCP tool call request
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpToolCallRequest {
    pub tool_name: String,
    pub arguments: HashMap<String, serde_json::Value>,
    pub context_id: Option<String>,
}

/// MCP tool call response
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpToolCallResponse {
    pub success: bool,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// Available MCP tools
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
    pub server: String,
}

/// List of available MCP tools
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpToolsResponse {
    pub tools: Vec<McpTool>,
}

/// Stream response for real-time updates
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamResponse<T> {
    pub id: String,
    pub data: T,
    pub timestamp: DateTime<Utc>,
    pub sequence: u64,
}

/// Conversation thread summary for Helix sync
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub title: String,
    pub message_count: usize,
    pub last_message_at: DateTime<Utc>,
    pub participants: Vec<String>,
    pub status: String,
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Full thread data for initial sync
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadData {
    pub thread_id: String,
    pub title: String,
    pub messages: Vec<MessageInfo>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub metadata: HashMap<String, serde_json::Value>,
}