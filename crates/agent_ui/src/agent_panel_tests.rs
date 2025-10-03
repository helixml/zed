/// Tests for ACP (Agent Client Protocol) external agent integration
///
/// NOTE: These tests are DISABLED because they test the OLD architecture
/// with ExternalSessionMapping and ContextToHelixSessionMapping which have
/// been removed per the new protocol spec (WEBSOCKET_PROTOCOL_SPEC.md).
///
/// The new architecture is stateless - Zed doesn't maintain session mappings.
/// Protocol-level tests are in external_websocket_sync/src/protocol_test.rs
///
/// TODO: Rewrite these as integration tests for the new callback-based architecture
///
/// Run tests with: `./run_acp_tests.sh` (uses --test-threads=1 for reliability)
/// Or: `cargo test --package agent_ui --lib --features external_websocket_sync -- agent_panel_tests --test-threads=1`
#[cfg(all(test, feature = "external_websocket_sync", feature = "DISABLED_OLD_ARCHITECTURE_TESTS"))]
mod external_agent_tests {
    use crate::agent_panel::{ActiveView, AgentPanel, AgentType};
    use acp_thread::AcpThreadEvent;
    use agent;
    use agent_settings::AgentSettings;
    use editor::EditorSettings;
    use external_websocket_sync_dep as external_websocket_sync;
    use external_websocket_sync::WebSocketSender;
    use fs::{self, FakeFs};
    use gpui::{Entity, SemanticVersion, TestAppContext, VisualTestContext};
    use language;
    use language_model;
    use parking_lot::RwLock;
    use project::Project;
    use prompt_store::{self, PromptBuilder};
    use release_channel;
    use settings::{Settings, SettingsStore};
    use std::sync::Arc;
    use theme::{self, ThemeSettings};
    use workspace::Workspace;
    use serde_json;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
            language::init(cx);
            Project::init_settings(cx);
            AgentSettings::register(cx);
            workspace::init_settings(cx);
            ThemeSettings::register(cx);
            release_channel::init(SemanticVersion::default(), cx);
            EditorSettings::register(cx);
            prompt_store::init(cx);
            
            // Initialize language model registry for agent threads
            language_model::init_settings(cx);
            
            // Initialize filesystem and thread store database for AgentPanel
            let fake_fs = fs::FakeFs::new(cx.background_executor().clone());
            <dyn fs::Fs>::set_global(fake_fs.clone(), cx);
            agent::thread_store::init(fake_fs, cx);
        });
    }

    async fn setup_agent_panel_for_test(
        cx: &mut TestAppContext,
    ) -> (Entity<AgentPanel>, &mut VisualTestContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, visual_cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        // Initialize WebSocket globals for testing
        visual_cx.update(|_window, cx| {
            // Create a channel matching the expected type
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            cx.set_global(WebSocketSender {
                sender: Arc::new(RwLock::new(Some(tx))),
            });
            cx.set_global(ExternalSessionMapping {
                sessions: Arc::new(RwLock::new(std::collections::HashMap::new())),
            });
            cx.set_global(ContextToHelixSessionMapping {
                contexts: Arc::new(RwLock::new(std::collections::HashMap::new())),
            });
        });

        let prompt_builder = Arc::new(PromptBuilder::new(None).unwrap());
        let workspace_weak = workspace.downgrade();
        
        // Use the actual window context to load the panel
        let panel = visual_cx.update(|window, cx| {
            let async_cx = window.to_async(cx);
            cx.spawn(async move |_cx| {
                AgentPanel::load(workspace_weak, prompt_builder, async_cx).await
            })
        }).await.unwrap();

        (panel, visual_cx)
    }

    #[gpui::test]
    async fn test_new_acp_thread_with_message_creates_thread(cx: &mut TestAppContext) {
        let (panel, cx) = setup_agent_panel_for_test(cx).await;

        let helix_session_id = "test-session-123".to_string();
        let message = "Hello from Helix";

        // Trigger ACP thread creation
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_acp_thread_with_message(message, helix_session_id.clone(), window, cx);
            });
        });

        cx.run_until_parked();

        // Verify the active view is now an ExternalAgentThread
        let is_external_thread = panel.read_with(cx, |panel, _cx| {
            matches!(panel.active_view, ActiveView::ExternalAgentThread { .. })
        });

        assert!(
            is_external_thread,
            "Active view should be set to ExternalAgentThread after calling new_acp_thread_with_message"
        );
    }

    #[gpui::test]
    async fn test_session_mapping_is_stored(cx: &mut TestAppContext) {
        let (panel, cx) = setup_agent_panel_for_test(cx).await;

        let helix_session_id = "test-mapping-456".to_string();
        let message = "Test session mapping";

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_acp_thread_with_message(message, helix_session_id.clone(), window, cx);
            });
        });

        // Wait for async session mapping (500ms delay + processing)
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(600));
        cx.run_until_parked();
        cx.executor().run_until_parked();

        // Verify session mapping was stored
        let mapping_exists = cx.read(|cx| {
            if let Some(session_mapping) = cx.try_global::<ExternalSessionMapping>() {
                let sessions = session_mapping.sessions.read();
                sessions.contains_key(&helix_session_id)
            } else {
                false
            }
        });

        assert!(
            mapping_exists,
            "Session mapping should be stored for Helix session ID"
        );
    }

    #[gpui::test]
    async fn test_reverse_session_mapping_is_stored(cx: &mut TestAppContext) {
        let (panel, cx) = setup_agent_panel_for_test(cx).await;

        let helix_session_id = "test-reverse-789".to_string();
        let message = "Test reverse mapping";

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_acp_thread_with_message(message, helix_session_id.clone(), window, cx);
            });
        });

        // Wait for async session mapping (500ms delay + processing)
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(600));
        cx.run_until_parked();
        cx.executor().run_until_parked();

        // Verify reverse mapping exists (ACP session ID -> Helix session ID)
        let reverse_mapping_exists = cx.read(|cx| {
            if let Some(reverse_mapping) = cx.try_global::<ContextToHelixSessionMapping>() {
                let contexts = reverse_mapping.contexts.read();
                !contexts.is_empty()
            } else {
                false
            }
        });

        assert!(
            reverse_mapping_exists,
            "Reverse session mapping should be created (ACP -> Helix)"
        );
    }

    #[gpui::test]
    async fn test_selected_agent_is_set_to_native(cx: &mut TestAppContext) {
        let (panel, cx) = setup_agent_panel_for_test(cx).await;

        // Set to a different agent first by creating a new agent thread
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_agent_thread(AgentType::Zed, window, cx);
            });
        });

        let helix_session_id = "test-agent-selection".to_string();

        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_acp_thread_with_message("Test", helix_session_id, window, cx);
            });
        });

        cx.run_until_parked();

        // Verify selected agent was changed to NativeAgent
        let is_native = panel.read_with(cx, |panel, _cx| {
            panel.selected_agent() == AgentType::NativeAgent
        });

        assert!(
            is_native,
            "Selected agent should be set to NativeAgent when creating ACP thread"
        );
    }

    #[gpui::test]
    async fn test_multiple_sessions_have_distinct_mappings(cx: &mut TestAppContext) {
        let (panel, cx) = setup_agent_panel_for_test(cx).await;

        let session_1 = "session-1".to_string();
        let session_2 = "session-2".to_string();

        // Create first thread
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_acp_thread_with_message("First", session_1.clone(), window, cx);
            });
        });

        // Wait for first session mapping
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(600));
        cx.run_until_parked();

        // Create second thread
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_acp_thread_with_message("Second", session_2.clone(), window, cx);
            });
        });

        // Wait for second session mapping
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(600));
        cx.run_until_parked();
        cx.executor().run_until_parked();

        // Verify both sessions are mapped
        let both_mapped = cx.read(|cx| {
            if let Some(session_mapping) = cx.try_global::<ExternalSessionMapping>() {
                let sessions = session_mapping.sessions.read();
                sessions.contains_key(&session_1) && sessions.contains_key(&session_2)
            } else {
                false
            }
        });

        assert!(
            both_mapped,
            "Both Helix sessions should have distinct ACP session mappings"
        );
    }

    #[gpui::test]
    async fn test_response_sent_back_to_helix(cx: &mut TestAppContext) {
        let (panel, cx) = setup_agent_panel_for_test(cx).await;

        let helix_session_id = "test-response-session".to_string();
        let message = "Test response sending";

        // Create a receiver to capture WebSocket messages
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        
        // Replace the WebSocket sender with our test receiver
        cx.update(|_window, cx| {
            cx.set_global(WebSocketSender {
                sender: Arc::new(RwLock::new(Some(tx))),
            });
        });

        // Create the ACP thread
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.new_acp_thread_with_message(message, helix_session_id.clone(), window, cx);
            });
        });

        // Wait for async session mapping
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(600));
        cx.run_until_parked();

        // Get the thread view
        let thread_view = panel.read_with(cx, |panel, _cx| {
            match &panel.active_view {
                ActiveView::ExternalAgentThread { thread_view } => Some(thread_view.clone()),
                _ => None,
            }
        }).expect("Should have an external thread view");

        // Get the thread entity and emit a Stopped event to simulate completion
        let thread_entity = thread_view.read_with(cx, |view, _cx| {
            view.thread().cloned()
        }).expect("Thread should exist");

        // Emit the Stopped event to trigger response sending
        cx.update(|_window, cx| {
            thread_entity.update(cx, |_thread, cx| {
                cx.emit(AcpThreadEvent::Stopped);
            });
        });

        cx.run_until_parked();
        cx.executor().run_until_parked();

        // Try to receive a message from the WebSocket sender
        let received_message = rx.try_recv();

        assert!(
            received_message.is_ok(),
            "Should have received a WebSocket message when thread stopped"
        );

        // Verify the message content
        if let Ok(msg) = received_message {
            use external_websocket_sync::tungstenite::Message;
            
            match msg {
                Message::Text(text) => {
                    let json: serde_json::Value = serde_json::from_str(&text)
                        .expect("Message should be valid JSON");
                    
                    assert_eq!(
                        json.get("type").and_then(|v| v.as_str()),
                        Some("message_completed"),
                        "Message type should be 'message_completed'"
                    );
                    
                    assert_eq!(
                        json.get("session_id").and_then(|v| v.as_str()),
                        Some(helix_session_id.as_str()),
                        "Session ID should match the Helix session"
                    );
                    
                    assert!(
                        json.get("content").is_some(),
                        "Message should have content field"
                    );
                }
                _ => panic!("Expected text message, got: {:?}", msg),
            }
        }
    }
}
