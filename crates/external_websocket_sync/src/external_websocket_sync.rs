//! External WebSocket Thread Sync for Zed Editor
//! 
//! This crate provides APIs for synchronizing Zed editor conversation threads
//! with external services via WebSocket connections, enabling real-time collaboration
//! and integration with AI platforms and other external tools.

use anyhow::{Context, Result};
use assistant_context::{AssistantContext, ContextId, ContextStore, Message, MessageId};
// use assistant_slash_command::SlashCommandWorkingSet;
use collections::HashMap;
use futures::{SinkExt, StreamExt};
use gpui::{App, AppContext, AsyncApp, Context as _, Entity, EventEmitter, SharedString, Subscription, Task, WeakEntity};
use language::LanguageRegistry;
use language_model;
use parking_lot::RwLock;
use project::Project;
use prompt_store::PromptBuilder;
use serde::{Deserialize, Serialize};
use session::{AppSession, Session};
use std::sync::Arc;
use uuid::Uuid;
use settings::{Settings, SettingsStore};

mod websocket_sync;

mod types;
pub use types::*;

mod sync;
pub use sync::*;

mod mcp;
pub use mcp::*;

mod settings;
pub use settings::*;

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
        let sync_service = app.new_global(move || Self::new(session, project, prompt_builder));
        
        // Start the sync service
        app.spawn(|cx| async move {
            if let Some(sync_service) = cx.try_global::<ExternalWebSocketSync>() {
                if let Err(e) = sync_service.start(cx).await {
                    log::error!("Failed to start external WebSocket sync: {}", e);
                }
            }
        }).detach();
    }

    /// Create new external WebSocket sync instance
    pub fn new(session: Arc<AppSession>, project: Entity<Project>, prompt_builder: Arc<PromptBuilder>) -> Self {
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
    pub async fn init_context_store(&mut self, cx: &mut AppContext) -> Result<()> {
        if self.context_store.is_some() {
            return Ok(()); // Already initialized
        }

        let project = self.project.clone();
        let prompt_builder = self.prompt_builder.clone();
        // let slash_commands = Arc::new(SlashCommandWorkingSet::default());

        log::info!("Initializing context store for Helix integration");
        
        match ContextStore::new(project, prompt_builder, None, cx).await {
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

    /// Start the sync service
    pub async fn start(&self, cx: &AppContext) -> Result<()> {
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
        let settings = HelixSettings::get_global(cx);
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
    fn load_config(&self, cx: &AppContext) -> Option<ExternalSyncConfig> {
        let settings = ExternalSyncSettings::get_global(cx);
        
        Some(ExternalSyncConfig {
            enabled: settings.enabled,
            websocket_sync: WebSocketSyncConfig {
                enabled: settings.websocket_sync.enabled,
                external_url: settings.websocket_sync.external_url.clone(),
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
    pub fn create_context(&self, title: Option<String>, cx: &mut AppContext) -> Result<ContextId> {
        let context_id = ContextId::new();
        
        // Create a real AssistantContext
        if let Some(_context_store) = &self.context_store {
            let languages = self.project.read(cx).languages().clone();
            let telemetry = self.project.read(cx).client().telemetry().clone();
            // let slash_commands = Arc::new(SlashCommandWorkingSet::default());
            
            let context = cx.new_entity(|cx| {
                AssistantContext::local(
                    languages,
                    Some(self.project.clone()),
                    Some(telemetry),
                    self.prompt_builder.clone(),
                    None,
                    cx,
                )
            });

            // Add to active contexts
            self.active_contexts.write().insert(context_id.clone(), context);

            // Notify via WebSocket
            self.notify_context_created(&context_id);

            log::info!("Created new conversation context: {} ({})", 
                      context_id.to_proto(), 
                      title.as_deref().unwrap_or("Untitled"));

            Ok(context_id)
        } else {
            // Try to create without context store for now
            log::warn!("Creating context without context store initialization");
            
            let languages = self.project.read(cx).languages().clone();
            let telemetry = self.project.read(cx).client().telemetry().clone();
            // let slash_commands = Arc::new(SlashCommandWorkingSet::default());
            
            let context = cx.new_entity(|cx| {
                AssistantContext::local(
                    languages,
                    Some(self.project.clone()),
                    Some(telemetry),
                    self.prompt_builder.clone(),
                    None,
                    cx,
                )
            });

            // Add to active contexts
            self.active_contexts.write().insert(context_id.clone(), context);

            // Notify via WebSocket
            self.notify_context_created(&context_id);

            log::info!("Created new conversation context: {} ({})", 
                      context_id.to_proto(), 
                      title.as_deref().unwrap_or("Untitled"));

            Ok(context_id)
        }
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
    pub fn get_context_messages(&self, context_id: &ContextId, cx: &AppContext) -> Result<Vec<MessageInfo>> {
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
                content: message.text().to_string(),
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
        cx: &mut AppContext,
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
    pub fn global(cx: &AppContext) -> Option<&Self> {
        cx.try_global::<Self>()
    }

    pub fn global_mut(cx: &mut AppContext) -> Option<&mut Self> {
        cx.try_global_mut::<Self>()
    }
}

/// Initialize the external WebSocket sync module
pub fn init(cx: &mut AppContext) {
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
    cx: &mut AppContext
) -> Result<()> {
    log::info!("Initializing full external WebSocket sync with assistant support");
    
    let sync_service = cx.new_global(move || ExternalWebSocketSync::new(session, project, prompt_builder));
    
    // Start the sync service
    cx.spawn(|cx| async move {
        if let Some(sync_service) = cx.try_global::<ExternalWebSocketSync>() {
            if let Err(e) = sync_service.start(cx).await {
                log::error!("Failed to start external WebSocket sync: {}", e);
            } else {
                log::info!("External WebSocket sync started successfully with project");
            }
        }
    }).detach();
    
    Ok(())
}

/// Simple global storage for session and prompt builder
static SYNC_SESSION: parking_lot::RwLock<Option<Arc<AppSession>>> = parking_lot::RwLock::new(None);
static SYNC_PROMPT_BUILDER: parking_lot::RwLock<Option<Arc<PromptBuilder>>> = parking_lot::RwLock::new(None);

/// Initialize with session and prompt builder, store for later use
pub async fn init_with_session(
    session: Arc<AppSession>,
    prompt_builder: Arc<PromptBuilder>,
    _cx: &mut AsyncApp,
) -> Result<()> {
    log::info!("Storing session and prompt builder for external WebSocket sync");
    
    *SYNC_SESSION.write() = Some(session);
    *SYNC_PROMPT_BUILDER.write() = Some(prompt_builder);
    
    Ok(())
}

/// Initialize with project when available (called from workspace creation)
pub fn init_with_project_when_available(project: Entity<Project>, cx: &mut AppContext) -> Result<()> {
    let session = SYNC_SESSION.read().clone();
    let prompt_builder = SYNC_PROMPT_BUILDER.read().clone();
    
    if let (Some(session), Some(prompt_builder)) = (session, prompt_builder) {
        log::info!("Initializing external WebSocket sync with project");
        
        // Check if sync service already exists globally
        if cx.try_global::<ExternalWebSocketSync>().is_some() {
            log::debug!("External WebSocket sync already exists globally");
            return Ok(());
        }
        
        let sync_service = cx.new_global(|| ExternalWebSocketSync::new(session, project, prompt_builder));
        
        // Start the sync service
        cx.spawn(|cx| async move {
            if let Some(sync_service) = cx.try_global::<ExternalWebSocketSync>() {
                if let Err(e) = sync_service.start(cx).await {
                    log::error!("Failed to start external WebSocket sync: {}", e);
                } else {
                    log::info!("External WebSocket sync started successfully with project");
                }
            }
        }).detach();
        
        Ok(())
    } else {
        log::warn!("Session or prompt builder not available for external WebSocket sync initialization");
        Ok(())
    }
}

/// Get the global external WebSocket sync instance
pub fn get_global_sync_service(cx: &AppContext) -> Option<&ExternalWebSocketSync> {
    cx.try_global::<ExternalWebSocketSync>()
}

/// Execute a function with the global sync service if available
pub fn with_sync_service<T>(
    cx: &AppContext,
    f: impl FnOnce(&ExternalWebSocketSync) -> T,
) -> Option<T> {
    get_global_sync_service(cx).map(f)
}

/// Execute an async function with the global sync service if available
pub async fn with_sync_service_async<T>(
    cx: &AppContext,
    f: impl FnOnce(&ExternalWebSocketSync) -> T,
) -> Option<T> {
    get_global_sync_service(cx).map(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestApp;
    use project::Project;
    use std::sync::Arc;
    
    #[gpui::test]
    async fn test_external_websocket_sync_initialization() {
        let app = TestApp::new();
        let session = Arc::new(AppSession::test());
        
        app.update(|cx| {
            // Create a test project
            let fs = fs::FakeFs::new(cx.background_executor().clone());
            let project = cx.new_model(|cx| Project::test(fs.clone(), [], cx));
            let prompt_builder = Arc::new(PromptBuilder::new());
            
            // Test that we can create an ExternalWebSocketSync
            let sync_service = ExternalWebSocketSync::new(session.clone(), project.clone(), prompt_builder);
            
            // Basic sanity checks
            assert_eq!(sync_service.active_contexts.read().len(), 0);
            assert!(sync_service.context_store.is_none());
            
            // Test session info
            let session_info = sync_service.get_session_info();
            assert_eq!(session_info.session_id, session.id().to_string());
            assert_eq!(session_info.active_contexts, 0);
            assert!(!session_info.websocket_connected);
        });
    }
    
    #[gpui::test] 
    async fn test_settings_integration() {
        let app = TestApp::new();
        
        app.update(|cx| {
            // Initialize settings
            settings::init(cx);
            
            // Get default settings
            let settings = HelixSettings::get_global(cx);
            
            // Test defaults
            assert!(!settings.enabled);
            assert!(!settings.websocket_sync.enabled);
            assert!(!settings.mcp.enabled);
            assert!(!settings.server.enabled);
            
            // Test URL generation
            let websocket_url = settings.websocket_url();
            assert!(websocket_url.starts_with("ws://"));
            assert!(websocket_url.contains("localhost:8080"));
        });
    }
    
    #[gpui::test]
    async fn test_context_creation() {
        let app = TestApp::new();
        let session = Arc::new(AppSession::test());
        
        app.update(|cx| {
            settings::init(cx);
            
            let fs = fs::FakeFs::new(cx.background_executor().clone());
            let project = cx.new_model(|cx| Project::test(fs.clone(), [], cx));
            let prompt_builder = Arc::new(PromptBuilder::new());
            
            let sync_service = ExternalWebSocketSync::new(session, project, prompt_builder);
            
            // Test that context creation works without context_store
            let result = sync_service.create_context(Some("Test Context".to_string()), cx);
            assert!(result.is_ok());
            let context_id = result.unwrap();
            assert!(!context_id.to_proto().is_empty());
        });
    }
    
    #[gpui::test]
    async fn test_end_to_end_integration() {
        let app = TestApp::new();
        let session = Arc::new(AppSession::test());
        
        app.update(|cx| {
            settings::init(cx);
            
            let fs = fs::FakeFs::new(cx.background_executor().clone());
            let project = cx.new_model(|cx| Project::test(fs.clone(), [], cx));
            let prompt_builder = Arc::new(PromptBuilder::new());
            
            // Test global initialization
            let sync_service = cx.new_global(|| ExternalWebSocketSync::new(session, project, prompt_builder));
            
            // Test that we can access the sync service globally
            let global_sync_service = get_global_sync_service(cx);
            assert!(global_sync_service.is_some());
            
            // Test with_sync_service helper
            let session_info = with_sync_service(cx, |sync_service| {
                sync_service.get_session_info()
            });
            assert!(session_info.is_some());
            let session_info = session_info.unwrap();
            
            // Verify session info
            assert!(!session_info.session_id.is_empty());
            assert_eq!(session_info.active_contexts, 0);
            assert!(!session_info.websocket_connected);
            assert_eq!(session_info.sync_clients, 0);
            
            // Test context creation
            let context_result = with_sync_service(cx, |sync_service| {
                sync_service.create_context(Some("Test End-to-End".to_string()), cx)
            });
            assert!(context_result.is_some());
            let context_id = context_result.unwrap();
            assert!(context_id.is_ok());
            
            // Verify context was created
            let updated_session_info = with_sync_service(cx, |sync_service| {
                sync_service.get_session_info()
            }).unwrap();
            assert_eq!(updated_session_info.active_contexts, 1);
            
            // Test getting contexts list
            let contexts = with_sync_service(cx, |sync_service| {
                sync_service.get_contexts()
            }).unwrap();
            assert_eq!(contexts.len(), 1);
            assert_eq!(contexts[0].title, "Test End-to-End");
            
            log::info!("End-to-end sync test passed successfully");
        });
    }
}