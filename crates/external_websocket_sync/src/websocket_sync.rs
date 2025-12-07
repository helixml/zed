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
        eprintln!("🔧 [WEBSOCKET] WebSocketSync::start() beginning");
        log::info!("🔧 [WEBSOCKET] WebSocketSync::start() beginning");

        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<SyncEvent>();
        eprintln!("✅ [WEBSOCKET] Created outgoing channel");
        log::info!("✅ [WEBSOCKET] Created outgoing channel");

        // Get session_id from environment variable
        let session_id = std::env::var("HELIX_SESSION_ID")
            .context("HELIX_SESSION_ID environment variable not set")?;
        eprintln!("🔧 [WEBSOCKET] Using session_id: {}", session_id);
        log::info!("🔧 [WEBSOCKET] Using session_id: {}", session_id);

        // Build WebSocket URL with full path and session_id
        let protocol = if config.use_tls { "wss" } else { "ws" };
        let url = format!("{}://{}/api/v1/external-agents/sync?session_id={}",
                         protocol, config.url, session_id);
        eprintln!("🔧 [WEBSOCKET] Constructed URL: {}", url);
        log::info!("🔧 [WEBSOCKET] Constructed URL: {}", url);

        let url = Url::parse(&url).context("Invalid WebSocket URL")?;
        eprintln!("✅ [WEBSOCKET] URL validated: {}", url);
        log::info!("✅ [WEBSOCKET] URL validated: {}", url);

        eprintln!("🔗 [WEBSOCKET] Attempting connection to {}", url);
        log::info!("🔗 [WEBSOCKET] Attempting connection to {}", url);

        // Build WebSocket request with Authorization header
        use tokio_tungstenite::tungstenite::http::Request;

        let mut request = Request::builder()
            .uri(url.as_str())
            .header("Host", url.host_str().unwrap_or("localhost"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", tokio_tungstenite::tungstenite::handshake::client::generate_key());

        // Add auth token if provided
        if !config.auth_token.is_empty() {
            let auth_header = format!("Bearer {}", config.auth_token);
            eprintln!("🔐 [WEBSOCKET] Adding Authorization header");
            log::info!("🔐 [WEBSOCKET] Adding Authorization header");
            request = request.header("Authorization", auth_header);
        }

        let request = request.body(()).context("Failed to build WebSocket request")?;
        eprintln!("🔧 [WEBSOCKET] Built WebSocket request with headers");
        log::info!("🔧 [WEBSOCKET] Built WebSocket request with headers");

        // Connect to WebSocket
        let (ws_stream, response) = match connect_async(request).await {
            Ok(result) => {
                eprintln!("✅ [WEBSOCKET] connect_async() succeeded");
                log::info!("✅ [WEBSOCKET] connect_async() succeeded");
                result
            }
            Err(e) => {
                eprintln!("❌ [WEBSOCKET] connect_async() failed: {}", e);
                log::error!("❌ [WEBSOCKET] connect_async() failed: {}", e);
                return Err(anyhow::anyhow!("Failed to connect to WebSocket server: {}", e));
            }
        };

        eprintln!("✅ [WEBSOCKET] WebSocket connected! Response status: {:?}", response.status());
        log::info!("✅ [WEBSOCKET] WebSocket connected! Response status: {:?}", response.status());

        let (mut ws_sink, mut ws_stream) = ws_stream.split();
        eprintln!("✅ [WEBSOCKET] Stream split into sink/stream");
        log::info!("✅ [WEBSOCKET] Stream split into sink/stream");

        // Debug: write file BEFORE spawning task
        let _ = std::fs::write("/tmp/before_outgoing_spawn.txt", "About to spawn outgoing task\n");

        // Spawn task to send outgoing events
        let outgoing_task = tokio::spawn(async move {
            // Write to file to debug outgoing task
            let _ = std::fs::write("/tmp/websocket_outgoing_started.txt", "Outgoing task started\n");
            eprintln!("📤 [WEBSOCKET-OUT] Outgoing task started, waiting for events to send");
            log::info!("📤 [WEBSOCKET-OUT] Outgoing task started, waiting for events to send");

            // Send a test ping immediately to verify outgoing works
            let test_ping = serde_json::json!({"event_type": "ping", "data": {"timestamp": chrono::Utc::now().timestamp()}});
            let ping_result = ws_sink.send(Message::Text(test_ping.to_string().into())).await;
            let _ = std::fs::write("/tmp/websocket_ping_result.txt", format!("Ping result: {:?}\n", ping_result));

            if let Err(e) = ping_result {
                let _ = std::fs::write("/tmp/websocket_ping_error.txt", format!("Ping error: {}\n", e));
                eprintln!("❌ [WEBSOCKET-OUT] Failed to send test ping: {}", e);
                log::error!("❌ [WEBSOCKET-OUT] Failed to send test ping: {}", e);
            } else {
                let _ = std::fs::write("/tmp/websocket_ping_success.txt", "Ping sent successfully\n");
                eprintln!("✅ [WEBSOCKET-OUT] Sent test ping successfully");
                log::info!("✅ [WEBSOCKET-OUT] Sent test ping successfully");
            }

            let mut event_count = 0;
            while let Some(event) = outgoing_rx.recv().await {
                event_count += 1;
                let _ = std::fs::write("/tmp/websocket_event_received.txt", format!("Received event #{}\n", event_count));
                eprintln!("📤 [WEBSOCKET-OUT] Received event to send: {:?}", std::mem::discriminant(&event));
                log::info!("📤 [WEBSOCKET-OUT] Received event to send: {:?}", std::mem::discriminant(&event));

                // Convert to OutgoingMessage format
                let outgoing = match event.to_outgoing_message() {
                    Ok(msg) => msg,
                    Err(e) => {
                        let _ = std::fs::write("/tmp/websocket_conversion_error.txt", format!("Conversion error: {}\n", e));
                        log::error!("❌ [WEBSOCKET-OUT] Failed to convert event: {}", e);
                        continue;
                    }
                };

                let json = match serde_json::to_string(&outgoing) {
                    Ok(j) => j,
                    Err(e) => {
                        let _ = std::fs::write("/tmp/websocket_serialize_error.txt", format!("Serialize error: {}\n", e));
                        log::error!("❌ [WEBSOCKET-OUT] Failed to serialize event: {}", e);
                        continue;
                    }
                };

                let _ = std::fs::write("/tmp/websocket_sending.txt", format!("Sending: {}\n", json));
                log::info!("📤 [WEBSOCKET-OUT] Sending JSON: {}", json);

                if let Err(e) = ws_sink.send(Message::Text(json.into())).await {
                    let _ = std::fs::write("/tmp/websocket_send_error.txt", format!("Send error: {}\n", e));
                    log::error!("❌ [WEBSOCKET-OUT] Failed to send WebSocket message: {}", e);
                    break;
                } else {
                    let _ = std::fs::write("/tmp/websocket_send_success.txt", format!("Sent event #{} successfully\n", event_count));
                }
                log::info!("✅ [WEBSOCKET-OUT] Message sent successfully");
            }
            let _ = std::fs::write("/tmp/websocket_outgoing_exiting.txt", "Outgoing task exiting\n");
            log::warn!("⚠️  [WEBSOCKET-OUT] Outgoing task exiting");
        });

        eprintln!("✅ [WEBSOCKET] Outgoing task spawned");
        log::info!("✅ [WEBSOCKET] Outgoing task spawned");

        // Spawn task to handle incoming messages
        tokio::spawn(async move {
            eprintln!("📥 [WEBSOCKET-IN] Incoming task started, waiting for messages");
            log::info!("📥 [WEBSOCKET-IN] Incoming task started, waiting for messages");
            while let Some(msg) = ws_stream.next().await {
                eprintln!("📥 [WEBSOCKET-IN] Received WebSocket message");
                log::info!("📥 [WEBSOCKET-IN] Received WebSocket message");
                match msg {
                    Ok(Message::Text(text)) => {
                        eprintln!("📥 [WEBSOCKET-IN] Received text: {}", text);
                        log::info!("📥 [WEBSOCKET-IN] Received text: {}", text);

                        if let Err(e) = Self::handle_incoming_message(&text).await {
                            eprintln!("❌ [WEBSOCKET-IN] Failed to handle message: {}", e);
                            log::error!("❌ [WEBSOCKET-IN] Failed to handle message: {}", e);
                        } else {
                            eprintln!("✅ [WEBSOCKET-IN] Message handled successfully");
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

    /// Handle incoming messages from external system (chat_message or open_thread)
    async fn handle_incoming_message(text: &str) -> Result<()> {
        eprintln!("🔧 [WEBSOCKET-IN] handle_incoming_message() called with: {}", text);
        log::info!("🔧 [WEBSOCKET-IN] handle_incoming_message() called with: {}", text);

        // Parse as generic command first
        #[derive(Deserialize)]
        struct Command {
            #[serde(rename = "type")]
            command_type: String,
            data: serde_json::Value,
        }

        let command: Command = match serde_json::from_str(text) {
            Ok(cmd) => cmd,
            Err(e) => {
                eprintln!("❌ [WEBSOCKET-IN] Failed to parse incoming message: {}", e);
                log::error!("❌ [WEBSOCKET-IN] Failed to parse incoming message: {}", e);
                eprintln!("❌ [WEBSOCKET-IN] Raw message was: {}", text);
                log::error!("❌ [WEBSOCKET-IN] Raw message was: {}", text);
                return Err(anyhow::anyhow!("Failed to parse incoming message: {}", e));
            }
        };
        eprintln!("✅ [WEBSOCKET-IN] Parsed command type: {}", command.command_type);
        log::info!("✅ [WEBSOCKET-IN] Parsed command type: {}", command.command_type);

        match command.command_type.as_str() {
            "chat_message" => Self::handle_chat_message(command.data).await,
            "open_thread" => Self::handle_open_thread(command.data).await,
            _ => {
                eprintln!("⚠️  [WEBSOCKET-IN] Ignoring unknown command: {}", command.command_type);
                log::warn!("⚠️  [WEBSOCKET-IN] Ignoring unknown command: {}", command.command_type);
                Ok(())
            }
        }
    }

    /// Handle chat_message command (create/send to thread)
    async fn handle_chat_message(data: serde_json::Value) -> Result<()> {
        let chat_msg: IncomingChatMessage = serde_json::from_value(data)
            .context("Failed to parse chat_message data")?;

        // CRITICAL: Ignore echoed user messages from Helix (they have role="user")
        // Helix broadcasts user messages back via WebSocket for UI sync, but we already processed the original
        if chat_msg.role.as_deref() == Some("user") {
            eprintln!("🔄 [WEBSOCKET-IN] Ignoring echoed user message (role=user) - already processed original");
            log::info!("🔄 [WEBSOCKET-IN] Ignoring echoed user message (role=user) - already processed original");
            return Ok(());
        }

        eprintln!("💬 [WEBSOCKET-IN] Processing chat_message: acp_thread_id={:?}, request_id={}, message_len={}",
                   chat_msg.acp_thread_id, chat_msg.request_id, chat_msg.message.len());
        log::info!("💬 [WEBSOCKET-IN] Processing chat_message: acp_thread_id={:?}, request_id={}, message_len={}",
                   chat_msg.acp_thread_id, chat_msg.request_id, chat_msg.message.len());

        // Request thread creation via callback
        let request = ThreadCreationRequest {
            acp_thread_id: chat_msg.acp_thread_id.clone(),
            message: chat_msg.message.clone(),
            request_id: chat_msg.request_id.clone(),
            agent_name: chat_msg.agent_name.clone(),
        };

        eprintln!("🎯 [WEBSOCKET-IN] Calling request_thread_creation()...");
        log::info!("🎯 [WEBSOCKET-IN] Calling request_thread_creation()...");
        crate::request_thread_creation(request)?;
        eprintln!("✅ [WEBSOCKET-IN] request_thread_creation() succeeded");
        log::info!("✅ [WEBSOCKET-IN] request_thread_creation() succeeded");

        Ok(())
    }

    /// Handle open_thread command (open existing thread in UI)
    async fn handle_open_thread(data: serde_json::Value) -> Result<()> {
        #[derive(Deserialize)]
        struct OpenThreadData {
            acp_thread_id: String,
        }

        let open_data: OpenThreadData = serde_json::from_value(data)
            .context("Failed to parse open_thread data")?;

        eprintln!("📖 [WEBSOCKET-IN] Processing open_thread command: acp_thread_id={}", open_data.acp_thread_id);
        log::info!("📖 [WEBSOCKET-IN] Processing open_thread command: acp_thread_id={}", open_data.acp_thread_id);

        // Request thread opening via callback (will load from database and display)
        let request = crate::ThreadOpenRequest {
            acp_thread_id: open_data.acp_thread_id.clone(),
        };

        eprintln!("🎯 [WEBSOCKET-IN] Calling request_thread_open()...");
        log::info!("🎯 [WEBSOCKET-IN] Calling request_thread_open()...");
        crate::request_thread_open(request)?;
        eprintln!("✅ [WEBSOCKET-IN] request_thread_open() succeeded");
        log::info!("✅ [WEBSOCKET-IN] request_thread_open() succeeded");

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
    let _ = std::fs::write("/tmp/init_websocket_service_called.txt", format!("init_websocket_service called with URL: {}\n", config.url));
    eprintln!("🔧 [WEBSOCKET] init_websocket_service() called with URL: {}", config.url);
    log::info!("🔧 [WEBSOCKET] init_websocket_service() called with URL: {}", config.url);

    // WebSocket uses tokio_tungstenite which requires Tokio runtime
    // Create a dedicated runtime for the WebSocket service
    std::thread::spawn(move || {
        eprintln!("🧵 [WEBSOCKET] Spawned dedicated thread for WebSocket");
        log::info!("🧵 [WEBSOCKET] Spawned dedicated thread for WebSocket");

        let rt = match tokio::runtime::Runtime::new() {
            Ok(r) => {
                eprintln!("✅ [WEBSOCKET] Created Tokio runtime");
                log::info!("✅ [WEBSOCKET] Created Tokio runtime");
                r
            }
            Err(e) => {
                eprintln!("❌ [WEBSOCKET] Failed to create Tokio runtime: {}", e);
                log::error!("❌ [WEBSOCKET] Failed to create Tokio runtime: {}", e);
                return;
            }
        };

        rt.block_on(async move {
            let _ = std::fs::write("/tmp/tokio_runtime_started.txt", "Tokio runtime started\n");
            eprintln!("🔌 [WEBSOCKET] Starting WebSocket service with Tokio runtime");
            log::info!("🔌 [WEBSOCKET] Starting WebSocket service with Tokio runtime");
            eprintln!("🔌 [WEBSOCKET] Config: enabled={}, url={}, use_tls={}",
                      config.enabled, config.url, config.use_tls);
            log::info!("🔌 [WEBSOCKET] Config: enabled={}, url={}, use_tls={}",
                      config.enabled, config.url, config.use_tls);

            eprintln!("🔌 [WEBSOCKET] About to call WebSocketSync::start()...");
            let _ = std::fs::write("/tmp/before_websocket_start.txt", "Before WebSocketSync::start()\n");
            match WebSocketSync::start(config).await {
                Ok(service) => {
                    let _ = std::fs::write("/tmp/websocket_start_succeeded.txt", "WebSocketSync::start() succeeded\n");
                    eprintln!("✅ [WEBSOCKET] WebSocketSync::start() succeeded");
                    log::info!("✅ [WEBSOCKET] WebSocketSync::start() succeeded");
                    *WEBSOCKET_SERVICE.lock() = Some(Arc::new(service));
                    let _ = std::fs::write("/tmp/websocket_service_stored.txt", "Service stored globally\n");
                    eprintln!("✅ [WEBSOCKET] WebSocket service initialized and stored globally");
                    log::info!("✅ [WEBSOCKET] WebSocket service initialized and stored globally");
                }
                Err(e) => {
                    eprintln!("❌ [WEBSOCKET] Failed to start WebSocket service: {}", e);
                    log::error!("❌ [WEBSOCKET] Failed to start WebSocket service: {}", e);
                    eprintln!("❌ [WEBSOCKET] Error details: {:?}", e);
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
    eprintln!("🔍 [WEBSOCKET] send_websocket_event() called with event type: {:?}", std::mem::discriminant(&event));
    if let Some(service) = get_websocket_service() {
        eprintln!("✅ [WEBSOCKET] Found WebSocket service, calling send_event()");
        let result = service.send_event(event);
        match &result {
            Ok(_) => eprintln!("✅ [WEBSOCKET] send_event() returned Ok"),
            Err(e) => eprintln!("❌ [WEBSOCKET] send_event() returned Err: {}", e),
        }
        result
    } else {
        eprintln!("❌ [WEBSOCKET] WebSocket service not initialized!");
        Err(anyhow::anyhow!("WebSocket service not initialized"))
    }
}
