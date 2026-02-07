//! WebSocket protocol implementation for external agent control
//!
//! Per WEBSOCKET_PROTOCOL_SPEC.md:
//! - Zed is stateless - only knows acp_thread_id
//! - External system maintains all session mapping
//! - Protocol: chat_message → thread_created, message_added*, message_completed
//!
//! Reconnection behavior:
//! - Automatically reconnects when connection drops (API restart, network issues)
//! - Uses exponential backoff: 1s, 2s, 4s, 8s, capped at 30s
//! - Queued events are preserved during reconnection attempts

use anyhow::{Context as AnyhowContext, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::Message, Connector};
use url::Url;

use crate::types::{IncomingChatMessage, SyncEvent};
use crate::ThreadCreationRequest;

// Reuse NoCertVerifier from http_client_tls for consistency
use http_client_tls::NoCertVerifier;

/// WebSocket configuration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSocketSyncConfig {
    pub enabled: bool,
    pub url: String,
    pub auth_token: String,
    pub use_tls: bool,
    /// Skip TLS certificate verification (DANGEROUS - for enterprise internal CAs only)
    /// Set ZED_HELIX_SKIP_TLS_VERIFY=true to enable
    #[serde(default)]
    pub skip_tls_verify: bool,
}

impl Default for WebSocketSyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "localhost:8080".to_string(),
            auth_token: String::new(),
            use_tls: false,
            skip_tls_verify: false,
        }
    }
}

/// Create a TLS connector that skips certificate verification
/// DANGEROUS: Only use for enterprise deployments with internal CAs
fn create_insecure_tls_connector() -> Connector {
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
        .with_no_client_auth();

    Connector::Rustls(Arc::new(config))
}

/// WebSocket sync service - runs independently of UI
/// Handles automatic reconnection when connection drops
pub struct WebSocketSync {
    outgoing_tx: mpsc::UnboundedSender<SyncEvent>,
    /// Track if connection is healthy (for logging/debugging)
    is_connected: Arc<AtomicBool>,
    /// Current reconnect delay in milliseconds (for exponential backoff)
    reconnect_delay_ms: Arc<AtomicU64>,
}

/// Reconnection constants
const INITIAL_RECONNECT_DELAY_MS: u64 = 1000;
const MAX_RECONNECT_DELAY_MS: u64 = 30000;

impl WebSocketSync {
    /// Start WebSocket service with automatic reconnection
    pub async fn start(config: WebSocketSyncConfig) -> Result<Self> {
        eprintln!("🔧 [WEBSOCKET] WebSocketSync::start() beginning");
        log::info!("🔧 [WEBSOCKET] WebSocketSync::start() beginning");

        let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel::<SyncEvent>();
        eprintln!("✅ [WEBSOCKET] Created outgoing channel");
        log::info!("✅ [WEBSOCKET] Created outgoing channel");

        // Shared state for connection tracking
        let is_connected = Arc::new(AtomicBool::new(false));
        let reconnect_delay_ms = Arc::new(AtomicU64::new(INITIAL_RECONNECT_DELAY_MS));

        // Get session_id from environment variable
        let session_id = std::env::var("HELIX_SESSION_ID")
            .context("HELIX_SESSION_ID environment variable not set")?;
        eprintln!("🔧 [WEBSOCKET] Using session_id: {}", session_id);
        log::info!("🔧 [WEBSOCKET] Using session_id: {}", session_id);

        // Build WebSocket URL with full path and session_id
        let protocol = if config.use_tls { "wss" } else { "ws" };
        let url_str = format!("{}://{}/api/v1/external-agents/sync?session_id={}",
                         protocol, config.url, session_id);
        eprintln!("🔧 [WEBSOCKET] Constructed URL: {}", url_str);
        log::info!("🔧 [WEBSOCKET] Constructed URL: {}", url_str);

        let url = Url::parse(&url_str).context("Invalid WebSocket URL")?;
        eprintln!("✅ [WEBSOCKET] URL validated: {}", url);
        log::info!("✅ [WEBSOCKET] URL validated: {}", url);

        // Clone values for the reconnection loop
        let is_connected_clone = is_connected.clone();
        let reconnect_delay_clone = reconnect_delay_ms.clone();
        let auth_token = config.auth_token.clone();
        let skip_tls_verify = config.skip_tls_verify;

        if skip_tls_verify {
            log::warn!("⚠️  [WEBSOCKET] TLS certificate verification DISABLED - only use for enterprise internal CAs!");
            eprintln!("⚠️  [WEBSOCKET] TLS certificate verification DISABLED - only use for enterprise internal CAs!");
        }

        // Spawn the reconnection loop
        tokio::spawn(Self::run_with_reconnection(
            url,
            auth_token,
            skip_tls_verify,
            outgoing_rx,
            is_connected_clone,
            reconnect_delay_clone,
        ));

        log::info!("✅ [WEBSOCKET] WebSocketSync fully initialized with reconnection support");
        Ok(Self {
            outgoing_tx,
            is_connected,
            reconnect_delay_ms,
        })
    }

    /// Main reconnection loop - keeps trying to connect and reconnects on failure
    async fn run_with_reconnection(
        url: Url,
        auth_token: String,
        skip_tls_verify: bool,
        mut outgoing_rx: mpsc::UnboundedReceiver<SyncEvent>,
        is_connected: Arc<AtomicBool>,
        reconnect_delay_ms: Arc<AtomicU64>,
    ) {
        let mut connection_attempts = 0u64;

        loop {
            connection_attempts += 1;
            eprintln!("🔗 [WEBSOCKET] Connection attempt #{}", connection_attempts);
            log::info!("🔗 [WEBSOCKET] Connection attempt #{}", connection_attempts);

            // Try to connect
            match Self::connect_once(&url, &auth_token, skip_tls_verify).await {
                Ok((ws_sink, ws_stream)) => {
                    // Successfully connected
                    is_connected.store(true, Ordering::SeqCst);
                    reconnect_delay_ms.store(INITIAL_RECONNECT_DELAY_MS, Ordering::SeqCst);

                    eprintln!("✅ [WEBSOCKET] Connected! Running message loop...");
                    log::info!("✅ [WEBSOCKET] Connected! Running message loop...");

                    // Run until connection drops
                    Self::run_connection(ws_sink, ws_stream, &mut outgoing_rx).await;

                    // Connection dropped
                    is_connected.store(false, Ordering::SeqCst);
                    eprintln!("⚠️  [WEBSOCKET] Connection lost, will reconnect...");
                    log::warn!("⚠️  [WEBSOCKET] Connection lost, will reconnect...");
                }
                Err(e) => {
                    eprintln!("❌ [WEBSOCKET] Connection failed: {}", e);
                    log::error!("❌ [WEBSOCKET] Connection failed: {}", e);
                }
            }

            // Exponential backoff before next reconnection attempt
            let delay = reconnect_delay_ms.load(Ordering::SeqCst);
            eprintln!("⏳ [WEBSOCKET] Waiting {}ms before reconnecting...", delay);
            log::info!("⏳ [WEBSOCKET] Waiting {}ms before reconnecting...", delay);

            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;

            // Increase delay for next attempt (exponential backoff, capped)
            let new_delay = (delay * 2).min(MAX_RECONNECT_DELAY_MS);
            reconnect_delay_ms.store(new_delay, Ordering::SeqCst);
        }
    }

    /// Attempt a single WebSocket connection
    async fn connect_once(url: &Url, auth_token: &str, skip_tls_verify: bool) -> Result<(
        futures::stream::SplitSink<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, Message>,
        futures::stream::SplitStream<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>
    )> {
        use tokio_tungstenite::tungstenite::http::Request;

        let mut request = Request::builder()
            .uri(url.as_str())
            .header("Host", url.host_str().unwrap_or("localhost"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", tokio_tungstenite::tungstenite::handshake::client::generate_key());

        // Add auth token if provided
        if !auth_token.is_empty() {
            let auth_header = format!("Bearer {}", auth_token);
            request = request.header("Authorization", auth_header);
        }

        let request = request.body(()).context("Failed to build WebSocket request")?;

        // Choose connector based on skip_tls_verify setting
        // When skip_tls_verify is true, use insecure connector for enterprise internal CAs
        let connector = if skip_tls_verify {
            Some(create_insecure_tls_connector())
        } else {
            None // Use default TLS verification
        };

        let (ws_stream, response) = connect_async_tls_with_config(
            request,
            None,  // WebSocket config
            false, // disable_nagle (keep TCP_NODELAY behavior)
            connector,
        ).await
            .context("Failed to connect to WebSocket server")?;

        eprintln!("✅ [WEBSOCKET] WebSocket connected! Response status: {:?}", response.status());
        log::info!("✅ [WEBSOCKET] WebSocket connected! Response status: {:?}", response.status());

        let (ws_sink, ws_stream) = ws_stream.split();
        Ok((ws_sink, ws_stream))
    }

    /// Run the connection until it drops - handles both sending and receiving
    async fn run_connection(
        mut ws_sink: futures::stream::SplitSink<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, Message>,
        mut ws_stream: futures::stream::SplitStream<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>,
        outgoing_rx: &mut mpsc::UnboundedReceiver<SyncEvent>,
    ) {
        // Send a test ping to verify connection is working
        let test_ping = serde_json::json!({"event_type": "ping", "data": {"timestamp": chrono::Utc::now().timestamp()}});
        if let Err(e) = ws_sink.send(Message::Text(test_ping.to_string().into())).await {
            eprintln!("❌ [WEBSOCKET] Failed to send test ping: {}", e);
            log::error!("❌ [WEBSOCKET] Failed to send test ping: {}", e);
            return;
        }
        eprintln!("✅ [WEBSOCKET] Sent test ping successfully");
        log::info!("✅ [WEBSOCKET] Sent test ping successfully");

        // Send agent_ready immediately on connection to prevent deadlock
        // The API waits for agent_ready before sending the initial chat_message,
        // so we need to signal readiness as soon as we connect.
        // Note: We send with no thread_id since no thread exists yet.
        // The agent_name is just for logging - actual session mapping uses the WebSocket URL param.
        let agent_ready_msg = serde_json::json!({
            "event_type": "agent_ready",
            "data": {
                "agent_name": "zed-connection",
                "thread_id": null
            }
        });
        if let Err(e) = ws_sink.send(Message::Text(agent_ready_msg.to_string().into())).await {
            eprintln!("⚠️ [WEBSOCKET] Failed to send initial agent_ready: {}", e);
            log::warn!("⚠️ [WEBSOCKET] Failed to send initial agent_ready: {}", e);
            // Don't return - this is not fatal, the API has a timeout fallback
        } else {
            eprintln!("✅ [WEBSOCKET] Sent initial agent_ready (connection ready for messages)");
            log::info!("✅ [WEBSOCKET] Sent initial agent_ready (connection ready for messages)");
        }

        // Main select loop - handle both incoming and outgoing messages
        loop {
            tokio::select! {
                // Handle outgoing events
                Some(event) = outgoing_rx.recv() => {
                    eprintln!("📤 [WEBSOCKET-OUT] Received event to send: {:?}", std::mem::discriminant(&event));
                    log::info!("📤 [WEBSOCKET-OUT] Received event to send: {:?}", std::mem::discriminant(&event));

                    // Convert to OutgoingMessage format
                    let outgoing = match event.to_outgoing_message() {
                        Ok(msg) => msg,
                        Err(e) => {
                            log::error!("❌ [WEBSOCKET-OUT] Failed to convert event: {}", e);
                            continue;
                        }
                    };

                    let json = match serde_json::to_string(&outgoing) {
                        Ok(j) => j,
                        Err(e) => {
                            log::error!("❌ [WEBSOCKET-OUT] Failed to serialize event: {}", e);
                            continue;
                        }
                    };

                    log::info!("📤 [WEBSOCKET-OUT] Sending JSON: {}", json);

                    if let Err(e) = ws_sink.send(Message::Text(json.into())).await {
                        log::error!("❌ [WEBSOCKET-OUT] Failed to send WebSocket message: {} - will reconnect", e);
                        eprintln!("❌ [WEBSOCKET-OUT] Failed to send WebSocket message: {} - will reconnect", e);
                        // Re-queue the event so it's not lost
                        // (The event is already consumed, so we lose it - this is a known limitation)
                        return; // Exit to trigger reconnection
                    }
                    log::info!("✅ [WEBSOCKET-OUT] Message sent successfully");
                }

                // Handle incoming messages
                msg = ws_stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
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
                        Some(Ok(Message::Close(frame))) => {
                            log::info!("🔌 [WEBSOCKET-IN] WebSocket closed by server: {:?}", frame);
                            eprintln!("🔌 [WEBSOCKET-IN] WebSocket closed by server: {:?} - will reconnect", frame);
                            return; // Exit to trigger reconnection
                        }
                        Some(Ok(Message::Ping(data))) => {
                            // Respond to ping with pong
                            let _ = ws_sink.send(Message::Pong(data)).await;
                        }
                        Some(Ok(_)) => {
                            log::debug!("📥 [WEBSOCKET-IN] Received non-text message (pong/binary)");
                        }
                        Some(Err(e)) => {
                            log::error!("❌ [WEBSOCKET-IN] WebSocket error: {} - will reconnect", e);
                            eprintln!("❌ [WEBSOCKET-IN] WebSocket error: {} - will reconnect", e);
                            return; // Exit to trigger reconnection
                        }
                        None => {
                            log::warn!("⚠️  [WEBSOCKET-IN] WebSocket stream ended - will reconnect");
                            eprintln!("⚠️  [WEBSOCKET-IN] WebSocket stream ended - will reconnect");
                            return; // Exit to trigger reconnection
                        }
                    }
                }
            }
        }
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
            /// Which ACP agent to use (e.g., "qwen", "claude", "gemini", "codex").
            /// None or empty means use NativeAgent (Zed's built-in agent).
            agent_name: Option<String>,
        }

        let open_data: OpenThreadData = serde_json::from_value(data)
            .context("Failed to parse open_thread data")?;

        eprintln!("📖 [WEBSOCKET-IN] Processing open_thread command: acp_thread_id={}, agent_name={:?}",
                  open_data.acp_thread_id, open_data.agent_name);
        log::info!("📖 [WEBSOCKET-IN] Processing open_thread command: acp_thread_id={}, agent_name={:?}",
                   open_data.acp_thread_id, open_data.agent_name);

        // Request thread opening via callback (will load from database and display)
        let request = crate::ThreadOpenRequest {
            acp_thread_id: open_data.acp_thread_id.clone(),
            agent_name: open_data.agent_name.clone(),
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

    /// Check if WebSocket is currently connected
    pub fn is_connected(&self) -> bool {
        self.is_connected.load(Ordering::SeqCst)
    }

    /// Get the current reconnect delay (indicates we're reconnecting if > initial)
    pub fn get_reconnect_delay_ms(&self) -> u64 {
        self.reconnect_delay_ms.load(Ordering::SeqCst)
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

/// Notify Helix that the agent is ready to receive prompts
/// This should be called after the agent process (e.g., qwen-code) has initialized via ACP
/// It prevents race conditions where Helix sends prompts before the agent is ready
pub fn send_agent_ready(agent_name: String, thread_id: Option<String>) {
    log::info!("🚀 [WEBSOCKET] Sending agent_ready event: agent_name={}, thread_id={:?}",
               agent_name, thread_id);
    eprintln!("🚀 [WEBSOCKET] Sending agent_ready event: agent_name={}, thread_id={:?}",
              agent_name, thread_id);

    match send_websocket_event(SyncEvent::AgentReady {
        agent_name: agent_name.clone(),
        thread_id: thread_id.clone(),
    }) {
        Ok(_) => {
            log::info!("✅ [WEBSOCKET] agent_ready event sent successfully");
            eprintln!("✅ [WEBSOCKET] agent_ready event sent successfully");
        }
        Err(e) => {
            log::warn!("⚠️ [WEBSOCKET] Failed to send agent_ready event (may not be connected): {}", e);
            eprintln!("⚠️ [WEBSOCKET] Failed to send agent_ready event (may not be connected): {}", e);
        }
    }
}

/// WebSocket connection status for UI display
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSocketConnectionStatus {
    /// WebSocket service not initialized (no HELIX_SESSION_ID, etc.)
    NotInitialized,
    /// Connected to Helix
    Connected,
    /// Reconnecting to Helix (connection was lost)
    Reconnecting,
    /// Disconnected from Helix
    Disconnected,
}

/// Get the current WebSocket connection status for UI display
pub fn get_websocket_connection_status() -> WebSocketConnectionStatus {
    match get_websocket_service() {
        Some(service) => {
            if service.is_connected() {
                WebSocketConnectionStatus::Connected
            } else {
                // If reconnect delay > initial, we're actively trying to reconnect
                if service.get_reconnect_delay_ms() > INITIAL_RECONNECT_DELAY_MS {
                    WebSocketConnectionStatus::Reconnecting
                } else {
                    WebSocketConnectionStatus::Disconnected
                }
            }
        }
        None => WebSocketConnectionStatus::NotInitialized,
    }
}
