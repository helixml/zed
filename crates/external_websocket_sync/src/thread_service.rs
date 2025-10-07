//! Thread management service for WebSocket integration
//!
//! This is the NON-UI service layer that manages ACP threads for external WebSocket control.
//! Called from workspace creation, contains all business logic.

use anyhow::Result;
use acp_thread::{AcpThread, AcpThreadEvent};
use action_log::ActionLog;
use agent2::HistoryStore;
use agent_client_protocol::{ContentBlock, PromptCapabilities, SessionId, TextContent};
use gpui::{App, Entity, EventEmitter, WeakEntity, prelude::*};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use fs::Fs;
use project::Project;
use tokio::sync::mpsc;
use util::ResultExt;
use watch;

use crate::{ExternalAgent, ThreadCreationRequest, SyncEvent};

/// Global registry of active ACP threads (service layer)
/// Stores STRONG references to keep threads alive for follow-up messages
static THREAD_REGISTRY: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, Entity<AcpThread>>>>>> =
    parking_lot::Mutex::new(None);

/// Global map of thread_id -> current_request_id
/// Tracks the request_id for the CURRENT/LATEST message being processed by each thread
/// This ensures message_completed events use the correct request_id (not the first one)
static THREAD_REQUEST_MAP: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, String>>>>> =
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
                    eprintln!(
                        "⚠️ [THREAD_SERVICE] Thread {} not found, creating new thread",
                        existing_thread_id
                    );
                    log::warn!(
                        "⚠️ [THREAD_SERVICE] Thread {} not found, creating new thread",
                        existing_thread_id
                    );
                }
            }

            // Create new ACP thread (synchronously via cx.update to avoid async context issues)
            eprintln!("🆕 [THREAD_SERVICE] Creating new ACP thread for request: {}", request.request_id);
            log::info!("🆕 [THREAD_SERVICE] Creating new ACP thread for request: {}", request.request_id);
            if let Err(e) = cx.update(|cx| {
                create_new_thread_sync(
                    project.clone(),
                    acp_history_store.clone(),
                    fs.clone(),
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
    eprintln!("🔨 [THREAD_SERVICE] Creating ACP thread...");
    log::info!("🔨 [THREAD_SERVICE] Creating ACP thread...");

    // Create agent server
    let agent = ExternalAgent::NativeAgent;
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

        eprintln!("✅ [THREAD_SERVICE] Connected to agent server");
        log::info!("✅ [THREAD_SERVICE] Connected to agent server");

        // Create thread using connection's new_thread method (properly registers session)
        eprintln!("🔨 [THREAD_SERVICE] Calling connection.new_thread()...");
        let cwd = std::path::Path::new("/workspace");
        let thread_creation_task = cx.update(|cx| {
            connection.new_thread(project_clone.clone(), cwd, cx)
        })?;

        let thread_entity = thread_creation_task.await?;

        let acp_thread_id = cx.update(|cx| {
            let thread_id = thread_entity.entity_id().to_string();
            eprintln!("✅ [THREAD_SERVICE] Created ACP thread: {}", thread_id);
            log::info!("✅ [THREAD_SERVICE] Created ACP thread: {}", thread_id);
            thread_id
        })?;

        // Keep thread entity alive for the duration of this task
        let _thread_keep_alive = thread_entity.clone();

        // Store the current request_id for this thread (so message_completed uses correct ID)
        set_thread_request_id(acp_thread_id.clone(), request_clone.request_id.clone());

        // Subscribe to thread events for streaming responses
        let thread_id_for_events = acp_thread_id.clone();
        cx.update(|cx| {
            cx.subscribe(&thread_entity, move |thread_entity, event, cx| {
                match event {
                    AcpThreadEvent::EntryUpdated(entry_idx) => {
                        eprintln!("🔔 [THREAD_SERVICE] EntryUpdated event received for entry {}", entry_idx);
                        // Get the updated content
                        let thread = thread_entity.read(cx);
                        if let Some(entry) = thread.entries().get(*entry_idx) {
                            // Extract content from AssistantMessage variant
                            let content = match entry {
                                acp_thread::AgentThreadEntry::AssistantMessage(msg) => {
                                    // Use content_only() to avoid "## Assistant" heading in Helix UI
                                    msg.content_only(cx)
                                }
                                _ => {
                                    eprintln!("⚠️ [THREAD_SERVICE] Entry {} is not an AssistantMessage, skipping", entry_idx);
                                    return; // Only send events for assistant messages
                                }
                            };

                            // Send message_added event
                            let event = SyncEvent::MessageAdded {
                                acp_thread_id: thread_id_for_events.clone(),
                                message_id: entry_idx.to_string(),
                                role: "assistant".to_string(),
                                content: content.clone(),
                                timestamp: chrono::Utc::now().timestamp(),
                            };

                            if let Err(e) = crate::send_websocket_event(event) {
                                eprintln!("❌ [THREAD_SERVICE] Failed to send message_added: {}", e);
                                log::error!("❌ [THREAD_SERVICE] Failed to send message_added: {}", e);
                            } else {
                                eprintln!("📤 [THREAD_SERVICE] Sent message_added chunk (entry {}): {} chars", entry_idx, content.len());
                                log::debug!("📤 [THREAD_SERVICE] Sent message_added chunk (entry {})", entry_idx);
                            }
                        }
                    }
                    AcpThreadEvent::Stopped => {
                        eprintln!("🛑 [THREAD_SERVICE] Thread Stopped event received");

                        // Get the CURRENT request_id for this thread (not the first one!)
                        let current_request_id = get_thread_request_id(&thread_id_for_events)
                            .unwrap_or_else(|| {
                                eprintln!("⚠️ [THREAD_SERVICE] No request_id found for thread {}, using empty string", thread_id_for_events);
                                String::new()
                            });

                        // Send message_completed event with CORRECT request_id
                        let event = SyncEvent::MessageCompleted {
                            acp_thread_id: thread_id_for_events.clone(),
                            message_id: "0".to_string(), // TODO: track actual message ID
                            request_id: current_request_id.clone(),
                        };

                        if let Err(e) = crate::send_websocket_event(event) {
                            eprintln!("❌ [THREAD_SERVICE] Failed to send message_completed: {}", e);
                            log::error!("❌ [THREAD_SERVICE] Failed to send message_completed: {}", e);
                        } else {
                            eprintln!("📤 [THREAD_SERVICE] Sent message_completed for request: {}", current_request_id);
                            log::info!("📤 [THREAD_SERVICE] Sent message_completed for request: {}", current_request_id);
                        }
                    }
                    _ => {}
                }
            })
            .detach();
        })?;

        // Register thread for follow-up messages (strong reference keeps it alive)
        register_thread(acp_thread_id.clone(), thread_entity.clone());
        eprintln!("📋 [THREAD_SERVICE] Registered thread: {} (strong reference)", acp_thread_id);
        log::info!("📋 [THREAD_SERVICE] Registered thread: {} (strong reference)", acp_thread_id);

        // Notify AgentPanel to display this thread (for auto-select in UI)
        if let Err(e) = crate::notify_thread_display(crate::ThreadDisplayNotification {
            thread_entity: thread_entity.clone(),
            helix_session_id: request_clone.request_id.clone(),
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

        // Send the initial message to the thread to trigger AI response
        eprintln!("🔧 [THREAD_SERVICE] About to send message to thread...");
        let send_task = cx.update(|cx| {
            eprintln!("🔧 [THREAD_SERVICE] Updating thread entity to send message...");
            let send_task = thread_entity.update(cx, |thread, cx| {
                let message = vec![ContentBlock::Text(TextContent {
                    text: request_clone.message.clone(),
                    annotations: None,
                    meta: None,
                })];
                eprintln!("🔧 [THREAD_SERVICE] Calling thread.send() with message: {}", request_clone.message);
                thread.send(message, cx)
            });
            eprintln!("✅ [THREAD_SERVICE] thread.send() returned Task");
            send_task
        })?;

        // Await the send task directly (don't spawn and detach)
        eprintln!("🔧 [THREAD_SERVICE] Awaiting send task...");
        match send_task.await {
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

    cx.update(|cx| {
        let send_task = thread.update(cx, |thread, cx| {
            let message = vec![ContentBlock::Text(TextContent {
                text: message.clone(),
                annotations: None,
                meta: None,
            })];
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
