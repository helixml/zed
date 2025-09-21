//! WebSocket sync client for connecting Zed to Helix

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream, MaybeTlsStream};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use url::Url;
use parking_lot::RwLock;
use std::sync::Arc;

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
    pub async fn new(config: WebSocketSyncConfig) -> Result<Self> {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let (command_sender, command_receiver) = mpsc::unbounded_channel();
        
        let is_connected = Arc::new(RwLock::new(false));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let mut sync = Self {
            config: config.clone(),
            event_sender,
            command_receiver: Arc::new(RwLock::new(Some(command_receiver))),
            is_connected: is_connected.clone(),
            shutdown_tx: Some(shutdown_tx),
        };

        // Start WebSocket connection task
        sync.start_connection_task(config, event_receiver, command_sender, is_connected.clone(), shutdown_rx).await?;

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
    ) -> Result<()> {
        let protocol = if config.use_tls { "wss" } else { "ws" };
        let url = format!(
            "{}://{}/api/v1/external-agents/sync?session_id={}",
            protocol,
            config.helix_url,
            config.session_id
        );

        log::info!("Starting WebSocket connection task for: {}", url);

        tokio::spawn(async move {
            let url = match Url::parse(&url) {
                Ok(url) => url,
                Err(e) => {
                    log::error!("Invalid WebSocket URL: {}", e);
                    return;
                }
            };

            // Try to connect with retries
            let websocket = match Self::connect_with_auth(&url, &config.auth_token).await {
                Ok(ws) => ws,
                Err(e) => {
                    log::error!("Failed to connect to WebSocket: {}", e);
                    return;
                }
            };

            *is_connected.write() = true;
            log::info!("Connected to Helix WebSocket");

            let (mut sink, mut stream) = websocket.split();

            // Handle outgoing events
            let event_task = {
                let is_connected = is_connected.clone();
                tokio::spawn(async move {
                    while let Some(event) = event_receiver.recv().await {
                        if !*is_connected.read() {
                            break;
                        }

                        let sync_message = SyncMessage {
                            session_id: config.session_id.clone(),
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

                        if let Err(e) = sink.send(Message::Text(message_text)).await {
                            log::error!("Failed to send WebSocket message: {}", e);
                            break;
                        }
                    }
                })
            };

            // Handle incoming messages
            let incoming_task = {
                let command_sender = command_sender.clone();
                let session_id = config.session_id.clone();
                tokio::spawn(async move {
                    while let Some(message) = stream.next().await {
                        match message {
                            Ok(Message::Text(text)) => {
                                if let Err(e) = Self::handle_incoming_message(&session_id, text, &command_sender).await {
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

    /// Connect with authentication
    async fn connect_with_auth(url: &Url, auth_token: &str) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        let request = if !auth_token.is_empty() {
            tungstenite::http::Request::builder()
                .uri(url.as_str())
                .header("Authorization", format!("Bearer {}", auth_token))
                .body(())
                .context("Failed to create WebSocket request")?
        } else {
            tungstenite::http::Request::builder()
                .uri(url.as_str())
                .body(())
                .context("Failed to create WebSocket request")?
        };

        let (websocket, _response) = connect_async(request)
            .await
            .context("Failed to connect to WebSocket")?;

        Ok(websocket)
    }

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
    async fn handle_incoming_message(
        session_id: &str, 
        text: String, 
        command_sender: &mpsc::UnboundedSender<HelixCommand>
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
            "Adding message to context {} from Helix: {} (role: {})",
            context_id,
            content,
            role
        );

        // TODO: Actually add the message to the Zed assistant context
        // This would require access to the HelixIntegration instance
        // and the assistant context store

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
    async fn handle_update_context_command(
        _session_id: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<()> {
        log::debug!("Update context command: {:?}", data);
        // TODO: Implement context updating
        Ok(())
    }

    /// Convert SyncEvent to event type string
    fn event_type_string(event: &SyncEvent) -> String {
        match event {
            SyncEvent::ContextCreated { .. } => "context_created".to_string(),
            SyncEvent::ContextDeleted { .. } => "context_deleted".to_string(),
            SyncEvent::MessageAdded { .. } => "message_added".to_string(),
            SyncEvent::MessageUpdated { .. } => "message_updated".to_string(),
            SyncEvent::MessageDeleted { .. } => "message_deleted".to_string(),
            SyncEvent::ContextTitleChanged { .. } => "context_title_changed".to_string(),
        }
    }

    /// Convert SyncEvent to data HashMap
    fn event_to_data(event: SyncEvent) -> HashMap<String, serde_json::Value> {
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
            match WebSocketSync::new(self.config.clone()).await {
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