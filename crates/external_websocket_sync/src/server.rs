//! HTTP server for Helix integration APIs

use anyhow::{Context, Result};
use axum::{
    extract::{Path, State, WebSocketUpgrade},
    http::StatusCode,
    response::Json,
    routing::{delete, get, post},
    Router,
};
use axum::extract::ws::{Message, WebSocket};
use axum::response::Response;
use collections::HashMap;
use futures::{SinkExt, StreamExt};
use gpui::App;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{net::SocketAddr, sync::Arc, time::Instant};
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::{
    cors::CorsLayer,
    trace::TraceLayer,
};
use uuid::Uuid;

use crate::{
    types::*,
    ExternalWebSocketSync,
};

/// Server configuration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub auth_token: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".to_string(),
            port: 3030,
            auth_token: None,
        }
    }
}

/// Server handle for managing the HTTP server
pub struct ServerHandle {
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl ServerHandle {
    pub async fn stop(self) -> Result<()> {
        let _ = self.shutdown_tx.send(());
        self.task.await??;
        Ok(())
    }
}

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    config: ServerConfig,
    start_time: Instant,
    websocket_clients: Arc<RwLock<HashMap<String, WebSocketClient>>>,
}

#[derive(Clone)]
struct WebSocketClient {
    id: String,
    sender: tokio::sync::mpsc::UnboundedSender<Message>,
}

impl ExternalWebSocketSync {
    /// Start the HTTP server
    pub async fn start_server(
        config: ServerConfig,
        cx: &App,
    ) -> Result<ServerHandle> {
        let addr: SocketAddr = format!("{}:{}", config.host, config.port)
            .parse()
            .context("Invalid server address")?;

        let app_state = AppState {
            config: config.clone(),
            start_time: Instant::now(),
            websocket_clients: Arc::new(RwLock::new(HashMap::default())),
        };

        let app = create_router(app_state);

        let listener = TcpListener::bind(&addr)
            .await
            .context("Failed to bind to address")?;

        log::info!("Helix integration server listening on {}", addr);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .context("Server error")
        });

        Ok(ServerHandle { shutdown_tx, task })
    }
}

/// Create the main router with all API routes
fn create_router(state: AppState) -> Router {
    Router::new()
        // Health check
        .route("/health", get(health_handler))
        // Session endpoints
        .route("/api/v1/session", get(get_session_handler))
        // Context endpoints
        .route("/api/v1/contexts", get(list_contexts_handler))
        .route("/api/v1/contexts", post(create_context_handler))
        .route("/api/v1/contexts/:context_id", get(get_context_handler))
        .route("/api/v1/contexts/:context_id", delete(delete_context_handler))
        // Message endpoints
        .route("/api/v1/contexts/:context_id/messages", get(get_messages_handler))
        .route("/api/v1/contexts/:context_id/messages", post(add_message_handler))
        // WebSocket endpoint for real-time sync
        .route("/api/v1/ws", get(websocket_handler))
        // MCP endpoints
        .route("/api/v1/mcp/tools", get(list_mcp_tools_handler))
        .route("/api/v1/mcp/tools/:tool_name/call", post(call_mcp_tool_handler))
        // Sync endpoints
        .route("/api/v1/sync/status", get(sync_status_handler))
        .route("/api/v1/sync/events", get(sync_events_handler))
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(CorsLayer::permissive())
        )
}

/// Health check handler
async fn health_handler(State(state): State<AppState>) -> Result<Json<HealthResponse>, StatusCode> {
    let uptime = state.start_time.elapsed().as_secs();
    
    // Get integration info from global if available
    // Note: This is a placeholder pattern since we don't have AppContext in axum handlers
    // In a real implementation, we'd need to store the integration reference in AppState
    let (active_contexts, sync_clients, session_id) = (0, 0, "zed-session".to_string());
    
    Ok(Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        session_id,
        uptime_seconds: uptime,
        active_contexts,
        sync_clients,
    }))
}

/// Get session information
async fn get_session_handler(State(state): State<AppState>) -> Result<Json<SessionInfo>, StatusCode> {
    // Return session info with placeholder data
    // TODO: Access global integration when AppContext is available in handlers
    let session_info = SessionInfo {
        session_id: "zed-session".to_string(),
        last_session_id: None,
        active_contexts: 0,
        websocket_connected: false,
        sync_clients: 0,
    };
    Ok(Json(session_info))
}

/// List all contexts
async fn list_contexts_handler(State(_state): State<AppState>) -> Result<Json<Vec<ContextInfo>>, StatusCode> {
    // Return empty contexts list for now
    // TODO: Access global integration to get real contexts
    let contexts = vec![
        ContextInfo {
            id: "example-context".to_string(),
            title: "Example Context".to_string(),
            message_count: 0,
            last_message_at: chrono::Utc::now(),
            status: "active".to_string(),
        }
    ];
    Ok(Json(contexts))
}

/// Create a new context
async fn create_context_handler(
    State(_state): State<AppState>,
    Json(request): Json<CreateContextRequest>,
) -> Result<Json<CreateContextResponse>, StatusCode> {
    let context_id = Uuid::new_v4().to_string();
    let title = request.title.unwrap_or_else(|| "New Conversation".to_string());
    
    log::info!("HTTP API: Creating context {} ({})", context_id, title);
    
    // TODO: Call integration.create_context() when AppContext is available
    // For now, just return success response
    
    Ok(Json(CreateContextResponse {
        context_id,
        title,
        created_at: chrono::Utc::now(),
    }))
}

/// Get context details
async fn get_context_handler(
    State(_state): State<AppState>,
    Path(context_id): Path<String>,
) -> Result<Json<ContextInfo>, StatusCode> {
    log::info!("HTTP API: Getting context {}", context_id);
    
    // TODO: Get actual context from integration when AppContext is available
    // For now, return a placeholder context
    Ok(Json(ContextInfo {
        id: context_id,
        title: "Active Conversation".to_string(),
        message_count: 0,
        last_message_at: chrono::Utc::now(),
        status: "active".to_string(),
    }))
}

/// Delete a context
async fn delete_context_handler(
    State(_state): State<AppState>,
    Path(context_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    log::info!("HTTP API: Deleting context {}", context_id);
    
    // TODO: Call integration.delete_context(&context_id) when AppContext is available
    // For now, just acknowledge the deletion
    
    Ok(StatusCode::NO_CONTENT)
}

/// Get messages from a context
async fn get_messages_handler(
    State(_state): State<AppState>,
    Path(context_id): Path<String>,
) -> Result<Json<Vec<MessageInfo>>, StatusCode> {
    log::info!("HTTP API: Getting messages for context {}", context_id);
    
    // TODO: Call integration.get_context_messages(&context_id, cx) when AppContext is available
    // For now, return empty list
    
    Ok(Json(vec![]))
}

/// Add a message to a context
async fn add_message_handler(
    State(_state): State<AppState>,
    Path(context_id): Path<String>,
    Json(request): Json<AddMessageRequest>,
) -> Result<Json<AddMessageResponse>, StatusCode> {
    log::info!("HTTP API: Adding message to context {}: {} (role: {})", 
              context_id, request.content, request.role);
    
    // Generate a message ID
    let message_id = chrono::Utc::now().timestamp_millis() as u64;
    
    // TODO: Call integration.add_message_to_context() when AppContext is available
    // For now, just acknowledge the message was received
    
    Ok(Json(AddMessageResponse {
        message_id,
        context_id,
        created_at: chrono::Utc::now(),
    }))
}

/// WebSocket handler for real-time sync
async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_websocket(socket, state))
}

async fn handle_websocket(socket: WebSocket, state: AppState) {
    let client_id = Uuid::new_v4().to_string();
    let (mut sender, mut receiver) = socket.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    // Store client
    let client = WebSocketClient {
        id: client_id.clone(),
        sender: tx,
    };
    state.websocket_clients.write().insert(client_id.clone(), client);

    // Handle outgoing messages
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Handle incoming messages
    let recv_task = tokio::spawn({
        let state = state.clone();
        let client_id = client_id.clone();
        async move {
            while let Some(msg) = receiver.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Err(e) = handle_websocket_message(&text, &state).await {
                            log::warn!("WebSocket message error: {}", e);
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Err(e) => {
                        log::warn!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    // Cleanup
    state.websocket_clients.write().remove(&client_id);
    log::info!("WebSocket client {} disconnected", client_id);
}

async fn handle_websocket_message(text: &str, _state: &AppState) -> Result<()> {
    let _message: WebSocketMessage = serde_json::from_str(text)?;
    // TODO: Handle different message types
    Ok(())
}

/// List available MCP tools
async fn list_mcp_tools_handler(
    State(_state): State<AppState>,
) -> Result<Json<McpToolsResponse>, StatusCode> {
    log::info!("HTTP API: Listing MCP tools");
    
    // TODO: Get actual MCP tools from the integration
    Ok(Json(McpToolsResponse {
        tools: vec![],
    }))
}

/// Call an MCP tool
async fn call_mcp_tool_handler(
    State(_state): State<AppState>,
    Path(tool_name): Path<String>,
    Json(request): Json<McpToolCallRequest>,
) -> Result<Json<McpToolCallResponse>, StatusCode> {
    log::info!("HTTP API: Calling MCP tool {}: {:?}", tool_name, request.arguments);
    
    // TODO: Call actual MCP tool via integration when available
    Ok(Json(McpToolCallResponse {
        success: false,
        result: None,
        error: Some("MCP integration not yet implemented".to_string()),
    }))
}

/// Get sync status
async fn sync_status_handler(
    State(_state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    log::info!("HTTP API: Getting sync status");
    
    // TODO: Get actual sync status from integration when available
    Ok(Json(json!({
        "enabled": true,
        "connected": false,
        "active_contexts": 0,
        "sync_clients": 0,
        "last_sync": null,
        "error": null
    })))
}

/// Get sync events (SSE endpoint)
async fn sync_events_handler(
    State(_state): State<AppState>,
) -> Result<Json<Vec<TimestampedSyncEvent>>, StatusCode> {
    log::info!("HTTP API: Getting sync events");
    
    // TODO: Return actual sync events from integration when available
    Ok(Json(vec![]))
}

/// Start HTTP server with given configuration (standalone function)
pub async fn start_server(config: ServerConfig) -> Result<ServerHandle> {
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .context("Invalid server address")?;

    let app_state = AppState {
        config: config.clone(),
        start_time: Instant::now(),
        websocket_clients: Arc::new(RwLock::new(HashMap::default())),
    };

    let app = create_router(app_state);

    let listener = TcpListener::bind(&addr)
        .await
        .context("Failed to bind to address")?;

    log::info!("Helix integration server listening on {}", addr);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .context("Server error")
    });

    Ok(ServerHandle { shutdown_tx, task })
}

/// Broadcast a message to all WebSocket clients
pub async fn broadcast_to_websockets(
    clients: Arc<RwLock<HashMap<String, WebSocketClient>>>,
    message: WebSocketMessage,
) -> Result<()> {
    let message_text = serde_json::to_string(&message)?;
    let ws_message = Message::Text(message_text);
    
    let clients_guard = clients.read();
    for client in clients_guard.values() {
        if let Err(e) = client.sender.send(ws_message.clone()) {
            log::warn!("Failed to send message to WebSocket client {}: {}", client.id, e);
        }
    }
    
    Ok(())
}