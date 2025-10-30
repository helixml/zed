//! Sync module for real-time synchronization with external servers

use anyhow::{Context, Result};
use collections::HashMap;
use futures::{SinkExt, StreamExt};
use gpui::{App, Task};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::time::interval;
use uuid::Uuid;

use crate::{
    types::*,
    ExternalWebSocketSync,
};

/// Sync client for communicating with external servers
pub struct SyncClient {
    id: String,
    helix_url: String,
    auth_token: Option<String>,
    event_sender: tokio::sync::mpsc::UnboundedSender<SyncEvent>,
    _task: Task<()>,
}

impl SyncClient {
    /// Create a new sync client
    pub fn new(
        helix_url: String,
        auth_token: Option<String>,
        cx: &mut App,
    ) -> Result<Self> {
        let id = Uuid::new_v4().to_string();
        let (event_sender, mut event_receiver) = tokio::sync::mpsc::unbounded_channel();

        let client_id = id.clone();
        let client_url = helix_url.clone();
        let client_token = auth_token.clone();

        // Start background task for sending events to Helix
        let task = cx.spawn(async move |_cx| {
            let mut client = ExternalSyncClient::new(client_url, client_token);
            
            while let Some(event) = event_receiver.recv().await {
                if let Err(e) = client.send_event(event).await {
                    log::warn!("Failed to send sync event to Helix: {}", e);
                }
            }
        });

        Ok(Self {
            id,
            helix_url,
            auth_token,
            event_sender,
            _task: task,
        })
    }

    /// Get client ID
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Send an event to Helix
    pub fn send_event(&self, event: SyncEvent) -> Result<()> {
        self.event_sender
            .send(event)
            .map_err(|_| anyhow::anyhow!("Failed to queue sync event"))?;
        Ok(())
    }
}

/// Internal HTTP client for communicating with external servers
struct ExternalSyncClient {
    client: reqwest::Client,
    base_url: String,
    auth_token: Option<String>,
}

impl ExternalSyncClient {
    fn new(base_url: String, auth_token: Option<String>) -> Self {
        let client = reqwest::Client::new();
        Self {
            client,
            base_url,
            auth_token,
        }
    }

    async fn send_event(&mut self, event: SyncEvent) -> Result<()> {
        let url = format!("{}/api/v1/external-agents/sync/events", self.base_url);
        
        let timestamped_event = TimestampedSyncEvent {
            event,
            timestamp: chrono::Utc::now(),
            session_id: "zed-session".to_string(), // TODO: Get actual session ID
        };

        let mut request = self.client.post(&url)
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(&timestamped_event)?);

        if let Some(token) = &self.auth_token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        let response = request.send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Helix sync request failed: {} - {}",
                status,
                text
            ));
        }

        log::debug!("Successfully sent sync event to Helix");
        Ok(())
    }
}

impl ExternalWebSocketSync {
    /// Start the sync service
    pub async fn start_sync_service(
        &mut self,
        config: SyncConfig,
        cx: &mut App,
    ) -> Result<()> {
        log::info!("Starting Helix sync service");

        // Create sync client
        let sync_client = SyncClient::new(
            config.helix_api_url.clone(),
            None, // TODO: Add auth token support
            cx,
        )?;

        // TODO: Register the client when method is implemented
        // self.register_sync_client(sync_client);

        // TODO: Start periodic sync task when downgrade method is available
        // let integration_weak = self.downgrade();
        let sync_interval = Duration::from_secs(config.sync_interval_seconds);

        cx.spawn(async move |_cx| {
            let mut interval = interval(sync_interval);

            loop {
                interval.tick().await;

                // TODO: Fix integration upgrade when downgrade method is available
                // For now, just break since integration_weak is not available
                break;
            }
        }).detach();

        log::info!("Helix sync service started");
        Ok(())
    }

    /// Perform periodic sync with Helix
    async fn perform_periodic_sync(integration: &gpui::Entity<Self>) -> Result<()> {
        // TODO: Fix session_info call when API is available
        let session_info = "placeholder-session".to_string(); // integration.read(cx).get_session_info();
        log::debug!(
            "Periodic sync - Session: {}, Contexts: {}, Clients: {}",
            "placeholder", // session_info.session_id,
            0, // session_info.active_contexts,
            0  // session_info.sync_clients
        );

        // TODO: Implement actual periodic sync logic
        // - Check for new contexts in Zed
        // - Send updates to Helix
        // - Handle bidirectional sync

        Ok(())
    }
}

/// Bidirectional sync manager
pub struct SyncManager {
    integration: gpui::WeakEntity<ExternalWebSocketSync>,
    sync_state: Arc<RwLock<SyncState>>,
    _tasks: Vec<Task<()>>,
}

#[derive(Default)]
struct SyncState {
    last_sync_timestamp: Option<chrono::DateTime<chrono::Utc>>,
    pending_events: Vec<TimestampedSyncEvent>,
    sync_errors: Vec<SyncError>,
}

#[derive(Clone, Debug)]
struct SyncError {
    timestamp: chrono::DateTime<chrono::Utc>,
    error: String,
    retry_count: u32,
}

impl SyncManager {
    pub fn new(integration: gpui::WeakEntity<ExternalWebSocketSync>) -> Self {
        Self {
            integration,
            sync_state: Arc::new(RwLock::new(SyncState::default())),
            _tasks: Vec::new(),
        }
    }

    /// Start bidirectional sync
    pub fn start_sync(&mut self, config: SyncConfig, cx: &mut App) -> Result<()> {
        // Start Zed -> Helix sync
        let zed_to_helix_task = self.start_zed_to_helix_sync(config.clone(), cx)?;
        
        // Start Helix -> Zed sync
        let helix_to_zed_task = self.start_helix_to_zed_sync(config.clone(), cx)?;

        self._tasks = vec![zed_to_helix_task, helix_to_zed_task];

        log::info!("Bidirectional sync started");
        Ok(())
    }

    /// Start sync from Zed to Helix
    fn start_zed_to_helix_sync(
        &self,
        config: SyncConfig,
        cx: &mut App,
    ) -> Result<Task<()>> {
        let _integration_weak = self.integration.clone();
        let _sync_state = self.sync_state.clone();

        let task = cx.spawn(async move |_cx| {
            let mut interval = interval(Duration::from_secs(config.sync_interval_seconds));

            loop {
                interval.tick().await;

                // TODO: Comment out integration upgrade until API is fixed
                // if let Some(integration) = integration_weak.upgrade() {
                //     // Check for changes in Zed contexts
                //     // TODO: Fix get_contexts call when API is available
                //     let contexts = Vec::new(); // integration.read(cx).get_contexts();
                //     
                //     // Compare with last known state and generate events
                //     // TODO: Implement change detection and event generation
                //     log::trace!("Zed -> Helix sync tick: {} contexts", contexts.len());
                // } else {
                //     break;
                // }
                
                // For now, just break since integration_weak is not available
                break;
            }
        });

        Ok(task)
    }

    /// Start sync from Helix to Zed
    fn start_helix_to_zed_sync(
        &self,
        config: SyncConfig,
        cx: &mut App,
    ) -> Result<Task<()>> {
        let integration_weak = self.integration.clone();
        let _sync_state = self.sync_state.clone();

        let task = cx.spawn(async move |_cx| {
            // TODO: Implement WebSocket or polling to receive updates from Helix
            // For now, just log that we're ready to receive
            log::info!("Ready to receive sync events from Helix at {}", config.helix_api_url);

            // Placeholder loop
            let mut interval = interval(Duration::from_secs(config.sync_interval_seconds * 2));
            loop {
                interval.tick().await;

                // TODO: Fix when integration_weak.upgrade() is available
                if false { // integration_weak.upgrade().is_none() {
                    break;
                }

                log::trace!("Helix -> Zed sync tick");
            }
        });

        Ok(task)
    }

    /// Get current sync status
    pub fn get_sync_status(&self) -> SyncStatus {
        let state = self.sync_state.read();
        SyncStatus {
            last_sync: state.last_sync_timestamp,
            pending_events: state.pending_events.len(),
            error_count: state.sync_errors.len(),
            is_connected: true, // TODO: Implement actual connection status
        }
    }
}

/// Sync status information
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncStatus {
    pub last_sync: Option<chrono::DateTime<chrono::Utc>>,
    pub pending_events: usize,
    pub error_count: usize,
    pub is_connected: bool,
}

/// Thread synchronization helper
pub struct ThreadSync {
    thread_id: String,
    context_id: String,
    last_message_id: Option<u64>,
    sync_direction: SyncDirection,
}

#[derive(Clone, Debug)]
pub enum SyncDirection {
    ZedToHelix,
    HelixToZed,
    Bidirectional,
}

impl ThreadSync {
    pub fn new(thread_id: String, context_id: String, direction: SyncDirection) -> Self {
        Self {
            thread_id,
            context_id,
            last_message_id: None,
            sync_direction: direction,
        }
    }

    /// Sync a thread from Zed to Helix
    pub async fn sync_to_helix(&mut self, messages: Vec<MessageInfo>) -> Result<()> {
        // TODO: Send messages to Helix thread
        log::info!(
            "Syncing {} messages from Zed context {} to Helix thread {}",
            messages.len(),
            self.context_id,
            self.thread_id
        );

        // Update last message ID
        if let Some(last_message) = messages.last() {
            self.last_message_id = Some(last_message.id);
        }

        Ok(())
    }

    /// Sync a thread from Helix to Zed
    pub async fn sync_from_helix(&mut self, thread_data: ThreadData) -> Result<()> {
        // TODO: Create/update Zed context with thread data
        log::info!(
            "Syncing thread {} from Helix to Zed context {} ({} messages)",
            self.thread_id,
            self.context_id,
            thread_data.messages.len()
        );

        Ok(())
    }
}

/// Event streaming for real-time sync
pub struct EventStream {
    sender: tokio::sync::broadcast::Sender<TimestampedSyncEvent>,
    _receiver: tokio::sync::broadcast::Receiver<TimestampedSyncEvent>,
}

impl EventStream {
    pub fn new() -> Self {
        let (sender, receiver) = tokio::sync::broadcast::channel(1000);
        Self {
            sender,
            _receiver: receiver,
        }
    }

    pub fn send_event(&self, event: SyncEvent) -> Result<()> {
        let timestamped_event = TimestampedSyncEvent {
            event,
            timestamp: chrono::Utc::now(),
            session_id: "zed".to_string(), // TODO: Get actual session ID
        };

        self.sender
            .send(timestamped_event)
            .map_err(|_| anyhow::anyhow!("Failed to broadcast sync event"))?;

        Ok(())
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<TimestampedSyncEvent> {
        self.sender.subscribe()
    }
}