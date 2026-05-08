//! External WebSocket Thread Sync for Zed Editor
//! 
//! This crate provides APIs for synchronizing Zed editor conversation threads
//! with external services via WebSocket connections, enabling real-time collaboration
//! and integration with AI platforms and other external tools.

use anyhow::Result;
use gpui::{App, EventEmitter, Global};
use tokio::sync::mpsc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use session::AppSession;
use std::sync::Arc;

mod websocket_sync;

mod thread_service;
pub use thread_service::*;

mod types;
pub use types::{ExternalAgent, *};

// mod sync;
// pub use sync::*;

mod mcp;
pub use mcp::*;

mod sync_settings;
pub use sync_settings::*;

// mod server;
// pub use server::*;

pub use websocket_sync::*;
pub use tungstenite;


/// Global WebSocket sender for sending responses back to external system
#[derive(Clone)]
pub struct WebSocketSender {
    pub sender: Arc<RwLock<Option<tokio::sync::mpsc::UnboundedSender<tungstenite::Message>>>>,
}

impl Default for WebSocketSender {
    fn default() -> Self {
        Self {
            sender: Arc::new(RwLock::new(None)),
        }
    }
}

impl Global for WebSocketSender {}

/// Static global for thread creation callback
static GLOBAL_THREAD_CREATION_CALLBACK: parking_lot::Mutex<Option<mpsc::UnboundedSender<ThreadCreationRequest>>> =
    parking_lot::Mutex::new(None);

/// Pending thread creation requests that arrived before callback was initialized
/// These get replayed when init_thread_creation_callback is called
static PENDING_THREAD_CREATION_REQUESTS: parking_lot::Mutex<Vec<ThreadCreationRequest>> =
    parking_lot::Mutex::new(Vec::new());

/// Pending thread open requests that arrived before callback was initialized
static PENDING_THREAD_OPEN_REQUESTS: parking_lot::Mutex<Vec<ThreadOpenRequest>> =
    parking_lot::Mutex::new(Vec::new());

/// Static global for thread display callback (notifies AgentPanel to auto-select thread)
static GLOBAL_THREAD_DISPLAY_CALLBACK: parking_lot::Mutex<Option<mpsc::UnboundedSender<ThreadDisplayNotification>>> =
    parking_lot::Mutex::new(None);

/// Pending thread display notifications that arrived before AgentPanel was ready
/// These get replayed when init_thread_display_callback is called
static PENDING_THREAD_DISPLAY_NOTIFICATIONS: parking_lot::Mutex<Vec<ThreadDisplayNotification>> =
    parking_lot::Mutex::new(Vec::new());

/// Static global for thread open callback (loads thread from database and displays it)
static GLOBAL_THREAD_OPEN_CALLBACK: parking_lot::Mutex<Option<mpsc::UnboundedSender<ThreadOpenRequest>>> =
    parking_lot::Mutex::new(None);

/// Static global for UI state query callback (queries AgentPanel's active view for E2E testing)
static GLOBAL_UI_STATE_QUERY_CALLBACK: parking_lot::Mutex<Option<mpsc::UnboundedSender<UiStateQueryRequest>>> =
    parking_lot::Mutex::new(None);

/// Static global for cancellation callback (cancels active ACP thread turn by request_id)
static GLOBAL_CANCELLATION_CALLBACK: parking_lot::Mutex<Option<mpsc::UnboundedSender<CancellationRequest>>> =
    parking_lot::Mutex::new(None);

/// Static global for cancel-thread callback.
/// Receives an acp_thread_id and immediately cancels that thread's running turn,
/// bypassing the sequential callback_rx loop (which would be blocked awaiting the turn).
static GLOBAL_CANCEL_THREAD_CALLBACK: parking_lot::Mutex<Option<mpsc::UnboundedSender<String>>> =
    parking_lot::Mutex::new(None);

/// Pending UI state queries that arrived before AgentPanel was ready
static PENDING_UI_STATE_QUERIES: parking_lot::Mutex<Vec<UiStateQueryRequest>> =
    parking_lot::Mutex::new(Vec::new());

/// Request to create ACP thread from external WebSocket message
#[derive(Clone, Debug)]
pub struct ThreadCreationRequest {
    pub acp_thread_id: Option<String>, // null = create new, Some(id) = use existing
    pub message: String,
    pub request_id: String,
    pub agent_name: Option<String>, // Which agent to use (zed-agent or qwen) - defaults to zed-agent
    /// When true, don't mark the entry as external-originated.
    /// This allows the NewEntry subscription to fire and sync the user message back to Helix,
    /// simulating a user typing directly in Zed's agent panel.
    pub simulate_input: bool,
}

/// Request to open existing ACP thread from database and display in UI
#[derive(Clone, Debug)]
pub struct ThreadOpenRequest {
    pub acp_thread_id: String,
    /// Which ACP agent to use (e.g., "qwen", "claude", "gemini", "codex").
    /// None or empty means use NativeAgent (Zed's built-in agent).
    pub agent_name: Option<String>,
}

/// Request to query UI state from AgentPanel (for E2E testing)
#[derive(Clone, Debug)]
pub struct UiStateQueryRequest {
    pub query_id: String,
}

/// Request to cancel an active ACP thread turn from Helix
#[derive(Clone, Debug)]
pub struct CancellationRequest {
    pub request_id: String,
}

/// Notification to display a thread in AgentPanel (for auto-select)
#[derive(Clone, Debug)]
pub struct ThreadDisplayNotification {
    pub thread_entity: gpui::Entity<acp_thread::AcpThread>,
    pub helix_session_id: String,
    pub agent_name: Option<String>, // Which agent created this thread (e.g., "qwen") - None means Zed Agent
}

/// Global callback for thread creation from WebSocket (set by agent_panel)
#[derive(Clone)]
pub struct ThreadCreationCallback {
    pub sender: mpsc::UnboundedSender<ThreadCreationRequest>,
}

impl Global for ThreadCreationCallback {}

/// Send thread creation request to agent_panel via callback
/// If callback is not yet initialized (Zed restart race condition), queue the request
/// and replay when init_thread_creation_callback is called
pub fn request_thread_creation(request: ThreadCreationRequest) -> Result<()> {
    log::info!("🔧 [CALLBACK] request_thread_creation() called: acp_thread_id={:?}, request_id={}",
               request.acp_thread_id, request.request_id);
    eprintln!("🔧 [CALLBACK] request_thread_creation() called: acp_thread_id={:?}, request_id={}",
               request.acp_thread_id, request.request_id);

    let sender = GLOBAL_THREAD_CREATION_CALLBACK.lock().clone();
    if let Some(sender) = sender {
        log::info!("✅ [CALLBACK] Found global callback sender, sending request...");
        sender.send(request)
            .map_err(|e| {
                log::error!("❌ [CALLBACK] Failed to send to channel: {:?}", e);
                anyhow::anyhow!("Failed to send thread creation request")
            })?;
        log::info!("✅ [CALLBACK] Request sent to callback channel successfully");
        Ok(())
    } else {
        // Queue the request for later - this handles Zed restart race condition
        // where WebSocket reconnects before workspace/thread_service is initialized
        log::warn!("⏳ [CALLBACK] Thread creation callback not yet initialized - queueing request for later replay");
        eprintln!("⏳ [CALLBACK] Thread creation callback not yet initialized - queueing request_id={} for later replay", request.request_id);
        PENDING_THREAD_CREATION_REQUESTS.lock().push(request);
        log::info!("✅ [CALLBACK] Request queued successfully (will replay when callback is registered)");
        eprintln!("✅ [CALLBACK] Request queued successfully (will replay when callback is registered)");
        Ok(())
    }
}

/// Initialize the global callback sender (called from thread_service or tests)
/// Also replays any pending requests that arrived before callback was initialized
pub fn init_thread_creation_callback(sender: mpsc::UnboundedSender<ThreadCreationRequest>) {
    log::info!("🔧 [CALLBACK] init_thread_creation_callback() called - registering global callback");
    eprintln!("🔧 [CALLBACK] init_thread_creation_callback() called - registering global callback");

    // Store the callback
    *GLOBAL_THREAD_CREATION_CALLBACK.lock() = Some(sender.clone());
    log::info!("✅ [CALLBACK] Global thread creation callback registered");
    eprintln!("✅ [CALLBACK] Global thread creation callback registered");

    // Replay any pending requests that arrived before callback was ready
    let pending: Vec<ThreadCreationRequest> = std::mem::take(&mut *PENDING_THREAD_CREATION_REQUESTS.lock());
    if !pending.is_empty() {
        log::info!("🔄 [CALLBACK] Replaying {} pending thread creation requests", pending.len());
        eprintln!("🔄 [CALLBACK] Replaying {} pending thread creation requests", pending.len());
        for request in pending {
            log::info!("🔄 [CALLBACK] Replaying request_id={}", request.request_id);
            eprintln!("🔄 [CALLBACK] Replaying request_id={}", request.request_id);
            if let Err(e) = sender.send(request) {
                log::error!("❌ [CALLBACK] Failed to replay pending request: {:?}", e);
                eprintln!("❌ [CALLBACK] Failed to replay pending request: {:?}", e);
            }
        }
        log::info!("✅ [CALLBACK] Finished replaying pending requests");
        eprintln!("✅ [CALLBACK] Finished replaying pending requests");
    }
}

/// Initialize the global thread display callback (called from agent_panel)
/// Also replays any pending notifications that arrived before AgentPanel was ready
pub fn init_thread_display_callback(sender: mpsc::UnboundedSender<ThreadDisplayNotification>) {
    log::info!("🔧 [CALLBACK] init_thread_display_callback() called - registering global callback");
    eprintln!("🔧 [CALLBACK] init_thread_display_callback() called - registering global callback");

    // Store the callback
    *GLOBAL_THREAD_DISPLAY_CALLBACK.lock() = Some(sender.clone());
    log::info!("✅ [CALLBACK] Global thread display callback registered");
    eprintln!("✅ [CALLBACK] Global thread display callback registered");

    // Replay any pending notifications that arrived before AgentPanel was ready
    let pending: Vec<ThreadDisplayNotification> = std::mem::take(&mut *PENDING_THREAD_DISPLAY_NOTIFICATIONS.lock());
    if !pending.is_empty() {
        log::info!("🔄 [CALLBACK] Replaying {} pending thread display notifications", pending.len());
        eprintln!("🔄 [CALLBACK] Replaying {} pending thread display notifications", pending.len());
        for notification in pending {
            log::info!("🔄 [CALLBACK] Replaying display notification for session: {}", notification.helix_session_id);
            eprintln!("🔄 [CALLBACK] Replaying display notification for session: {}", notification.helix_session_id);
            if let Err(e) = sender.send(notification) {
                log::error!("❌ [CALLBACK] Failed to replay display notification: {:?}", e);
                eprintln!("❌ [CALLBACK] Failed to replay display notification: {:?}", e);
            }
        }
        log::info!("✅ [CALLBACK] Finished replaying pending display notifications");
        eprintln!("✅ [CALLBACK] Finished replaying pending display notifications");
    }
}

/// Initialize the global thread open callback (called from thread_service)
/// Also replays any pending requests that arrived before callback was initialized
pub fn init_thread_open_callback(sender: mpsc::UnboundedSender<ThreadOpenRequest>) {
    log::info!("🔧 [CALLBACK] init_thread_open_callback() called - registering global callback");
    eprintln!("🔧 [CALLBACK] init_thread_open_callback() called - registering global callback");

    // Store the callback
    *GLOBAL_THREAD_OPEN_CALLBACK.lock() = Some(sender.clone());
    log::info!("✅ [CALLBACK] Global thread open callback registered");
    eprintln!("✅ [CALLBACK] Global thread open callback registered");

    // Replay any pending requests that arrived before callback was ready
    let pending: Vec<ThreadOpenRequest> = std::mem::take(&mut *PENDING_THREAD_OPEN_REQUESTS.lock());
    if !pending.is_empty() {
        log::info!("🔄 [CALLBACK] Replaying {} pending thread open requests", pending.len());
        eprintln!("🔄 [CALLBACK] Replaying {} pending thread open requests", pending.len());
        for request in pending {
            log::info!("🔄 [CALLBACK] Replaying thread open for acp_thread_id={}", request.acp_thread_id);
            eprintln!("🔄 [CALLBACK] Replaying thread open for acp_thread_id={}", request.acp_thread_id);
            if let Err(e) = sender.send(request) {
                log::error!("❌ [CALLBACK] Failed to replay pending open request: {:?}", e);
                eprintln!("❌ [CALLBACK] Failed to replay pending open request: {:?}", e);
            }
        }
        log::info!("✅ [CALLBACK] Finished replaying pending open requests");
        eprintln!("✅ [CALLBACK] Finished replaying pending open requests");
    }
}

/// Request opening a thread (called from WebSocket handler)
/// If callback is not yet initialized (Zed restart race condition), queue the request
pub fn request_thread_open(request: ThreadOpenRequest) -> Result<()> {
    log::info!("🔧 [CALLBACK] request_thread_open() called: acp_thread_id={}", request.acp_thread_id);
    eprintln!("🔧 [CALLBACK] request_thread_open() called: acp_thread_id={}", request.acp_thread_id);

    let sender = GLOBAL_THREAD_OPEN_CALLBACK.lock().clone();
    if let Some(sender) = sender {
        log::info!("✅ [CALLBACK] Found global callback sender, sending request...");
        sender.send(request)
            .map_err(|e| {
                log::error!("❌ [CALLBACK] Failed to send to channel: {:?}", e);
                anyhow::anyhow!("Failed to send thread open request")
            })?;
        log::info!("✅ [CALLBACK] Request sent to callback channel successfully");
        Ok(())
    } else {
        // Queue the request for later - this handles Zed restart race condition
        log::warn!("⏳ [CALLBACK] Thread open callback not yet initialized - queueing request for later replay");
        eprintln!("⏳ [CALLBACK] Thread open callback not yet initialized - queueing acp_thread_id={} for later replay", request.acp_thread_id);
        PENDING_THREAD_OPEN_REQUESTS.lock().push(request);
        log::info!("✅ [CALLBACK] Open request queued successfully (will replay when callback is registered)");
        eprintln!("✅ [CALLBACK] Open request queued successfully (will replay when callback is registered)");
        Ok(())
    }
}

/// Initialize the global UI state query callback (called from agent_panel)
/// Also replays any pending queries that arrived before AgentPanel was ready
pub fn init_ui_state_query_callback(sender: mpsc::UnboundedSender<UiStateQueryRequest>) {
    log::info!("🔧 [CALLBACK] init_ui_state_query_callback() called - registering global callback");
    eprintln!("🔧 [CALLBACK] init_ui_state_query_callback() called - registering global callback");

    *GLOBAL_UI_STATE_QUERY_CALLBACK.lock() = Some(sender.clone());

    let pending: Vec<UiStateQueryRequest> = std::mem::take(&mut *PENDING_UI_STATE_QUERIES.lock());
    if !pending.is_empty() {
        log::info!("🔄 [CALLBACK] Replaying {} pending UI state queries", pending.len());
        for request in pending {
            if let Err(e) = sender.send(request) {
                log::error!("❌ [CALLBACK] Failed to replay UI state query: {:?}", e);
            }
        }
    }
}

/// Request UI state from AgentPanel (called from WebSocket handler)
/// If AgentPanel isn't ready yet, the query is queued for later replay
pub fn request_ui_state_query(request: UiStateQueryRequest) -> Result<()> {
    log::info!("🔧 [CALLBACK] request_ui_state_query() called: query_id={}", request.query_id);
    eprintln!("🔧 [CALLBACK] request_ui_state_query() called: query_id={}", request.query_id);

    let sender = GLOBAL_UI_STATE_QUERY_CALLBACK.lock().clone();
    if let Some(sender) = sender {
        sender.send(request)
            .map_err(|_| anyhow::anyhow!("Failed to send UI state query"))?;
        Ok(())
    } else {
        PENDING_UI_STATE_QUERIES.lock().push(request);
        Ok(())
    }
}

/// Initialize the global cancellation callback (called from thread_service)
pub fn init_cancellation_callback(sender: mpsc::UnboundedSender<CancellationRequest>) {
    log::info!("[CALLBACK] init_cancellation_callback() called - registering global callback");
    *GLOBAL_CANCELLATION_CALLBACK.lock() = Some(sender);
}

/// Request cancellation of an active thread turn (called from WebSocket handler)
/// Unlike thread creation, cancellation requests are not queued — if the handler
/// isn't ready, we immediately send back a noop turn_cancelled event.
pub fn request_thread_cancellation(request: CancellationRequest) -> Result<()> {
    log::info!("[CALLBACK] request_thread_cancellation() called: request_id={}", request.request_id);

    let sender = GLOBAL_CANCELLATION_CALLBACK.lock().clone();
    if let Some(sender) = sender {
        sender.send(request)
            .map_err(|_| anyhow::anyhow!("Failed to send cancellation request"))?;
        Ok(())
    } else {
        log::warn!("[CALLBACK] Cancellation callback not initialized - sending noop turn_cancelled");
        send_websocket_event(SyncEvent::TurnCancelled {
            request_id: request.request_id,
            status: "noop".to_string(),
        })?;
        Ok(())
    }
}

/// Notify AgentPanel to display a thread (for auto-select)
/// If AgentPanel isn't ready yet, the notification is queued for later replay
pub fn notify_thread_display(notification: ThreadDisplayNotification) -> Result<()> {
    log::info!("🔧 [CALLBACK] notify_thread_display() called for session: {}", notification.helix_session_id);
    eprintln!("🔧 [CALLBACK] notify_thread_display() called for session: {}", notification.helix_session_id);

    let sender = GLOBAL_THREAD_DISPLAY_CALLBACK.lock().clone();
    if let Some(sender) = sender {
        log::info!("✅ [CALLBACK] Found global display callback sender, sending notification...");
        eprintln!("✅ [CALLBACK] Found global display callback sender, sending notification...");
        sender.send(notification)
            .map_err(|e| {
                log::error!("❌ [CALLBACK] Failed to send to channel: {:?}", e);
                eprintln!("❌ [CALLBACK] Failed to send to channel: {:?}", e);
                anyhow::anyhow!("Failed to send thread display notification")
            })?;
        log::info!("✅ [CALLBACK] Notification sent to callback channel successfully");
        eprintln!("✅ [CALLBACK] Notification sent to callback channel successfully");
        Ok(())
    } else {
        // Queue the notification for later replay when AgentPanel initializes
        log::warn!("⏳ [CALLBACK] Thread display callback not yet initialized - queueing notification for later replay");
        eprintln!("⏳ [CALLBACK] Thread display callback not yet initialized - queueing session={} for later replay", notification.helix_session_id);
        PENDING_THREAD_DISPLAY_NOTIFICATIONS.lock().push(notification);
        log::info!("✅ [CALLBACK] Display notification queued successfully (will replay when AgentPanel is ready)");
        eprintln!("✅ [CALLBACK] Display notification queued successfully (will replay when AgentPanel is ready)");
        Ok(())
    }
}

/// Type alias for compatibility with existing code
pub type HelixIntegration = ExternalWebSocketSync;

/// Main external WebSocket thread sync service
pub struct ExternalWebSocketSync {
    session: Arc<AppSession>,
    websocket_sync: Option<WebSocketSync>,
    sync_clients: Arc<RwLock<Vec<String>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExternalSyncConfig {
    /// Enable external WebSocket sync
    pub enabled: bool,
    /// WebSocket sync configuration
    pub websocket_sync: WebSocketSyncConfig,
    /// MCP configuration
    pub mcp: McpConfig,
}

impl Default for ExternalSyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            websocket_sync: WebSocketSyncConfig::default(),
            mcp: McpConfig::default(),
        }
    }
}

impl ExternalWebSocketSync {
    /// Create new external WebSocket sync instance
    pub fn new(session: Arc<AppSession>) -> Self {
        Self {
            session,
            websocket_sync: None,
            sync_clients: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Load configuration from settings
    fn load_config(&self, cx: &App) -> Option<ExternalSyncConfig> {
        let settings = ExternalSyncSettings::get_global(cx);
        
        Some(ExternalSyncConfig {
            enabled: settings.enabled,
            websocket_sync: WebSocketSyncConfig {
                enabled: settings.websocket_sync.enabled,
                url: settings.websocket_sync.external_url.clone(),
                auth_token: settings.websocket_sync.auth_token.clone().unwrap_or_default(),
                use_tls: settings.websocket_sync.use_tls,
                skip_tls_verify: settings.websocket_sync.skip_tls_verify,
            },
            mcp: McpConfig {
                enabled: settings.mcp.enabled,
                server_configs: settings.mcp.servers.iter().map(|s| crate::types::McpServerConfig {
                    name: s.name.clone(),
                    command: s.command.clone(),
                    args: s.args.clone(),
                    env: s.env.clone(),
                }).collect(),
            },
        })
    }

    /// Get session information
    pub fn get_session_info(&self) -> SessionInfo {
        SessionInfo {
            session_id: self.session.id().to_string(),
            last_session_id: self.session.last_session_id().map(|s| s.to_string()),
            active_contexts: 0,
            websocket_connected: false,
            sync_clients: self.sync_clients.read().len(),
        }
    }
}

impl EventEmitter<ExternalSyncEvent> for ExternalWebSocketSync {}

impl Global for ExternalWebSocketSync {}

/// Events emitted by the external WebSocket sync
#[derive(Clone, Debug)]
pub enum ExternalSyncEvent {
    ContextCreated { context_id: String },
    ContextDeleted { context_id: String },
    MessageAdded { context_id: String, message_id: u64 },
    SyncClientConnected { client_id: String },
    SyncClientDisconnected { client_id: String },
    /// External system requests thread creation (e.g., Helix sends first message)
    ExternalThreadCreationRequested {
        helix_session_id: String,
        message: String,
        request_id: String,
    },
    /// Thread was created and should be displayed in UI (e.g., response to Helix message)
    ThreadCreatedForDisplay {
        thread_entity: gpui::Entity<acp_thread::AcpThread>,
        helix_session_id: String,
    },
}

/// Global access to external WebSocket sync
impl ExternalWebSocketSync {
    pub fn global(cx: &App) -> Option<&Self> {
        cx.try_global::<Self>()
    }

    pub fn global_mut(cx: &mut App) -> Option<&mut Self> {
        if cx.has_global::<Self>() {
            Some(cx.global_mut::<Self>())
        } else {
            log::error!("⚠️ [EXTERNAL_WEBSOCKET_SYNC] ExternalWebSocketSync global not available for mutable access");
            None
        }
    }
}

/// Initialize the external WebSocket sync module
pub fn init(cx: &mut App) {
    log::info!("Initializing external WebSocket sync module");

    // Initialize settings
    sync_settings::init(cx);

    // Create global WebSocket sender
    cx.set_global(WebSocketSender::default());

    // TODO: Auto-start WebSocket service when enabled
    // Currently disabled because tokio_tungstenite requires Tokio runtime
    // which isn't available during GPUI init. Need to either:
    // 1. Use smol-based WebSocket library (compatible with GPUI), or
    // 2. Create Tokio runtime wrapper, or
    // 3. Start WebSocket from workspace creation (has executor context)
    //
    // For now: WebSocket must be started manually via init_websocket_service()
    // or will be started when first workspace is created (if we add that)

    let settings = ExternalSyncSettings::get_global(cx);
    if settings.enabled && settings.websocket_sync.enabled {
        log::warn!("⚠️  [WEBSOCKET] WebSocket sync enabled in settings but auto-start not yet supported");
        log::warn!("⚠️  [WEBSOCKET] WebSocket requires Tokio runtime - incompatible with GPUI init");
        log::warn!("⚠️  [WEBSOCKET] Will start when workspace is created (has executor)");
    } else {
        log::info!("⚠️  [WEBSOCKET] WebSocket sync disabled in settings");
    }

    log::info!("External WebSocket sync module initialization completed");
}


/// Get the global external WebSocket sync instance
pub fn get_global_sync_service(cx: &App) -> Option<&ExternalWebSocketSync> {
    cx.try_global::<ExternalWebSocketSync>()
}

#[cfg(any(test, feature = "test-support"))]
pub mod mock_helix_server;

#[cfg(test)]
mod protocol_test;

/// Request that the currently running turn on a thread be cancelled immediately.
/// This bypasses the sequential callback_rx loop (which blocks waiting for turn
/// completion) by routing through a dedicated cancel GPUI task.
/// Called when Helix sends a chat_message with interrupt=true.
pub fn request_cancel_thread(acp_thread_id: String) -> Result<()> {
    eprintln!("⚡ [CANCEL] request_cancel_thread() called for thread: {}", acp_thread_id);
    log::info!("⚡ [CANCEL] request_cancel_thread() called for thread: {}", acp_thread_id);

    let sender = GLOBAL_CANCEL_THREAD_CALLBACK.lock().clone();
    if let Some(sender) = sender {
        sender.send(acp_thread_id)
            .map_err(|_| anyhow::anyhow!("Failed to send cancel request"))?;
        Ok(())
    } else {
        // Cancel task not yet initialized — log and ignore. The new message will
        // be processed sequentially as before (no worse than the old behaviour).
        eprintln!("⚠️ [CANCEL] Cancel callback not yet initialized, skipping cancel for thread: {}", acp_thread_id);
        log::warn!("⚠️ [CANCEL] Cancel callback not yet initialized, skipping cancel for thread: {}", acp_thread_id);
        Ok(())
    }
}

/// Initialize the global cancel-thread callback (called from thread_service).
pub fn init_cancel_thread_callback(sender: mpsc::UnboundedSender<String>) {
    eprintln!("🔧 [CANCEL] init_cancel_thread_callback() called - registering global callback");
    log::info!("🔧 [CANCEL] init_cancel_thread_callback() called - registering global callback");
    *GLOBAL_CANCEL_THREAD_CALLBACK.lock() = Some(sender);
}


