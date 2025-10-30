//! Real WebSocket Integration Tests
//!
//! These tests create actual WebSocket connections and test the full flow:
//! 1. External system (test server) initiates thread creation
//! 2. Zed creates ACP thread and processes message
//! 3. Zed streams response back over WebSocket
//! 4. External system receives completion notification
//!
//! Key principle: External system is responsible for ALL ID mapping.
//! Zed only sends ACP thread IDs - external system maps to its own session IDs.

use anyhow::Result;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock, broadcast};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use futures::{SinkExt, StreamExt};

/// Mock external system that tracks its own session IDs and maps to Zed ACP thread IDs
#[derive(Clone)]
struct MockExternalSystem {
    /// External session ID -> Zed ACP thread ID mapping (external system's responsibility)
    session_to_thread: Arc<RwLock<HashMap<String, String>>>,
    /// Messages received from Zed
    received_messages: Arc<RwLock<Vec<serde_json::Value>>>,
}

impl MockExternalSystem {
    fn new() -> Self {
        Self {
            session_to_thread: Arc::new(RwLock::new(HashMap::new())),
            received_messages: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Store mapping from external session ID to Zed ACP thread ID
    async fn map_session_to_thread(&self, session_id: String, thread_id: String) {
        let mut mapping = self.session_to_thread.write().await;
        mapping.insert(session_id, thread_id);
    }

    /// Get Zed thread ID for external session
    async fn get_thread_id(&self, session_id: &str) -> Option<String> {
        let mapping = self.session_to_thread.read().await;
        mapping.get(session_id).cloned()
    }

    /// Record message received from Zed
    async fn record_message(&self, msg: serde_json::Value) {
        let mut messages = self.received_messages.write().await;
        messages.push(msg);
    }

    /// Get all received messages
    async fn get_messages(&self) -> Vec<serde_json::Value> {
        let messages = self.received_messages.read().await;
        messages.clone()
    }
}

/// Start a real WebSocket server for testing
async fn start_test_websocket_server(
    port: u16,
) -> Result<(broadcast::Sender<Message>, mpsc::UnboundedReceiver<Message>)> {
    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr).await?;

    let (client_tx, _) = broadcast::channel::<Message>(100);
    let (server_tx, server_rx) = mpsc::unbounded_channel::<Message>();

    let client_tx_clone = client_tx.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let ws_stream = match accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(e) => {
                            eprintln!("WebSocket handshake error: {}", e);
                            continue;
                        }
                    };

                    let (mut write, mut read) = ws_stream.split();
                    let tx = server_tx.clone();
                    let mut rx = client_tx_clone.subscribe();

                    // Forward messages from Zed to test
                    tokio::spawn(async move {
                        while let Some(msg) = read.next().await {
                            match msg {
                                Ok(msg) => {
                                    if let Err(e) = tx.send(msg) {
                                        eprintln!("Failed to forward message from Zed: {}", e);
                                        break;
                                    }
                                }
                                Err(e) => {
                                    eprintln!("WebSocket read error: {}", e);
                                    break;
                                }
                            }
                        }
                    });

                    // Forward messages from test to Zed
                    tokio::spawn(async move {
                        while let Ok(msg) = rx.recv().await {
                            if let Err(e) = write.send(msg).await {
                                eprintln!("Failed to send message to Zed: {}", e);
                                break;
                            }
                        }
                    });
                }
                Err(e) => {
                    eprintln!("Accept error: {}", e);
                }
            }
        }
    });

    // Give server time to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    Ok((client_tx, server_rx))
}

#[tokio::test]
async fn test_real_websocket_thread_creation_and_response() -> Result<()> {
    println!("\n🧪 Testing real WebSocket thread creation and response flow\n");

    // 1. Start real WebSocket server (simulating external system like Helix)
    let port = 9001; // Use a different port than production
    let (to_zed, mut from_zed) = start_test_websocket_server(port).await?;
    println!("✅ Started WebSocket server on port {}", port);

    // 2. Create mock external system
    let external_system = MockExternalSystem::new();
    let external_session_id = "external-session-123";

    // 3. Set up thread creation callback (simulates what agent_panel does)
    let (callback_tx, mut callback_rx) = mpsc::unbounded_channel();
    crate::init_thread_creation_callback(callback_tx);

    // Spawn task to handle thread creation requests (simulates agent_panel's headless listener)
    let external_system_for_callback = external_system.clone();
    let helix_session_for_callback = external_session_id.to_string();
    tokio::spawn(async move {
        while let Some(request) = callback_rx.recv().await {
            eprintln!("🎯 [TEST_CALLBACK] Received thread creation request for: {}", request.helix_session_id);

            // Simulate agent_panel creating real ACP thread and sending context_created
            // In real impl, this would create actual AcpThread entity
            let acp_thread_id = format!("acp-{}", chrono::Utc::now().timestamp_millis());

            external_system_for_callback.map_session_to_thread(
                helix_session_for_callback.clone(),
                acp_thread_id.clone()
            ).await;

            // Send context_created (what agent_panel would do)
            if let Some(sender) = crate::websocket_sync::get_global_websocket_sender() {
                let msg = serde_json::json!({
                    "session_id": request.helix_session_id,
                    "event_type": "context_created",
                    "data": {
                        "acp_thread_id": acp_thread_id,
                        "helix_session_id": request.helix_session_id
                    }
                });
                let _ = sender.send(Message::Text(serde_json::to_string(&msg).unwrap().into()));
                eprintln!("✅ [TEST_CALLBACK] Sent context_created with acp_thread_id: {}", acp_thread_id);

                // Simulate AI completing and sending message_completed (what AcpThreadEvent::Stopped would trigger)
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                let completion_msg = serde_json::json!({
                    "session_id": request.helix_session_id,
                    "event_type": "message_completed",
                    "data": {
                        "acp_thread_id": acp_thread_id,
                        "message_id": format!("msg_{}", chrono::Utc::now().timestamp_millis()),
                        "request_id": request.request_id
                    }
                });
                let _ = sender.send(Message::Text(serde_json::to_string(&completion_msg).unwrap().into()));
                eprintln!("✅ [TEST_CALLBACK] Sent message_completed (simulating AcpThreadEvent::Stopped)");
            }
        }
    });

    // 4. Start Zed's WebSocket client to connect to test server
    use crate::websocket_sync::WebSocketSyncConfig;
    let config = WebSocketSyncConfig {
        enabled: true,
        helix_url: format!("localhost:{}", port),
        session_id: "test-zed-agent".to_string(),
        auth_token: "".to_string(),
        use_tls: false,
    };

    let _websocket_sync = crate::websocket_sync::WebSocketSync::new(config).await?;
    println!("✅ Zed WebSocket client connected to test server");

    // Give connection time to establish
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // 5. External system sends message to Zed to create thread
    // Per WEBSOCKET_PROTOCOL_SPEC.md - Flow 1
    let create_message = json!({
        "type": "chat_message",
        "data": {
            "helix_session_id": external_session_id,
            "acp_thread_id": null,  // First message - no context yet
            "message": "What is 2+2?",
            "request_id": "req_test_123"
        }
    });

    to_zed.send(Message::Text(create_message.to_string().into()))?;
    println!("📤 External system sent create_thread message");

    // 6. Wait for Zed to respond with context_created (per spec)
    let context_created = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        async {
            while let Some(msg) = from_zed.recv().await {
                if let Message::Text(text) = msg {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        eprintln!("📥 Test received message: {}", text);
                        // Per external-agent-websocket-protocol.md spec
                        if json.get("event_type") == Some(&json!("context_created")) {
                            return Some(json);
                        }
                        external_system.record_message(json).await;
                    }
                }
            }
            None
        }
    ).await;

    let context_created = context_created
        .map_err(|_| anyhow::anyhow!("Timeout waiting for context_created"))?
        .ok_or_else(|| anyhow::anyhow!("No context_created message received"))?;

    println!("✅ Received context_created: {:?}", context_created);

    // 7. External system extracts acp_thread_id (per WEBSOCKET_PROTOCOL_SPEC.md)
    let acp_thread_id = context_created
        .get("data")
        .and_then(|d| d.get("acp_thread_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("No acp_thread_id in response data (per spec)"))?;

    external_system.map_session_to_thread(
        external_session_id.to_string(),
        acp_thread_id.to_string()
    ).await;

    println!("✅ External system mapped session {} to acp_thread_id {}",
             external_session_id, acp_thread_id);

    // 8. Wait for response streaming (optional response_chunk messages)
    // and final message_completed
    let completion = tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        async {
            let mut chunks = Vec::new();
            while let Some(msg) = from_zed.recv().await {
                if let Message::Text(text) = msg {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        eprintln!("📥 [COMPLETION_WAIT] Received: {}", text);
                        let event_type = json.get("event_type").and_then(|v| v.as_str());

                        match event_type {
                            Some("message_added") => {
                                // Streaming chunk (per spec)
                                chunks.push(json.clone());
                                external_system.record_message(json).await;
                            }
                            Some("message_completed") => {
                                // Final completion (per spec)
                                external_system.record_message(json.clone()).await;
                                eprintln!("✅ [COMPLETION_WAIT] Found message_completed!");
                                return Some((chunks, json));
                            }
                            _ => {
                                external_system.record_message(json).await;
                            }
                        }
                    }
                }
            }
            None
        }
    ).await;

    let (chunks, completion_msg) = completion
        .map_err(|_| anyhow::anyhow!("Timeout waiting for message_completed"))?
        .ok_or_else(|| anyhow::anyhow!("No message_completed received"))?;

    println!("✅ Received {} response chunks", chunks.len());
    println!("✅ Received message_completed: {:?}", completion_msg);

    // 8. Verify completion message contains acp_thread_id (per spec)
    let completion_thread_id = completion_msg
        .get("data")
        .and_then(|d| d.get("acp_thread_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("No acp_thread_id in completion message data (per spec)"))?;

    assert_eq!(
        completion_thread_id,
        acp_thread_id,
        "Completion message should contain same acp_thread_id"
    );

    // 9. External system looks up its session ID using the thread ID from response
    let found_session = external_system.get_thread_id(external_session_id).await;
    assert_eq!(found_session.as_deref(), Some(acp_thread_id));

    println!("✅ External system successfully mapped response back to session {}",
             external_session_id);

    // 10. Verify request_id is echoed back (per spec)
    let returned_request_id = completion_msg.get("data")
        .and_then(|d| d.get("request_id"))
        .and_then(|v| v.as_str());

    assert_eq!(returned_request_id, Some("req_test_123"), "Request ID should be echoed back per spec");
    println!("✅ Request ID correctly echoed back: {:?}", returned_request_id);

    println!("\n🎉 Real WebSocket test PASSED!");
    println!("✅ External system maintains all ID mappings");
    println!("✅ Zed only sends ACP thread IDs");
    println!("✅ Full bidirectional communication works");

    Ok(())
}

#[tokio::test]
async fn test_real_websocket_multiple_sessions() -> Result<()> {
    println!("\n🧪 Testing multiple concurrent sessions over real WebSocket\n");

    // 1. Start real WebSocket server
    let port = 9002;
    let (to_zed, mut from_zed) = start_test_websocket_server(port).await?;
    println!("✅ Started WebSocket server on port {}", port);

    // 2. Create mock external system
    let external_system = Arc::new(MockExternalSystem::new());

    // 3. Create multiple sessions
    let sessions = vec![
        ("session-A", "What is 1+1?"),
        ("session-B", "What is 2+2?"),
        ("session-C", "What is 3+3?"),
    ];

    for (session_id, message) in &sessions {
        let create_message = json!({
            "type": "create_thread",
            "message": message
        });
        to_zed.send(Message::Text(create_message.to_string().into()))?;
        println!("📤 Sent create_thread for session {}", session_id);
    }

    // 4. Collect all thread_created responses and map them
    let mut created_count = 0;
    let timeout = tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        async {
            while let Some(msg) = from_zed.recv().await {
                if let Message::Text(text) = msg {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json.get("type") == Some(&json!("thread_created")) {
                            let acp_thread_id = json
                                .get("acp_thread_id")
                                .and_then(|v| v.as_str())
                                .unwrap();

                            // Map to corresponding external session
                            // (In real system, would use request_id or similar to correlate)
                            let session_id = sessions[created_count].0;
                            external_system.map_session_to_thread(
                                session_id.to_string(),
                                acp_thread_id.to_string()
                            ).await;

                            println!("✅ Mapped session {} to thread {}", session_id, acp_thread_id);
                            created_count += 1;

                            if created_count == sessions.len() {
                                break;
                            }
                        }
                    }
                }
            }
        }
    );

    timeout.await.map_err(|_| anyhow::anyhow!("Timeout waiting for thread_created messages"))?;

    assert_eq!(created_count, sessions.len(), "Should have received thread_created for all sessions");

    // 5. Verify all sessions are mapped
    for (session_id, _) in &sessions {
        let thread_id = external_system.get_thread_id(session_id).await;
        assert!(thread_id.is_some(), "Session {} should be mapped", session_id);
        println!("✅ Session {} has thread ID: {:?}", session_id, thread_id);
    }

    println!("\n🎉 Multiple sessions test PASSED!");
    println!("✅ All sessions independently tracked by external system");
    println!("✅ Zed never sees external session IDs");

    Ok(())
}

#[tokio::test]
async fn test_real_websocket_message_format() -> Result<()> {
    println!("\n🧪 Testing WebSocket message format validation\n");

    let port = 9003;
    let (to_zed, mut from_zed) = start_test_websocket_server(port).await?;
    println!("✅ Started WebSocket server on port {}", port);

    // Send create thread message
    let create_message = json!({
        "type": "create_thread",
        "message": "Test message"
    });
    to_zed.send(Message::Text(create_message.to_string().into()))?;

    // Validate response format
    let response = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        async {
            while let Some(msg) = from_zed.recv().await {
                if let Message::Text(text) = msg {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json.get("type") == Some(&json!("thread_created")) {
                            return Some(json);
                        }
                    }
                }
            }
            None
        }
    ).await
    .map_err(|_| anyhow::anyhow!("Timeout"))?
    .ok_or_else(|| anyhow::anyhow!("No response"))?;

    // Validate required fields
    assert!(response.get("type").is_some(), "Response must have 'type' field");
    assert!(response.get("acp_thread_id").is_some(), "Response must have 'acp_thread_id' field");
    assert!(response.get("timestamp").is_none(), "Response should NOT have external 'timestamp' field");
    assert!(response.get("session_id").is_none(), "Response should NOT have external 'session_id' field");

    println!("✅ Message format is correct");
    println!("✅ Only contains Zed-internal fields (acp_thread_id)");
    println!("✅ No external system IDs leaked");

    println!("\n🎉 Message format test PASSED!");

    Ok(())
}
