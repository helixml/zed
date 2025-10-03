//! Thread management service for WebSocket integration
//!
//! This is the NON-UI layer that manages ACP threads for external WebSocket control.
//! It sits between the WebSocket protocol layer and the ACP thread system.

use anyhow::Result;
use acp_thread::{AcpThread, AcpThreadEvent};
use agent2::HistoryStore;
use gpui::{App, AsyncApp, Context, Entity, WeakEntity};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

use crate::{ThreadCreationRequest, SyncEvent};

/// Global registry of active ACP threads (non-UI layer)
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

/// Handle thread creation request from WebSocket
/// This should be called from the WebSocket callback, NOT from UI code
pub async fn handle_thread_creation_request(
    request: ThreadCreationRequest,
    history_store: WeakEntity<HistoryStore>,
    mut cx: AsyncApp,
) -> Result<()> {
    if let Some(acp_thread_id) = request.acp_thread_id {
        // Follow-up message - send to existing thread
        if let Some(thread) = get_thread(&acp_thread_id) {
            thread.update(&mut cx, |thread, cx| {
                thread.run_user_prompt(request.message, cx)
            })?;
            log::info!("📬 Sent follow-up message to thread: {}", acp_thread_id);
        } else {
            log::error!("❌ Thread not found: {}", acp_thread_id);
        }
    } else {
        // Create new thread
        let acp_thread_id = cx.update(|cx| {
            // TODO: Create ACP thread via HistoryStore
            // For now, return a placeholder
            format!("acp-thread-{}", uuid::Uuid::new_v4())
        })?;

        // Send thread_created event
        let event = SyncEvent::ThreadCreated {
            acp_thread_id: acp_thread_id.clone(),
            request_id: request.request_id,
        };

        crate::send_websocket_event(event)?;
        log::info!("📤 Sent thread_created: {}", acp_thread_id);

        // TODO: Subscribe to thread events and send message_added/message_completed
    }

    Ok(())
}
