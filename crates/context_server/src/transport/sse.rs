//! SSE Transport implementation for the old MCP HTTP+SSE protocol (2024-11-05)
//!
//! This implements the deprecated but still widely-used SSE transport where:
//! 1. Client connects to SSE endpoint via GET
//! 2. Server sends an `endpoint` event with POST URL
//! 3. Client POSTs messages to that endpoint
//! 4. Server sends responses via SSE `message` events

use anyhow::{anyhow, Context as _, Result};
use async_trait::async_trait;
use collections::HashMap;
use futures::{AsyncBufReadExt, Stream, StreamExt};
use gpui::BackgroundExecutor;
use http_client::{AsyncBody, HttpClient, Request, http::Method};
use parking_lot::Mutex as SyncMutex;
use smol::channel;
use std::{pin::Pin, sync::Arc};

use crate::transport::Transport;

/// SSE Transport for the old MCP HTTP+SSE protocol (2024-11-05 spec)
///
/// This transport:
/// 1. Opens a GET connection to the SSE endpoint
/// 2. Waits for an `endpoint` event containing the POST URL
/// 3. POSTs JSON-RPC messages to that endpoint
/// 4. Receives responses via SSE `message` events on the original stream
pub struct SseTransport {
    http_client: Arc<dyn HttpClient>,
    sse_endpoint: String,
    post_endpoint: Arc<SyncMutex<Option<String>>>,
    executor: BackgroundExecutor,
    response_tx: channel::Sender<String>,
    response_rx: channel::Receiver<String>,
    error_tx: channel::Sender<String>,
    error_rx: channel::Receiver<String>,
    initialized: Arc<SyncMutex<bool>>,
    headers: HashMap<String, String>,
}

impl SseTransport {
    pub fn new(
        http_client: Arc<dyn HttpClient>,
        sse_endpoint: String,
        headers: HashMap<String, String>,
        executor: BackgroundExecutor,
    ) -> Self {
        let (response_tx, response_rx) = channel::unbounded();
        let (error_tx, error_rx) = channel::unbounded();

        Self {
            http_client,
            sse_endpoint,
            post_endpoint: Arc::new(SyncMutex::new(None)),
            executor,
            response_tx,
            response_rx,
            error_tx,
            error_rx,
            initialized: Arc::new(SyncMutex::new(false)),
            headers,
        }
    }

    /// Connect to the SSE endpoint and start listening for events
    pub async fn connect(&self) -> Result<()> {
        let mut request_builder = Request::builder()
            .method(Method::GET)
            .uri(&self.sse_endpoint)
            .header("Accept", "text/event-stream")
            .header("Cache-Control", "no-cache");

        for (key, value) in &self.headers {
            request_builder = request_builder.header(key.as_str(), value.as_str());
        }

        let request = request_builder.body(AsyncBody::empty())?;

        log::info!("[SSE Transport] Connecting to SSE endpoint: {}", self.sse_endpoint);

        let response = self.http_client.send(request).await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "SSE connection failed with status: {}",
                response.status()
            ));
        }

        // Check content type
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !content_type.contains("text/event-stream") {
            return Err(anyhow!(
                "Expected text/event-stream content type, got: {}",
                content_type
            ));
        }

        log::info!("[SSE Transport] Connected, starting event stream processing");

        // Spawn task to process SSE events
        let response_tx = self.response_tx.clone();
        let error_tx = self.error_tx.clone();
        let post_endpoint = self.post_endpoint.clone();
        let initialized = self.initialized.clone();

        self.executor
            .spawn(async move {
                if let Err(e) = Self::process_sse_stream(
                    response,
                    response_tx,
                    error_tx,
                    post_endpoint,
                    initialized,
                )
                .await
                {
                    log::error!("[SSE Transport] Stream processing error: {}", e);
                }
            })
            .detach();

        // Wait for the endpoint event (with timeout)
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);

        while start.elapsed() < timeout {
            if self.post_endpoint.lock().is_some() {
                log::info!("[SSE Transport] Received endpoint event, ready to send messages");
                return Ok(());
            }
            smol::Timer::after(std::time::Duration::from_millis(50)).await;
        }

        Err(anyhow!(
            "Timeout waiting for endpoint event from SSE server"
        ))
    }

    /// Process the SSE event stream
    async fn process_sse_stream(
        response: http_client::Response<AsyncBody>,
        response_tx: channel::Sender<String>,
        error_tx: channel::Sender<String>,
        post_endpoint: Arc<SyncMutex<Option<String>>>,
        initialized: Arc<SyncMutex<bool>>,
    ) -> Result<()> {
        let body = response.into_body();
        let reader = futures::io::BufReader::new(body);
        let mut lines = reader.lines();

        let mut current_event: Option<String> = None;
        let mut current_data: Vec<String> = Vec::new();

        while let Some(line_result) = lines.next().await {
            let line = match line_result {
                Ok(line) => line,
                Err(e) => {
                    let _ = error_tx.send(format!("SSE read error: {}", e)).await;
                    break;
                }
            };

            if line.is_empty() {
                // Empty line signals end of event
                if !current_data.is_empty() {
                    let data = current_data.join("\n");
                    let event_type = current_event.take().unwrap_or_else(|| "message".to_string());

                    match event_type.as_str() {
                        "endpoint" => {
                            // The endpoint event contains the URL for POSTing messages
                            log::info!("[SSE Transport] Received endpoint event: {}", data);
                            *post_endpoint.lock() = Some(data.trim().to_string());
                            *initialized.lock() = true;
                        }
                        "message" => {
                            // Message events contain JSON-RPC responses
                            log::debug!("[SSE Transport] Received message event: {} bytes", data.len());
                            if let Err(e) = response_tx.send(data).await {
                                log::error!("[SSE Transport] Failed to forward message: {}", e);
                                break;
                            }
                        }
                        "ping" => {
                            log::trace!("[SSE Transport] Received ping");
                        }
                        "error" => {
                            log::error!("[SSE Transport] Server error event: {}", data);
                            let _ = error_tx.send(data).await;
                        }
                        _ => {
                            log::debug!("[SSE Transport] Unknown event type '{}': {}", event_type, data);
                        }
                    }
                    current_data.clear();
                }
                current_event = None;
            } else if let Some(event_name) = line.strip_prefix("event:") {
                current_event = Some(event_name.trim().to_string());
            } else if let Some(data) = line.strip_prefix("data:") {
                current_data.push(data.trim().to_string());
            } else if line.starts_with("id:") || line.starts_with("retry:") {
                // Ignore id and retry fields
            } else if line.starts_with(":") {
                // Comment line, ignore
            }
        }

        log::info!("[SSE Transport] Stream ended");
        Ok(())
    }

    /// Send a message to the POST endpoint
    async fn send_message(&self, message: String) -> Result<()> {
        let post_endpoint = self
            .post_endpoint
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("POST endpoint not yet received from server"))?;

        log::debug!(
            "[SSE Transport] Sending message to {}: {} bytes",
            post_endpoint,
            message.len()
        );

        let mut request_builder = Request::builder()
            .method(Method::POST)
            .uri(&post_endpoint)
            .header("Content-Type", "application/json");

        for (key, value) in &self.headers {
            request_builder = request_builder.header(key.as_str(), value.as_str());
        }

        let request = request_builder.body(AsyncBody::from(message.into_bytes()))?;

        let mut response = self.http_client.send(request).await?;

        if !response.status().is_success() {
            let mut error_body = String::new();
            futures::AsyncReadExt::read_to_string(response.body_mut(), &mut error_body)
                .await
                .ok();
            return Err(anyhow!(
                "POST request failed with status {}: {}",
                response.status(),
                error_body
            ));
        }

        // For the old SSE transport, the response comes via the SSE stream,
        // not in the HTTP response body. However, some servers may include
        // a response in the body as well, so we check.
        let mut body = String::new();
        futures::AsyncReadExt::read_to_string(response.body_mut(), &mut body)
            .await
            .ok();

        if !body.is_empty() && body.trim() != "ok" && body.trim() != "accepted" {
            // If there's a non-trivial response body, forward it
            log::debug!("[SSE Transport] POST response body: {}", body);
            // Some servers return the response directly in the POST response
            // rather than via SSE, so we should handle both cases
            if body.starts_with("{") {
                self.response_tx
                    .send(body)
                    .await
                    .map_err(|e| anyhow!("Failed to forward POST response: {}", e))?;
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Transport for SseTransport {
    async fn send(&self, message: String) -> Result<()> {
        // Auto-connect on first send if not already connected
        if !*self.initialized.lock() {
            self.connect().await.context("Failed to connect to SSE endpoint")?;
        }
        self.send_message(message).await
    }

    fn receive(&self) -> Pin<Box<dyn Stream<Item = String> + Send>> {
        Box::pin(self.response_rx.clone())
    }

    fn receive_err(&self) -> Pin<Box<dyn Stream<Item = String> + Send>> {
        Box::pin(self.error_rx.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use http_client::FakeHttpClient;
    use http_client::http::Response;
    use parking_lot::Mutex as ParkingMutex;

    #[gpui::test]
    fn test_sse_transport_endpoint_event(cx: &mut TestAppContext) {
        let executor = cx.executor();

        // Simulate SSE response with endpoint event followed by a message
        let sse_response = "event: endpoint\ndata: http://localhost:3000/message\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";

        let http_client = FakeHttpClient::create(move |req| {
            let sse_response = sse_response.to_string();
            async move {
                if req.method() == Method::GET {
                    // SSE endpoint connection
                    Ok(Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(sse_response.into())
                        .unwrap())
                } else {
                    // POST endpoint
                    Ok(Response::builder()
                        .status(200)
                        .body("ok".to_string().into())
                        .unwrap())
                }
            }
        });

        let transport = Arc::new(SseTransport::new(
            http_client,
            "http://localhost:3000/sse".to_string(),
            HashMap::default(),
            executor.clone(),
        ));

        // Start connection in a spawned task
        let transport_clone = transport.clone();
        executor
            .spawn(async move {
                transport_clone.connect().await.unwrap();
            })
            .detach();

        // Drive the executor to process the async tasks
        cx.run_until_parked();

        // Verify we got the POST endpoint
        assert_eq!(
            *transport.post_endpoint.lock(),
            Some("http://localhost:3000/message".to_string())
        );

        // Check we can receive messages - the stream task should have processed the message event
        // Use executor to poll the receiver
        let transport_clone = transport.clone();
        let msg = executor.block(async move {
            let mut receiver = transport_clone.receive();
            use futures::StreamExt;
            receiver.next().await
        });

        assert!(msg.is_some());
        assert!(msg.unwrap().contains("jsonrpc"));
    }

    #[gpui::test]
    fn test_sse_transport_send_message(cx: &mut TestAppContext) {
        let executor = cx.executor();

        // SSE response with endpoint event, then a message event (simulating server response)
        let sse_response = "event: endpoint\ndata: http://localhost:3000/message\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"success\":true}}\n\n";

        let http_client = FakeHttpClient::create(move |req| {
            let sse_response = sse_response.to_string();
            async move {
                if req.method() == Method::GET {
                    Ok(Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(sse_response.into())
                        .unwrap())
                } else {
                    // POST - return simple acknowledgment (real SSE servers send response via SSE stream)
                    Ok(Response::builder()
                        .status(200)
                        .body("accepted".to_string().into())
                        .unwrap())
                }
            }
        });

        let transport = Arc::new(SseTransport::new(
            http_client,
            "http://localhost:3000/sse".to_string(),
            HashMap::default(),
            executor.clone(),
        ));

        // First, connect to the SSE endpoint
        let transport_clone = transport.clone();
        executor
            .spawn(async move {
                transport_clone.connect().await.unwrap();
            })
            .detach();

        // Drive executor to complete the connection and SSE stream processing
        cx.run_until_parked();

        // Verify connection succeeded
        assert!(transport.post_endpoint.lock().is_some());

        // Now send a message
        let transport_clone = transport.clone();
        executor
            .spawn(async move {
                transport_clone
                    .send("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"test\"}".to_string())
                    .await
                    .unwrap();
            })
            .detach();

        // Drive executor to complete the send
        cx.run_until_parked();

        // The response should be available from the SSE stream (the message event)
        let msg = transport.response_rx.try_recv().ok();

        assert!(msg.is_some());
        assert!(msg.unwrap().contains("success"));
    }

    #[gpui::test]
    fn test_sse_transport_initialize_flow(cx: &mut TestAppContext) {
        // Test the full MCP initialize flow: connect -> initialize request -> initialized notification
        let executor = cx.executor();

        // Track requests received by the mock server
        let requests_received: Arc<ParkingMutex<Vec<String>>> = Arc::new(ParkingMutex::new(Vec::new()));
        let requests_clone = requests_received.clone();

        // SSE stream: endpoint event, then initialize response
        let sse_response = concat!(
            "event: endpoint\n",
            "data: http://localhost:3000/message\n",
            "\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"test-server\",\"version\":\"1.0\"}}}\n",
            "\n"
        );

        let http_client = FakeHttpClient::create(move |req| {
            let sse_response = sse_response.to_string();
            let requests_clone = requests_clone.clone();
            async move {
                if req.method() == Method::GET {
                    Ok(Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(sse_response.into())
                        .unwrap())
                } else {
                    // Capture the POST body
                    let mut body = req.into_body();
                    let mut body_str = String::new();
                    futures::AsyncReadExt::read_to_string(&mut body, &mut body_str).await.ok();
                    requests_clone.lock().push(body_str);

                    Ok(Response::builder()
                        .status(200)
                        .body("accepted".into())
                        .unwrap())
                }
            }
        });

        let transport = Arc::new(SseTransport::new(
            http_client,
            "http://localhost:3000/sse".to_string(),
            HashMap::default(),
            executor.clone(),
        ));

        // Connect
        let transport_clone = transport.clone();
        executor
            .spawn(async move {
                transport_clone.connect().await.unwrap();
            })
            .detach();
        cx.run_until_parked();

        // Send initialize request
        let transport_clone = transport.clone();
        executor
            .spawn(async move {
                transport_clone
                    .send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"zed","version":"1.0"}}}"#.to_string())
                    .await
                    .unwrap();
            })
            .detach();
        cx.run_until_parked();

        // Verify the request was sent
        let requests = requests_received.lock();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("initialize"));
        assert!(requests[0].contains("protocolVersion"));
        drop(requests);

        // Verify we got the response
        let msg = transport.response_rx.try_recv().ok();
        assert!(msg.is_some());
        let response = msg.unwrap();
        assert!(response.contains("protocolVersion"));
        assert!(response.contains("test-server"));
    }

    #[gpui::test]
    fn test_sse_transport_with_auth_headers(cx: &mut TestAppContext) {
        // Test that custom headers (like Authorization) are sent with requests
        let executor = cx.executor();

        let headers_received: Arc<ParkingMutex<Vec<(String, String)>>> = Arc::new(ParkingMutex::new(Vec::new()));
        let headers_clone = headers_received.clone();

        let sse_response = "event: endpoint\ndata: http://localhost:3000/message\n\n";

        let http_client = FakeHttpClient::create(move |req| {
            let sse_response = sse_response.to_string();
            let headers_clone = headers_clone.clone();
            async move {
                // Capture headers
                for (name, value) in req.headers() {
                    if name.as_str() == "authorization" || name.as_str() == "x-custom-header" {
                        headers_clone.lock().push((
                            name.to_string(),
                            value.to_str().unwrap_or("").to_string(),
                        ));
                    }
                }

                if req.method() == Method::GET {
                    Ok(Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(sse_response.into())
                        .unwrap())
                } else {
                    Ok(Response::builder()
                        .status(200)
                        .body("accepted".into())
                        .unwrap())
                }
            }
        });

        let mut headers = HashMap::default();
        headers.insert("Authorization".to_string(), "Bearer test-token-123".to_string());
        headers.insert("X-Custom-Header".to_string(), "custom-value".to_string());

        let transport = Arc::new(SseTransport::new(
            http_client,
            "http://localhost:3000/sse".to_string(),
            headers,
            executor.clone(),
        ));

        // Connect (should send headers with GET request)
        let transport_clone = transport.clone();
        executor
            .spawn(async move {
                transport_clone.connect().await.unwrap();
            })
            .detach();
        cx.run_until_parked();

        // Send a message (should send headers with POST request)
        let transport_clone = transport.clone();
        executor
            .spawn(async move {
                transport_clone
                    .send(r#"{"jsonrpc":"2.0","id":1,"method":"test"}"#.to_string())
                    .await
                    .unwrap();
            })
            .detach();
        cx.run_until_parked();

        // Verify headers were sent
        let headers = headers_received.lock();
        // Should have headers from both GET and POST requests
        assert!(headers.len() >= 2, "Expected at least 2 header captures, got {}", headers.len());
        
        let auth_headers: Vec<_> = headers.iter().filter(|(k, _)| k == "authorization").collect();
        assert!(!auth_headers.is_empty(), "Authorization header should be present");
        assert!(auth_headers.iter().any(|(_, v)| v == "Bearer test-token-123"));
    }

    #[gpui::test]
    fn test_sse_transport_connection_error(cx: &mut TestAppContext) {
        // Test handling of connection errors
        let executor = cx.executor();

        let http_client = FakeHttpClient::create(move |_req| {
            async move {
                Ok(Response::builder()
                    .status(500)
                    .body("Internal Server Error".into())
                    .unwrap())
            }
        });

        let transport = Arc::new(SseTransport::new(
            http_client,
            "http://localhost:3000/sse".to_string(),
            HashMap::default(),
            executor.clone(),
        ));

        // Try to connect - should fail
        let transport_clone = transport.clone();
        let result: Arc<ParkingMutex<Option<anyhow::Result<()>>>> = Arc::new(ParkingMutex::new(None));
        let result_clone = result.clone();
        
        executor
            .spawn(async move {
                let r = transport_clone.connect().await;
                *result_clone.lock() = Some(r);
            })
            .detach();
        cx.run_until_parked();

        // Verify connection failed
        let result = result.lock();
        assert!(result.is_some());
        assert!(result.as_ref().unwrap().is_err());
    }

    #[gpui::test]
    fn test_sse_transport_wrong_content_type(cx: &mut TestAppContext) {
        // Test handling of wrong content type response
        let executor = cx.executor();

        let http_client = FakeHttpClient::create(move |_req| {
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header("content-type", "application/json")  // Wrong content type!
                    .body("{}".into())
                    .unwrap())
            }
        });

        let transport = Arc::new(SseTransport::new(
            http_client,
            "http://localhost:3000/sse".to_string(),
            HashMap::default(),
            executor.clone(),
        ));

        let transport_clone = transport.clone();
        let result: Arc<ParkingMutex<Option<anyhow::Result<()>>>> = Arc::new(ParkingMutex::new(None));
        let result_clone = result.clone();
        
        executor
            .spawn(async move {
                let r = transport_clone.connect().await;
                *result_clone.lock() = Some(r);
            })
            .detach();
        cx.run_until_parked();

        // Verify connection failed due to wrong content type
        let result = result.lock();
        assert!(result.is_some());
        let err = result.as_ref().unwrap().as_ref().unwrap_err();
        assert!(err.to_string().contains("text/event-stream"));
    }
}