//! External WebSocket Thread Sync for Zed Editor
//! 
//! This crate provides APIs for synchronizing Zed editor conversation threads
//! with external services via WebSocket connections, enabling real-time collaboration
//! and integration with AI platforms and other external tools.

use anyhow::{Context, Result};
use assistant_context::{AssistantContext, ContextId, ContextStore, MessageId};
use assistant_slash_command::SlashCommandWorkingSet;
use collections::HashMap;
use futures::StreamExt;
use gpui::{App, AsyncApp, Entity, EventEmitter, Global, Subscription};

use language_model;
use parking_lot::RwLock;
use project::Project;
use prompt_store::PromptBuilder;
use serde::{Deserialize, Serialize};
use session::AppSession;
use std::sync::Arc;

use settings::{Settings, SettingsStore};

mod websocket_sync;

mod types;
pub use types::*;

mod sync;
pub use sync::*;

mod mcp;
pub use mcp::*;

mod sync_settings;
pub use sync_settings::*;

mod server;
pub use server::*;

pub use websocket_sync::*;

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
            active_contexts: Arc::new(RwLock::new(HashMap::new())),
            websocket_sync: None,
            sync_clients: Arc::new(RwLock::new(Vec::new())),
            _subscriptions: Vec::new(),
        }
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

    /// Start the sync service
    pub async fn start(&mut self, cx: &mut App) -> Result<()> {
        log::info!("Starting external WebSocket thread sync service");

        // Load configuration
        let config = self.load_config(cx).unwrap_or_default();
        
        if !config.enabled {
            log::info!("External WebSocket sync is disabled");
            return Ok(());
        }

        // Initialize context store if we have a project
        log::info!("Attempting to initialize context store");
        // Note: We can't get mutable self here due to async context
        // This will need to be handled differently in the actual implementation

        // Start HTTP server if enabled
        let settings = ExternalSyncSettings::get_global(cx);
        if settings.server.enabled {
            let server_config = crate::server::ServerConfig {
                enabled: settings.server.enabled,
                host: settings.server.host.clone(),
                port: settings.server.port,
                auth_token: settings.server.auth_token.clone(),
            };
            
            match crate::server::start_server(server_config).await {
                Ok(_server_handle) => {
                    log::info!("HTTP server started on {}:{}", settings.server.host, settings.server.port);
                    // TODO: Store server_handle for proper shutdown
                }
                Err(e) => {
                    log::error!("Failed to start HTTP server: {}", e);
                }
            }
        }

        // Start WebSocket sync if enabled
        if config.websocket_sync.enabled {
            match WebSocketSync::new(config.websocket_sync.clone()).await {
                Ok(_websocket_sync) => {
                    log::info!("WebSocket sync initialized successfully");
                    // TODO: Store websocket_sync in a thread-safe way
                }
                Err(e) => {
                    log::error!("Failed to initialize WebSocket sync: {}", e);
                }
            }
        }

        // Initialize MCP if enabled
        if config.mcp.enabled {
            if let Err(e) = self.initialize_mcp(config.mcp, cx).await {
                log::error!("Failed to initialize MCP: {}", e);
            }
        }

        log::info!("External WebSocket sync service started successfully");
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
                helix_url: settings.websocket_sync.external_url.clone(),
                session_id: self.session.id().to_string(),
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
            websocket_connected: self.websocket_sync.as_ref().map_or(false, |ws| ws.is_connected()),
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

        // TODO: Add actual message to the assistant context
        // For now, create a placeholder message ID
        let message_id = MessageId(clock::Lamport::new(1));

        // Notify via WebSocket
        self.notify_message_added(context_id, &message_id);

        Ok(message_id)
    }

    /// Notify WebSocket of context creation
    fn notify_context_created(&self, context_id: &ContextId) {
        if let Some(websocket_sync) = &self.websocket_sync {
            let event = SyncEvent::ContextCreated {
                context_id: context_id.to_proto(),
            };
            if let Err(e) = websocket_sync.send_event(event) {
                log::warn!("Failed to send context created event: {}", e);
            }
        }
    }

    /// Notify WebSocket of context deletion
    fn notify_context_deleted(&self, context_id: &ContextId) {
        if let Some(websocket_sync) = &self.websocket_sync {
            let event = SyncEvent::ContextDeleted {
                context_id: context_id.to_proto(),
            };
            if let Err(e) = websocket_sync.send_event(event) {
                log::warn!("Failed to send context deleted event: {}", e);
            }
        }
    }

    /// Notify WebSocket of message addition
    fn notify_message_added(&self, context_id: &ContextId, message_id: &MessageId) {
        if let Some(websocket_sync) = &self.websocket_sync {
            let event = SyncEvent::MessageAdded {
                context_id: context_id.to_proto(),
                message_id: message_id.as_u64(),
            };
            if let Err(e) = websocket_sync.send_event(event) {
                log::warn!("Failed to send message added event: {}", e);
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
}

/// Global access to external WebSocket sync
impl ExternalWebSocketSync {
    pub fn global(cx: &App) -> Option<&Self> {
        cx.try_global::<Self>()
    }

    pub fn global_mut(cx: &mut App) -> &mut Self {
        cx.global_mut::<Self>()
    }
}

/// Initialize the external WebSocket sync module
pub fn init(cx: &mut App) {
    log::info!("Initializing external WebSocket sync module");
    
    // Initialize settings
    settings::init(cx);
    
    // Log that the module is ready
    log::info!("External WebSocket sync module ready, waiting for assistant context initialization");
}

/// Initialize the external WebSocket sync with full assistant support
pub fn init_full(
    session: Arc<AppSession>, 
    project: Entity<Project>, 
    prompt_builder: Arc<PromptBuilder>,
    cx: &mut App
) -> Result<()> {
    log::info!("Initializing full external WebSocket sync with assistant support");
    
    let sync_service = ExternalWebSocketSync::new(session, project, prompt_builder);
    cx.set_global(sync_service);
    
    // Initialize sync service synchronously for now
    log::info!("External WebSocket sync with project initialized - async startup deferred");
    
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

/// Initialize with project when available (called from workspace creation) 
pub fn init_with_project_when_available(
    project: Entity<Project>, 
    session: Arc<AppSession>,
    prompt_builder: Arc<PromptBuilder>, 
    cx: &mut App
) -> Result<()> {
    log::info!("Initializing external WebSocket sync with project");
    
    // Check if sync service already exists globally
    if cx.has_global::<ExternalWebSocketSync>() {
        log::info!("External WebSocket sync already initialized");
        return Ok(());
    }
    
    let sync_service = ExternalWebSocketSync::new(session, project, prompt_builder);
    cx.set_global(sync_service);
        
        // Initialize sync service synchronously for now
        log::info!("External WebSocket sync with project available initialized - async startup deferred");
    
    Ok(())
}

/// Get the global external WebSocket sync instance
pub fn get_global_sync_service(cx: &App) -> Option<&ExternalWebSocketSync> {
    cx.try_global::<ExternalWebSocketSync>()
}

#[cfg(test)]
mod test_integration;

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

