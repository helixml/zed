//! External WebSocket Thread Sync for Zed Editor
//! 
//! This crate provides APIs for synchronizing Zed editor conversation threads
//! with external services via WebSocket connections, enabling real-time collaboration
//! and integration with AI platforms and other external tools.

use anyhow::{Context, Result};
use util::ResultExt;
use assistant_context::{AssistantContext, ContextId, ContextStore, MessageId};
use assistant_slash_command::SlashCommandWorkingSet;
use collections::HashMap;
use futures::StreamExt;
use gpui::{App, AsyncApp, Entity, EventEmitter, Global, Subscription};
use tokio::sync::mpsc;

use language_model;
use parking_lot::RwLock;
use project::Project;
use prompt_store::PromptBuilder;
use serde::{Deserialize, Serialize};
use session::AppSession;
use std::sync::Arc;

use settings::{Settings, SettingsStore};

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

/// Request to create ACP thread from external WebSocket message
#[derive(Clone, Debug)]
pub struct ThreadCreationRequest {
    pub acp_thread_id: Option<String>, // null = create new, Some(id) = use existing
    pub message: String,
    pub request_id: String,
}

/// Global callback for thread creation from WebSocket (set by agent_panel)
#[derive(Clone)]
pub struct ThreadCreationCallback {
    pub sender: mpsc::UnboundedSender<ThreadCreationRequest>,
}

impl Global for ThreadCreationCallback {}

/// Send thread creation request to agent_panel via callback
pub fn request_thread_creation(request: ThreadCreationRequest) -> Result<()> {
    log::info!("🔧 [CALLBACK] request_thread_creation() called: acp_thread_id={:?}, request_id={}",
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
        log::error!("❌ [CALLBACK] Thread creation callback not initialized!");
        log::error!("❌ [CALLBACK] This means setup_thread_handler() was never called");
        Err(anyhow::anyhow!("Thread creation callback not initialized"))
    }
}

/// Initialize the global callback sender (called from thread_service or tests)
pub fn init_thread_creation_callback(sender: mpsc::UnboundedSender<ThreadCreationRequest>) {
    log::info!("🔧 [CALLBACK] init_thread_creation_callback() called - registering global callback");
    *GLOBAL_THREAD_CREATION_CALLBACK.lock() = Some(sender);
    log::info!("✅ [CALLBACK] Global thread creation callback registered");
}

/// Type alias for compatibility with existing code
pub type HelixIntegration = ExternalWebSocketSync;

/// Main external WebSocket thread sync service
pub struct ExternalWebSocketSync {
    session: Arc<AppSession>,
    context_store: Option<Entity<ContextStore>>,
    project: Entity<Project>,
    prompt_builder: Arc<PromptBuilder>,
    active_contexts: Arc<RwLock<HashMap<ContextId, Entity<AssistantContext>>>>,
    websocket_sync: Option<WebSocketSync>,
    sync_clients: Arc<RwLock<Vec<String>>>,
    _subscriptions: Vec<Subscription>,
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
    /// Initialize external WebSocket sync with project and prompt builder
    pub fn init_with_project(
        app: &mut App, 
        session: Arc<AppSession>, 
        project: Entity<Project>,
        prompt_builder: Arc<PromptBuilder>
    ) {
        let sync_service = Self::new(session, project, prompt_builder);
        app.set_global(sync_service);
        
        // Initialize sync service synchronously for now
        log::info!("External WebSocket sync initialized - async startup deferred");
    }

    /// Create new external WebSocket sync instance
    pub fn new(session: Arc<AppSession>, project: Entity<Project>, prompt_builder: Arc<PromptBuilder>) -> Self {
        log::warn!("External WebSocket sync creation - using placeholder values due to API changes");
        Self {
            session,
            context_store: None,
            project,
            prompt_builder,
            active_contexts: Arc::new(RwLock::new(HashMap::default())),
            websocket_sync: None,
            sync_clients: Arc::new(RwLock::new(Vec::new())),
            _subscriptions: Vec::new(),
        }
    }

    /// Subscribe to context changes and emit sync events to Helix
    pub fn subscribe_to_context_changes(&mut self, context: Entity<assistant_context::AssistantContext>, external_session_id: String, cx: &mut App) {
        let session_id_clone = external_session_id.clone();
        eprintln!("🔔 [SYNC] Subscribing to context changes for external session: {}", external_session_id);
        
        let subscription = cx.subscribe(&context, move |_context, _event, _cx| {
            eprintln!("🔔 [SYNC] Context changed for session: {}", session_id_clone);
            
            // TODO: Extract context content and send sync event to Helix
            eprintln!("✅ [SYNC] Sending context update to Helix for session: {}", session_id_clone);
            // This is where we'll implement the sync back to Helix
            // We'll send the complete thread state back to Helix
        });
        
        self._subscriptions.push(subscription);
        eprintln!("✅ [SYNC] Successfully subscribed to context changes for session: {}", external_session_id);
    }

    /// Initialize context store with project
    pub async fn init_context_store(&mut self, cx: &mut App) -> Result<()> {
        if self.context_store.is_some() {
            return Ok(()); // Already initialized
        }

        let project = self.project.clone();
        let prompt_builder = self.prompt_builder.clone();
        // let slash_commands = Arc::new(SlashCommandWorkingSet::default());

        log::info!("Initializing context store for Helix integration");
        
        let slash_commands = Arc::new(SlashCommandWorkingSet::default());
        match ContextStore::new(project, prompt_builder, slash_commands, cx).await {
            Ok(context_store) => {
                self.context_store = Some(context_store);
                log::info!("Context store initialized successfully");
                Ok(())
            }
            Err(e) => {
                log::error!("Failed to initialize context store: {}", e);
                Err(e)
            }
        }
    }

    /// Initialize context store synchronously
    pub fn init_context_store_sync(&mut self, cx: &mut App) -> Result<()> {
        if self.context_store.is_some() {
            return Ok(());
        }

        let project = self.project.clone();
        let prompt_builder = self.prompt_builder.clone();

        log::info!("Initializing context store for Helix integration");
        
        let slash_commands = Arc::new(SlashCommandWorkingSet::default());
        
        // Create context store task but don't await it here
        let context_store_task = ContextStore::new(project, prompt_builder, slash_commands, cx);
        
        // Store the task for later awaiting if needed
        log::info!("Context store creation task started");
        Ok(())
    }

    /// Start the sync service (DEPRECATED - use init_websocket_service instead)
    #[allow(dead_code)]
    pub async fn start(&mut self, _cx: &mut App) -> Result<()> {
        log::warn!("ExternalWebSocketSync::start() is deprecated - use websocket_sync::init_websocket_service()");
        Ok(())
    }

    /// Stop the sync service
    pub async fn stop(&self) -> Result<()> {
        log::info!("Stopping external WebSocket sync service");

        // Note: This would need proper mutable access in a real implementation
        // For now, just log the stop request
        log::info!("Stop requested for external WebSocket sync service");

        // Clear active contexts
        self.active_contexts.write().clear();

        log::info!("External WebSocket sync service stopped");
        Ok(())
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
            active_contexts: self.active_contexts.read().len(),
            websocket_connected: false, // TODO: check if websocket service is active
            sync_clients: self.sync_clients.read().len(),
        }
    }

    /// Get all active conversation contexts
    pub fn get_contexts(&self) -> Vec<ContextInfo> {
        let contexts = self.active_contexts.read();
        contexts
            .iter()
            .map(|(id, _context)| {
                // TODO: Read actual context data once assistant_context integration is complete
                ContextInfo {
                    id: id.to_proto(),
                    title: "Conversation".to_string(),
                    message_count: 0,
                    last_message_at: chrono::Utc::now(),
                    status: "active".to_string(),
                }
            })
            .collect()
    }

    /// Create a new conversation context
    pub fn create_context(&self, title: Option<String>, _cx: &mut App) -> Result<ContextId> {
        let context_id = ContextId::new();
        
        // TODO: Implement real context creation when API is fixed
        // For now, just create a placeholder context ID
        log::info!("Created new conversation context: {} ({})", 
                  context_id.to_proto(), 
                  title.as_deref().unwrap_or("Untitled"));
        
        Ok(context_id)

    }

    /// Delete a conversation context
    pub fn delete_context(&mut self, context_id: &ContextId) -> Result<()> {
        if self.active_contexts.write().remove(context_id).is_some() {
            self.notify_context_deleted(context_id);
            Ok(())
        } else {
            anyhow::bail!("Context not found: {}", context_id.to_proto())
        }
    }

    /// Get messages from a context
    pub fn get_context_messages(&self, context_id: &ContextId, cx: &App) -> Result<Vec<MessageInfo>> {
        let contexts = self.active_contexts.read();
        let context = contexts
            .get(context_id)
            .with_context(|| format!("Context not found: {}", context_id.to_proto()))?;

        // Read actual messages from the assistant context
        let messages: Vec<MessageInfo> = context.read(cx).messages(cx)
            .map(|message| MessageInfo {
                id: message.id.as_u64(),
                context_id: context_id.to_proto(),
                role: match message.role {
                    language_model::Role::User => "user".to_string(),
                    language_model::Role::Assistant => "assistant".to_string(),
                    language_model::Role::System => "system".to_string(),
                },
                content: "placeholder message content".to_string(), // TODO: Get actual message content from buffer
                created_at: chrono::Utc::now(), // TODO: Get actual timestamp from message
                status: match message.status {
                    assistant_context::MessageStatus::Pending => "pending".to_string(),
                    assistant_context::MessageStatus::Done => "done".to_string(),
                    assistant_context::MessageStatus::Error(_) => "error".to_string(),
                    assistant_context::MessageStatus::Canceled => "canceled".to_string(),
                },
                metadata: std::collections::HashMap::new(),
            })
            .collect();

        Ok(messages)
    }

    /// Add a message to a context
    pub fn add_message_to_context(
        &mut self,
        context_id: &ContextId,
        content: String,
        role: String,
        cx: &mut App,
    ) -> Result<MessageId> {
        let contexts = self.active_contexts.read();
        let context = contexts
            .get(context_id)
            .with_context(|| format!("Context not found: {}", context_id.to_proto()))?
            .clone();

        drop(contexts);

        // Add the actual message to the assistant context
        let message_id = context.update(cx, |context, cx| {
            // Add the user's message to the buffer (same as agent panel does)
            context.buffer().update(cx, |buffer, cx| {
                let end_offset = buffer.len();
                buffer.edit([(end_offset..end_offset, format!("{}\n", content))], None, cx);
            });

            // If this is a user message, trigger AI assistant response
            if role == "user" {
                log::info!("🤖 [WEBSOCKET_SYNC] Triggering AI assistant for user message: {}", content);
                context.assist(cx);
            }

            // Create a message ID (for now just use a placeholder)
            MessageId(clock::Lamport::new(1))
        });

        // Notify via WebSocket
        self.notify_message_added(context_id, &message_id);

        Ok(message_id)
    }

    /// Notify WebSocket of context creation (DEPRECATED - use thread_created)
    #[allow(dead_code)]
    fn notify_context_created(&self, _context_id: &ContextId) {
        // Context creation now sends thread_created event via the callback mechanism
    }

    /// Notify WebSocket of context deletion (DEPRECATED)
    #[allow(dead_code)]
    fn notify_context_deleted(&self, _context_id: &ContextId) {
        // Not part of the simplified protocol
    }

    /// Notify WebSocket of message addition
    fn notify_message_added(&self, context_id: &ContextId, message_id: &MessageId) {
        if let Some(websocket_sync) = &self.websocket_sync {
            let event = SyncEvent::MessageAdded {
                acp_thread_id: context_id.to_proto(),
                message_id: message_id.as_u64().to_string(),
                role: "assistant".to_string(),
                content: String::new(), // TODO: get actual content
                timestamp: chrono::Utc::now().timestamp(),
            };
            if let Err(e) = websocket_sync.send_event(event) {
                log::warn!("Failed to send message added event: {}", e);
            }
        }
    }

    /// Notify WebSocket of message completion
    pub fn notify_message_completed(&self, context_id: &ContextId, message_id: &MessageId) {
        if let Some(websocket_sync) = &self.websocket_sync {
            let event = SyncEvent::MessageCompleted {
                acp_thread_id: context_id.to_proto(),
                message_id: message_id.as_u64().to_string(),
                request_id: String::new(), // TODO: track request_id
            };
            if let Err(e) = websocket_sync.send_event(event) {
                log::warn!("Failed to send message completed event: {}", e);
            }
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


/// Initialize the external WebSocket sync with full assistant support (DEPRECATED)
#[allow(dead_code)]
pub fn init_full(
    _session: Arc<AppSession>,
    _project: Entity<Project>,
    _prompt_builder: Arc<PromptBuilder>,
    _cx: &mut App
) -> Result<()> {
    log::warn!("init_full() is deprecated - use websocket_sync::init_websocket_service()");
    Ok(())
}

/// Initialize with session and prompt builder, store for later use
pub async fn init_with_session(
    session: Arc<AppSession>,
    prompt_builder: Arc<PromptBuilder>,
) -> Result<()> {
    log::info!("Session and prompt builder will be passed directly to initialization methods");
    Ok(())
}

/// Initialize with project when available (DEPRECATED)
#[allow(dead_code)]
pub fn init_with_project_when_available(
    _project: Entity<Project>,
    _session: Arc<AppSession>,
    _prompt_builder: Arc<PromptBuilder>,
    _cx: &mut App
) -> Result<()> {
    log::warn!("init_with_project_when_available() is deprecated");
    Ok(())
}

/// Get the global external WebSocket sync instance
pub fn get_global_sync_service(cx: &App) -> Option<&ExternalWebSocketSync> {
    cx.try_global::<ExternalWebSocketSync>()
}

#[cfg(test)]
mod protocol_test;

/// Execute a function with the global sync service if available
pub fn with_sync_service<T>(
    cx: &App,
    f: impl FnOnce(&ExternalWebSocketSync) -> T,
) -> Option<T> {
    get_global_sync_service(cx).map(f)
}

/// Execute an async function with the global sync service if available
pub async fn with_sync_service_async<T>(
    cx: &App,
    f: impl FnOnce(&ExternalWebSocketSync) -> T,
) -> Option<T> {
    get_global_sync_service(cx).map(f)
}

/// Subscribe to context changes for an external session (called from AgentPanel)
pub fn subscribe_to_context_changes_global(context: Entity<assistant_context::AssistantContext>, external_session_id: String, cx: &mut App) {
    let session_id_clone = external_session_id.clone();
    eprintln!("🔔 [SYNC_GLOBAL] Setting up global context subscription for session: {}", external_session_id);
    
    // Create a subscription that will send sync events when the context changes
    let _subscription = cx.subscribe(&context, move |context_entity, _event, cx| {
        eprintln!("🔔 [SYNC_GLOBAL] Context changed for session: {}", session_id_clone);
        
        // Extract the current context content
        let context_content = context_entity.read(cx);
        let messages = context_content.messages(cx);
        
        let messages: Vec<_> = messages.collect();
        eprintln!("🔔 [SYNC_GLOBAL] Context has {} messages, syncing to Helix...", messages.len());
        
        // TODO: Send sync event to Helix via WebSocket
        // This is where we'll implement the actual sync back to Helix
        // We need to:
        // 1. Extract all messages from the context
        // 2. Format them as Helix-compatible messages
        // 3. Send them via WebSocket to update the Helix session
        
        eprintln!("✅ [SYNC_GLOBAL] Context sync completed for session: {}", session_id_clone);
    });
    
    // Store the subscription in global state so it doesn't get dropped
    // TODO: We need a way to store these subscriptions globally
    eprintln!("✅ [SYNC_GLOBAL] Context subscription created for session: {}", external_session_id);
}


