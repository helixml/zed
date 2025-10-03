//! WebSocket protocol implementation for external agent control
//!
//! Per WEBSOCKET_PROTOCOL_SPEC.md:
//! - Zed is stateless - only knows acp_thread_id
//! - External system maintains all session mapping
//! - Protocol: chat_message → thread_created, message_added*, message_completed

use anyhow::{Context as AnyhowContext, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

use crate::types::{IncomingChatMessage, SyncEvent};
use crate::ThreadCreationRequest;

/// WebSocket configuration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSocketSyncConfig {
    pub enabled: bool,
    pub url: String,
    pub auth_token: String,
    pub use_tls: bool,
}

impl Default for WebSocketSyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "localhost:8080".to_string(),
            auth_token: String::new(),
            use_tls: false,
        }
    }
}

/// WebSocket sync service - runs independently of UI
pub struct WebSocketSync {
    outgoing_tx: mpsc::UnboundedSender<SyncEvent>,
}

impl WebSocketSync {
    /// Start WebSocket service
    pub async fn start(config: WebSocketSyncConfig) -> Result<Self> {
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<SyncEvent>();

        // Build WebSocket URL
        let protocol = if config.use_tls { "wss" } else { "ws" };
        let url = format!("{}://{}", protocol, config.url);
        let url = Url::parse(&url).context("Invalid WebSocket URL")?;

        log::info!("🔗 Starting WebSocket connection to {}", url);

        // Connect to WebSocket
        let (ws_stream, _) = connect_async(url.as_str()).await
            .context("Failed to connect to WebSocket server")?;

        log::info!("✅ WebSocket connected");

        let (mut ws_sink, mut ws_stream) = ws_stream.split();

        // Spawn task to send outgoing events
        tokio::spawn(async move {
            while let Some(event) = outgoing_rx.recv().await {
                let json = match serde_json::to_string(&event) {
                    Ok(j) => j,
                    Err(e) => {
                        log::error!("Failed to serialize event: {}", e);
                        continue;
                    }
                };

                log::info!("📤 Sending: {}", json);

                if let Err(e) = ws_sink.send(Message::Text(json.into())).await {
                    log::error!("Failed to send WebSocket message: {}", e);
                    break;
                }
            }
        });

        // Spawn task to handle incoming messages
        tokio::spawn(async move {
            while let Some(msg) = ws_stream.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        log::info!("📥 Received: {}", text);

                        if let Err(e) = Self::handle_incoming_message(&text).await {
                            log::error!("Failed to handle message: {}", e);
                        }
                    }
                    Ok(Message::Close(_)) => {
                        log::info!("WebSocket closed");
                        break;
                    }
                    Err(e) => {
                        log::error!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok(Self { outgoing_tx })
    }

    /// Handle incoming chat_message from external system
    async fn handle_incoming_message(text: &str) -> Result<()> {
        // Parse as generic command first
        #[derive(Deserialize)]
        struct Command {
            #[serde(rename = "type")]
            command_type: String,
            data: IncomingChatMessage,
        }

        let command: Command = serde_json::from_str(text)
            .context("Failed to parse incoming message")?;

        if command.command_type != "chat_message" {
            log::warn!("Unknown command type: {}", command.command_type);
            return Ok(());
        }

        log::info!("💬 Processing chat_message: acp_thread_id={:?}, request_id={}",
                   command.data.acp_thread_id, command.data.request_id);

        // Request thread creation via callback
        let request = ThreadCreationRequest {
            acp_thread_id: command.data.acp_thread_id,
            message: command.data.message,
            request_id: command.data.request_id,
        };

        crate::request_thread_creation(request)?;

        Ok(())
    }

    /// Send event to external system
    pub fn send_event(&self, event: SyncEvent) -> Result<()> {
        self.outgoing_tx.send(event)
            .map_err(|_| anyhow::anyhow!("Failed to send event"))
    }
}

/// Global WebSocket service instance
static WEBSOCKET_SERVICE: parking_lot::Mutex<Option<Arc<WebSocketSync>>> =
    parking_lot::Mutex::new(None);

/// Initialize global WebSocket service
pub fn init_websocket_service(config: WebSocketSyncConfig) {
    tokio::spawn(async move {
        match WebSocketSync::start(config).await {
            Ok(service) => {
                *WEBSOCKET_SERVICE.lock() = Some(Arc::new(service));
                log::info!("✅ WebSocket service initialized");
            }
            Err(e) => {
                log::error!("❌ Failed to start WebSocket service: {}", e);
            }
        }
    });
}

/// Get global WebSocket service
pub fn get_websocket_service() -> Option<Arc<WebSocketSync>> {
    WEBSOCKET_SERVICE.lock().clone()
}

/// Send event via global service
pub fn send_websocket_event(event: SyncEvent) -> Result<()> {
    if let Some(service) = get_websocket_service() {
        service.send_event(event)
    } else {
        Err(anyhow::anyhow!("WebSocket service not initialized"))
    }
}
