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
        log::info!("🔧 [WEBSOCKET] WebSocketSync::start() beginning");

        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<SyncEvent>();
        log::info!("✅ [WEBSOCKET] Created outgoing channel");

        // Build WebSocket URL
        let protocol = if config.use_tls { "wss" } else { "ws" };
        let url = format!("{}://{}", protocol, config.url);
        log::info!("🔧 [WEBSOCKET] Parsed URL: {}", url);

        let url = Url::parse(&url).context("Invalid WebSocket URL")?;
        log::info!("✅ [WEBSOCKET] URL validated: {}", url);

        log::info!("🔗 [WEBSOCKET] Attempting connection to {}", url);

        // Connect to WebSocket
        let (ws_stream, response) = connect_async(url.as_str()).await
            .context("Failed to connect to WebSocket server")?;

        log::info!("✅ [WEBSOCKET] WebSocket connected! Response status: {:?}", response.status());

        let (mut ws_sink, mut ws_stream) = ws_stream.split();
        log::info!("✅ [WEBSOCKET] Stream split into sink/stream");

        // Spawn task to send outgoing events
        tokio::spawn(async move {
            log::info!("📤 [WEBSOCKET-OUT] Outgoing task started, waiting for events to send");
            while let Some(event) = outgoing_rx.recv().await {
                log::info!("📤 [WEBSOCKET-OUT] Received event to send: {:?}", std::mem::discriminant(&event));

                let json = match serde_json::to_string(&event) {
                    Ok(j) => j,
                    Err(e) => {
                        log::error!("❌ [WEBSOCKET-OUT] Failed to serialize event: {}", e);
                        continue;
                    }
                };

                log::info!("📤 [WEBSOCKET-OUT] Sending JSON: {}", json);

                if let Err(e) = ws_sink.send(Message::Text(json.into())).await {
                    log::error!("❌ [WEBSOCKET-OUT] Failed to send WebSocket message: {}", e);
                    break;
                }
                log::info!("✅ [WEBSOCKET-OUT] Message sent successfully");
            }
            log::warn!("⚠️  [WEBSOCKET-OUT] Outgoing task exiting");
        });
        log::info!("✅ [WEBSOCKET] Outgoing task spawned");

        // Spawn task to handle incoming messages
        tokio::spawn(async move {
            log::info!("📥 [WEBSOCKET-IN] Incoming task started, waiting for messages");
            while let Some(msg) = ws_stream.next().await {
                log::info!("📥 [WEBSOCKET-IN] Received WebSocket message");
                match msg {
                    Ok(Message::Text(text)) => {
                        log::info!("📥 [WEBSOCKET-IN] Received text: {}", text);

                        if let Err(e) = Self::handle_incoming_message(&text).await {
                            log::error!("❌ [WEBSOCKET-IN] Failed to handle message: {}", e);
                        } else {
                            log::info!("✅ [WEBSOCKET-IN] Message handled successfully");
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        log::info!("🔌 [WEBSOCKET-IN] WebSocket closed: {:?}", frame);
                        break;
                    }
                    Err(e) => {
                        log::error!("❌ [WEBSOCKET-IN] WebSocket error: {}", e);
                        break;
                    }
                    _ => {
                        log::debug!("📥 [WEBSOCKET-IN] Received non-text message (ping/pong/binary)");
                    }
                }
            }
            log::warn!("⚠️  [WEBSOCKET-IN] Incoming task exiting");
        });
        log::info!("✅ [WEBSOCKET] Incoming task spawned");

        log::info!("✅ [WEBSOCKET] WebSocketSync fully initialized");
        Ok(Self { outgoing_tx })
    }

    /// Handle incoming chat_message from external system
    async fn handle_incoming_message(text: &str) -> Result<()> {
        log::info!("🔧 [WEBSOCKET-IN] handle_incoming_message() called with: {}", text);

        // Parse as generic command first
        #[derive(Deserialize)]
        struct Command {
            #[serde(rename = "type")]
            command_type: String,
            data: IncomingChatMessage,
        }

        let command: Command = serde_json::from_str(text)
            .context("Failed to parse incoming message")?;
        log::info!("✅ [WEBSOCKET-IN] Parsed command type: {}", command.command_type);

        if command.command_type != "chat_message" {
            log::warn!("⚠️  [WEBSOCKET-IN] Ignoring non-chat command: {}", command.command_type);
            return Ok(());
        }

        log::info!("💬 [WEBSOCKET-IN] Processing chat_message: acp_thread_id={:?}, request_id={}, message_len={}",
                   command.data.acp_thread_id, command.data.request_id, command.data.message.len());

        // Request thread creation via callback
        let request = ThreadCreationRequest {
            acp_thread_id: command.data.acp_thread_id.clone(),
            message: command.data.message.clone(),
            request_id: command.data.request_id.clone(),
        };

        log::info!("🎯 [WEBSOCKET-IN] Calling request_thread_creation()...");
        crate::request_thread_creation(request)?;
        log::info!("✅ [WEBSOCKET-IN] request_thread_creation() succeeded");

        Ok(())
    }

    /// Send event to external system
    pub fn send_event(&self, event: SyncEvent) -> Result<()> {
        self.outgoing_tx.send(event)
            .map_err(|_| anyhow::anyhow!("Failed to send event"))
    }
}

/// Global WebSocket service instance
pub(crate) static WEBSOCKET_SERVICE: parking_lot::Mutex<Option<Arc<WebSocketSync>>> =
    parking_lot::Mutex::new(None);

/// Initialize global WebSocket service
pub fn init_websocket_service(config: WebSocketSyncConfig) {
    log::info!("🔧 [WEBSOCKET] init_websocket_service() called with URL: {}", config.url);

    // WebSocket uses tokio_tungstenite which requires Tokio runtime
    // Create a dedicated runtime for the WebSocket service
    std::thread::spawn(move || {
        log::info!("🧵 [WEBSOCKET] Spawned dedicated thread for WebSocket");

        let rt = match tokio::runtime::Runtime::new() {
            Ok(r) => {
                log::info!("✅ [WEBSOCKET] Created Tokio runtime");
                r
            }
            Err(e) => {
                log::error!("❌ [WEBSOCKET] Failed to create Tokio runtime: {}", e);
                return;
            }
        };

        rt.block_on(async move {
            log::info!("🔌 [WEBSOCKET] Starting WebSocket service with Tokio runtime");
            log::info!("🔌 [WEBSOCKET] Config: enabled={}, url={}, use_tls={}",
                      config.enabled, config.url, config.use_tls);

            match WebSocketSync::start(config).await {
                Ok(service) => {
                    *WEBSOCKET_SERVICE.lock() = Some(Arc::new(service));
                    log::info!("✅ [WEBSOCKET] WebSocket service initialized and stored globally");
                }
                Err(e) => {
                    log::error!("❌ [WEBSOCKET] Failed to start WebSocket service: {}", e);
                    log::error!("❌ [WEBSOCKET] Error details: {:?}", e);
                    return;
                }
            }

            // Keep runtime alive
            log::info!("🔌 [WEBSOCKET] WebSocket runtime active and waiting for messages");
            std::future::pending::<()>().await;
        });
    });

    log::info!("✅ [WEBSOCKET] WebSocket thread spawned");
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
