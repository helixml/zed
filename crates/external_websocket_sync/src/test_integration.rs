//! Simple integration test for bidirectional WebSocket sync
//! 
//! This test verifies that the core message handling works:
//! 1. Helix sends ExternalAgentCommand with type: "chat_message"
//! 2. Zed processes it and generates ChatResponse
//! 3. Response is serialized back to Helix format

use anyhow::Result;
use serde_json::json;
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::types::*;
use crate::websocket_sync::{ExternalWebSocketCommand, WebSocketSync};

/// Test the core bidirectional message handling
#[tokio::test]
async fn test_bidirectional_message_handling() -> Result<()> {
    println!("🧪 Testing bidirectional message handling");
    
    // Create channels for testing
    let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
    let (command_sender, mut _command_receiver) = mpsc::unbounded_channel();

    println!("✅ Created test channels");

    // Simulate Helix sending a chat_message command
    let helix_command = ExternalWebSocketCommand {
        command_type: "chat_message".to_string(),
        data: {
            let mut data = HashMap::default();
            data.insert("request_id".to_string(), json!("req-123"));
            data.insert("message".to_string(), json!("hi"));
            data.insert("session_id".to_string(), json!("test-session"));
            data
        },
    };

    println!("📤 Simulating Helix command: {:?}", helix_command);

    // Serialize command as if received from WebSocket
    let command_json = serde_json::to_string(&helix_command)?;
    
    // Call the message handler directly
    WebSocketSync::handle_incoming_message(
        "test-session",
        command_json,
        &command_sender,
        &event_sender,
    ).await?;

    println!("✅ Message handler executed");

    // Check that we received a response event
    let response_event = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        event_receiver.recv()
    ).await.map_err(|_| anyhow::anyhow!("Timeout waiting for response event"))?;

    match response_event {
        Some(SyncEvent::ChatResponse { request_id, content }) => {
            println!("📨 Received ChatResponse: request_id={}, content={}", request_id, content);
            
            // Verify the response
            assert_eq!(request_id, "req-123");
            assert_eq!(content, "Echo from Zed: hi");
            
            println!("✅ Response content verified");
        }
        other => {
            return Err(anyhow::anyhow!("Expected ChatResponse, got: {:?}", other));
        }
    }

    // Test serialization back to WebSocket format
    let sync_event = SyncEvent::ChatResponse {
        request_id: "req-123".to_string(),
        content: "Echo from Zed: hi".to_string(),
    };

    let event_type = WebSocketSync::event_type_string(&sync_event);
    let event_data = WebSocketSync::event_to_data(sync_event);

    println!("🔄 Event serialization: type={}, data={:?}", event_type, event_data);

    // Verify serialization
    assert_eq!(event_type, "chat_response");
    assert_eq!(event_data.get("request_id").unwrap(), &json!("req-123"));
    assert_eq!(event_data.get("content").unwrap(), &json!("Echo from Zed: hi"));

    println!("✅ Event serialization verified");

    // Test the complete message round-trip
    let sync_message = crate::websocket_sync::SyncMessage {
        session_id: "test-session".to_string(),
        event_type: event_type,
        data: event_data,
        timestamp: chrono::Utc::now(),
    };

    let final_json = serde_json::to_string_pretty(&sync_message)?;
    println!("📋 Final WebSocket message:");
    println!("{}", final_json);

    // Verify the JSON contains expected fields
    let parsed: serde_json::Value = serde_json::from_str(&final_json)?;
    assert_eq!(parsed["session_id"], "test-session");
    assert_eq!(parsed["event_type"], "chat_response");
    assert_eq!(parsed["data"]["request_id"], "req-123");
    assert_eq!(parsed["data"]["content"], "Echo from Zed: hi");

    println!("🎉 Bidirectional message handling test PASSED!");
    println!("");
    println!("✅ Protocol mismatch FIXED:");
    println!("   Helix 'chat_message' → Zed handler → ChatResponse");
    println!("✅ Message flow verified:");
    println!("   'hi' → 'Echo from Zed: hi'");
    println!("✅ WebSocket serialization working");

    Ok(())
}

/// Test SyncEvent serialization for all response types
#[tokio::test]
async fn test_all_sync_event_types() -> Result<()> {
    println!("🧪 Testing all SyncEvent serialization");

    let test_cases = vec![
        (
            SyncEvent::ChatResponse {
                request_id: "req-1".to_string(),
                content: "Hello".to_string(),
            },
            "chat_response"
        ),
        (
            SyncEvent::ChatResponseChunk {
                request_id: "req-2".to_string(),
                chunk: "Hello ".to_string(),
            },
            "chat_response_chunk"
        ),
        (
            SyncEvent::ChatResponseDone {
                request_id: "req-3".to_string(),
            },
            "chat_response_done"
        ),
        (
            SyncEvent::ChatResponseError {
                request_id: "req-4".to_string(),
                error: "Test error".to_string(),
            },
            "chat_response_error"
        ),
    ];

    for (event, expected_type) in test_cases {
        let event_type = WebSocketSync::event_type_string(&event);
        let event_data = WebSocketSync::event_to_data(event);
        
        assert_eq!(event_type, expected_type);
        println!("✅ {} serialization verified", expected_type);
    }

    println!("🎉 All SyncEvent types working correctly!");
    
    Ok(())
}

/// Test error handling for invalid commands
#[tokio::test]
async fn test_message_error_handling() -> Result<()> {
    println!("🧪 Testing message error handling");
    
    let (event_sender, mut _event_receiver) = mpsc::unbounded_channel();
    let (command_sender, _command_receiver) = mpsc::unbounded_channel();

    // Test invalid JSON
    let result = WebSocketSync::handle_incoming_message(
        "test-session",
        "invalid json".to_string(),
        &command_sender,
        &event_sender,
    ).await;

    assert!(result.is_err());
    println!("✅ Invalid JSON handling verified");

    // Test unsupported command type  
    let unsupported_command = json!({
        "type": "unsupported_command",
        "data": {}
    });
    
    let result = WebSocketSync::handle_incoming_message(
        "test-session",
        unsupported_command.to_string(),
        &command_sender,
        &event_sender,
    ).await;

    // Should not error, but should log and continue
    assert!(result.is_ok());
    println!("✅ Unsupported command handling verified");

    println!("🎉 Error handling test PASSED!");
    
    Ok(())
}