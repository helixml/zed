//! Thread management service for WebSocket integration
//!
//! This is the NON-UI service layer that manages ACP threads for external WebSocket control.
//! Called from workspace creation, contains all business logic.

use anyhow::Result;
use acp_thread::{AcpThread, AcpThreadEvent};
use agent::ThreadStore;
use agent_client_protocol::schema as acp;
use acp::{ContentBlock, TextContent};
use util::path_list::PathList;
use gpui::{App, Entity, WeakEntity};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use fs::Fs;
use project::Project;
use tokio::sync::mpsc;
use util::ResultExt;
use crate::{ExternalAgent, ThreadCreationRequest, ThreadOpenRequest, SyncEvent};

/// Global registry of active ACP threads (service layer)
/// Stores STRONG references to keep threads alive for follow-up messages
static THREAD_REGISTRY: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, Entity<AcpThread>>>>>> =
    parking_lot::Mutex::new(None);

/// Keeps strong references to ALL threads ever created/loaded, preventing them
/// from being released when the UI switches to a different thread. Unlike
/// THREAD_REGISTRY (which gets cleaned up by unregister_thread on UI transitions),
/// this map is append-only. This ensures follow-up messages to non-visible threads
/// can find the thread entity without needing load_session.
static THREAD_KEEP_ALIVE: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, Entity<AcpThread>>>>>> =
    parking_lot::Mutex::new(None);

/// Global map of acp_thread_id -> agent_session_id
/// The agent (e.g. Claude Code) uses its own session IDs that differ from Zed's thread UUIDs.
/// We store this mapping when a thread is created so we can pass the correct session ID
/// to load_session when reloading a thread that was unloaded.
static THREAD_AGENT_SESSION_MAP: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, String>>>>> =
    parking_lot::Mutex::new(None);

/// Global map of acp_thread_id -> agent_name (e.g. "claude", "qwen", "zed-agent").
///
/// Helix's `SendChatMessage` path used for follow-up messages on existing
/// threads strips `agent_name` from the chat_message command (only the
/// initial create-thread message carries it). When the wrapper-side wedge
/// recovery fires on a follow-up, the request's `agent_name` is therefore
/// missing, and routing to the wrong `ExternalAgent` variant (e.g.
/// NativeAgent for a claude thread) makes `force_close_session` dispatch
/// to the trait default instead of `AcpConnection`'s override. Recording
/// the agent at thread-create / thread-load time lets the recovery path
/// look up the correct agent independently of the incoming request.
static THREAD_AGENT_NAME_MAP: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, String>>>>> =
    parking_lot::Mutex::new(None);

/// Global map of thread_id -> current_request_id
/// Tracks the request_id for the CURRENT/LATEST message being processed by each thread
/// This ensures message_completed events use the correct request_id (not the first one)
static THREAD_REQUEST_MAP: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, String>>>>> =
    parking_lot::Mutex::new(None);

/// Reverse map of request_id -> thread_id for cancel_current_turn lookups
static REQUEST_TO_THREAD_MAP: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, String>>>>> =
    parking_lot::Mutex::new(None);

/// Global map of thread_id -> Set of entry indices that originated from external system
/// Prevents echoing external messages back (initial + follow-ups)
static EXTERNAL_ORIGINATED_ENTRIES: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, HashSet<usize>>>>>> =
    parking_lot::Mutex::new(None);

/// Set of thread_ids that already have a persistent event subscription
/// Prevents creating duplicate subscriptions when follow-up messages arrive
static PERSISTENT_SUBSCRIPTIONS: parking_lot::Mutex<Option<Arc<RwLock<HashSet<String>>>>> =
    parking_lot::Mutex::new(None);

/// Guards against concurrent thread loading. Only one thread load can be
/// in progress at a time (the UI only shows one thread anyway). Prevents
/// double-load when workspace restore and open_thread race.
static THREAD_LOAD_IN_PROGRESS: parking_lot::Mutex<Option<String>> =
    parking_lot::Mutex::new(None);

/// Pending `UserCreatedThread` emissions for draft threads.
///
/// The agent panel speculatively creates a "draft" thread (via
/// `agent_panel::activate_draft` → `ConversationView::new` with
/// `resume_session_id=None`) every time the panel is shown — even when
/// the user is not asking for a new chat. Emitting `UserCreatedThread`
/// to Helix at thread-creation time made every container restart leak
/// an empty "New Chat" `helix_session` row in `spec_task_zed_threads`,
/// each one tying to a duplicate Claude ACP spawn that races against
/// the existing one for the npm `_npx/<hash>` cache. See
/// `helix/design/2026-05-13-mcp-cache-contention-and-duplicate-claude-spawn.md`.
///
/// Fix: when the draft thread is created we register the (acp_thread_id,
/// title) tuple here instead of emitting immediately. The existing
/// `ensure_thread_subscription` `NewEntry` handler checks this map on
/// every new entry; on the first user-role entry the emit is flushed and
/// the entry removed. Threads the user never types into are never
/// announced to Helix and never produce a phantom session row.
static PENDING_USER_CREATED_EMITS: std::sync::LazyLock<parking_lot::Mutex<HashMap<String, Option<String>>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Defer the `UserCreatedThread` emission for `acp_thread_id` until the
/// first user-role `NewEntry` arrives on the thread. Called from
/// `ConversationView::initial_state` for the `!is_resume` branch.
pub fn defer_user_created_thread(acp_thread_id: String, title: Option<String>) {
    eprintln!(
        "📌 [THREAD_SERVICE] Deferring UserCreatedThread for {} (title={:?}) until first user message",
        acp_thread_id, title
    );
    log::info!(
        "📌 [THREAD_SERVICE] Deferring UserCreatedThread for {} (title={:?}) until first user message",
        acp_thread_id, title
    );
    PENDING_USER_CREATED_EMITS
        .lock()
        .insert(acp_thread_id, title);
}

/// Flush a deferred `UserCreatedThread` emission if one is pending for
/// this acp_thread_id. Called from the `ensure_thread_subscription`
/// `NewEntry` handler the first time a user-role entry is observed on
/// the thread. Returns true if an emission was flushed.
fn try_flush_pending_user_created_thread(acp_thread_id: &str) -> bool {
    let pending = PENDING_USER_CREATED_EMITS.lock().remove(acp_thread_id);
    let Some(title) = pending else {
        return false;
    };
    eprintln!(
        "📤 [THREAD_SERVICE] Flushing deferred UserCreatedThread for {} on first user message",
        acp_thread_id
    );
    log::info!(
        "📤 [THREAD_SERVICE] Flushing deferred UserCreatedThread for {} on first user message",
        acp_thread_id
    );
    if let Err(e) = crate::send_websocket_event(crate::SyncEvent::UserCreatedThread {
        acp_thread_id: acp_thread_id.to_string(),
        title,
    }) {
        log::error!(
            "Failed to send deferred UserCreatedThread WebSocket event for {}: {}",
            acp_thread_id, e
        );
    }
    true
}

/// Drop a pending `UserCreatedThread` emission without firing it. Used
/// when a thread that was registered for deferred emission is being
/// disposed (e.g. the user dismisses the draft without ever typing).
pub fn drop_pending_user_created_thread(acp_thread_id: &str) -> bool {
    PENDING_USER_CREATED_EMITS
        .lock()
        .remove(acp_thread_id)
        .is_some()
}

/// Try to acquire the thread load lock. Returns true if acquired (no other
/// load in progress), false if another thread is currently loading.
/// Must be paired with `release_thread_load_lock` when the load completes.
pub fn try_acquire_thread_load_lock(thread_id: &str) -> bool {
    let mut loading = THREAD_LOAD_IN_PROGRESS.lock();
    if loading.is_some() {
        eprintln!(
            "⏳ [THREAD_SERVICE] Thread load lock busy (current={:?}), skipping load of {}",
            loading, thread_id
        );
        false
    } else {
        eprintln!("🔒 [THREAD_SERVICE] Acquired thread load lock for {} (from panel restoration)", thread_id);
        *loading = Some(thread_id.to_string());
        true
    }
}

/// Release the thread load lock after a load completes or fails.
pub fn release_thread_load_lock() {
    let mut loading = THREAD_LOAD_IN_PROGRESS.lock();
    eprintln!("🔓 [THREAD_SERVICE] Released thread load lock (was {:?}, from panel restoration)", loading);
    *loading = None;
}

/// Returns the thread_id currently held in the load lock, if any.
pub fn get_load_in_progress_thread() -> Option<String> {
    THREAD_LOAD_IN_PROGRESS.lock().clone()
}

/// Streaming throttle state per message entry.
/// Keyed by "{thread_id}:{entry_idx}" to support multi-entry streaming.
static STREAMING_THROTTLE: parking_lot::Mutex<Option<Arc<RwLock<HashMap<String, StreamingThrottleState>>>>> =
    parking_lot::Mutex::new(None);

/// Minimum interval between message_added events for the same entry.
/// Reduces Zed→Go wire traffic by ~90% (10 events/sec instead of 100+).
const STREAMING_THROTTLE_INTERVAL: Duration = Duration::from_millis(100);

/// Per-entry throttle state for streaming events.
struct StreamingThrottleState {
    last_sent: Instant,
    pending_content: Option<PendingMessage>,
    flush_scheduled: bool,
}

/// Content waiting to be sent when the throttle window expires.
struct PendingMessage {
    acp_thread_id: String,
    message_id: String,
    role: String,
    content: String,
    request_id: String,
    entry_type: String,
    tool_name: String,
    tool_status: String,
}

/// Initialize the thread registry
pub fn init_thread_registry() {
    let mut registry = THREAD_REGISTRY.lock();
    if registry.is_none() {
        *registry = Some(Arc::new(RwLock::new(HashMap::new())));
    }

    let mut keep_alive = THREAD_KEEP_ALIVE.lock();
    if keep_alive.is_none() {
        *keep_alive = Some(Arc::new(RwLock::new(HashMap::new())));
    }

    let mut session_map = THREAD_AGENT_SESSION_MAP.lock();
    if session_map.is_none() {
        *session_map = Some(Arc::new(RwLock::new(HashMap::new())));
    }

    let mut agent_name_map = THREAD_AGENT_NAME_MAP.lock();
    if agent_name_map.is_none() {
        *agent_name_map = Some(Arc::new(RwLock::new(HashMap::new())));
    }

    let mut request_map = THREAD_REQUEST_MAP.lock();
    if request_map.is_none() {
        *request_map = Some(Arc::new(RwLock::new(HashMap::new())));
    }

    let mut external_map = EXTERNAL_ORIGINATED_ENTRIES.lock();
    if external_map.is_none() {
        *external_map = Some(Arc::new(RwLock::new(HashMap::new())));
    }

    let mut persistent_subs = PERSISTENT_SUBSCRIPTIONS.lock();
    if persistent_subs.is_none() {
        *persistent_subs = Some(Arc::new(RwLock::new(HashSet::new())));
    }

    let mut request_to_thread = REQUEST_TO_THREAD_MAP.lock();
    if request_to_thread.is_none() {
        *request_to_thread = Some(Arc::new(RwLock::new(HashMap::new())));
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

/// Check if a thread already has a persistent event subscription
fn has_persistent_subscription(thread_id: &str) -> bool {
    init_thread_registry();
    let subs = PERSISTENT_SUBSCRIPTIONS.lock();
    subs.as_ref().map_or(false, |s| s.read().contains(thread_id))
}

/// Mark a thread as having a persistent event subscription
fn mark_persistent_subscription(thread_id: String) {
    init_thread_registry();
    let subs = PERSISTENT_SUBSCRIPTIONS.lock();
    if let Some(s) = subs.as_ref() {
        s.write().insert(thread_id);
    }
}

/// Initialize the streaming throttle state
fn init_streaming_throttle() {
    let mut throttle = STREAMING_THROTTLE.lock();
    if throttle.is_none() {
        *throttle = Some(Arc::new(RwLock::new(HashMap::new())));
    }
}

/// Flush pending throttled content for all entries in a thread OTHER than the
/// specified entry index. Called from the NewEntry handler to ensure the
/// preceding text entry's content is sent before a new entry (e.g. tool_call).
fn flush_stale_pending_for_thread(acp_thread_id: &str, exclude_entry_idx: usize) {
    init_streaming_throttle();
    let thread_prefix = format!("{}:", acp_thread_id);
    let exclude_key = format!("{}:{}", acp_thread_id, exclude_entry_idx);
    let now = Instant::now();

    let mut stale_pending: Vec<PendingMessage> = Vec::new();
    {
        let throttle_map = STREAMING_THROTTLE.lock();
        let Some(map) = throttle_map.as_ref() else { return };
        let mut map = map.write();

        for (k, state) in map.iter_mut() {
            if k.starts_with(&thread_prefix) && *k != exclude_key {
                if let Some(pending) = state.pending_content.take() {
                    state.last_sent = now;
                    state.flush_scheduled = false;
                    stale_pending.push(pending);
                }
            }
        }
    }

    for pending in stale_pending {
        let _ = crate::send_websocket_event(SyncEvent::MessageAdded {
            acp_thread_id: pending.acp_thread_id,
            message_id: pending.message_id,
            role: pending.role,
            content: pending.content,
            request_id: pending.request_id,
            entry_type: pending.entry_type,
            tool_name: pending.tool_name,
            tool_status: pending.tool_status,
            timestamp: chrono::Utc::now().timestamp(),
        });
    }
}

/// Throttled send of message_added events. Only sends if enough time has passed
/// since the last send for this entry. Otherwise, stores the content as pending.
/// Returns true if the event was sent, false if throttled.
fn throttled_send_message_added(
    acp_thread_id: &str,
    entry_idx: usize,
    role: &str,
    content: String,
    request_id: &str,
    entry_type: &str,
    tool_name: &str,
    tool_status: &str,
) -> bool {
    init_streaming_throttle();
    let key = format!("{}:{}", acp_thread_id, entry_idx);
    let thread_prefix = format!("{}:", acp_thread_id);
    let now = Instant::now();

    // Hold locks only while reading/mutating state, collect messages to send after release.
    let mut stale_pending: Vec<PendingMessage> = Vec::new();
    let mut current_to_send: Option<PendingMessage> = None;
    let sent: bool;
    let mut spawn_flush_timer = false;

    {
        let throttle_map = STREAMING_THROTTLE.lock();
        let Some(map) = throttle_map.as_ref() else { return false };
        let mut map = map.write();

        // Flush pending content for all OTHER entries in this thread.
        // This ensures each entry's final content (e.g. tool call
        // "Status: Completed") is sent before we move on, rather than
        // waiting for the end-of-turn flush.
        for (k, state) in map.iter_mut() {
            if k.starts_with(&thread_prefix) && *k != key {
                if let Some(pending) = state.pending_content.take() {
                    state.last_sent = now;
                    stale_pending.push(pending);
                    state.flush_scheduled = false;
                }
            }
        }

        let state = map.entry(key.clone()).or_insert_with(|| StreamingThrottleState {
            last_sent: Instant::now() - STREAMING_THROTTLE_INTERVAL,
            pending_content: None,
            flush_scheduled: false,
        });

        // Tool call entries bypass the throttle — they're infrequent and must
        // arrive promptly so the preceding text entry's stale-pending flush
        // reaches the API before the tool call does.
        if now.duration_since(state.last_sent) >= STREAMING_THROTTLE_INTERVAL
            || entry_type == "tool_call"
        {
            state.last_sent = now;
            state.pending_content = None;
            state.flush_scheduled = false;
            current_to_send = Some(PendingMessage {
                acp_thread_id: acp_thread_id.to_string(),
                message_id: entry_idx.to_string(),
                role: role.to_string(),
                content,
                request_id: request_id.to_string(),
                entry_type: entry_type.to_string(),
                tool_name: tool_name.to_string(),
                tool_status: tool_status.to_string(),
            });
            sent = true;
        } else {
            if !state.flush_scheduled {
                state.flush_scheduled = true;
                spawn_flush_timer = true;
            }
            state.pending_content = Some(PendingMessage {
                acp_thread_id: acp_thread_id.to_string(),
                message_id: entry_idx.to_string(),
                role: role.to_string(),
                content,
                request_id: request_id.to_string(),
                entry_type: entry_type.to_string(),
                tool_name: tool_name.to_string(),
                tool_status: tool_status.to_string(),
            });
            sent = false;
        }
    } // locks released

    // Send stale pending messages from other entries first
    for pending in stale_pending {
        let _ = crate::send_websocket_event(SyncEvent::MessageAdded {
            acp_thread_id: pending.acp_thread_id,
            message_id: pending.message_id,
            role: pending.role,
            content: pending.content,
            request_id: pending.request_id,
            entry_type: pending.entry_type,
            tool_name: pending.tool_name,
            tool_status: pending.tool_status,
            timestamp: chrono::Utc::now().timestamp(),
        });
    }

    // Then send the current entry if not throttled
    if let Some(msg) = current_to_send {
        let _ = crate::send_websocket_event(SyncEvent::MessageAdded {
            acp_thread_id: msg.acp_thread_id,
            message_id: msg.message_id,
            role: msg.role,
            content: msg.content,
            request_id: msg.request_id,
            entry_type: msg.entry_type,
            tool_name: msg.tool_name,
            tool_status: msg.tool_status,
            timestamp: chrono::Utc::now().timestamp(),
        });
    }

    if spawn_flush_timer {
        smol::spawn(trailing_flush_timer(key)).detach();
    }

    sent
}

async fn trailing_flush_timer(key: String) {
    smol::Timer::after(STREAMING_THROTTLE_INTERVAL).await;

    let pending: Option<PendingMessage> = {
        let throttle_map = STREAMING_THROTTLE.lock();
        let Some(map) = throttle_map.as_ref() else { return };
        let mut map = map.write();
        let Some(state) = map.get_mut(&key) else { return };
        state.flush_scheduled = false;
        let msg = state.pending_content.take();
        if msg.is_some() {
            state.last_sent = Instant::now();
        }
        msg
    };

    if let Some(msg) = pending {
        let _ = crate::send_websocket_event(SyncEvent::MessageAdded {
            acp_thread_id: msg.acp_thread_id,
            message_id: msg.message_id,
            role: msg.role,
            content: msg.content,
            request_id: msg.request_id,
            entry_type: msg.entry_type,
            tool_name: msg.tool_name,
            tool_status: msg.tool_status,
            timestamp: chrono::Utc::now().timestamp(),
        });
    }
}

/// Flush all pending throttled messages for a given thread and clean up throttle state.
/// Called before message_completed to ensure the final content is sent.
pub fn flush_streaming_throttle(acp_thread_id: &str) {
    init_streaming_throttle();

    // Collect pending messages under lock, then send after releasing
    let pending_messages: Vec<PendingMessage>;
    {
        let throttle_map = STREAMING_THROTTLE.lock();
        let Some(map) = throttle_map.as_ref() else { return };
        let mut map = map.write();

        let prefix = format!("{}:", acp_thread_id);
        let keys_to_remove: Vec<String> = map.keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();

        pending_messages = keys_to_remove.iter()
            .filter_map(|key| map.remove(key))
            .filter_map(|state| state.pending_content)
            .collect();
    }

    // Send all pending messages outside the lock
    for pending in pending_messages {
        let _ = crate::send_websocket_event(SyncEvent::MessageAdded {
            acp_thread_id: pending.acp_thread_id,
            message_id: pending.message_id,
            role: pending.role,
            content: pending.content,
            request_id: pending.request_id,
            entry_type: pending.entry_type,
            tool_name: pending.tool_name,
            tool_status: pending.tool_status,
            timestamp: chrono::Utc::now().timestamp(),
        });
    }
}

/// Set the current request_id for a thread (used when sending new message to thread)
pub fn set_thread_request_id(acp_thread_id: String, request_id: String) {
    init_thread_registry();
    let map = THREAD_REQUEST_MAP.lock();
    if let Some(m) = map.as_ref() {
        m.write().insert(acp_thread_id.clone(), request_id.clone());
    }
    // Also update reverse map for cancel_current_turn lookups
    let reverse_map = REQUEST_TO_THREAD_MAP.lock();
    if let Some(m) = reverse_map.as_ref() {
        m.write().insert(request_id, acp_thread_id);
    }
}

/// Get the current request_id for a thread
pub fn get_thread_request_id(acp_thread_id: &str) -> Option<String> {
    let map = THREAD_REQUEST_MAP.lock();
    map.as_ref()?.read().get(acp_thread_id).cloned()
}

/// Look up the thread_id for a given request_id (reverse map)
pub fn get_thread_id_for_request(request_id: &str) -> Option<String> {
    let map = REQUEST_TO_THREAD_MAP.lock();
    map.as_ref()?.read().get(request_id).cloned()
}

/// Store the agent's session ID for a thread, so we can pass it to load_session later.
/// The agent (e.g. Claude Code) assigns its own session ID which differs from the Zed thread UUID.
pub fn set_agent_session_id(acp_thread_id: &str, agent_session_id: String) {
    let map = THREAD_AGENT_SESSION_MAP.lock();
    if let Some(m) = map.as_ref() {
        m.write().insert(acp_thread_id.to_string(), agent_session_id);
    }
}

/// Get the agent's session ID for a thread (for passing to load_session).
pub fn get_agent_session_id(acp_thread_id: &str) -> Option<String> {
    let map = THREAD_AGENT_SESSION_MAP.lock();
    map.as_ref().and_then(|m| m.read().get(acp_thread_id).cloned())
}

/// Record which agent backs a thread (e.g. "claude", "qwen", "zed-agent").
/// Called at create/load time so the wrapper-side wedge recovery path can
/// route back to the correct agent even when Helix's follow-up
/// `SendChatMessage` strips `agent_name` from the chat_message command.
pub fn set_thread_agent_name(acp_thread_id: &str, agent_name: String) {
    init_thread_registry();
    let map = THREAD_AGENT_NAME_MAP.lock();
    if let Some(m) = map.as_ref() {
        m.write().insert(acp_thread_id.to_string(), agent_name);
    }
}

/// Look up the recorded agent_name for a thread. Returns None if no
/// create/load event recorded it (e.g. legacy threads from before this
/// map was introduced).
pub fn get_thread_agent_name(acp_thread_id: &str) -> Option<String> {
    let map = THREAD_AGENT_NAME_MAP.lock();
    map.as_ref().and_then(|m| m.read().get(acp_thread_id).cloned())
}

/// Register an active thread (stores strong reference to keep thread alive)
pub fn register_thread(acp_thread_id: String, thread: Entity<AcpThread>) {
    init_thread_registry();
    let registry = THREAD_REGISTRY.lock();
    if let Some(reg) = registry.as_ref() {
        let mut map = reg.write();
        if let Some(existing) = map.get(&acp_thread_id) {
            if existing.entity_id() != thread.entity_id() {
                log::warn!(
                    "⚠️ [THREAD_SERVICE] register_thread: overwriting thread '{}' with different entity (old={:?}, new={:?})",
                    acp_thread_id,
                    existing.entity_id(),
                    thread.entity_id(),
                );
                eprintln!(
                    "⚠️ [THREAD_SERVICE] register_thread: overwriting thread '{}' with different entity (old={:?}, new={:?})",
                    acp_thread_id,
                    existing.entity_id(),
                    thread.entity_id(),
                );
            }
        }
        map.insert(acp_thread_id.clone(), thread.clone());
    }

    // Also keep a permanent strong reference so the entity survives UI transitions
    let keep_alive = THREAD_KEEP_ALIVE.lock();
    if let Some(ka) = keep_alive.as_ref() {
        ka.write().insert(acp_thread_id, thread);
    }
}

/// Remove a thread from the registry (e.g., when a headless view is dropped).
pub fn unregister_thread(acp_thread_id: &str) {
    let registry = THREAD_REGISTRY.lock();
    if let Some(reg) = registry.as_ref() {
        if reg.write().remove(acp_thread_id).is_some() {
            eprintln!("🗑️ [THREAD_SERVICE] unregister_thread: removed '{}'", acp_thread_id);
            log::info!("🗑️ [THREAD_SERVICE] unregister_thread: removed '{}'", acp_thread_id);
        }
    }

    // Clear persistent subscription so that if this thread is reloaded later
    // (e.g., follow-up to a non-visible thread), ensure_thread_subscription
    // will create a fresh subscription on the new Entity<AcpThread>.
    // Without this, the old subscription (attached to the dropped entity) is
    // gone but the flag remains, so no new subscription is created and events
    // like Stopped/message_completed are silently lost.
    let subs = PERSISTENT_SUBSCRIPTIONS.lock();
    if let Some(s) = subs.as_ref() {
        if s.write().remove(acp_thread_id) {
            eprintln!("🗑️ [THREAD_SERVICE] unregister_thread: cleared persistent subscription for '{}'", acp_thread_id);
        }
    }
}

/// Like `unregister_thread` but only removes the registry entry if it currently
/// holds the entity the caller expected. Use this from `cx.on_release` of a
/// `ConversationView`/`ThreadView` so an old view being dropped doesn't clobber
/// a fresh registration that another caller (e.g. `load_session_async` after a
/// Zed restart) just placed under the same `acp_thread_id`.
///
/// Without this guard, the sequence is: panel restoration's old view is
/// dropped, its `on_release` calls `unregister_thread(session_id)`, which
/// removes the live entity that `register_thread` placed moments earlier when
/// Helix sent `open_thread` on reconnect. Subsequent ACP `SessionUpdate`
/// dispatches reach the live entity but `THREAD_REGISTRY` reports "not found",
/// the persistent-subscription flag is wrongly cleared, and any future
/// `ensure_thread_subscription` call creates a duplicate subscription —
/// duplicating `MessageAdded`/`MessageCompleted` sends on subsequent turns.
pub fn unregister_thread_if_matches(acp_thread_id: &str, expected_entity_id: gpui::EntityId) {
    let mut removed_from_registry = false;
    {
        let registry = THREAD_REGISTRY.lock();
        if let Some(reg) = registry.as_ref() {
            let mut map = reg.write();
            if let Some(existing) = map.get(acp_thread_id) {
                if existing.entity_id() == expected_entity_id {
                    map.remove(acp_thread_id);
                    removed_from_registry = true;
                    eprintln!(
                        "🗑️ [THREAD_SERVICE] unregister_thread_if_matches: removed '{}' (entity={:?})",
                        acp_thread_id, expected_entity_id
                    );
                    log::info!(
                        "🗑️ [THREAD_SERVICE] unregister_thread_if_matches: removed '{}' (entity={:?})",
                        acp_thread_id, expected_entity_id
                    );
                } else {
                    eprintln!(
                        "🛡️ [THREAD_SERVICE] unregister_thread_if_matches: skipping '{}' — registry holds different entity (existing={:?}, dropping={:?})",
                        acp_thread_id, existing.entity_id(), expected_entity_id
                    );
                    log::info!(
                        "🛡️ [THREAD_SERVICE] unregister_thread_if_matches: skipping '{}' — registry holds different entity (existing={:?}, dropping={:?})",
                        acp_thread_id, existing.entity_id(), expected_entity_id
                    );
                }
            }
        }
    }

    // Only clear the persistent-subscription flag when we actually removed the
    // registry entry. If we left a different entity in place, its subscription
    // is still live and the flag must stay set; otherwise a later
    // `ensure_thread_subscription` call would create a duplicate subscription
    // on the same entity and double-send sync events to Helix.
    if removed_from_registry {
        let subs = PERSISTENT_SUBSCRIPTIONS.lock();
        if let Some(s) = subs.as_ref() {
            if s.write().remove(acp_thread_id) {
                eprintln!(
                    "🗑️ [THREAD_SERVICE] unregister_thread_if_matches: cleared persistent subscription for '{}'",
                    acp_thread_id
                );
            }
        }
    }
}

/// Get an active thread as weak reference
pub fn get_thread(acp_thread_id: &str) -> Option<WeakEntity<AcpThread>> {
    // Check the active registry first
    let registry = THREAD_REGISTRY.lock();
    if let Some(entity) = registry.as_ref().and_then(|r| r.read().get(acp_thread_id).cloned()) {
        return Some(entity.downgrade());
    }
    drop(registry);

    // Fall back to the keep-alive map (threads survive UI transitions here)
    let keep_alive = THREAD_KEEP_ALIVE.lock();
    keep_alive.as_ref().and_then(|ka| ka.read().get(acp_thread_id).map(|e| e.downgrade()))
}

/// Ensure a thread has an event subscription for syncing to Helix.
///
/// This is the SINGLE source of truth for thread event subscriptions. All code paths
/// that create or load threads must call this to set up the subscription. It is
/// idempotent — if a persistent subscription already exists, it does nothing.
///
/// Handles three events:
/// - `NewEntry`: new user/assistant message → send `message_added`
/// - `EntryUpdated`: streaming tokens / tool call updates → throttled `message_added`
/// - `Stopped`: turn completed → flush throttle + send `message_completed`
pub fn ensure_thread_subscription(
    thread_entity: &Entity<AcpThread>,
    thread_id: &str,
    cx: &mut App,
) {
    if has_persistent_subscription(thread_id) {
        eprintln!("🔔 [THREAD_SERVICE] Thread {} already has persistent subscription, skipping", thread_id);
        return;
    }

    let entity_id = thread_entity.entity_id();
    eprintln!(
        "🔔 [THREAD_SERVICE] Creating NEW subscription for thread {} on entity {:?}",
        thread_id, entity_id
    );
    log::info!(
        "🔔 [THREAD_SERVICE] Creating NEW subscription for thread {} on entity {:?}",
        thread_id, entity_id
    );

    let thread_id_for_sub = thread_id.to_string();
    mark_persistent_subscription(thread_id.to_string());

    // Track the request_id that was active when the current turn started.
    // Used by the Stopped handler to flush with the correct request_id,
    // even if a follow-up/interrupt message has already updated the global
    // THREAD_REQUEST_MAP to the next turn's request_id.
    let turn_request_id: std::cell::RefCell<String> = std::cell::RefCell::new(
        crate::get_thread_request_id(thread_id).unwrap_or_default()
    );
    // The previous turn's request_id, kept so that background events
    // (e.g. async tool completions) for entries from a completed turn
    // can still be tagged with the correct request_id.
    let prev_turn_request_id: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
    // Tracks the last request_id for which message_completed was already sent.
    // When Stopped fires for a turn that produced no assistant entries (e.g. the
    // interrupt turn is immediately cancelled by Claude Code before generating
    // any tokens), turn_request_id is never updated by NewEntry and still holds
    // the previous turn's id. Detecting this via last_completed_request_id lets
    // us fall back to the global THREAD_REQUEST_MAP which already points to the
    // new turn's request_id.
    let last_completed_request_id: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());

    let sub_entity_id = entity_id;
    cx.subscribe(thread_entity, move |thread_entity, event, cx| {
        let current_entity_id = thread_entity.entity_id();
        eprintln!(
            "🔔 [THREAD_SERVICE] Subscription FIRED for thread {} on entity {:?} (subscribed to {:?}), event: {:?}",
            thread_id_for_sub, current_entity_id, sub_entity_id, std::mem::discriminant(event)
        );
        match event {
            AcpThreadEvent::NewEntry => {
                let thread = thread_entity.read(cx);
                let latest_idx = thread.entries().len().saturating_sub(1);
                if is_external_originated_entry(&thread_id_for_sub, latest_idx) {
                    return;
                }
                if let Some(entry) = thread.entries().get(latest_idx) {
                    let (role, content, entry_type) = match entry {
                        acp_thread::AgentThreadEntry::UserMessage(msg) => {
                            ("user", msg.content.to_markdown(cx).to_string(), "text")
                        }
                        acp_thread::AgentThreadEntry::AssistantMessage(msg) => {
                            ("assistant", msg.content_only(cx), "text")
                        }
                        acp_thread::AgentThreadEntry::ToolCall(tool_call) => {
                            ("assistant", tool_call.to_markdown(cx), "tool_call")
                        }
                        _ => return,
                    };
                    let (tool_name, tool_status) = match entry {
                        acp_thread::AgentThreadEntry::ToolCall(tool_call) => {
                            (tool_call.label.read(cx).source().to_string(), tool_call.status.to_string())
                        }
                        _ => (String::new(), String::new()),
                    };
                    // A new UserMessage entry is the unambiguous start of a
                    // new turn — force-rotate turn_request_id from the global
                    // map regardless of whether the previous turn completed
                    // cleanly. The chat_message handler updates
                    // THREAD_REQUEST_MAP when the user's prompt arrives, so by
                    // the time NewEntry fires for the UserMessage the global
                    // map already holds the new turn's id.
                    //
                    // Without this, an interrupted/cancelled prior turn leaves
                    // last_completed_request_id stale, the assistant-only
                    // rotation below never fires, and every assistant entry
                    // (including the re-emit loop further down) carries the
                    // PRIOR turn's request_id back to Helix. Helix then routes
                    // those events to the wrong interaction — fixing this
                    // resolves both the "messages streaming in Zed don't show
                    // up in Helix" and the "prefix mangled with another
                    // message" symptoms.
                    if role == "user" {
                        // First user message in a draft thread is the cue
                        // to flush the deferred UserCreatedThread emit.
                        // See `defer_user_created_thread` for the reason
                        // we don't emit at thread-creation time.
                        try_flush_pending_user_created_thread(&thread_id_for_sub);

                        let current = turn_request_id.borrow().clone();
                        *prev_turn_request_id.borrow_mut() = current;
                        let global_rid = crate::get_thread_request_id(&thread_id_for_sub)
                            .unwrap_or_default();
                        *turn_request_id.borrow_mut() = global_rid;
                        // Reset completion tracking — the assistant-only
                        // rotation expects current==completed to detect "I'm
                        // done with this turn, move on", which is true at the
                        // start of a fresh turn.
                        *last_completed_request_id.borrow_mut() = String::new();
                    }
                    // Update turn_request_id from the global map only at turn
                    // boundaries — i.e. when the current turn_request_id has
                    // already been used for a message_completed (matches
                    // last_completed_request_id). This prevents a follow-up
                    // message that overwrites the global map from poisoning
                    // the current turn's request_id via a late NewEntry.
                    if role == "assistant" {
                        let current = turn_request_id.borrow().clone();
                        let completed = last_completed_request_id.borrow().clone();
                        if current.is_empty() || current == completed {
                            // Rotate: current → prev before overwriting.
                            *prev_turn_request_id.borrow_mut() = current;
                            let global_rid = crate::get_thread_request_id(&thread_id_for_sub)
                                .unwrap_or_default();
                            *turn_request_id.borrow_mut() = global_rid;
                        }
                    }
                    // Use the turn-scoped request_id for all events, not the
                    // global THREAD_REQUEST_MAP which can be overwritten by a
                    // follow-up/interrupt message between turns.
                    let rid = turn_request_id.borrow().clone();
                    // Re-send preceding entries FROM THE CURRENT TURN with their
                    // current content. flush_streaming_text() was called right before
                    // push_entry(), so all Markdown entities have their complete text.
                    // Only send entries after the last UserMessage to avoid leaking
                    // old turn entries into the current interaction on the Go side.
                    let entries = thread.entries();
                    let turn_start = entries.iter().enumerate().rev()
                        .find_map(|(i, e)| matches!(e, acp_thread::AgentThreadEntry::UserMessage(_)).then_some(i + 1))
                        .unwrap_or(0);
                    for prev_idx in turn_start..latest_idx {
                        if let Some(prev_entry) = thread.entries().get(prev_idx) {
                            let (prev_role, prev_content, prev_entry_type) = match prev_entry {
                                acp_thread::AgentThreadEntry::UserMessage(msg) => {
                                    ("user", msg.content.to_markdown(cx).to_string(), "text")
                                }
                                acp_thread::AgentThreadEntry::AssistantMessage(msg) => {
                                    ("assistant", msg.content_only(cx), "text")
                                }
                                acp_thread::AgentThreadEntry::ToolCall(tool_call) => {
                                    ("assistant", tool_call.to_markdown(cx), "tool_call")
                                }
                                _ => continue,
                            };
                            let (prev_tool_name, prev_tool_status) = match prev_entry {
                                acp_thread::AgentThreadEntry::ToolCall(tool_call) => {
                                    (tool_call.label.read(cx).source().to_string(), tool_call.status.to_string())
                                }
                                _ => (String::new(), String::new()),
                            };
                            let _ = crate::send_websocket_event(SyncEvent::MessageAdded {
                                acp_thread_id: thread_id_for_sub.clone(),
                                message_id: prev_idx.to_string(),
                                role: prev_role.to_string(),
                                content: prev_content,
                                request_id: rid.clone(),
                                entry_type: prev_entry_type.to_string(),
                                tool_name: prev_tool_name,
                                tool_status: prev_tool_status,
                                timestamp: chrono::Utc::now().timestamp(),
                            });
                        }
                    }

                    let _ = crate::send_websocket_event(SyncEvent::MessageAdded {
                        acp_thread_id: thread_id_for_sub.clone(),
                        message_id: latest_idx.to_string(),
                        role: role.to_string(),
                        content,
                        request_id: rid,
                        entry_type: entry_type.to_string(),
                        tool_name,
                        tool_status,
                        timestamp: chrono::Utc::now().timestamp(),
                    });
                }
            }
            AcpThreadEvent::EntryUpdated(entry_idx) => {
                let thread = thread_entity.read(cx);
                if let Some(entry) = thread.entries().get(*entry_idx) {
                    let (content, entry_type, tool_name, tool_status) = match entry {
                        acp_thread::AgentThreadEntry::AssistantMessage(msg) => {
                            (msg.content_only(cx), "text", String::new(), String::new())
                        }
                        acp_thread::AgentThreadEntry::ToolCall(tool_call) => {
                            let name = tool_call.label.read(cx).source().to_string();
                            let status = tool_call.status.to_string();
                            (tool_call.to_markdown(cx), "tool_call", name, status)
                        }
                        _ => return,
                    };
                    // Pick the right request_id based on whether this entry
                    // belongs to the current turn or a previous one.  Claude
                    // Code can deliver background events (tool completions,
                    // text flushes) asynchronously via session_notification
                    // after a turn has ended.  These must be tagged with the
                    // PREVIOUS turn's request_id so the Helix side routes
                    // them to the correct interaction.
                    let entries = thread.entries();
                    let turn_start = entries.iter().enumerate().rev()
                        .find_map(|(i, e)| matches!(e, acp_thread::AgentThreadEntry::UserMessage(_)).then_some(i + 1))
                        .unwrap_or(0);
                    let rid = if *entry_idx < turn_start {
                        let prev = prev_turn_request_id.borrow().clone();
                        if !prev.is_empty() {
                            prev
                        } else {
                            turn_request_id.borrow().clone()
                        }
                    } else {
                        turn_request_id.borrow().clone()
                    };
                    throttled_send_message_added(
                        &thread_id_for_sub,
                        *entry_idx,
                        "assistant",
                        content,
                        &rid,
                        entry_type,
                        &tool_name,
                        &tool_status,
                    );
                }
            }
            AcpThreadEvent::Stopped(_) => {
                flush_streaming_throttle(&thread_id_for_sub);

                // AcpThread calls flush_streaming_text before emitting Stopped, so all
                // Markdown entities now have their complete buffered text. Send corrected
                // content for ALL entries — EntryUpdated events during streaming carried
                // incomplete content (text was still in the streaming buffer at that point),
                // so intermediate text entries were truncated. Re-sending with the now-complete
                // content is safe: the Go accumulator uses overwrite semantics for known message_ids.
                let thread = thread_entity.read(cx);
                let entries = thread.entries();
                // Use the request_id captured when this turn started, NOT the current
                // global request_id. If a follow-up/interrupt message arrived before
                // Stopped fires, the global ID already points to the next turn.
                let rid = turn_request_id.borrow().clone();

                // Find the start of the current turn: the entry AFTER the last UserMessage.
                // Only flush entries from the current turn — sending old entries would cause
                // them to leak into the current interaction's response_entries on the Go side.
                let turn_start = entries.iter().enumerate().rev()
                    .find_map(|(i, e)| matches!(e, acp_thread::AgentThreadEntry::UserMessage(_)).then_some(i + 1))
                    .unwrap_or(0);

                for (idx, entry) in entries.iter().enumerate().skip(turn_start) {
                    match entry {
                        acp_thread::AgentThreadEntry::AssistantMessage(msg) => {
                            let content = msg.content_only(cx);
                            if !content.is_empty() {
                                crate::send_websocket_event(SyncEvent::MessageAdded {
                                    acp_thread_id: thread_id_for_sub.clone(),
                                    message_id: idx.to_string(),
                                    role: "assistant".to_string(),
                                    content,
                                    request_id: rid.clone(),
                                    entry_type: "text".to_string(),
                                    tool_name: String::new(),
                                    tool_status: String::new(),
                                    timestamp: chrono::Utc::now().timestamp(),
                                }).log_err();
                            }
                        }
                        acp_thread::AgentThreadEntry::ToolCall(tool_call) => {
                            let content = tool_call.to_markdown(cx);
                            if !content.is_empty() {
                                let name = tool_call.label.read(cx).source().to_string();
                                let status = tool_call.status.to_string();
                                crate::send_websocket_event(SyncEvent::MessageAdded {
                                    acp_thread_id: thread_id_for_sub.clone(),
                                    message_id: idx.to_string(),
                                    role: "assistant".to_string(),
                                    content,
                                    request_id: rid.clone(),
                                    entry_type: "tool_call".to_string(),
                                    tool_name: name,
                                    tool_status: status,
                                    timestamp: chrono::Utc::now().timestamp(),
                                }).log_err();
                            }
                        }
                        _ => {}
                    }
                }

                // Use the turn's captured request_id for message_completed.
                // FALLBACK: if turn_request_id still holds the previous turn's id
                // (because no assistant NewEntry fired to update it — happens when
                // the interrupt turn is immediately cancelled with no output), use
                // the current global THREAD_REQUEST_MAP which points to this turn.
                let captured_rid = turn_request_id.borrow().clone();
                let last_completed = last_completed_request_id.borrow().clone();
                let completed_rid = if !captured_rid.is_empty() && captured_rid == last_completed {
                    // turn_request_id is stale — the previous turn already used it.
                    // Use the global map which has the current turn's request_id.
                    let fallback = crate::get_thread_request_id(&thread_id_for_sub)
                        .unwrap_or_else(|| captured_rid.clone());
                    eprintln!(
                        "📤 [THREAD_SERVICE] Stopped: turn_request_id={} already used, falling back to global={}",
                        captured_rid, fallback
                    );
                    fallback
                } else {
                    captured_rid
                };
                *last_completed_request_id.borrow_mut() = completed_rid.clone();
                eprintln!(
                    "📤 [THREAD_SERVICE] Stopped event: sending message_completed for thread {} (request_id={})",
                    thread_id_for_sub, completed_rid
                );
                let _ = crate::send_websocket_event(SyncEvent::MessageCompleted {
                    acp_thread_id: thread_id_for_sub.clone(),
                    message_id: "0".to_string(),
                    request_id: completed_rid,
                });
            }
            _ => {}
        }
    }).detach();
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
    acp_history_store: Entity<ThreadStore>,
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

    // Spawn dedicated cancel task — runs independently of the callback_rx loop so it
    // can cancel a running turn even while callback_rx.recv().await is blocked.
    let (cancel_tx, mut cancel_rx) = mpsc::unbounded_channel::<String>();
    crate::init_cancel_thread_callback(cancel_tx);
    cx.spawn(async move |cx| {
        eprintln!("⚡ [CANCEL_TASK] Cancel task started, waiting for cancel requests...");
        log::info!("⚡ [CANCEL_TASK] Cancel task started, waiting for cancel requests...");
        while let Some(acp_thread_id) = cancel_rx.recv().await {
            eprintln!("⚡ [CANCEL_TASK] Received cancel request for thread: {}", acp_thread_id);
            log::info!("⚡ [CANCEL_TASK] Received cancel request for thread: {}", acp_thread_id);
            if let Some(thread) = crate::get_thread(&acp_thread_id) {
                let result = cx.update(|cx| {
                    thread.update(cx, |t, cx| { t.cancel(cx) })
                });
                match result {
                    Ok(_) => {
                        eprintln!("✅ [CANCEL_TASK] Cancelled running turn on thread: {}", acp_thread_id);
                        log::info!("✅ [CANCEL_TASK] Cancelled running turn on thread: {}", acp_thread_id);
                    }
                    Err(e) => {
                        eprintln!("⚠️ [CANCEL_TASK] Failed to cancel thread {}: {}", acp_thread_id, e);
                        log::warn!("⚠️ [CANCEL_TASK] Failed to cancel thread {}: {}", acp_thread_id, e);
                    }
                }
            } else {
                eprintln!("⚠️ [CANCEL_TASK] Thread {} not found in registry, skipping cancel", acp_thread_id);
                log::warn!("⚠️ [CANCEL_TASK] Thread {} not found in registry, skipping cancel", acp_thread_id);
            }
        }
    }).detach();

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

                    // Notify AgentPanel to display this thread (it may not be currently visible)
                    // This ensures the UI switches to the correct thread before the message is sent
                    if let Some(thread_entity) = thread.upgrade() {
                        if let Err(e) = crate::notify_thread_display(crate::ThreadDisplayNotification {
                            thread_entity: thread_entity.clone(),
                            helix_session_id: existing_thread_id.clone(),
                            agent_name: request.agent_name.clone(),
                        }) {
                            eprintln!("⚠️ [THREAD_SERVICE] Failed to notify thread display for follow-up: {}", e);
                        }
                    }

                    let initial_message = request.message.clone();
                    let initial_err = handle_follow_up_message(
                        thread,
                        existing_thread_id.clone(),
                        request.request_id.clone(),
                        request.message,
                        request.simulate_input,
                        cx.clone()
                    ).await;

                    if let Err(e) = initial_err {
                        let err_msg = e.to_string();
                        if is_claude_acp_drain_race(&err_msg) {
                            // The claude-agent-acp SDK Query is wedged — the
                            // in-place retry inside handle_follow_up_message
                            // has already exhausted; only a wrapper-side
                            // teardown will unstick it. Force-close the
                            // session, reload the thread from disk transcript,
                            // and retry the send against the fresh entity.
                            eprintln!(
                                "🧹 [THREAD_SERVICE] claude-acp wedge detected on {} — attempting force-reset + reload (recovery)",
                                existing_thread_id
                            );
                            log::warn!(
                                "🧹 [THREAD_SERVICE] claude-acp wedge detected on {} — attempting force-reset + reload (recovery)",
                                existing_thread_id
                            );

                            let reset_res = force_close_agent_session(
                                project_for_create.clone(),
                                acp_history_store_for_create.clone(),
                                fs_for_create.clone(),
                                existing_thread_id.clone(),
                                request.agent_name.clone(),
                                cx.clone(),
                            ).await;

                            match reset_res {
                                Ok(()) => {
                                    // Prefer the recorded agent_name for the
                                    // reload too — same reason as force_close
                                    // (Helix's SendChatMessage drops the
                                    // request's agent_name on follow-ups).
                                    let reload_agent_name = get_thread_agent_name(existing_thread_id)
                                        .or_else(|| request.agent_name.clone());
                                    let load_res = load_thread_from_agent(
                                        project_for_create.clone(),
                                        acp_history_store_for_create.clone(),
                                        fs_for_create.clone(),
                                        existing_thread_id.clone(),
                                        reload_agent_name,
                                        cx.clone(),
                                    ).await;
                                    match load_res {
                                        Ok(fresh_thread) => {
                                            eprintln!(
                                                "✅ [THREAD_SERVICE] Reloaded thread {} after force-reset; retrying send",
                                                existing_thread_id
                                            );
                                            if let Err(e2) = handle_follow_up_message(
                                                fresh_thread,
                                                existing_thread_id.clone(),
                                                request.request_id.clone(),
                                                initial_message,
                                                request.simulate_input,
                                                cx.clone(),
                                            ).await {
                                                eprintln!(
                                                    "❌ [THREAD_SERVICE] Retry after force-reset still failed: {}",
                                                    e2
                                                );
                                                log::error!(
                                                    "❌ [THREAD_SERVICE] Retry after force-reset still failed: {}",
                                                    e2
                                                );
                                                let error_event = SyncEvent::ThreadLoadError {
                                                    acp_thread_id: existing_thread_id.clone(),
                                                    request_id: request.request_id.clone(),
                                                    error: format!(
                                                        "Failed to send follow-up (after force-reset + reload): {}",
                                                        e2
                                                    ),
                                                };
                                                if let Err(send_err) = crate::send_websocket_event(error_event) {
                                                    eprintln!("❌ [THREAD_SERVICE] Failed to send error event: {}", send_err);
                                                }
                                            }
                                            continue;
                                        }
                                        Err(load_err) => {
                                            // Force-close succeeded (wrapper teardown done) but
                                            // we couldn't rehydrate the transcript. The load error
                                            // is the actionable signal — surface it to Helix so
                                            // operators see the actual failure cause, not the
                                            // already-recovered drain-race error.
                                            eprintln!(
                                                "❌ [THREAD_SERVICE] Force-reset succeeded but reload failed: {} (orig drain-race: {})",
                                                load_err, e
                                            );
                                            log::error!(
                                                "❌ [THREAD_SERVICE] Force-reset succeeded but reload failed: {} (orig drain-race: {})",
                                                load_err, e
                                            );
                                            let error_event = SyncEvent::ThreadLoadError {
                                                acp_thread_id: existing_thread_id.clone(),
                                                request_id: request.request_id.clone(),
                                                error: format!(
                                                    "Failed to reload thread after wrapper force-reset: {} (recovery was triggered by: {})",
                                                    load_err, e
                                                ),
                                            };
                                            if let Err(send_err) = crate::send_websocket_event(error_event) {
                                                eprintln!("❌ [THREAD_SERVICE] Failed to send error event: {}", send_err);
                                            }
                                            continue;
                                        }
                                    }
                                }
                                Err(close_err) => {
                                    // Force-close itself failed — wrapper is unreachable or
                                    // doesn't support force_close. Surface both errors so
                                    // operators can see the recovery attempt failed and why.
                                    eprintln!(
                                        "❌ [THREAD_SERVICE] Force-close failed: {} (orig drain-race: {})",
                                        close_err, e
                                    );
                                    log::error!(
                                        "❌ [THREAD_SERVICE] Force-close failed: {} (orig drain-race: {})",
                                        close_err, e
                                    );
                                    let error_event = SyncEvent::ThreadLoadError {
                                        acp_thread_id: existing_thread_id.clone(),
                                        request_id: request.request_id.clone(),
                                        error: format!(
                                            "Force-close failed during drain-race recovery: {} (recovery was triggered by: {})",
                                            close_err, e
                                        ),
                                    };
                                    if let Err(send_err) = crate::send_websocket_event(error_event) {
                                        eprintln!("❌ [THREAD_SERVICE] Failed to send error event: {}", send_err);
                                    }
                                    continue;
                                }
                            }
                        }

                        eprintln!("❌ [THREAD_SERVICE] Failed to send follow-up message: {}", e);
                        log::error!("❌ [THREAD_SERVICE] Failed to send follow-up message: {}", e);

                        // Non-drain-race follow-up failure (or recovery was attempted but
                        // already sent its own error event above and continued past this).
                        let error_event = SyncEvent::ThreadLoadError {
                            acp_thread_id: existing_thread_id.clone(),
                            request_id: request.request_id.clone(),
                            error: format!("Failed to send follow-up: {}", e),
                        };
                        if let Err(send_err) = crate::send_websocket_event(error_event) {
                            eprintln!("❌ [THREAD_SERVICE] Failed to send error event: {}", send_err);
                        }
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
                                request.simulate_input,
                                cx.clone()
                            ).await {
                                eprintln!("❌ [THREAD_SERVICE] Failed to send message to loaded thread: {}", e);
                                log::error!("❌ [THREAD_SERVICE] Failed to send message to loaded thread: {}", e);
                            }
                            continue;
                        }
                        Err(e) => {
                            // Thread can't be reloaded. This should be rare now that
                            // THREAD_KEEP_ALIVE keeps all thread entities alive.
                            // Report the error back to Helix rather than silently
                            // creating a new thread (which would lose conversation context).
                            eprintln!(
                                "❌ [THREAD_SERVICE] Failed to load thread {} from agent: {} - sending error to Helix",
                                existing_thread_id, e
                            );
                            log::error!(
                                "❌ [THREAD_SERVICE] Failed to load thread {} from agent: {}",
                                existing_thread_id, e
                            );

                            let error_event = SyncEvent::ThreadLoadError {
                                acp_thread_id: existing_thread_id.clone(),
                                request_id: request.request_id.clone(),
                                error: format!("Failed to load thread: {}", e),
                            };
                            if let Err(send_err) = crate::send_websocket_event(error_event) {
                                eprintln!("❌ [THREAD_SERVICE] Failed to send error event: {}", send_err);
                            }

                            continue;
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

    // Create callback channel for cancellation requests
    let (cancel_tx, mut cancel_rx) = mpsc::unbounded_channel::<crate::CancellationRequest>();
    crate::init_cancellation_callback(cancel_tx);

    // Spawn handler task to process cancellation requests
    cx.spawn(async move |cx| {
        while let Some(request) = cancel_rx.recv().await {
            log::info!("[THREAD_SERVICE] Received cancellation request: request_id={}", request.request_id);

            // Look up which thread this request_id belongs to
            let thread_id = get_thread_id_for_request(&request.request_id);
            let thread = thread_id.as_deref().and_then(get_thread);

            match thread {
                Some(weak_thread) => {
                    // Probe whether there is a turn actually in flight BEFORE issuing
                    // the cancel. This decides whether we report "cancelled" or "noop"
                    // back to Helix, and lets us emit turn_cancelled BEFORE message_completed.
                    let was_running = cx.update(|cx| {
                        if let Some(thread_entity) = weak_thread.upgrade() {
                            thread_entity.update(cx, |thread, _cx| {
                                matches!(thread.status(), acp_thread::ThreadStatus::Generating)
                            })
                        } else {
                            false
                        }
                    });

                    if was_running {
                        // Send turn_cancelled FIRST so Helix marks the interaction as
                        // Interrupted before message_completed (triggered by the
                        // synchronously-emitted Stopped(Cancelled)) arrives and would
                        // otherwise race it into the Completed terminal state.
                        // GPUI flushes queued events at the end of the entity update
                        // closure below, so the subscription that fans Stopped out as
                        // message_completed would otherwise win the race against the
                        // turn_cancelled send that follows.
                        if let Err(e) = crate::send_websocket_event(SyncEvent::TurnCancelled {
                            request_id: request.request_id.clone(),
                            status: "cancelled".to_string(),
                        }) {
                            log::error!("[THREAD_SERVICE] Failed to send turn_cancelled event: {}", e);
                        }

                        let task = cx.update(|cx| {
                            if let Some(thread_entity) = weak_thread.upgrade() {
                                thread_entity.update(cx, |thread, cx| {
                                    thread.cancel(cx)
                                })
                            } else {
                                gpui::Task::ready(())
                            }
                        });
                        task.await;
                        log::info!("[THREAD_SERVICE] Cancelled turn for request_id={}", request.request_id);
                    } else {
                        log::info!("[THREAD_SERVICE] Thread exists but no turn running for request_id={}, sending noop", request.request_id);
                        if let Err(e) = crate::send_websocket_event(SyncEvent::TurnCancelled {
                            request_id: request.request_id,
                            status: "noop".to_string(),
                        }) {
                            log::error!("[THREAD_SERVICE] Failed to send turn_cancelled noop event: {}", e);
                        }
                    }
                }
                None => {
                    log::info!("[THREAD_SERVICE] No active thread for request_id={}, sending noop", request.request_id);
                    if let Err(e) = crate::send_websocket_event(SyncEvent::TurnCancelled {
                        request_id: request.request_id,
                        status: "noop".to_string(),
                    }) {
                        log::error!("[THREAD_SERVICE] Failed to send turn_cancelled noop event: {}", e);
                    }
                }
            }
        }
        anyhow::Ok(())
    })
    .detach();

    log::info!("✅ [THREAD_SERVICE] WebSocket thread handler initialized");
}

/// Create a new ACP thread and send the initial message (synchronous version)
fn create_new_thread_sync(
    project: Entity<Project>,
    acp_history_store: Entity<ThreadStore>,
    fs: Arc<dyn Fs>,
    request: ThreadCreationRequest,
    cx: &mut App,
) -> Result<()> {
    log::info!("[THREAD_SERVICE] Creating ACP thread with agent: {:?}", request.agent_name);

    let agent = match request.agent_name.as_deref() {
        Some("zed-agent") | None => ExternalAgent::NativeAgent,
        Some(name) => {
            // Map Helix agent names to Zed registry agent IDs.
            // Helix sends "claude" but the Zed registry uses "claude-acp".
            let zed_name = match name {
                "claude" => agent_servers::CLAUDE_AGENT_ID,
                other => other,
            };
            ExternalAgent::Custom {
                name: gpui::SharedString::from(zed_name.to_string()),
                command: project::agent_server_store::AgentServerCommand {
                    path: std::path::PathBuf::new(),
                    args: vec![],
                    env: None,
                },
            }
        }
    };

    // Spawn async task to complete the connection and create the thread
    let request_clone = request.clone();
    let project_clone = project.clone();
    cx.spawn(async move |cx| {
        eprintln!("🚀 [THREAD_SERVICE] Spawn task started for request: {}", request_clone.request_id);

        // Retry connecting up to 10 times with 1s delay when the agent is not yet registered.
        // This handles the race where the AgentRegistryStore's async network fetch (for registry
        // agents like claude-acp) hasn't completed by the time the first message arrives.
        let max_retries = 10u32;
        let connection: std::rc::Rc<dyn acp_thread::AgentConnection> = {
            let mut attempt = 0u32;
            loop {
                let shared = cx.update(|cx| {
                    let server = agent.server(fs.clone(), acp_history_store.clone());
                    let agent_id = server.agent_id();
                    let agent_server_store = project.read(cx).agent_server_store().clone();
                    let delegate = agent_servers::AgentServerDelegate::new(
                        agent_server_store,
                        None,
                        None,
                    );
                    eprintln!("🔌 [THREAD_SERVICE] Requesting cached connection (attempt {}/{})...", attempt + 1, max_retries + 1);
                    agent_servers::AgentConnectionCache::request_connection(
                        cx,
                        project.clone(),
                        agent_id,
                        server,
                        delegate,
                    )
                });

                eprintln!("⏳ [THREAD_SERVICE] Awaiting connection task...");
                match shared.await {
                    Ok(conn) => {
                        eprintln!("✅ [THREAD_SERVICE] Connected to agent successfully");
                        break conn;
                    }
                    Err(e) if attempt < max_retries && e.to_string().contains("not registered") => {
                        eprintln!("⚠️ [THREAD_SERVICE] Agent not registered yet (attempt {}/{}), retrying in 1s: {:?}", attempt + 1, max_retries, e);
                        // Evict the cached failure so the next iteration triggers a fresh connect.
                        cx.update(|cx| {
                            let server = agent.server(fs.clone(), acp_history_store.clone());
                            agent_servers::AgentConnectionCache::evict(cx, project.entity_id(), server.agent_id());
                        });
                        attempt += 1;
                        cx.background_executor().timer(Duration::from_secs(1)).await;
                    }
                    Err(e) => {
                        eprintln!("❌ [THREAD_SERVICE] Failed to connect to agent: {:?}", e);
                        return Err(anyhow::anyhow!("agent connect failed: {:?}", e));
                    }
                }
            }
        };

        // Authenticate if required
        let auth_methods = connection.auth_methods();
        if let Some(first_method) = auth_methods.first() {
            let connection_for_auth = connection.clone();
            let auth_task = cx.update(|cx| {
                connection_for_auth.authenticate(first_method.id().clone(), cx)
            });
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
                })
            });
        let connection_for_tools = connection.clone();
        let connection_for_model = connection.clone();
        let work_dirs = PathList::new(&[cwd.clone()]);
        eprintln!("🧵 [THREAD_SERVICE] Calling new_session() with cwd={:?}", cwd);
        let thread_entity: Entity<AcpThread> = match cx.update(|cx| {
            connection.new_session(project_clone.clone(), work_dirs, cx)
        }).await {
            Ok(entity) => {
                eprintln!("✅ [THREAD_SERVICE] new_session() succeeded");
                entity
            }
            Err(e) => {
                eprintln!("❌ [THREAD_SERVICE] new_session() failed: {}", e);
                return Err(e);
            }
        };

        // Wait for the NativeAgent's model list to be populated.
        // NativeAgent::new() spawns authenticate_all_language_model_providers()
        // which runs asynchronously. When it completes, ProviderStateChanged
        // fires, NativeAgent refreshes its model list. We wait for that refresh.
        let session_id = cx.update(|cx| thread_entity.read(cx).session_id().clone());
        {
            let mut model_watch = cx.update(|cx| {
                connection_for_model.model_selector(&session_id)
                    .and_then(|selector| selector.watch(cx))
            });
            if let Some(ref mut watch_rx) = model_watch {
                let wait_for_models = async {
                    let _ = watch_rx.changed().await;
                };
                let timeout = async {
                    smol::Timer::after(std::time::Duration::from_secs(15)).await;
                    log::warn!("[THREAD_SERVICE] Timed out waiting for models (15s), proceeding");
                };
                futures::future::select(Box::pin(wait_for_models), Box::pin(timeout)).await;
            }
        }

        // The settings system may set the default model to zed.dev (from default.json),
        // which can't be resolved without a Zed account. The NativeAgent's auto-model
        // logic uses registry.default_model() which may return None in this case.
        // If the thread still has no model after the watch fired, explicitly select
        // the first available model from the authenticated providers.
        if let Some(selector) = cx.update(|_cx| connection_for_model.model_selector(&session_id)) {
            let has_model = cx.update(|cx| selector.selected_model(cx)).await;
            let needs_model = match has_model {
                Ok(_) => false,
                Err(_) => true,
            };
            if needs_model {
                let model_list_result = cx.update(|cx| selector.list_models(cx)).await;
                if let Ok(model_list) = model_list_result {
                    let first_model_id = match &model_list {
                        acp_thread::AgentModelList::Flat(models) => models.first().map(|m| m.id.clone()),
                        acp_thread::AgentModelList::Grouped(groups) => {
                            groups.values().flat_map(|v| v.iter()).next().map(|m| m.id.clone())
                        }
                    };
                    if let Some(model_id) = first_model_id {
                        if let Err(e) = cx.update(|cx| selector.select_model(model_id.clone(), cx)).await {
                            log::warn!("[THREAD_SERVICE] Failed to select model: {}", e);
                        }
                    }
                }
            }
        }

        // Wait for MCP context server tools to finish loading before sending
        // the first message, so the LLM request includes all available tools.
        let tools_ready_task = cx.update(|cx| connection_for_tools.wait_for_tools_ready(cx));
        tools_ready_task.await;

        let acp_thread_id = cx.update(|cx| {
            let thread_id = thread_entity.read(cx).session_id().to_string();
            log::info!("[THREAD_SERVICE] Created ACP thread: {}", thread_id);
            thread_id
        });

        // Keep thread entity alive for the duration of this task
        let _thread_keep_alive = thread_entity.clone();

        // Store the current request_id for this thread (so message_completed uses correct ID)
        set_thread_request_id(acp_thread_id.clone(), request_clone.request_id.clone());

        // NOTE: WebSocket event sending is now handled centrally in ThreadView.handle_thread_event
        // This avoids duplicate events when thread is both created here and displayed in UI via from_existing_thread

        // Register thread for follow-up messages (strong reference keeps it alive)
        register_thread(acp_thread_id.clone(), thread_entity.clone());

        // Store the agent's session ID so load_session uses the right ID later.
        // ACP agents like Claude Code assign their own session IDs that differ
        // from the Zed thread UUID.
        cx.update(|cx| {
            let agent_sid = thread_entity.read(cx).session_id().to_string();
            eprintln!("📋 [THREAD_SERVICE] Registered thread: {} → agent session: {}", acp_thread_id, agent_sid);
            log::info!("📋 [THREAD_SERVICE] Registered thread: {} → agent session: {}", acp_thread_id, agent_sid);
            set_agent_session_id(&acp_thread_id, agent_sid);
        });

        // Send agent_ready event to Helix (signals that agent is ready to receive prompts)
        // This prevents race conditions where Helix sends continue prompts before agent is initialized
        let agent_name_for_ready = request_clone.agent_name.clone().unwrap_or_else(|| "zed-agent".to_string());
        // Persist the agent backing this thread so wrapper-side wedge
        // recovery can route to the correct AgentConnection later, even
        // when Helix's follow-up SendChatMessage strips agent_name.
        set_thread_agent_name(&acp_thread_id, agent_name_for_ready.clone());
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
        // Unless simulate_input=true, in which case we want the sync to fire
        if !request_clone.simulate_input {
            let entry_idx_to_mark = cx.update(|cx| {
                thread_entity.read(cx).entries().len()
            });
            mark_external_originated_entry(acp_thread_id.clone(), entry_idx_to_mark);
            eprintln!("🏷️ [THREAD_SERVICE] Marked entry {} as external-originated (won't echo back)", entry_idx_to_mark);
        } else {
            eprintln!("🎭 [THREAD_SERVICE] simulate_input=true, NOT marking entry as external-originated (will sync back)");
        }

        // Subscribe to thread events PERSISTENTLY so that:
        // Subscribe to thread events so streaming responses sync to Helix
        // and future user-typed messages in Zed's UI also sync back.
        cx.update(|cx| {
            ensure_thread_subscription(&thread_entity, &acp_thread_id, cx);
        });

        // Send the initial message to the thread to trigger AI response
        eprintln!("🔧 [THREAD_SERVICE] About to send message to thread...");
        let send_task = cx.update(|cx| {
            thread_entity.update(cx, |thread: &mut AcpThread, cx| {
                let message = vec![ContentBlock::Text(
                    TextContent::new(request_clone.message.clone())
                )];
                eprintln!("🔧 [THREAD_SERVICE] Calling thread.send() with message: {}", request_clone.message);
                thread.send(message, cx)
            })
        });

        // Await the send task directly (don't spawn and detach)
        eprintln!("🔧 [THREAD_SERVICE] Awaiting send task...");
        match send_task.await {
            Ok(_) => {
                eprintln!("✅ [THREAD_SERVICE] Send task completed successfully - message sent to AI");
                log::info!("✅ [THREAD_SERVICE] Send task completed successfully");
            }
            Err(e) => {
                eprintln!("❌ [THREAD_SERVICE] Send task failed: {:#}", e);
                log::error!("❌ [THREAD_SERVICE] Send task failed: {:#}", e);
            }
        }

        eprintln!("✅ [THREAD_SERVICE] Message send awaited - AI response complete");
        log::info!("✅ [THREAD_SERVICE] Message send awaited - AI response complete");

        // NOTE: MessageCompleted is now sent by the persistent subscription's
        // Stopped event handler (above). This ensures ALL turn completions emit
        // MessageCompleted, whether initiated by Helix or by direct Zed UI input.
        // Previously, this code sent MessageCompleted here, but that missed turns
        // the user typed directly into Zed's agent panel.

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
    simulate_input: bool,
    cx: gpui::AsyncApp,
) -> Result<()> {
    log::info!("💬 [THREAD_SERVICE] Sending follow-up message: {} (simulate_input={})", message, simulate_input);

    // CRITICAL: Update the request_id for this thread so message_completed uses the correct ID!
    set_thread_request_id(thread_id.clone(), request_id.clone());
    eprintln!("🔄 [THREAD_SERVICE] Updated request_id for thread {} to {}", thread_id, request_id);
    log::info!("🔄 [THREAD_SERVICE] Updated request_id for thread {} to {}", thread_id, request_id);

    // Mark the entry that will be created as external-originated (unless simulating user input)
    // When simulate_input=true, we want the NewEntry subscription to fire so the user message
    // syncs back to Helix (testing the Zed → Helix direction)
    if !simulate_input {
        let entry_idx_to_mark = cx.update(|cx| {
            thread.update(cx, |thread, _| thread.entries().len())
        })?;
        mark_external_originated_entry(thread_id.clone(), entry_idx_to_mark);
        eprintln!("🏷️ [THREAD_SERVICE] Marked entry {} as external-originated (follow-up)", entry_idx_to_mark);
    } else {
        eprintln!("🎭 [THREAD_SERVICE] simulate_input=true, NOT marking entry as external-originated (will sync back)");
    }

    // Ensure subscription exists (idempotent — skips if already present)
    cx.update(|cx| {
        if let Some(thread_entity) = thread.upgrade() {
            ensure_thread_subscription(&thread_entity, &thread_id, cx);
        }
    });

    // The claude-agent-acp wrapper exposes a wedge after a cancelled turn:
    // a follow-up `prompt` arrives, the SDK Query is still finalizing the
    // prior assistant turn, and the wrapper returns
    //   `Internal error: [ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null`.
    // One quick in-place retry catches the genuine sub-second drain race.
    // When the SDK Query is *permanently* wedged (the common case observed
    // on Drone CI Phase 9), retrying the same handle never recovers - the
    // caller of handle_follow_up_message is responsible for force-resetting
    // the agent session and re-invoking us on a fresh thread entity.
    let max_attempts = 2;
    let retry_delay = Duration::from_millis(500);
    for attempt in 1..=max_attempts {
        let send_task = cx.update(|cx| {
            thread.update(cx, |thread: &mut AcpThread, cx| {
                let message = vec![ContentBlock::Text(
                    TextContent::new(message.clone())
                )];
                thread.send(message, cx)
            })
        })?;

        match send_task.await {
            Ok(_) => {
                eprintln!("✅ [THREAD_SERVICE] Follow-up send completed successfully");
                break;
            }
            Err(e) => {
                let msg = e.to_string();
                let is_acp_drain_race = is_claude_acp_drain_race(&msg);
                if is_acp_drain_race && attempt < max_attempts {
                    eprintln!(
                        "⚠️ [THREAD_SERVICE] Follow-up hit claude-agent-acp drain race (attempt {}/{}), retrying after {:?}: {}",
                        attempt, max_attempts, retry_delay, msg
                    );
                    log::warn!(
                        "⚠️ [THREAD_SERVICE] Follow-up hit claude-agent-acp drain race (attempt {}/{}), retrying after {:?}: {}",
                        attempt, max_attempts, retry_delay, msg
                    );
                    cx.background_executor().timer(retry_delay).await;
                    continue;
                }
                eprintln!("❌ [THREAD_SERVICE] Follow-up send failed: {}", e);
                return Err(e);
            }
        }
    }

    // NOTE: MessageCompleted is now sent by the persistent subscription's
    // Stopped event handler. See create_new_thread_sync for rationale.

    log::info!("✅ [THREAD_SERVICE] Follow-up message sent successfully");
    Ok(())
}

/// Returns true if the given error string matches the wedged-Query signature
/// from `@zed-industries/claude-agent-acp` (Anthropic Claude Code SDK
/// re-raised through the wrapper). When this fires, the wrapper's
/// session-side `Query` is permanently stuck until we drive it through
/// `teardownSession` via a raw `session/close`.
///
/// Strict triple-match: keep the recovery path narrow so unrelated agent
/// errors fall through to the normal error path unchanged.
pub(crate) fn is_claude_acp_drain_race(msg: &str) -> bool {
    msg.contains("ede_diagnostic")
        && msg.contains("result_type=user")
        && msg.contains("stop_reason=null")
}

/// Force-close an agent-side session at the wire level, bypassing Zed's
/// ref-counted `close_session` path (which is a no-op while an
/// `AcpThread` entity still holds a reference). After the wrapper
/// teardown lands, the registry entry for `acp_thread_id` is dropped so
/// a subsequent `load_thread_from_agent` recreates the session cleanly.
///
/// Used by the recovery path in `claude` follow-ups: when the SDK Query
/// wedges after a mid-stream cancel, force-closing is the only way to
/// unstick it; the next `loadSession` then hits the empty-map branch in
/// the wrapper's `getOrCreateSession` and spawns a fresh Query that
/// resumes from the on-disk transcript.
async fn force_close_agent_session(
    project: Entity<Project>,
    acp_history_store: Entity<ThreadStore>,
    fs: Arc<dyn Fs>,
    acp_thread_id: String,
    agent_name: Option<String>,
    cx: gpui::AsyncApp,
) -> Result<()> {
    // The chat_message that triggered recovery may have lost its
    // agent_name (Helix's follow-up SendChatMessage path strips it).
    // The thread itself, however, was created with a specific agent —
    // we recorded that at thread-create / thread-load time. Prefer the
    // recorded value over the request's, otherwise we'd route to
    // NativeAgent for a thread that's actually backed by claude (or
    // qwen, gemini, etc.), and force_close_session would dispatch to
    // the trait default and the recovery would fail.
    let recorded_agent = get_thread_agent_name(&acp_thread_id);
    let effective_agent_name = recorded_agent.clone().or(agent_name.clone());

    log::info!(
        "🧹 [THREAD_SERVICE] force_close_agent_session: thread={} (request agent={:?}, recorded={:?}, effective={:?})",
        acp_thread_id, agent_name, recorded_agent, effective_agent_name
    );

    let agent = match effective_agent_name.as_deref() {
        Some("zed-agent") | Some("") | None => ExternalAgent::NativeAgent,
        Some(name) => {
            let zed_name = match name {
                "claude" => agent_servers::CLAUDE_AGENT_ID,
                other => other,
            };
            ExternalAgent::Custom {
                name: gpui::SharedString::from(zed_name.to_string()),
                command: project::agent_server_store::AgentServerCommand {
                    path: std::path::PathBuf::new(),
                    args: vec![],
                    env: None,
                },
            }
        }
    };

    let server = agent.server(fs, acp_history_store.clone());

    let shared = cx.update(|cx| {
        let agent_id = server.agent_id();
        let agent_server_store = project.read(cx).agent_server_store().clone();
        let delegate = agent_servers::AgentServerDelegate::new(
            agent_server_store,
            None,
            None,
        );
        agent_servers::AgentConnectionCache::request_connection(
            cx,
            project.clone(),
            agent_id,
            server.clone(),
            delegate,
        )
    });

    let connection: std::rc::Rc<dyn acp_thread::AgentConnection> = shared
        .await
        .map_err(|e| anyhow::anyhow!("agent connect for force-close failed: {:?}", e))?;

    let agent_sid =
        get_agent_session_id(&acp_thread_id).unwrap_or_else(|| acp_thread_id.clone());
    let session_id = acp::SessionId::new(agent_sid.clone());

    // Drop ALL Helix-side references to the wedged entity FIRST, so any
    // in-flight handler (e.g. the cancel-task loop, a follow-up callback)
    // that calls `get_thread` during the recovery window cannot resurrect
    // the wedged entity from the keep-alive map and prompt-send into a
    // session whose wrapper-side state is being torn down underneath us.
    //
    // We also clear EXTERNAL_ORIGINATED_ENTRIES — its indices are keyed
    // on the OLD entity's entries vector, which the reloaded entity will
    // not share. Leaving stale indices behind would suppress sync for
    // matching-index entries on the fresh thread.
    //
    // We deliberately keep THREAD_AGENT_SESSION_MAP intact: the agent
    // assigns its own session id at thread creation, and the wrapper's
    // load_session-with-resume reuses that same id to rehydrate from
    // the on-disk transcript. Clearing it would break the reload.
    unregister_thread(&acp_thread_id);
    {
        let keep_alive = THREAD_KEEP_ALIVE.lock();
        if let Some(ka) = keep_alive.as_ref() {
            ka.write().remove(&acp_thread_id);
        }
    }
    {
        let map = EXTERNAL_ORIGINATED_ENTRIES.lock();
        if let Some(m) = map.as_ref() {
            m.write().remove(&acp_thread_id);
        }
    }

    let close_task = cx.update(|cx| connection.clone().force_close_session(&session_id, cx));
    close_task.await?;
    log::info!(
        "🧹 [THREAD_SERVICE] force_close_agent_session: wrapper teardown complete for {} (agent session {})",
        acp_thread_id, agent_sid
    );

    Ok(())
}

/// Load an existing thread from the agent (async version for use in message handler)
/// This connects to the agent, loads the session via ACP protocol, registers it, and returns a weak reference.
async fn load_thread_from_agent(
    project: Entity<Project>,
    acp_history_store: Entity<ThreadStore>,
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
        Some(name) => {
            let zed_name = match name {
                "claude" => agent_servers::CLAUDE_AGENT_ID,
                other => other,
            };
            ExternalAgent::Custom {
                name: gpui::SharedString::from(zed_name.to_string()),
                command: project::agent_server_store::AgentServerCommand {
                    path: std::path::PathBuf::new(),
                    args: vec![],
                    env: None,
                },
            }
        }
    };

    let server = agent.server(fs, acp_history_store.clone());

    // Get agent server store and create (or reuse cached) connection
    let (shared, cwd) = cx.update(|cx| {
        let agent_id = server.agent_id();
        let agent_server_store = project.read(cx).agent_server_store().clone();
        let delegate = agent_servers::AgentServerDelegate::new(
            agent_server_store,
            None,
            None,
        );
        let shared = agent_servers::AgentConnectionCache::request_connection(
            cx,
            project.clone(),
            agent_id,
            server.clone(),
            delegate,
        );
        // Use ZED_WORK_DIR for consistency with session storage
        let cwd = std::env::var("ZED_WORK_DIR")
            .ok()
            .map(|dir| std::path::PathBuf::from(dir))
            .unwrap_or_else(|| {
                project.read(cx).worktrees(cx).next()
                    .map(|wt| wt.read(cx).abs_path().to_path_buf())
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
            });
        (shared, cwd)
    });

    let connection: std::rc::Rc<dyn acp_thread::AgentConnection> = shared
        .await
        .map_err(|e| anyhow::anyhow!("agent connect failed: {:?}", e))?;

    eprintln!("✅ [THREAD_SERVICE] Connected to agent for loading thread");
    log::info!("✅ [THREAD_SERVICE] Connected to agent for loading thread");

    // Check if agent supports session loading
    {
        let connection = connection.clone();
        let supports_load = cx.update(|_cx| connection.supports_load_session());
        if !supports_load {
            let err = anyhow::anyhow!("Agent does not support session loading");
            eprintln!("⚠️ [THREAD_SERVICE] {}", err);
            log::warn!("⚠️ [THREAD_SERVICE] {}", err);
            return Err(err);
        }
    }

    // Load the thread from agent using the agent's own session ID (not the Zed thread UUID).
    // ACP agents like Claude Code assign their own session IDs during new_session.
    let agent_sid = get_agent_session_id(&acp_thread_id).unwrap_or_else(|| acp_thread_id.clone());
    eprintln!("📂 [THREAD_SERVICE] load_session: zed_thread={} agent_session={}", acp_thread_id, agent_sid);
    log::info!("📂 [THREAD_SERVICE] load_session: zed_thread={} agent_session={}", acp_thread_id, agent_sid);
    let session_id = acp::SessionId::new(agent_sid);
    let work_dirs = PathList::new(&[cwd.clone()]);
    let project_clone = project.clone();
    // Clone the connection before passing to load_session, which consumes its Rc<Self>.
    // We must keep a strong reference alive until load_task completes, because the
    // spawned tasks inside open_thread/load_thread only hold WeakEntity<NativeAgent>.
    // Without this, the NativeAgent entity is released and those tasks fail with
    // "entity released".
    let _connection_keepalive = connection.clone();
    let load_task = cx.update(|cx| {
        connection.load_session(session_id, project_clone, work_dirs, None, cx)
    });

    let thread_entity: Entity<AcpThread> = load_task.await?;

    let loaded_thread_id = cx.update(|cx| {
        thread_entity.read(cx).session_id().to_string()
    });

    eprintln!("✅ [THREAD_SERVICE] Loaded thread from agent: {}", loaded_thread_id);
    log::info!("✅ [THREAD_SERVICE] Loaded thread from agent: {}", loaded_thread_id);

    // Subscribe to thread events for streaming responses
    cx.update(|cx| {
        ensure_thread_subscription(&thread_entity, &loaded_thread_id, cx);
    });

    // Register thread for future access
    register_thread(loaded_thread_id.clone(), thread_entity.clone());
    set_agent_session_id(&acp_thread_id, loaded_thread_id.clone());
    eprintln!("📋 [THREAD_SERVICE] Registered loaded thread: {} → agent session: {}", acp_thread_id, loaded_thread_id);
    log::info!("📋 [THREAD_SERVICE] Registered loaded thread: {} → agent session: {}", acp_thread_id, loaded_thread_id);

    // Send agent_ready event to Helix (signals that agent is ready to receive prompts)
    let agent_name_for_ready = agent_name.clone().unwrap_or_else(|| "zed-agent".to_string());
    // Persist the agent backing this thread (see set_thread_agent_name doc).
    set_thread_agent_name(&loaded_thread_id, agent_name_for_ready.clone());
    if loaded_thread_id != acp_thread_id {
        // Helix-side IDs (request key) and agent-side session IDs (load key)
        // can differ; record under both so lookups by either find it.
        set_thread_agent_name(&acp_thread_id, agent_name_for_ready.clone());
    }
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
    acp_history_store: Entity<ThreadStore>,
    fs: Arc<dyn Fs>,
    request: ThreadOpenRequest,
    cx: &mut App,
) -> Result<()> {
    eprintln!("📖 [THREAD_SERVICE] Opening existing ACP thread: {}, agent_name: {:?}",
              request.acp_thread_id, request.agent_name);
    log::info!("📖 [THREAD_SERVICE] Opening existing ACP thread: {}, agent_name: {:?}",
               request.acp_thread_id, request.agent_name);

    // Check if thread is already in registry — ensure it has a subscription
    // (subscription tracking resets on process restart even if thread entity survived)
    if let Some(thread_weak) = get_thread(&request.acp_thread_id) {
        eprintln!("✅ [THREAD_SERVICE] Thread already loaded in registry: {}", request.acp_thread_id);
        log::info!("✅ [THREAD_SERVICE] Thread already loaded in registry: {}", request.acp_thread_id);
        if let Some(thread_entity) = thread_weak.upgrade() {
            ensure_thread_subscription(&thread_entity, &request.acp_thread_id, cx);
        }
        // TODO: Still need to notify AgentPanel to display it
        return Ok(());
    }

    // Check if any thread is already being loaded (async load in progress from
    // workspace restore or a concurrent open_thread). Without this guard, two
    // async loads race: both pass the registry check above, both spawn load tasks,
    // and the second overwrites the first's entity in the registry — orphaning
    // the first entity's event subscriptions.
    //
    // If panel restoration already holds the lock, WAIT for it to finish rather
    // than skipping — panel restoration registers the entity, and once it releases
    // the lock we can find the entity via the fast path and just subscribe +
    // send agent_ready without doing a duplicate load.
    {
        let mut loading = THREAD_LOAD_IN_PROGRESS.lock();
        if let Some(in_progress) = loading.as_ref() {
            let in_progress = in_progress.clone();
            eprintln!("⏳ [THREAD_SERVICE] Load lock held by '{}', waiting for release before handling '{}'",
                      in_progress, request.acp_thread_id);
            log::info!("⏳ [THREAD_SERVICE] Load lock held by '{}', waiting for release before handling '{}'",
                       in_progress, request.acp_thread_id);
            drop(loading); // release parking_lot lock before spawning

            let acp_thread_id = request.acp_thread_id.clone();
            let agent_name = request.agent_name.clone();
            cx.spawn(async move |cx| {
                // Poll until the load lock is released (max 30 s).
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    smol::Timer::after(Duration::from_millis(50)).await;
                    if THREAD_LOAD_IN_PROGRESS.lock().is_none() {
                        eprintln!("🔓 [THREAD_SERVICE] Load lock released, rechecking fast path for '{}'", acp_thread_id);
                        break;
                    }
                    if Instant::now() > deadline {
                        eprintln!("⚠️ [THREAD_SERVICE] Timed out waiting for load lock for '{}', proceeding anyway", acp_thread_id);
                        break;
                    }
                }

                // Recheck registry — whoever held the lock should have registered the entity.
                if let Some(thread_weak) = get_thread(&acp_thread_id) {
                    if let Some(thread_entity) = thread_weak.upgrade() {
                        let flag_was_set = {
                            let subs = PERSISTENT_SUBSCRIPTIONS.lock();
                            subs.as_ref().map(|s| s.read().contains(&acp_thread_id)).unwrap_or(false)
                        };
                        let entity_id = thread_entity.entity_id();
                        eprintln!(
                            "🔍 [THREAD_SERVICE] After lock wait: fast-path entity={:?}, subscription flag was_set={} for '{}'",
                            entity_id, flag_was_set, acp_thread_id
                        );
                        cx.update(|cx| {
                            ensure_thread_subscription(&thread_entity, &acp_thread_id, cx);
                        });
                    }
                    let agent_name_for_ready = agent_name.unwrap_or_else(|| "zed-agent".to_string());
                    eprintln!("🚀 [THREAD_SERVICE] Sending agent_ready after lock-wait for '{}'", acp_thread_id);
                    // Persist the agent backing this thread (see set_thread_agent_name doc).
                    set_thread_agent_name(&acp_thread_id, agent_name_for_ready.clone());
                    crate::send_agent_ready(agent_name_for_ready, Some(acp_thread_id));
                } else {
                    eprintln!("⚠️ [THREAD_SERVICE] Lock released but entity not in registry for '{}' — no action taken", acp_thread_id);
                    log::warn!("⚠️ [THREAD_SERVICE] Lock released but entity not in registry for '{}'", acp_thread_id);
                }
            }).detach();
            return Ok(());
        }
        eprintln!("🔒 [THREAD_SERVICE] Acquired thread load lock for {}", request.acp_thread_id);
        log::info!("🔒 [THREAD_SERVICE] Acquired thread load lock for {}", request.acp_thread_id);
        *loading = Some(request.acp_thread_id.clone());
    }

    // Thread not in registry - need to load from agent
    // Select agent based on agent_name (same logic as create_new_thread_sync)
    let agent = match request.agent_name.as_deref() {
        Some("zed-agent") | Some("") | None => ExternalAgent::NativeAgent,
        Some(name) => {
            let zed_name = match name {
                "claude" => agent_servers::CLAUDE_AGENT_ID,
                other => other,
            };
            ExternalAgent::Custom {
                name: gpui::SharedString::from(zed_name.to_string()),
                command: project::agent_server_store::AgentServerCommand {
                    path: std::path::PathBuf::new(),
                    args: vec![],
                    env: None,
                },
            }
        }
    };
    eprintln!("🔧 [THREAD_SERVICE] Selected agent: {:?}", agent);
    log::info!("🔧 [THREAD_SERVICE] Selected agent: {:?}", agent);

    let server = agent.server(fs, acp_history_store.clone());
    let agent_id = server.agent_id();

    // Get agent server store from project
    let agent_server_store = project.read(cx).agent_server_store().clone();

    // Create delegate for connection
    let delegate = agent_servers::AgentServerDelegate::new(
        agent_server_store,
        None,
        None,
    );

    // Get cached connection or create a new one (deduped against concurrent
    // callers in workspace restore / load_thread_from_agent / agent_connection_store).
    let shared = agent_servers::AgentConnectionCache::request_connection(
        cx,
        project.clone(),
        agent_id,
        server.clone(),
        delegate,
    );

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
        // Drop guard: clear THREAD_LOAD_IN_PROGRESS on exit (success or error)
        struct ClearLoadingGuard;
        impl Drop for ClearLoadingGuard {
            fn drop(&mut self) {
                let mut loading = THREAD_LOAD_IN_PROGRESS.lock();
                eprintln!("🔓 [THREAD_SERVICE] Released thread load lock (was {:?})", loading);
                log::info!("🔓 [THREAD_SERVICE] Released thread load lock (was {:?})", loading);
                *loading = None;
            }
        }
        let _loading_guard = ClearLoadingGuard;

        let connection = match shared.await {
            Ok(result) => result,
            Err(e) => {
                eprintln!("❌ [THREAD_SERVICE] Failed to connect to agent: {:?}", e);
                log::error!("❌ [THREAD_SERVICE] Failed to connect to agent: {:?}", e);
                return Err(anyhow::anyhow!("agent connect failed: {:?}", e));
            }
        };

        eprintln!("✅ [THREAD_SERVICE] Connected to agent server for thread loading");
        log::info!("✅ [THREAD_SERVICE] Connected to agent server for thread loading");

        // Check if agent supports session loading
        {
            let connection = connection.clone();
            if !cx.update(|_cx| connection.supports_load_session()) {
                eprintln!("⚠️ [THREAD_SERVICE] Agent does not support session loading");
                log::warn!("⚠️ [THREAD_SERVICE] Agent does not support session loading");
                return Err(anyhow::anyhow!("Agent does not support session loading"));
            }
        }

        eprintln!("🔨 [THREAD_SERVICE] Calling connection.load_session() to load from agent...");
        log::info!("🔨 [THREAD_SERVICE] Calling connection.load_session() to load from agent...");

        // Use the generic AgentConnection::load_session() method
        // This works for both NativeAgent (from local DB) and ACP agents (via session/load protocol)
        let session_id = acp::SessionId::new(request_clone.acp_thread_id.clone());
        let work_dirs = PathList::new(&[cwd.clone()]);
        // Clone the connection before passing to load_session, which consumes its Rc<Self>.
        // We must keep a strong reference alive until load_task completes, because the
        // spawned tasks inside open_thread/load_thread only hold WeakEntity<NativeAgent>.
        // Without this, the NativeAgent entity is released and those tasks fail with
        // "entity released".
        let _connection_keepalive = connection.clone();
        let load_task = cx.update(|cx| {
            connection.load_session(session_id, project_clone, work_dirs, None, cx)
        });

        let thread_entity: Entity<AcpThread> = match load_task.await {
            Ok(entity) => entity,
            Err(e) => {
                eprintln!("❌ [THREAD_SERVICE] connection.load_session() failed: {}", e);
                log::error!("❌ [THREAD_SERVICE] connection.load_session() failed: {}", e);
                return Err(e);
            }
        };

        let acp_thread_id = cx.update(|cx| {
            let thread_id = thread_entity.read(cx).session_id().to_string();
            eprintln!("✅ [THREAD_SERVICE] Loaded ACP thread from agent: {} (session_id)", thread_id);
            log::info!("✅ [THREAD_SERVICE] Loaded ACP thread from agent: {} (session_id)", thread_id);
            thread_id
        });

        // Register thread for future access (strong reference keeps it alive)
        register_thread(acp_thread_id.clone(), thread_entity.clone());
        if !request_clone.acp_thread_id.is_empty() {
            set_agent_session_id(&request_clone.acp_thread_id, acp_thread_id.clone());
            eprintln!("📋 [THREAD_SERVICE] Registered thread: {} → agent session: {}", request_clone.acp_thread_id, acp_thread_id);
            log::info!("📋 [THREAD_SERVICE] Registered thread: {} → agent session: {}", request_clone.acp_thread_id, acp_thread_id);
        } else {
            eprintln!("📋 [THREAD_SERVICE] Registered thread: {} (strong reference)", acp_thread_id);
            log::info!("📋 [THREAD_SERVICE] Registered thread: {} (strong reference)", acp_thread_id);
        }

        // CRITICAL: Subscribe to thread events so streaming responses sync to Helix.
        // This mirrors create_new_thread_sync (line ~1218). Without this call,
        // after a Zed restart the thread loads successfully but its NewEntry /
        // EntryUpdated / Stopped events have no listener, so message_added is
        // never emitted and the interaction stays stuck in "waiting" forever.
        //
        // We MUST clear the persistent-subscription flag first.  Panel restoration
        // may have already called ensure_thread_subscription on a *different* entity
        // (the one it created before load_session ran here), setting the flag.
        // If we skip the call because the flag is set, the subscription stays on
        // the panel-restoration entity while we registered a brand-new entity from
        // load_session — events on the new entity are never forwarded to Helix.
        {
            let flag_was_set = {
                let subs = PERSISTENT_SUBSCRIPTIONS.lock();
                subs.as_ref().map(|s| s.read().contains(&acp_thread_id)).unwrap_or(false)
            };
            let load_session_entity_id = thread_entity.entity_id();
            eprintln!(
                "🔍 [THREAD_SERVICE] open_existing_thread_sync slow path: subscription flag was_set={}, load_session entity={:?} for '{}'",
                flag_was_set, load_session_entity_id, acp_thread_id
            );
            log::info!(
                "🔍 [THREAD_SERVICE] open_existing_thread_sync slow path: subscription flag was_set={}, load_session entity={:?} for '{}'",
                flag_was_set, load_session_entity_id, acp_thread_id
            );
            // Clear any stale flag from a racing panel restoration that used a
            // different entity before load_session completed here.
            let subs = PERSISTENT_SUBSCRIPTIONS.lock();
            if let Some(s) = subs.as_ref() {
                if s.write().remove(&acp_thread_id) {
                    eprintln!("🔄 [THREAD_SERVICE] Cleared stale subscription flag for '{}' (was set on a different entity)", acp_thread_id);
                    log::info!("🔄 [THREAD_SERVICE] Cleared stale subscription flag for '{}' before re-subscribing on load_session entity", acp_thread_id);
                }
            }
        }
        cx.update(|cx| {
            ensure_thread_subscription(&thread_entity, &acp_thread_id, cx);
        });

        // Send agent_ready event to Helix (signals that agent is ready to receive prompts)
        // (THREAD_LOAD_IN_PROGRESS is cleared by the ClearLoadingGuard drop guard)
        let agent_name_for_ready = request_clone.agent_name.clone().unwrap_or_else(|| "zed-agent".to_string());
        // Persist the agent backing this thread (see set_thread_agent_name doc).
        set_thread_agent_name(&acp_thread_id, agent_name_for_ready.clone());
        if !request_clone.acp_thread_id.is_empty() && request_clone.acp_thread_id != acp_thread_id {
            // Helix-side ID may differ from agent-side session ID; record under both.
            set_thread_agent_name(&request_clone.acp_thread_id, agent_name_for_ready.clone());
        }
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

#[cfg(test)]
mod pending_user_created_emit_tests {
    //! Regression tests for the deferred-emission machinery that backs Fix 1a
    //! in `helix/design/2026-05-13-mcp-cache-contention-and-duplicate-claude-spawn.md`.
    //!
    //! These tests fail if `defer_user_created_thread` is replaced with an
    //! immediate `send_websocket_event(SyncEvent::UserCreatedThread { … })` call
    //! at the conversation_view.rs / acp/thread_view.rs sites — because then
    //! the pending map would never be populated and the assertions below
    //! would fail.
    //!
    //! The user-visible bug pattern these guard against:
    //! - Zed's agent panel `activate_draft` always creates a non-resume
    //!   ConversationView for the empty input editor.
    //! - Pre-fix that fired `UserCreatedThread` to Helix immediately, even
    //!   though the user had not typed anything.
    //! - Helix dutifully recorded a phantom `helix_session` +
    //!   `spec_task_zed_threads` row per container restart, plus a duplicate
    //!   Claude ACP spawn whose npm exec children raced with the existing
    //!   ones for the `_npx/<hash>` cache → 180s `chrome-devtools/github
    //!   context server failed to start: Context server request timeout`.

    use super::{
        defer_user_created_thread,
        drop_pending_user_created_thread,
        try_flush_pending_user_created_thread,
        PENDING_USER_CREATED_EMITS,
    };

    /// Each test gets a unique acp_thread_id so they don't trip over one
    /// another (the pending-emits map is a process-global static).
    fn fresh_thread_id(label: &str) -> String {
        format!(
            "test-{}-{}",
            label,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn defer_registers_in_pending_map_without_sending() {
        let id = fresh_thread_id("defer-only");
        assert!(
            !PENDING_USER_CREATED_EMITS.lock().contains_key(&id),
            "pending map must not contain id before defer"
        );

        defer_user_created_thread(id.clone(), Some("draft".to_string()));

        let pending = PENDING_USER_CREATED_EMITS.lock();
        assert!(
            pending.contains_key(&id),
            "defer_user_created_thread MUST register the id in the pending map; \
             if this assertion fails it means the conversation_view.rs site has \
             reverted to immediate send_websocket_event(UserCreatedThread {{…}}) \
             — see helix/design/2026-05-13-mcp-cache-contention-and-duplicate-claude-spawn.md"
        );
        assert_eq!(
            pending.get(&id).cloned().flatten(),
            Some("draft".to_string()),
            "title should be preserved verbatim"
        );
    }

    #[test]
    fn flush_clears_the_pending_entry_and_returns_true() {
        let id = fresh_thread_id("flush");
        defer_user_created_thread(id.clone(), Some("draft".to_string()));
        assert!(PENDING_USER_CREATED_EMITS.lock().contains_key(&id));

        let flushed = try_flush_pending_user_created_thread(&id);
        assert!(flushed, "flush must return true when an entry was pending");
        assert!(
            !PENDING_USER_CREATED_EMITS.lock().contains_key(&id),
            "flush must remove the entry from the pending map"
        );
    }

    #[test]
    fn flush_is_a_noop_when_nothing_is_pending() {
        let id = fresh_thread_id("noop");
        let flushed = try_flush_pending_user_created_thread(&id);
        assert!(
            !flushed,
            "flush must return false when no pending entry exists for the id"
        );
    }

    #[test]
    fn drop_removes_pending_entry_without_flushing() {
        let id = fresh_thread_id("drop");
        defer_user_created_thread(id.clone(), Some("draft".to_string()));

        let dropped = drop_pending_user_created_thread(&id);
        assert!(dropped, "drop returns true when an entry existed");
        assert!(
            !PENDING_USER_CREATED_EMITS.lock().contains_key(&id),
            "drop must remove the entry"
        );

        // A subsequent flush should be a no-op since drop already cleared.
        let flushed = try_flush_pending_user_created_thread(&id);
        assert!(
            !flushed,
            "flush after drop must return false — the entry has been disposed"
        );
    }

    #[test]
    fn flush_only_fires_once_per_pending_entry() {
        // The NewEntry handler in ensure_thread_subscription calls
        // try_flush_pending_user_created_thread on EVERY user-role NewEntry
        // (not just the first), so the implementation MUST self-cleanup
        // after the first successful flush. Otherwise the second
        // user message in a draft would re-emit UserCreatedThread to Helix
        // and Helix would create a duplicate session.
        let id = fresh_thread_id("once");
        defer_user_created_thread(id.clone(), None);

        let first = try_flush_pending_user_created_thread(&id);
        assert!(first, "first flush must succeed");

        let second = try_flush_pending_user_created_thread(&id);
        assert!(
            !second,
            "second flush must NOT re-emit — the entry was consumed by the first flush"
        );
    }

    #[test]
    fn defer_overwrites_existing_pending_entry_with_new_title() {
        // If the same acp_thread_id is somehow registered twice (shouldn't
        // happen in practice, but defensive), the second defer wins —
        // ensures we don't leak stale title metadata.
        let id = fresh_thread_id("overwrite");
        defer_user_created_thread(id.clone(), Some("first".to_string()));
        defer_user_created_thread(id.clone(), Some("second".to_string()));

        let pending = PENDING_USER_CREATED_EMITS.lock();
        assert_eq!(
            pending.get(&id).cloned().flatten(),
            Some("second".to_string()),
            "later defer must overwrite the earlier entry"
        );
    }
}

#[cfg(test)]
mod agent_ready_on_reconnect_tests {
    //! Regression test for the agent-readiness handshake on reconnect.
    //!
    //! When the external sync client reconnects it sends `open_thread`, which
    //! suppresses the connection loop's 5s fallback `agent_ready`
    //! (see `websocket_sync.rs`) and delegates emitting the ready signal to the
    //! thread service. `open_existing_thread_sync` has four load paths: the fresh
    //! load, the slow-path sync load, and the lock-wait recheck all emit
    //! `agent_ready` after loading — but the "already loaded in registry"
    //! fast-path returns `Ok(())` WITHOUT emitting it. So when a thread is still in
    //! the in-process registry on reconnect, `agent_ready` is never sent and the
    //! client's readiness wait times out.
    //!
    //! This test pins the fast-path to the same contract as the other three paths.
    //! It is RED until the registry fast-path emits `agent_ready` before returning.

    use super::*;
    use crate::types::SyncEvent;
    use crate::websocket_sync::{WebSocketSync, WEBSOCKET_SERVICE};
    use acp_thread::{AgentConnection, StubAgentConnection};
    use fs::FakeFs;
    use gpui::{AppContext as _, TestAppContext};
    use project::Project;
    use settings::SettingsStore;
    use std::rc::Rc;
    use util::path_list::PathList;

    fn init_test(cx: &mut TestAppContext) {
        env_logger::try_init().ok();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[gpui::test]
    async fn open_thread_on_already_registered_thread_emits_agent_ready(cx: &mut TestAppContext) {
        init_test(cx);

        // Build a real AcpThread entity via the stub connection and register it —
        // simulating a thread entity that survived in the in-process registry
        // across a WebSocket reconnect (the precise fast-path trigger).
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let connection = Rc::new(StubAgentConnection::new());
        let thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project.clone(), PathList::default(), cx)
            })
            .await
            .expect("stub new_session should succeed");

        // The external thread id (the registry key) is distinct from the ACP
        // session id. Unique per run so the process-global registry / subscription
        // statics don't collide with other tests.
        let acp_thread_id = format!(
            "test-reconnect-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        register_thread(acp_thread_id.clone(), thread);
        assert!(
            get_thread(&acp_thread_id).is_some(),
            "precondition: thread must be in the registry so open_existing_thread_sync \
             takes the 'already loaded' fast-path"
        );

        // Install a recording WebSocket service so send_agent_ready is observable
        // (without it, send_websocket_event errors on an uninitialised service).
        let (service, mut events_rx) = WebSocketSync::new_test();
        *WEBSOCKET_SERVICE.lock() = Some(service);

        // Exercise the reconnect `open_thread` against the already-registered thread.
        let request = ThreadOpenRequest {
            acp_thread_id: acp_thread_id.clone(),
            agent_name: Some("zed-agent".to_string()),
        };
        let history_store = cx.update(|cx| cx.new(|cx| ThreadStore::new(cx)));
        let unused_fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        cx.update(|cx| {
            open_existing_thread_sync(project.clone(), history_store, unused_fs, request, cx)
                .expect("open_existing_thread_sync should return Ok on the fast-path");
        });
        cx.run_until_parked();

        // Drain emitted events and count AgentReady for this thread.
        let mut agent_ready_count = 0;
        while let Ok(event) = events_rx.try_recv() {
            if let SyncEvent::AgentReady { thread_id, .. } = event {
                if thread_id.as_deref() == Some(acp_thread_id.as_str()) {
                    agent_ready_count += 1;
                }
            }
        }

        // Tear down all process-global state this test touched, BEFORE the
        // assertion, so it runs on both the pass and fail paths:
        //  - the recording WebSocket service (else it leaks into other tests);
        //  - the registry + keep-alive strong refs to the AcpThread entity (else
        //    the gpui test harness panics with "leaked handle" at App teardown,
        //    since THREAD_KEEP_ALIVE intentionally holds a permanent strong ref).
        *WEBSOCKET_SERVICE.lock() = None;
        unregister_thread(&acp_thread_id);
        if let Some(keep_alive) = THREAD_KEEP_ALIVE.lock().as_ref() {
            keep_alive.write().remove(&acp_thread_id);
        }
        cx.run_until_parked();

        assert_eq!(
            agent_ready_count, 1,
            "open_thread on an already-registered thread MUST emit exactly one \
             agent_ready, like the other three load paths in \
             open_existing_thread_sync. The registry fast-path currently omits it; \
             because the 5s fallback agent_ready was suppressed when open_thread \
             arrived, the readiness wait then times out."
        );
    }
}
