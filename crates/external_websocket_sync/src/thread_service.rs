//! Thread management service for WebSocket integration
//!
//! This is the NON-UI service layer that manages ACP threads for external WebSocket control.
//! Called from workspace creation, contains all business logic.

use anyhow::Result;
use acp_thread::{AcpThread, AcpThreadEvent};
use action_log::ActionLog;
use agent2::HistoryStore;
use agent_client_protocol::{ContentBlock, PromptCapabilities, SessionId, TextContent};
use gpui::{App, Entity, WeakEntity, prelude::*};
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
static THREAD_REGISTRY: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, WeakEntity<AcpThread>>>>>> =
    parking_lot::Mutex::new(None);

/// Initialize the thread registry
pub fn init_thread_registry() {
    let mut registry = THREAD_REGISTRY.lock();
    if registry.is_none() {
        *registry = Some(Arc::new(RwLock::new(HashMap::new())));
    }
}

/// Register an active thread
pub fn register_thread(acp_thread_id: String, thread: WeakEntity<AcpThread>) {
    init_thread_registry();
    let registry = THREAD_REGISTRY.lock();
    if let Some(reg) = registry.as_ref() {
        reg.write().insert(acp_thread_id, thread);
    }
}

/// Get an active thread
pub fn get_thread(acp_thread_id: &str) -> Option<WeakEntity<AcpThread>> {
    let registry = THREAD_REGISTRY.lock();
    registry.as_ref()?.read().get(acp_thread_id).cloned()
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
        log::info!("🔧 [THREAD_SERVICE] Handler task started, waiting for requests...");

        while let Some(request) = callback_rx.recv().await {
            log::info!(
                "📨 [THREAD_SERVICE] Received thread creation request: acp_thread_id={:?}, request_id={}",
                request.acp_thread_id,
                request.request_id
            );

            // Check if this is a follow-up message to existing thread
            if let Some(existing_thread_id) = &request.acp_thread_id {
                if let Some(thread) = get_thread(existing_thread_id) {
                    log::info!(
                        "🔄 [THREAD_SERVICE] Sending to existing thread: {}",
                        existing_thread_id
                    );
                    if let Err(e) = handle_follow_up_message(thread, request.message, cx.clone()).await {
                        log::error!("❌ [THREAD_SERVICE] Failed to send follow-up message: {}", e);
                    }
                    continue;
                }
                log::warn!(
                    "⚠️ [THREAD_SERVICE] Thread {} not found, creating new thread",
                    existing_thread_id
                );
            }

            // Create new ACP thread (synchronously via cx.update to avoid async context issues)
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

        log::info!("✅ [THREAD_SERVICE] Connected to agent server");

        // Create ACP thread entity
        let (acp_thread_id, thread_entity, action_log_entity) = cx.update(|cx| {
            // Create action log entity
            let action_log = cx.new(|_| ActionLog::new(project_clone.clone()));
            let (_, prompt_caps_rx) = watch::channel(PromptCapabilities::default());

            // Create thread entity
            let thread = cx.new(|cx| {
                acp_thread::AcpThread::new(
                    gpui::SharedString::from("External Agent Session"),
                    connection.clone(),
                    project_clone.clone(),
                    action_log.clone(),
                    SessionId(Arc::from(uuid::Uuid::new_v4().to_string())),
                    prompt_caps_rx.clone(),
                    cx,
                )
            });

            let thread_id = thread.entity_id().to_string();
            log::info!("✅ [THREAD_SERVICE] Created ACP thread: {}", thread_id);

            (thread_id, thread, action_log)
        })?;

        // Keep action_log entity alive for the thread's lifetime
        let _action_log_keep_alive = action_log_entity;

        // Subscribe to thread events for streaming responses
        let thread_id_for_events = acp_thread_id.clone();
        let request_id_for_events = request_clone.request_id.clone();
        cx.update(|cx| {
            cx.subscribe(&thread_entity, move |thread_entity, event, cx| {
                match event {
                    AcpThreadEvent::EntryUpdated(entry_idx) => {
                        // Get the updated content
                        let thread = thread_entity.read(cx);
                        if let Some(entry) = thread.entries().get(*entry_idx) {
                            // Extract content from AssistantMessage variant
                            let content = match entry {
                                acp_thread::AgentThreadEntry::AssistantMessage(msg) => {
                                    msg.to_markdown(cx)
                                }
                                _ => return, // Only send events for assistant messages
                            };

                            // Send message_added event
                            let event = SyncEvent::MessageAdded {
                                acp_thread_id: thread_id_for_events.clone(),
                                message_id: entry_idx.to_string(),
                                role: "assistant".to_string(),
                                content,
                                timestamp: chrono::Utc::now().timestamp(),
                            };

                            if let Err(e) = crate::send_websocket_event(event) {
                                log::error!("❌ [THREAD_SERVICE] Failed to send message_added: {}", e);
                            } else {
                                log::debug!("📤 [THREAD_SERVICE] Sent message_added chunk (entry {})", entry_idx);
                            }
                        }
                    }
                    AcpThreadEvent::Stopped => {
                        // Send message_completed event
                        let event = SyncEvent::MessageCompleted {
                            acp_thread_id: thread_id_for_events.clone(),
                            message_id: "0".to_string(), // TODO: track actual message ID
                            request_id: request_id_for_events.clone(),
                        };

                        if let Err(e) = crate::send_websocket_event(event) {
                            log::error!("❌ [THREAD_SERVICE] Failed to send message_completed: {}", e);
                        } else {
                            log::info!("📤 [THREAD_SERVICE] Sent message_completed for request: {}", request_id_for_events);
                        }
                    }
                    _ => {}
                }
            })
            .detach();
        })?;

        // Register thread for follow-up messages
        register_thread(acp_thread_id.clone(), thread_entity.downgrade());
        log::info!("📋 [THREAD_SERVICE] Registered thread: {}", acp_thread_id);

        // Send thread_created event via WebSocket
        let thread_created_event = SyncEvent::ThreadCreated {
            acp_thread_id: acp_thread_id.clone(),
            request_id: request_clone.request_id.clone(),
        };

        if let Err(e) = crate::send_websocket_event(thread_created_event) {
            log::error!("❌ [THREAD_SERVICE] Failed to send thread_created event: {}", e);
        } else {
            log::info!("📤 [THREAD_SERVICE] Sent thread_created: {}", acp_thread_id);
        }

        // Send the initial message to the thread to trigger AI response
        cx.update(|cx| {
            let send_task = thread_entity.update(cx, |thread, cx| {
                let message = vec![ContentBlock::Text(TextContent {
                    text: request_clone.message.clone(),
                    annotations: None,
                    meta: None,
                })];
                thread.send(message, cx)
            });
            // Spawn the send task
            cx.spawn(async move |_| {
                send_task.await.log_err();
                anyhow::Ok(())
            }).detach();
        })?;

        log::info!("✅ [THREAD_SERVICE] Sent initial message to thread - AI should start responding");

        anyhow::Ok(())
    }).detach();

    Ok(())
}

/// Handle a follow-up message to an existing thread
async fn handle_follow_up_message(
    thread: WeakEntity<AcpThread>,
    message: String,
    cx: gpui::AsyncApp,
) -> Result<()> {
    log::info!("💬 [THREAD_SERVICE] Sending follow-up message: {}", message);

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
