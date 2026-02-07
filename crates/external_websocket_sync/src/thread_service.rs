//! Thread management service for WebSocket integration
//!
//! This is the NON-UI service layer that manages ACP threads for external WebSocket control.
//! Called from workspace creation, contains all business logic.

use anyhow::Result;
use acp_thread::{AcpThread, AcpThreadEvent};
use action_log::ActionLog;
use agent::HistoryStore;
use agent_client_protocol::{ContentBlock, PromptCapabilities, SessionId, TextContent};
use gpui::{App, Entity, EventEmitter, WeakEntity, prelude::*};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use fs::Fs;
use project::Project;
use tokio::sync::mpsc;
use util::ResultExt;
use watch;

use settings::Settings as _;
use crate::{ExternalAgent, ThreadCreationRequest, ThreadOpenRequest, SyncEvent};

/// Global registry of active ACP threads (service layer)
/// Stores STRONG references to keep threads alive for follow-up messages
static THREAD_REGISTRY: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, Entity<AcpThread>>>>>> =
    parking_lot::Mutex::new(None);

/// Global map of thread_id -> current_request_id
/// Tracks the request_id for the CURRENT/LATEST message being processed by each thread
/// This ensures message_completed events use the correct request_id (not the first one)
static THREAD_REQUEST_MAP: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, String>>>>> =
    parking_lot::Mutex::new(None);

/// Global map of thread_id -> Set of entry indices that originated from external system
/// Prevents echoing external messages back (initial + follow-ups)
static EXTERNAL_ORIGINATED_ENTRIES: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, HashSet<usize>>>>>> =
    parking_lot::Mutex::new(None);

/// Initialize the thread registry
pub fn init_thread_registry() {
    let mut registry = THREAD_REGISTRY.lock();
    if registry.is_none() {
        *registry = Some(Arc::new(RwLock::new(HashMap::new())));
    }

    let mut request_map = THREAD_REQUEST_MAP.lock();
    if request_map.is_none() {
        *request_map = Some(Arc::new(RwLock::new(HashMap::new())));
    }

    let mut external_map = EXTERNAL_ORIGINATED_ENTRIES.lock();
    if external_map.is_none() {
        *external_map = Some(Arc::new(RwLock::new(HashMap::new())));
    }
}

/// Mark an entry as originated from external system (won't be echoed back)
fn mark_external_originated_entry(thread_id: String, entry_idx: usize) {
    init_thread_registry();
    let map = EXTERNAL_ORIGINATED_ENTRIES.lock();
    if let Some(m) = map.as_ref() {
        m.write().entry(thread_id).or_insert_with(HashSet::new).insert(entry_idx);
    }
}

/// Check if entry originated from external system
pub fn is_external_originated_entry(thread_id: &str, entry_idx: usize) -> bool {
    let map = EXTERNAL_ORIGINATED_ENTRIES.lock();
    if let Some(m) = map.as_ref() {
        m.read().get(thread_id).map_or(false, |set| set.contains(&entry_idx))
    } else {
        false
    }
}

/// Set the current request_id for a thread (used when sending new message to thread)
pub fn set_thread_request_id(acp_thread_id: String, request_id: String) {
    init_thread_registry();
    let map = THREAD_REQUEST_MAP.lock();
    if let Some(m) = map.as_ref() {
        m.write().insert(acp_thread_id, request_id);
    }
}

/// Get the current request_id for a thread
pub fn get_thread_request_id(acp_thread_id: &str) -> Option<String> {
    let map = THREAD_REQUEST_MAP.lock();
    map.as_ref()?.read().get(acp_thread_id).cloned()
}

/// Register an active thread (stores strong reference to keep thread alive)
pub fn register_thread(acp_thread_id: String, thread: Entity<AcpThread>) {
    init_thread_registry();
    let registry = THREAD_REGISTRY.lock();
    if let Some(reg) = registry.as_ref() {
        reg.write().insert(acp_thread_id, thread);
    }
}

/// Get an active thread as weak reference
pub fn get_thread(acp_thread_id: &str) -> Option<WeakEntity<AcpThread>> {
    let registry = THREAD_REGISTRY.lock();
    registry.as_ref()?.read().get(acp_thread_id).map(|e| e.downgrade())
}

/// Setup WebSocket thread handler for a workspace
///
/// Called during workspace creation from zed.rs.
/// Contains ALL the business logic for thread creation and management.
///
/// This is the NON-UI service layer that creates and manages ACP threads in response
/// to WebSocket messages from external systems (e.g., Helix).
pub fn setup_thread_handler(
    project: Entity<Project>,
    acp_history_store: Entity<HistoryStore>,
    fs: Arc<dyn Fs>,
    cx: &mut App,
) {
    log::info!("🔧 [THREAD_SERVICE] Setting up WebSocket thread handler");

    // Create callback channel for thread creation requests
    let (callback_tx, mut callback_rx) = mpsc::unbounded_channel::<ThreadCreationRequest>();

    // Register callback globally so WebSocket sync can send requests
    crate::init_thread_creation_callback(callback_tx);
    log::info!("✅ [THREAD_SERVICE] Thread creation callback registered");

    // Create callback channel for thread open requests
    let (open_callback_tx, mut open_callback_rx) = mpsc::unbounded_channel::<ThreadOpenRequest>();

    // Register open callback globally
    crate::init_thread_open_callback(open_callback_tx);
    log::info!("✅ [THREAD_SERVICE] Thread open callback registered");

    // Clone resources for both spawned tasks
    let project_for_create = project.clone();
    let acp_history_store_for_create = acp_history_store.clone();
    let fs_for_create = fs.clone();
    let project_for_open = project.clone();
    let acp_history_store_for_open = acp_history_store.clone();
    let fs_for_open = fs.clone();

    // Spawn handler task to process thread creation requests
    cx.spawn(async move |cx| {
        eprintln!("🔧 [THREAD_SERVICE] Handler task started, waiting for requests...");
        log::info!("🔧 [THREAD_SERVICE] Handler task started, waiting for requests...");

        while let Some(request) = callback_rx.recv().await {
            eprintln!(
                "📨 [THREAD_SERVICE] Received thread creation request: acp_thread_id={:?}, request_id={}",
                request.acp_thread_id,
                request.request_id
            );
            log::info!(
                "📨 [THREAD_SERVICE] Received thread creation request: acp_thread_id={:?}, request_id={}",
                request.acp_thread_id,
                request.request_id
            );

            // Check if this is a follow-up message to existing thread
            if let Some(existing_thread_id) = &request.acp_thread_id {
                eprintln!("🔍 [THREAD_SERVICE] Checking for existing thread: '{}'", existing_thread_id);
                log::info!("🔍 [THREAD_SERVICE] Checking for existing thread: '{}'", existing_thread_id);

                // Skip empty string thread IDs (these are new thread requests)
                if existing_thread_id.is_empty() {
                    eprintln!("⚠️ [THREAD_SERVICE] Empty thread ID, creating new thread");
                    log::warn!("⚠️ [THREAD_SERVICE] Empty thread ID, creating new thread");
                } else if let Some(thread) = get_thread(existing_thread_id) {
                    eprintln!(
                        "🔄 [THREAD_SERVICE] Sending to existing thread: {}",
                        existing_thread_id
                    );
                    log::info!(
                        "🔄 [THREAD_SERVICE] Sending to existing thread: {}",
                        existing_thread_id
                    );
                    if let Err(e) = handle_follow_up_message(
                        thread,
                        existing_thread_id.clone(),
                        request.request_id.clone(),
                        request.message,
                        cx.clone()
                    ).await {
                        eprintln!("❌ [THREAD_SERVICE] Failed to send follow-up message: {}", e);
                        log::error!("❌ [THREAD_SERVICE] Failed to send follow-up message: {}", e);
                    }
                    continue;
                } else {
                    // Thread not in registry - try to load from agent first
                    eprintln!(
                        "🔄 [THREAD_SERVICE] Thread {} not in registry, attempting to load from agent...",
                        existing_thread_id
                    );
                    log::info!(
                        "🔄 [THREAD_SERVICE] Thread {} not in registry, attempting to load from agent...",
                        existing_thread_id
                    );

                    // Try to load the session from the agent
                    let load_result = load_thread_from_agent(
                        project_for_create.clone(),
                        acp_history_store_for_create.clone(),
                        fs_for_create.clone(),
                        existing_thread_id.clone(),
                        request.agent_name.clone(),
                        cx.clone(),
                    ).await;

                    match load_result {
                        Ok(thread) => {
                            eprintln!(
                                "✅ [THREAD_SERVICE] Successfully loaded thread {} from agent, sending message",
                                existing_thread_id
                            );
                            log::info!(
                                "✅ [THREAD_SERVICE] Successfully loaded thread {} from agent, sending message",
                                existing_thread_id
                            );
                            // Send the message to the loaded thread
                            if let Err(e) = handle_follow_up_message(
                                thread,
                                existing_thread_id.clone(),
                                request.request_id.clone(),
                                request.message,
                                cx.clone()
                            ).await {
                                eprintln!("❌ [THREAD_SERVICE] Failed to send message to loaded thread: {}", e);
                                log::error!("❌ [THREAD_SERVICE] Failed to send message to loaded thread: {}", e);
                            }
                            continue;
                        }
                        Err(e) => {
                            // CRITICAL: Do NOT create a new thread when we have a valid acp_thread_id!
                            // The thread likely exists but was loaded via UI (agent_panel clicked the session),
                            // so it's not in our THREAD_REGISTRY. Creating a new thread would cause:
                            // 1. Duplicate sessions with different acp_thread_ids
                            // 2. message_completed sent with wrong acp_thread_id
                            // 3. Helix text box never clears (waiting for original acp_thread_id)
                            eprintln!(
                                "❌ [THREAD_SERVICE] Failed to load thread {} from agent: {} - NOT creating new thread (thread may be active via UI)",
                                existing_thread_id, e
                            );
                            log::error!(
                                "❌ [THREAD_SERVICE] Failed to load thread {} from agent: {} - NOT creating new thread",
                                existing_thread_id, e
                            );

                            // Send error event back to Helix so user knows something went wrong
                            let error_event = SyncEvent::ThreadLoadError {
                                acp_thread_id: existing_thread_id.clone(),
                                request_id: request.request_id.clone(),
                                error: format!("Failed to load thread: {}", e),
                            };
                            if let Err(send_err) = crate::send_websocket_event(error_event) {
                                eprintln!("❌ [THREAD_SERVICE] Failed to send error event: {}", send_err);
                            }

                            continue; // Do NOT fall through to create_new_thread_sync!
                        }
                    }
                }
            }

            // Create new ACP thread (synchronously via cx.update to avoid async context issues)
            eprintln!("🆕 [THREAD_SERVICE] Creating new ACP thread for request: {}", request.request_id);
            log::info!("🆕 [THREAD_SERVICE] Creating new ACP thread for request: {}", request.request_id);
            if let Err(e) = cx.update(|cx| {
                create_new_thread_sync(
                    project_for_create.clone(),
                    acp_history_store_for_create.clone(),
                    fs_for_create.clone(),
                    request,
                    cx,
                )
            }) {
                log::error!("❌ [THREAD_SERVICE] Failed to create thread: {}", e);
            }
        }

        log::warn!("⚠️ [THREAD_SERVICE] Handler task exiting - callback channel closed");
        anyhow::Ok(())
    })
    .detach();

    // Spawn handler task to process thread open requests
    cx.spawn(async move |cx| {
        eprintln!("🔧 [THREAD_SERVICE] Open thread handler task started, waiting for requests...");
        log::info!("🔧 [THREAD_SERVICE] Open thread handler task started, waiting for requests...");

        while let Some(request) = open_callback_rx.recv().await {
            eprintln!(
                "📨 [THREAD_SERVICE] Received thread open request: acp_thread_id={}",
                request.acp_thread_id
            );
            log::info!(
                "📨 [THREAD_SERVICE] Received thread open request: acp_thread_id={}",
                request.acp_thread_id
            );

            // Open the thread via agent (loads from database)
            if let Err(e) = cx.update(|cx| {
                open_existing_thread_sync(
                    project_for_open.clone(),
                    acp_history_store_for_open.clone(),
                    fs_for_open.clone(),
                    request,
                    cx,
                )
            }) {
                log::error!("❌ [THREAD_SERVICE] Failed to open thread: {}", e);
            }
        }

        log::warn!("⚠️ [THREAD_SERVICE] Open thread handler task exiting - callback channel closed");
        anyhow::Ok(())
    })
    .detach();

    log::info!("✅ [THREAD_SERVICE] WebSocket thread handler initialized");
}

/// Create a new ACP thread and send the initial message (synchronous version)
fn create_new_thread_sync(
    project: Entity<Project>,
    acp_history_store: Entity<HistoryStore>,
    fs: Arc<dyn Fs>,
    request: ThreadCreationRequest,
    cx: &mut App,
) -> Result<()> {
    log::info!("[THREAD_SERVICE] Creating ACP thread with agent: {:?}", request.agent_name);

    // Ensure language model providers are authenticated before connecting.
    // NativeAgent::new() (called during connect) reads the model list via refresh_list().
    // If providers aren't authenticated yet, the model list is empty and new_thread()
    // creates threads with model=None, causing "No language model configured" errors.
    {
        let registry = language_model::LanguageModelRegistry::global(cx);
        let providers = registry.read(cx).providers().clone();
        eprintln!("🔧 [THREAD_SERVICE] Pre-authenticating {} language model providers...", providers.len());
        for provider in &providers {
            eprintln!("🔧 [THREAD_SERVICE]   Authenticating provider: {}", provider.name().0);
            provider.authenticate(cx);
        }

        // Ensure the default model is selected in the registry
        let settings = agent_settings::AgentSettings::get_global(cx);
        if let Some(ref default_model) = settings.default_model {
            eprintln!("🔧 [THREAD_SERVICE] Setting default model: {}/{}", default_model.provider.0, default_model.model);
            let selected = language_model::SelectedModel {
                provider: language_model::LanguageModelProviderId::from(default_model.provider.0.clone()),
                model: language_model::LanguageModelId::from(default_model.model.clone()),
            };
            language_model::LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.select_default_model(Some(&selected), cx);
                let has = registry.default_model().is_some();
                eprintln!("🔧 [THREAD_SERVICE] Registry default_model set: {}", has);
            });
        }
    }

    let agent = match request.agent_name.as_deref() {
        Some("zed-agent") | None => ExternalAgent::NativeAgent,
        Some(name) => ExternalAgent::Custom {
            name: gpui::SharedString::from(name.to_string()),
            command: project::agent_server_store::AgentServerCommand {
                path: std::path::PathBuf::new(),
                args: vec![],
                env: None,
            },
        },
    };
    let server = agent.server(fs, acp_history_store.clone());

    // Get agent server store from project
    let agent_server_store = project.read(cx).agent_server_store().clone();

    // Create delegate for connection
    let delegate = agent_servers::AgentServerDelegate::new(
        agent_server_store,
        project.clone(),
        None, // status_tx
        None, // new_version_tx
    );

    // Connect to get AgentConnection
    let connection_task = server.connect(None, delegate, cx);

    // Spawn async task to complete the connection and create the thread
    let request_clone = request.clone();
    let project_clone = project.clone();
    cx.spawn(async move |cx| {
        let (connection, _spawn_task) = connection_task
            .await
            .log_err()
            .ok_or_else(|| anyhow::anyhow!("Failed to connect to agent"))?;

        // Authenticate if required
        let auth_methods = connection.auth_methods();
        if let Some(first_method) = auth_methods.first() {
            let auth_task = cx.update(|cx| {
                connection.authenticate(first_method.id.clone(), cx)
            })?;
            if let Err(e) = auth_task.await {
                log::warn!("[THREAD_SERVICE] Authentication failed (continuing): {}", e);
            }
        }

        // Use ZED_WORK_DIR for consistency with agent_panel.rs and thread_view.rs
        // This ensures sessions created here can be found when listing/loading sessions
        // from the UI (which also uses ZED_WORK_DIR as the cwd for project hash calculation)
        let cwd = std::env::var("ZED_WORK_DIR")
            .ok()
            .map(|dir| std::path::PathBuf::from(dir))
            .unwrap_or_else(|| {
                // Fallback to first worktree if ZED_WORK_DIR not set
                cx.update(|cx| {
                    project_clone.read(cx).worktrees(cx).next()
                        .map(|wt| wt.read(cx).abs_path().to_path_buf())
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
                }).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default())
            });
        let thread_entity = cx.update(|cx| {
            connection.new_thread(project_clone.clone(), &cwd, cx)
        })?.await?;

        let acp_thread_id = cx.update(|cx| {
            let thread_id = thread_entity.read(cx).session_id().to_string();
            log::info!("[THREAD_SERVICE] Created ACP thread: {}", thread_id);
            thread_id
        })?;

        // Keep thread entity alive for the duration of this task
        let _thread_keep_alive = thread_entity.clone();

        // Store the current request_id for this thread (so message_completed uses correct ID)
        set_thread_request_id(acp_thread_id.clone(), request_clone.request_id.clone());

        // NOTE: WebSocket event sending is now handled centrally in ThreadView.handle_thread_event
        // This avoids duplicate events when thread is both created here and displayed in UI via from_existing_thread

        // Register thread for follow-up messages (strong reference keeps it alive)
        register_thread(acp_thread_id.clone(), thread_entity.clone());
        eprintln!("📋 [THREAD_SERVICE] Registered thread: {} (strong reference)", acp_thread_id);
        log::info!("📋 [THREAD_SERVICE] Registered thread: {} (strong reference)", acp_thread_id);

        // Send agent_ready event to Helix (signals that agent is ready to receive prompts)
        // This prevents race conditions where Helix sends continue prompts before agent is initialized
        let agent_name_for_ready = request_clone.agent_name.clone().unwrap_or_else(|| "zed-agent".to_string());
        crate::send_agent_ready(agent_name_for_ready, Some(acp_thread_id.clone()));

        // Notify AgentPanel to display this thread (for auto-select in UI)
        if let Err(e) = crate::notify_thread_display(crate::ThreadDisplayNotification {
            thread_entity: thread_entity.clone(),
            helix_session_id: request_clone.request_id.clone(),
            agent_name: request_clone.agent_name.clone(), // Pass agent name for correct UI label
        }) {
            eprintln!("⚠️ [THREAD_SERVICE] Failed to notify thread display: {}", e);
            log::warn!("⚠️ [THREAD_SERVICE] Failed to notify thread display: {}", e);
        } else {
            eprintln!("📤 [THREAD_SERVICE] Notified AgentPanel to display thread");
            log::info!("📤 [THREAD_SERVICE] Notified AgentPanel to display thread");
        }

        // Send thread_created event via WebSocket
        let thread_created_event = SyncEvent::ThreadCreated {
            acp_thread_id: acp_thread_id.clone(),
            request_id: request_clone.request_id.clone(),
        };

        if let Err(e) = crate::send_websocket_event(thread_created_event) {
            eprintln!("❌ [THREAD_SERVICE] Failed to send thread_created event: {}", e);
            log::error!("❌ [THREAD_SERVICE] Failed to send thread_created event: {}", e);
        } else {
            eprintln!("📤 [THREAD_SERVICE] Sent thread_created: {}", acp_thread_id);
            log::info!("📤 [THREAD_SERVICE] Sent thread_created: {}", acp_thread_id);
        }

        // Mark the entry that will be created as external-originated (so we don't echo it back)
        let entry_idx_to_mark = cx.update(|cx| {
            thread_entity.read(cx).entries().len()
        })?;
        mark_external_originated_entry(acp_thread_id.clone(), entry_idx_to_mark);
        eprintln!("🏷️ [THREAD_SERVICE] Marked entry {} as external-originated (won't echo back)", entry_idx_to_mark);

        // Send the initial message to the thread to trigger AI response
        eprintln!("🔧 [THREAD_SERVICE] About to send message to thread...");
        let send_result = cx.update(|cx| {
            thread_entity.update(cx, |thread, cx| {
                let message = vec![ContentBlock::Text(
                    TextContent::new(request_clone.message.clone())
                )];
                eprintln!("🔧 [THREAD_SERVICE] Calling thread.send() with message: {}", request_clone.message);
                thread.send(message, cx)
            })
        })?;

        // Await the send future
        eprintln!("🔧 [THREAD_SERVICE] Awaiting send task...");
        match send_result.await {
            Ok(_) => {
                eprintln!("✅ [THREAD_SERVICE] Send task completed successfully - message sent to AI");
                log::info!("✅ [THREAD_SERVICE] Send task completed successfully");
            }
            Err(e) => {
                eprintln!("❌ [THREAD_SERVICE] Send task failed: {}", e);
                log::error!("❌ [THREAD_SERVICE] Send task failed: {}", e);
            }
        }

        eprintln!("✅ [THREAD_SERVICE] Message send awaited - AI should be responding");
        log::info!("✅ [THREAD_SERVICE] Message send awaited - AI should be responding");

        anyhow::Ok(())
    }).detach();

    Ok(())
}

/// Handle a follow-up message to an existing thread
async fn handle_follow_up_message(
    thread: WeakEntity<AcpThread>,
    thread_id: String,
    request_id: String,
    message: String,
    cx: gpui::AsyncApp,
) -> Result<()> {
    log::info!("💬 [THREAD_SERVICE] Sending follow-up message: {}", message);

    // CRITICAL: Update the request_id for this thread so message_completed uses the correct ID!
    set_thread_request_id(thread_id.clone(), request_id.clone());
    eprintln!("🔄 [THREAD_SERVICE] Updated request_id for thread {} to {}", thread_id, request_id);
    log::info!("🔄 [THREAD_SERVICE] Updated request_id for thread {} to {}", thread_id, request_id);

    // Mark the entry that will be created as external-originated
    let entry_idx_to_mark = cx.update(|cx| {
        thread.update(cx, |thread, _| thread.entries().len())
    })??;
    mark_external_originated_entry(thread_id.clone(), entry_idx_to_mark);
    eprintln!("🏷️ [THREAD_SERVICE] Marked entry {} as external-originated (follow-up)", entry_idx_to_mark);

    cx.update(|cx| {
        let send_task = thread.update(cx, |thread, cx| {
            let message = vec![ContentBlock::Text(
                TextContent::new(message.clone())
            )];
            thread.send(message, cx)
        })?;
        // Spawn the send task
        cx.spawn(async move |_| {
            send_task.await.log_err();
            anyhow::Ok(())
        }).detach();
        anyhow::Ok(())
    })??;

    log::info!("✅ [THREAD_SERVICE] Follow-up message sent successfully");
    Ok(())
}

/// Load an existing thread from the agent (async version for use in message handler)
/// This connects to the agent, loads the session via ACP protocol, registers it, and returns a weak reference.
async fn load_thread_from_agent(
    project: Entity<Project>,
    acp_history_store: Entity<HistoryStore>,
    fs: Arc<dyn Fs>,
    acp_thread_id: String,
    agent_name: Option<String>,
    cx: gpui::AsyncApp,
) -> Result<WeakEntity<AcpThread>> {
    eprintln!("📂 [THREAD_SERVICE] load_thread_from_agent: {} (agent: {:?})", acp_thread_id, agent_name);
    log::info!("📂 [THREAD_SERVICE] load_thread_from_agent: {} (agent: {:?})", acp_thread_id, agent_name);

    // Select agent based on agent_name
    let agent = match agent_name.as_deref() {
        Some("zed-agent") | Some("") | None => ExternalAgent::NativeAgent,
        Some(name) => ExternalAgent::Custom {
            name: gpui::SharedString::from(name.to_string()),
            command: project::agent_server_store::AgentServerCommand {
                path: std::path::PathBuf::new(),
                args: vec![],
                env: None,
            },
        },
    };

    let server = agent.server(fs, acp_history_store.clone());

    // Get agent server store and create connection
    let (connection_task, cwd) = cx.update(|cx| {
        let agent_server_store = project.read(cx).agent_server_store().clone();
        let delegate = agent_servers::AgentServerDelegate::new(
            agent_server_store,
            project.clone(),
            None,
            None,
        );
        let connection_task = server.connect(None, delegate, cx);
        // Use ZED_WORK_DIR for consistency with session storage
        let cwd = std::env::var("ZED_WORK_DIR")
            .ok()
            .map(|dir| std::path::PathBuf::from(dir))
            .unwrap_or_else(|| {
                project.read(cx).worktrees(cx).next()
                    .map(|wt| wt.read(cx).abs_path().to_path_buf())
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
            });
        (connection_task, cwd)
    })?;

    let (connection, _spawn_task) = connection_task.await?;

    eprintln!("✅ [THREAD_SERVICE] Connected to agent for loading thread");
    log::info!("✅ [THREAD_SERVICE] Connected to agent for loading thread");

    // Check if agent supports session loading
    if !connection.supports_session_load() {
        let err = anyhow::anyhow!("Agent does not support session loading");
        eprintln!("⚠️ [THREAD_SERVICE] {}", err);
        log::warn!("⚠️ [THREAD_SERVICE] {}", err);
        return Err(err);
    }

    // Load the thread from agent
    let session_id = agent_client_protocol::SessionId::new(acp_thread_id.clone());
    let project_clone = project.clone();
    let load_task = cx.update(|cx| {
        connection.load_thread(session_id, project_clone, &cwd, cx)
    })?;

    let thread_entity = load_task.await?;

    let loaded_thread_id = cx.update(|cx| {
        thread_entity.read(cx).session_id().to_string()
    })?;

    eprintln!("✅ [THREAD_SERVICE] Loaded thread from agent: {}", loaded_thread_id);
    log::info!("✅ [THREAD_SERVICE] Loaded thread from agent: {}", loaded_thread_id);

    // Subscribe to thread events for streaming responses (same as create_new_thread_sync)
    let thread_id_for_events = loaded_thread_id.clone();
    cx.update(|cx| {
        cx.subscribe(&thread_entity, move |thread_entity, event, cx| {
            match event {
                AcpThreadEvent::NewEntry => {
                    eprintln!("🆕 [THREAD_SERVICE] NewEntry event received (loaded thread)");
                    let thread = thread_entity.read(cx);
                    let latest_idx = thread.entries().len().saturating_sub(1);
                    if is_external_originated_entry(&thread_id_for_events, latest_idx) {
                        eprintln!("🔄 [THREAD_SERVICE] Entry {} from external system, skipping echo", latest_idx);
                        return;
                    }
                    if let Some(entry) = thread.entries().get(latest_idx) {
                        if let acp_thread::AgentThreadEntry::UserMessage(msg) = entry {
                            let content = msg.content.to_markdown(cx).to_string();
                            eprintln!("👤 [THREAD_SERVICE] User typed in Zed (loaded thread), syncing: {} chars", content.len());
                            let event = SyncEvent::MessageAdded {
                                acp_thread_id: thread_id_for_events.clone(),
                                message_id: latest_idx.to_string(),
                                role: "user".to_string(),
                                content: content.clone(),
                                timestamp: chrono::Utc::now().timestamp(),
                            };
                            if let Err(e) = crate::send_websocket_event(event) {
                                eprintln!("❌ [THREAD_SERVICE] Failed to send user message: {}", e);
                            }
                        }
                    }
                }
                AcpThreadEvent::EntryUpdated(entry_idx) => {
                    eprintln!("🔔 [THREAD_SERVICE] EntryUpdated event for entry {} (loaded thread)", entry_idx);
                    let thread = thread_entity.read(cx);
                    if let Some(entry) = thread.entries().get(*entry_idx) {
                        // Handle both AssistantMessage and ToolCall (which contains diffs)
                        let content = match entry {
                            acp_thread::AgentThreadEntry::AssistantMessage(msg) => {
                                msg.content_only(cx)
                            }
                            acp_thread::AgentThreadEntry::ToolCall(tool_call) => {
                                tool_call.to_markdown(cx)
                            }
                            acp_thread::AgentThreadEntry::UserMessage(_) => return,
                        };
                        let event = SyncEvent::MessageAdded {
                            acp_thread_id: thread_id_for_events.clone(),
                            message_id: entry_idx.to_string(),
                            role: "assistant".to_string(),
                            content: content.clone(),
                            timestamp: chrono::Utc::now().timestamp(),
                        };
                        if let Err(e) = crate::send_websocket_event(event) {
                            eprintln!("❌ [THREAD_SERVICE] Failed to send message_added: {}", e);
                        }
                    }
                }
                AcpThreadEvent::Stopped => {
                    // NOTE: MessageCompleted is sent by thread_view.rs when the UI is displayed.
                    // We don't send it here to avoid duplicate events.
                    // notify_thread_display() always creates a UI view after loading a thread.
                    eprintln!("🛑 [THREAD_SERVICE] Thread Stopped event (loaded thread) - UI handles MessageCompleted");
                }
                _ => {}
            }
        })
        .detach();
    })?;

    // Register thread for future access
    register_thread(loaded_thread_id.clone(), thread_entity.clone());
    eprintln!("📋 [THREAD_SERVICE] Registered loaded thread: {}", loaded_thread_id);
    log::info!("📋 [THREAD_SERVICE] Registered loaded thread: {}", loaded_thread_id);

    // Send agent_ready event to Helix (signals that agent is ready to receive prompts)
    let agent_name_for_ready = agent_name.clone().unwrap_or_else(|| "zed-agent".to_string());
    crate::send_agent_ready(agent_name_for_ready, Some(loaded_thread_id.clone()));

    // Notify AgentPanel to display this thread
    if let Err(e) = crate::notify_thread_display(crate::ThreadDisplayNotification {
        thread_entity: thread_entity.clone(),
        helix_session_id: loaded_thread_id.clone(),
        agent_name: agent_name.clone(),
    }) {
        eprintln!("⚠️ [THREAD_SERVICE] Failed to notify thread display: {}", e);
    }

    Ok(thread_entity.downgrade())
}

/// Open an existing ACP thread from database and display it (synchronous version)
fn open_existing_thread_sync(
    project: Entity<Project>,
    acp_history_store: Entity<HistoryStore>,
    fs: Arc<dyn Fs>,
    request: ThreadOpenRequest,
    cx: &mut App,
) -> Result<()> {
    eprintln!("📖 [THREAD_SERVICE] Opening existing ACP thread: {}, agent_name: {:?}",
              request.acp_thread_id, request.agent_name);
    log::info!("📖 [THREAD_SERVICE] Opening existing ACP thread: {}, agent_name: {:?}",
               request.acp_thread_id, request.agent_name);

    // Check if thread is already in registry
    if let Some(_thread_weak) = get_thread(&request.acp_thread_id) {
        eprintln!("✅ [THREAD_SERVICE] Thread already loaded in registry: {}", request.acp_thread_id);
        log::info!("✅ [THREAD_SERVICE] Thread already loaded in registry: {}", request.acp_thread_id);
        // TODO: Still need to notify AgentPanel to display it
        return Ok(());
    }

    // Thread not in registry - need to load from agent
    // Select agent based on agent_name (same logic as create_new_thread_sync)
    let agent = match request.agent_name.as_deref() {
        Some("zed-agent") | Some("") | None => ExternalAgent::NativeAgent,
        Some(name) => ExternalAgent::Custom {
            name: gpui::SharedString::from(name.to_string()),
            command: project::agent_server_store::AgentServerCommand {
                path: std::path::PathBuf::new(),
                args: vec![],
                env: None,
            },
        },
    };
    eprintln!("🔧 [THREAD_SERVICE] Selected agent: {:?}", agent);
    log::info!("🔧 [THREAD_SERVICE] Selected agent: {:?}", agent);

    let server = agent.server(fs, acp_history_store.clone());

    // Get agent server store from project
    let agent_server_store = project.read(cx).agent_server_store().clone();

    // Create delegate for connection
    let delegate = agent_servers::AgentServerDelegate::new(
        agent_server_store,
        project.clone(),
        None, // status_tx
        None, // new_version_tx
    );

    // Connect to get AgentConnection
    let connection_task = server.connect(None, delegate, cx);

    // Use ZED_WORK_DIR for consistency with session storage
    let cwd = std::env::var("ZED_WORK_DIR")
        .ok()
        .map(|dir| std::path::PathBuf::from(dir))
        .unwrap_or_else(|| {
            project.read(cx).worktrees(cx).next()
                .map(|wt| wt.read(cx).abs_path().to_path_buf())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
        });

    // Spawn async task to load the thread from agent
    let request_clone = request.clone();
    let project_clone = project.clone();
    cx.spawn(async move |cx| {
        let (connection, _spawn_task) = match connection_task.await {
            Ok(result) => result,
            Err(e) => {
                eprintln!("❌ [THREAD_SERVICE] Failed to connect to agent: {}", e);
                log::error!("❌ [THREAD_SERVICE] Failed to connect to agent: {}", e);
                return Err(e);
            }
        };

        eprintln!("✅ [THREAD_SERVICE] Connected to agent server for thread loading");
        log::info!("✅ [THREAD_SERVICE] Connected to agent server for thread loading");

        // Check if agent supports session loading
        if !connection.supports_session_load() {
            eprintln!("⚠️ [THREAD_SERVICE] Agent does not support session loading");
            log::warn!("⚠️ [THREAD_SERVICE] Agent does not support session loading");
            return Err(anyhow::anyhow!("Agent does not support session loading"));
        }

        eprintln!("🔨 [THREAD_SERVICE] Calling connection.load_thread() to load from agent...");
        log::info!("🔨 [THREAD_SERVICE] Calling connection.load_thread() to load from agent...");

        // Convert string to SessionId
        let session_id = agent_client_protocol::SessionId::new(request_clone.acp_thread_id.clone());

        // Use the generic AgentConnection::load_thread() method
        // This works for both NativeAgent (from local DB) and ACP agents (via session/load protocol)
        let load_task = cx.update(|cx| {
            connection.load_thread(session_id, project_clone, &cwd, cx)
        })?;

        let thread_entity = match load_task.await {
            Ok(entity) => entity,
            Err(e) => {
                eprintln!("❌ [THREAD_SERVICE] connection.load_thread() failed: {}", e);
                log::error!("❌ [THREAD_SERVICE] connection.load_thread() failed: {}", e);
                return Err(e);
            }
        };

        let acp_thread_id = cx.update(|cx| {
            let thread_id = thread_entity.read(cx).session_id().to_string();
            eprintln!("✅ [THREAD_SERVICE] Loaded ACP thread from agent: {} (session_id)", thread_id);
            log::info!("✅ [THREAD_SERVICE] Loaded ACP thread from agent: {} (session_id)", thread_id);
            thread_id
        })?;

        // Register thread for future access (strong reference keeps it alive)
        register_thread(acp_thread_id.clone(), thread_entity.clone());
        eprintln!("📋 [THREAD_SERVICE] Registered thread: {} (strong reference)", acp_thread_id);
        log::info!("📋 [THREAD_SERVICE] Registered thread: {} (strong reference)", acp_thread_id);

        // Send agent_ready event to Helix (signals that agent is ready to receive prompts)
        let agent_name_for_ready = request_clone.agent_name.clone().unwrap_or_else(|| "zed-agent".to_string());
        crate::send_agent_ready(agent_name_for_ready, Some(acp_thread_id.clone()));

        // Notify AgentPanel to display this thread (for auto-select in UI)
        if let Err(e) = crate::notify_thread_display(crate::ThreadDisplayNotification {
            thread_entity: thread_entity.clone(),
            helix_session_id: acp_thread_id.clone(),
            agent_name: request_clone.agent_name.clone(),
        }) {
            eprintln!("⚠️ [THREAD_SERVICE] Failed to notify thread display: {}", e);
            log::warn!("⚠️ [THREAD_SERVICE] Failed to notify thread display: {}", e);
        } else {
            eprintln!("📤 [THREAD_SERVICE] Notified AgentPanel to display opened thread");
            log::info!("📤 [THREAD_SERVICE] Notified AgentPanel to display opened thread");
        }

        eprintln!("✅ [THREAD_SERVICE] Thread opened and displayed successfully");
        log::info!("✅ [THREAD_SERVICE] Thread opened and displayed successfully");

        anyhow::Ok(())
    }).detach();

    Ok(())
}
