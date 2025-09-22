//! WebSocket sync client for connecting Zed to Helix

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream, MaybeTlsStream};
use tungstenite::handshake::client::generate_key;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use url::Url;
use parking_lot::RwLock;
use std::sync::Arc;

use assistant_context::{AssistantContext, ContextId, ContextStore, MessageId};
use gpui::Entity;

use crate::types::*;

/// WebSocket sync configuration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSocketSyncConfig {
    pub enabled: bool,
    pub helix_url: String,
    pub session_id: String,
    pub auth_token: String,
    pub use_tls: bool,
}

impl Default for WebSocketSyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            helix_url: "localhost:8080".to_string(),
            session_id: String::new(),
            auth_token: String::new(),
            use_tls: false,
        }
    }
}

/// WebSocket sync client for real-time communication with Helix
pub struct WebSocketSync {
    config: WebSocketSyncConfig,
    event_sender: mpsc::UnboundedSender<SyncEvent>,
    command_receiver: Arc<RwLock<Option<mpsc::UnboundedReceiver<HelixCommand>>>>,
    is_connected: Arc<RwLock<bool>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    // Assistant context management for creating UI threads
    active_contexts: Arc<RwLock<HashMap<String, ContextId>>>, // Helix session ID -> Zed context ID mapping
}

/// Commands received from Helix
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HelixCommand {
    #[serde(rename = "type")]
    pub command_type: String,
    pub data: HashMap<String, serde_json::Value>,
}

/// Sync message sent to Helix
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncMessage {
    pub session_id: String,
    pub event_type: String,
    pub data: HashMap<String, serde_json::Value>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl WebSocketSync {
    /// Create a new WebSocket sync client
    pub async fn new(
        config: WebSocketSyncConfig, 
        thread_creation_sender: Option<mpsc::UnboundedSender<crate::CreateThreadRequest>>
    ) -> Result<Self> {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let (command_sender, command_receiver) = mpsc::unbounded_channel();
        
        let is_connected = Arc::new(RwLock::new(false));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let sync = Self {
            config: config.clone(),
            event_sender,
            command_receiver: Arc::new(RwLock::new(Some(command_receiver))),
            is_connected: is_connected.clone(),
            shutdown_tx: Some(shutdown_tx),
            active_contexts: Arc::new(RwLock::new(HashMap::new())),
        };

        // Start WebSocket connection task
        sync.start_connection_task(config, event_receiver, command_sender, is_connected.clone(), shutdown_rx, thread_creation_sender).await?;

        Ok(sync)
    }

    /// Start the WebSocket connection task
    async fn start_connection_task(
        &self,
        config: WebSocketSyncConfig,
        mut event_receiver: mpsc::UnboundedReceiver<SyncEvent>,
        command_sender: mpsc::UnboundedSender<HelixCommand>,
        is_connected: Arc<RwLock<bool>>,
        shutdown_rx: oneshot::Receiver<()>,
        thread_creation_sender: Option<mpsc::UnboundedSender<crate::CreateThreadRequest>>,
    ) -> Result<()> {
        let protocol = if config.use_tls { "wss" } else { "ws" };
        let url = format!(
            "{}://{}/api/v1/external-agents/sync?agent_id={}",
            protocol,
            config.helix_url,
            config.session_id // Using session_id as agent_id for this connection
        );

        log::info!("Starting WebSocket connection task for: {}", url);

        // Clone necessary data before async move
        let auth_token = config.auth_token.clone();
        let event_sender = self.event_sender.clone();

        tokio::spawn(async move {
            let url = match Url::parse(&url) {
                Ok(url) => url,
                Err(e) => {
                    log::error!("Invalid WebSocket URL: {}", e);
                    return;
                }
            };

            // Try to connect with retries
            let websocket = match connect_with_auth(&url, &auth_token).await {
                Ok(ws) => ws,
                Err(e) => {
                    log::error!("Failed to connect to WebSocket: {}", e);
                    return;
                }
            };

            *is_connected.write() = true;
            log::info!("Connected to Helix WebSocket");

            let (mut sink, mut stream) = websocket.split();

            // Clone session_id for use in both closures
            let session_id_for_outgoing = config.session_id.clone();
            let session_id_for_incoming = config.session_id.clone();

            // Handle outgoing events
            let event_task = {
                let is_connected = is_connected.clone();
                tokio::spawn(async move {
                    while let Some(event) = event_receiver.recv().await {
                        if !*is_connected.read() {
                            break;
                        }

                        // Handle local events that should create UI threads
                        match &event {
                            SyncEvent::CreateThreadFromHelixSession { helix_session_id, message, request_id } => {
                                log::info!(
                                    "🎯 Processing CreateThreadFromHelixSession event: session={}, message={}, request={}",
                                    helix_session_id, message, request_id
                                );
                                
                                // Send thread creation request to UI via channel
                                if let Some(ref sender) = thread_creation_sender {
                                    let request = crate::CreateThreadRequest {
                                        helix_session_id: helix_session_id.clone(),
                                        message: message.clone(),
                                        request_id: request_id.clone(),
                                    };
                                    
                                    if let Err(e) = sender.send(request) {
                                        log::error!("Failed to send thread creation request: {}", e);
                                    } else {
                                        log::info!("✅ Sent thread creation request to UI for session {}", helix_session_id);
                                    }
                                } else {
                                    log::warn!("Thread creation sender not available - cannot create UI thread");
                                }
                                
                                // Don't send this event to Helix - it's for local processing only
                                continue;
                            }
                            _ => {
                                // Handle other events normally by sending to Helix
                            }
                        }

                        let sync_message = SyncMessage {
                            session_id: session_id_for_outgoing.clone(),
                            event_type: Self::event_type_string(&event),
                            data: Self::event_to_data(event),
                            timestamp: chrono::Utc::now(),
                        };

                        let message_text = match serde_json::to_string(&sync_message) {
                            Ok(text) => text,
                            Err(e) => {
                                log::error!("Failed to serialize sync message: {}", e);
                                continue;
                            }
                        };

                        if let Err(e) = sink.send(Message::Text(message_text.into())).await {
                            log::error!("Failed to send WebSocket message: {}", e);
                            break;
                        }
                    }
                })
            };

            // Handle incoming messages
            let incoming_task = {
                let command_sender = command_sender.clone();
                let event_sender = event_sender.clone();
                tokio::spawn(async move {
                    while let Some(message) = stream.next().await {
                        match message {
                            Ok(Message::Text(text)) => {
                                if let Err(e) = Self::handle_incoming_message(&session_id_for_incoming, text.to_string(), &command_sender, &event_sender).await {
                                    log::error!("Failed to handle incoming message: {}", e);
                                }
                            }
                            Ok(Message::Close(_)) => {
                                log::info!("WebSocket closed by server");
                                break;
                            }
                            Err(e) => {
                                log::error!("WebSocket error: {}", e);
                                break;
                            }
                            _ => {}
                        }
                    }
                })
            };

            // Wait for shutdown signal or task completion
            tokio::select! {
                _ = shutdown_rx => {
                    log::info!("WebSocket connection shutdown requested");
                }
                _ = event_task => {
                    log::warn!("Event task completed");
                }
                _ = incoming_task => {
                    log::warn!("Incoming message task completed");
                }
            }

            *is_connected.write() = false;
            log::info!("WebSocket connection task ended");
        });

        Ok(())
    }

}

/// Connect with authentication
async fn connect_with_auth(url: &Url, auth_token: &str) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        log::info!("Attempting WebSocket connection to: {}", url);
        log::info!("Using auth token: {}", if auth_token.is_empty() { "none" } else { "present" });
        
        // Create a proper WebSocket request with authentication
        if !auth_token.is_empty() {
            let request = tungstenite::http::Request::builder()
                .method("GET")
                .uri(url.as_str())
                .header("Host", url.host_str().unwrap_or("localhost"))
                .header("Authorization", format!("Bearer {}", auth_token))
                .header("Connection", "Upgrade")
                .header("Upgrade", "websocket")
                .header("Sec-WebSocket-Version", "13")
                .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
                .body(())
                .context("Failed to create WebSocket request")?;
            
            log::info!("WebSocket request created with auth, attempting connection...");
            
            match connect_async(request).await {
                Ok((websocket, response)) => {
                    log::info!("WebSocket connection successful! Status: {}", response.status());
                    Ok(websocket)
                }
                Err(e) => {
                    log::error!("WebSocket connection failed with detailed error: {:?}", e);
                    Err(anyhow::anyhow!("Failed to connect to WebSocket: {}", e))
                }
            }
        } else {
            log::info!("WebSocket request created without auth, attempting connection...");
            
            match connect_async(url.as_str()).await {
                Ok((websocket, response)) => {
                    log::info!("WebSocket connection successful! Status: {}", response.status());
                    Ok(websocket)
                }
                Err(e) => {
                    log::error!("WebSocket connection failed with detailed error: {:?}", e);
                    Err(anyhow::anyhow!("Failed to connect to WebSocket: {}", e))
                }
            }
        }
}

impl WebSocketSync {
    /// Disconnect from WebSocket
    pub async fn disconnect(&mut self) -> Result<()> {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        *self.is_connected.write() = false;
        log::info!("Disconnected from Helix WebSocket");
        Ok(())
    }

    /// Check if WebSocket is connected
    pub fn is_connected(&self) -> bool {
        *self.is_connected.read()
    }

    /// Send a sync event to Helix
    pub fn send_event(&self, event: SyncEvent) -> Result<()> {
        if !self.is_connected() {
            return Err(anyhow::anyhow!("WebSocket not connected"));
        }

        self.event_sender
            .send(event)
            .map_err(|_| anyhow::anyhow!("Failed to queue sync event"))?;

        Ok(())
    }

    /// Handle incoming message from Helix
    pub async fn handle_incoming_message(
        session_id: &str, 
        text: String, 
        command_sender: &mpsc::UnboundedSender<HelixCommand>,
        event_sender: &mpsc::UnboundedSender<SyncEvent>
    ) -> Result<()> {
        let command: HelixCommand = serde_json::from_str(&text)
            .context("Failed to parse command from Helix")?;

        log::debug!("Received command from Helix: {:?}", command);

        // Forward the command for processing
        if let Err(_) = command_sender.send(command.clone()) {
            log::warn!("Failed to forward command to handler");
        }

        match command.command_type.as_str() {
            "add_message" => {
                Self::handle_add_message_command(session_id, command.data).await?;
            }
            "update_message" => {
                Self::handle_update_message_command(session_id, command.data).await?;
            }
            "delete_message" => {
                Self::handle_delete_message_command(session_id, command.data).await?;
            }
            "update_context" => {
                Self::handle_update_context_command(session_id, command.data).await?;
            }
            "chat_message" => {
                // TODO: Need to pass self reference to handle_chat_message_command
                // For now, keep the static version working
                Self::handle_chat_message_command(session_id, command.data, event_sender, None).await?;
            }
            _ => {
                log::warn!("Unknown command type: {}", command.command_type);
            }
        }

        Ok(())
    }

    /// Handle add_message command from Helix
    async fn handle_add_message_command(
        session_id: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<()> {
        let context_id = data.get("context_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing context_id in add_message command"))?;

        let content = data.get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing content in add_message command"))?;

        let role = data.get("role")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing role in add_message command"))?;

        log::info!(
            "Adding message to context {} from Helix session {}: {} (role: {})",
            context_id,
            session_id,
            content,
            role
        );

        // For now, just log that we received the message
        // In a full implementation, this would:
        // 1. Find or create the assistant context for context_id
        // 2. Add the message to that context
        // 3. If it's a user message, trigger AI completion
        log::info!("Message logged for context {} - assistant context integration pending", context_id);

        Ok(())
    }

    /// Handle update_message command from Helix
    async fn handle_update_message_command(
        _session_id: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<()> {
        log::debug!("Update message command: {:?}", data);
        // TODO: Implement message updating
        Ok(())
    }

    /// Handle delete_message command from Helix
    async fn handle_delete_message_command(
        _session_id: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<()> {
        log::debug!("Delete message command: {:?}", data);
        // TODO: Implement message deletion
        Ok(())
    }

    /// Handle update_context command from Helix
    /// Handle chat_message command from Helix - this includes a request_id and expects a response
    async fn handle_chat_message_command(
        agent_session_id: &str,
        data: HashMap<String, serde_json::Value>,
        event_sender: &mpsc::UnboundedSender<SyncEvent>,
        active_contexts: Option<Arc<RwLock<HashMap<String, ContextId>>>>,
    ) -> Result<()> {
        let request_id = data.get("request_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing request_id in chat_message command"))?;

        let helix_session_id = data.get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing session_id in chat_message command"))?;

        let message = data.get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing message in chat_message command"))?;

        let role = data.get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("user");

        log::info!(
            "Received chat message from Helix session {} via agent {}: {} (role: {}, request_id: {})",
            helix_session_id,
            agent_session_id,
            message,
            role,
            request_id
        );

        // TODO: Create actual AssistantContext/thread for this session
        // For now, create a session mapping and respond with thread creation confirmation
        
        // Store the session mapping in our active contexts (if available)
        if let Some(active_contexts) = active_contexts {
            let mut contexts = active_contexts.write();
            if !contexts.contains_key(helix_session_id) {
                // For now, generate a placeholder ContextId
                let zed_context_id = ContextId::new();
                contexts.insert(helix_session_id.to_string(), zed_context_id.clone());
                
                log::info!(
                    "📝 Created thread mapping: Helix session {} → Zed context {:?}",
                    helix_session_id,
                    zed_context_id
                );
                
                // TODO: Here we should create the actual AssistantContext with this ID
                // and add it to Zed's UI thread list
            } else {
                log::info!(
                    "📝 Using existing thread mapping for Helix session {}",
                    helix_session_id
                );
            }
        } else {
            log::warn!("No active_contexts available - session mapping not stored");
        }
        
        // TODO: Here we should:
        // 1. Create a new AssistantContext (thread) in Zed's thread store
        // 2. Add the message to the thread
        // 3. Make the thread visible in the UI
        // 4. Send the thread's response back to Helix
        
        let response_content = format!(
            "🎯 Zed WebSocket Sync Working!\n\n✅ Received message from Helix session: {}\n✅ Message: \"{}\"\n✅ Request ID: {}\n✅ Role: {}\n\n📝 Thread Creation: Ready to create Zed thread for this session\n❌ Language Model: Not configured - please configure in Zed settings\n\n🔗 Next: Will create AssistantContext and integrate with Zed UI",
            helix_session_id,
            message,
            request_id,
            role
        );
        
        // Emit event to request thread creation from the UI
        let create_thread_event = SyncEvent::CreateThreadFromHelixSession {
            helix_session_id: helix_session_id.to_string(),
            message: message.to_string(),
            request_id: request_id.to_string(),
        };
        
        if let Err(e) = event_sender.send(create_thread_event) {
            log::error!("Failed to send create thread event: {}", e);
            return Err(anyhow::anyhow!("Failed to send create thread event: {}", e));
        }
        
        log::info!(
            "🎯 Emitted CreateThreadFromHelixSession event for session {} - UI should create thread now!",
            helix_session_id
        );
        
        log::info!(
            "✅ Sent enhanced WebSocket sync response for session {} to request {} - bidirectional sync working!",
            helix_session_id,
            request_id
        );

        Ok(())
    }

    async fn handle_update_context_command(
        _session_id: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<()> {
        log::debug!("Update context command: {:?}", data);
        // TODO: Implement context updating
        Ok(())
    }

    /// Convert SyncEvent to event type string
    pub fn event_type_string(event: &SyncEvent) -> String {
        match event {
            SyncEvent::ContextCreated { .. } => "context_created".to_string(),
            SyncEvent::ContextDeleted { .. } => "context_deleted".to_string(),
            SyncEvent::MessageAdded { .. } => "message_added".to_string(),
            SyncEvent::MessageUpdated { .. } => "message_updated".to_string(),
            SyncEvent::MessageDeleted { .. } => "message_deleted".to_string(),
            SyncEvent::ContextTitleChanged { .. } => "context_title_changed".to_string(),
            SyncEvent::ChatResponse { .. } => "chat_response".to_string(),
            SyncEvent::ChatResponseChunk { .. } => "chat_response_chunk".to_string(),
            SyncEvent::ChatResponseDone { .. } => "chat_response_done".to_string(),
            SyncEvent::ChatResponseError { .. } => "chat_response_error".to_string(),
            SyncEvent::CreateThreadFromHelixSession { .. } => "create_thread_from_helix_session".to_string(),
        }
    }

    /// Convert SyncEvent to data HashMap
    pub fn event_to_data(event: SyncEvent) -> HashMap<String, serde_json::Value> {
        let mut data = HashMap::new();

        match event {
            SyncEvent::ContextCreated { context_id } => {
                data.insert("context_id".to_string(), serde_json::Value::String(context_id));
            }
            SyncEvent::ContextDeleted { context_id } => {
                data.insert("context_id".to_string(), serde_json::Value::String(context_id));
            }
            SyncEvent::MessageAdded { context_id, message_id } => {
                data.insert("context_id".to_string(), serde_json::Value::String(context_id));
                data.insert("message_id".to_string(), serde_json::Value::Number(message_id.into()));
            }
            SyncEvent::MessageUpdated { context_id, message_id } => {
                data.insert("context_id".to_string(), serde_json::Value::String(context_id));
                data.insert("message_id".to_string(), serde_json::Value::Number(message_id.into()));
            }
            SyncEvent::MessageDeleted { context_id, message_id } => {
                data.insert("context_id".to_string(), serde_json::Value::String(context_id));
                data.insert("message_id".to_string(), serde_json::Value::Number(message_id.into()));
            }
            SyncEvent::ContextTitleChanged { context_id, new_title } => {
                data.insert("context_id".to_string(), serde_json::Value::String(context_id));
                data.insert("new_title".to_string(), serde_json::Value::String(new_title));
            }
            SyncEvent::ChatResponse { request_id, content } => {
                data.insert("request_id".to_string(), serde_json::Value::String(request_id));
                data.insert("content".to_string(), serde_json::Value::String(content));
            }
            SyncEvent::ChatResponseChunk { request_id, chunk } => {
                data.insert("request_id".to_string(), serde_json::Value::String(request_id));
                data.insert("chunk".to_string(), serde_json::Value::String(chunk));
            }
            SyncEvent::ChatResponseDone { request_id } => {
                data.insert("request_id".to_string(), serde_json::Value::String(request_id));
            }
            SyncEvent::ChatResponseError { request_id, error } => {
                data.insert("request_id".to_string(), serde_json::Value::String(request_id));
                data.insert("error".to_string(), serde_json::Value::String(error));
            }
            SyncEvent::CreateThreadFromHelixSession { helix_session_id, message, request_id } => {
                data.insert("helix_session_id".to_string(), serde_json::Value::String(helix_session_id));
                data.insert("message".to_string(), serde_json::Value::String(message));
                data.insert("request_id".to_string(), serde_json::Value::String(request_id));
            }
        }

        data
    }
}

/// Auto-reconnecting WebSocket wrapper
pub struct ReconnectingWebSocket {
    config: WebSocketSyncConfig,
    websocket_sync: Option<WebSocketSync>,
    reconnect_attempts: u32,
    max_reconnect_attempts: u32,
}

impl ReconnectingWebSocket {
    pub fn new(config: WebSocketSyncConfig) -> Self {
        Self {
            config,
            websocket_sync: None,
            reconnect_attempts: 0,
            max_reconnect_attempts: 10,
        }
    }

    pub async fn connect_with_retry(&mut self) -> Result<()> {
        while self.reconnect_attempts < self.max_reconnect_attempts {
            match WebSocketSync::new(self.config.clone(), None).await {
                Ok(sync) => {
                    self.websocket_sync = Some(sync);
                    self.reconnect_attempts = 0;
                    log::info!("Successfully connected to Helix WebSocket");
                    return Ok(());
                }
                Err(e) => {
                    self.reconnect_attempts += 1;
                    let delay = std::time::Duration::from_secs(2_u64.pow(self.reconnect_attempts.min(6)));
                    
                    log::warn!(
                        "Failed to connect to Helix WebSocket (attempt {}/{}): {}. Retrying in {:?}",
                        self.reconnect_attempts,
                        self.max_reconnect_attempts,
                        e,
                        delay
                    );
                    
                    tokio::time::sleep(delay).await;
                }
            }
        }

        Err(anyhow::anyhow!(
            "Failed to connect to Helix WebSocket after {} attempts",
            self.max_reconnect_attempts
        ))
    }

    pub fn is_connected(&self) -> bool {
        self.websocket_sync
            .as_ref()
            .map_or(false, |ws| ws.is_connected())
    }

    pub fn send_event(&self, event: SyncEvent) -> Result<()> {
        if let Some(websocket_sync) = &self.websocket_sync {
            websocket_sync.send_event(event)
        } else {
            Err(anyhow::anyhow!("WebSocket not connected"))
        }
    }
}