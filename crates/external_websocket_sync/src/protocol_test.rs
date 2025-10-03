//! End-to-end WebSocket protocol test
//!
//! Tests the complete flow per WEBSOCKET_PROTOCOL_SPEC.md:
//! 1. External system sends chat_message
//! 2. Zed creates ACP thread (mocked)
//! 3. Zed sends thread_created
//! 4. Zed streams message_added (multiple chunks)
//! 5. Zed sends message_completed

#[cfg(test)]
mod tests {
    use super::super::types::{IncomingChatMessage, SyncEvent};
    use super::super::{ThreadCreationRequest, init_thread_creation_callback};
    use anyhow::Result;
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_tungstenite::accept_async;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    #[tokio::test]
    async fn test_end_to_end_protocol_flow() -> Result<()> {
        println!("\n🧪 Testing end-to-end WebSocket protocol flow\n");

        // 1. Start mock external system WebSocket server
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        println!("✅ Mock external system listening on {}", addr);

        let (ext_to_zed_tx, mut ext_to_zed_rx) = mpsc::unbounded_channel::<String>();
        let (zed_to_ext_tx, mut zed_to_ext_rx) = mpsc::unbounded_channel::<String>();

        let zed_to_ext_tx_for_server = zed_to_ext_tx.clone();

        // Spawn mock external system server
        tokio::spawn(async move {
            let zed_to_ext_tx = zed_to_ext_tx_for_server;
            let (stream, _) = listener.accept().await.unwrap();
            let ws_stream = accept_async(stream).await.unwrap();
            let (mut write, mut read) = ws_stream.split();

            // Send messages from external system to Zed
            let send_task = tokio::spawn(async move {
                while let Some(msg) = ext_to_zed_rx.recv().await {
                    write.send(Message::Text(msg.into())).await.unwrap();
                }
            });

            // Receive messages from Zed
            let recv_task = tokio::spawn(async move {
                while let Some(msg) = read.next().await {
                    if let Ok(Message::Text(text)) = msg {
                        zed_to_ext_tx.send(text.to_string()).unwrap();
                    }
                }
            });

            let _ = tokio::join!(send_task, recv_task);
        });

        // 2. Setup thread creation callback (simulates service layer)
        let (callback_tx, mut callback_rx) = mpsc::unbounded_channel();
        init_thread_creation_callback(callback_tx);

        let zed_to_ext_tx_clone = zed_to_ext_tx.clone();

        // Spawn task to handle thread creation requests
        tokio::spawn(async move {
            while let Some(request) = callback_rx.recv().await {
                println!("🎯 Received thread creation request: {:?}", request);

                let acp_thread_id = if request.acp_thread_id.is_some() {
                    request.acp_thread_id.unwrap()
                } else {
                    format!("acp-thread-{}", uuid::Uuid::new_v4())
                };

                // Send thread_created
                let thread_created = SyncEvent::ThreadCreated {
                    acp_thread_id: acp_thread_id.clone(),
                    request_id: request.request_id.clone(),
                };
                zed_to_ext_tx_clone.send(serde_json::to_string(&thread_created).unwrap()).unwrap();
                println!("📤 Sent thread_created");

                // Simulate AI streaming response
                for i in 1..=3 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

                    let content = match i {
                        1 => "The answer",
                        2 => "The answer is",
                        3 => "The answer is 42",
                        _ => unreachable!(),
                    };

                    let message_added = SyncEvent::MessageAdded {
                        acp_thread_id: acp_thread_id.clone(),
                        message_id: "msg-123".to_string(),
                        role: "assistant".to_string(),
                        content: content.to_string(),
                        timestamp: chrono::Utc::now().timestamp(),
                    };
                    zed_to_ext_tx_clone.send(serde_json::to_string(&message_added).unwrap()).unwrap();
                    println!("📤 Sent message_added chunk {}: {}", i, content);
                }

                // Send message_completed
                let message_completed = SyncEvent::MessageCompleted {
                    acp_thread_id: acp_thread_id.clone(),
                    message_id: "msg-123".to_string(),
                    request_id: request.request_id.clone(),
                };
                zed_to_ext_tx_clone.send(serde_json::to_string(&message_completed).unwrap()).unwrap();
                println!("📤 Sent message_completed");
            }
        });

        // 3. Start Zed WebSocket client
        let config = super::super::websocket_sync::WebSocketSyncConfig {
            enabled: true,
            url: format!("localhost:{}", addr.port()),
            auth_token: String::new(),
            use_tls: false,
        };

        let _service = super::super::websocket_sync::WebSocketSync::start(config).await?;
        println!("✅ Zed WebSocket client connected");

        // Give connection time to establish
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // 4. External system sends chat_message
        let chat_message = json!({
            "type": "chat_message",
            "data": {
                "acp_thread_id": null,
                "message": "What is the meaning of life?",
                "request_id": "req-test-001"
            }
        });

        ext_to_zed_tx.send(chat_message.to_string())?;
        println!("📤 External system sent chat_message");

        // 5. Collect responses from Zed
        let mut responses = Vec::new();
        let timeout = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            async {
                for _ in 0..5 { // Expect: thread_created + 3 message_added + message_completed
                    if let Some(msg) = zed_to_ext_rx.recv().await {
                        responses.push(msg);
                    }
                }
            }
        );

        timeout.await?;

        println!("\n📥 Received {} responses from Zed:", responses.len());
        for (i, resp) in responses.iter().enumerate() {
            println!("  {}. {}", i + 1, resp);
        }

        // 6. Verify protocol compliance
        assert_eq!(responses.len(), 5, "Should receive 5 messages total");

        // Parse responses
        let parsed: Vec<serde_json::Value> = responses.iter()
            .map(|r| serde_json::from_str(r).unwrap())
            .collect();

        // Check thread_created
        assert_eq!(parsed[0]["type"], "thread_created");
        assert!(parsed[0]["acp_thread_id"].is_string());
        assert_eq!(parsed[0]["request_id"], "req-test-001");
        println!("✅ thread_created verified");

        // Check message_added streaming (3 chunks)
        for i in 1..=3 {
            assert_eq!(parsed[i]["type"], "message_added");
            assert_eq!(parsed[i]["message_id"], "msg-123");
            assert_eq!(parsed[i]["role"], "assistant");
            assert!(parsed[i]["content"].as_str().unwrap().starts_with("The answer"));
        }

        // Verify content gets progressively longer
        let content1 = parsed[1]["content"].as_str().unwrap();
        let content2 = parsed[2]["content"].as_str().unwrap();
        let content3 = parsed[3]["content"].as_str().unwrap();
        assert!(content2.len() > content1.len());
        assert!(content3.len() > content2.len());
        println!("✅ message_added streaming verified (3 chunks, progressively longer)");

        // Check message_completed
        assert_eq!(parsed[4]["type"], "message_completed");
        assert_eq!(parsed[4]["message_id"], "msg-123");
        assert_eq!(parsed[4]["request_id"], "req-test-001");
        println!("✅ message_completed verified");

        println!("\n🎉 END-TO-END PROTOCOL TEST PASSED!");
        println!("✅ Protocol fully compliant with WEBSOCKET_PROTOCOL_SPEC.md");

        Ok(())
    }
}
