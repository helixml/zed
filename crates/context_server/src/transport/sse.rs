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
    use futures::StreamExt;
    use gpui::TestAppContext;
    use http_client::FakeHttpClient;
    use http_client::http::Response;

    #[gpui::test]
    async fn test_sse_transport_endpoint_event(cx: &mut TestAppContext) {
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

        let transport = SseTransport::new(
            http_client,
            "http://localhost:3000/sse".to_string(),
            HashMap::default(),
            executor.clone(),
        );

        // Connect to SSE endpoint
        transport.connect().await.unwrap();

        // Verify we got the POST endpoint
        assert_eq!(
            *transport.post_endpoint.lock(),
            Some("http://localhost:3000/message".to_string())
        );

        // Check we can receive messages
        let mut receiver = transport.receive();
        let msg = receiver.next().await;
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("jsonrpc"));
    }

    #[gpui::test]
    async fn test_sse_transport_send_message(cx: &mut TestAppContext) {
        let executor = cx.executor();
        
        let sse_response = "event: endpoint\ndata: http://localhost:3000/message\n\n";
        
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
                    // POST - return a JSON-RPC response
                    Ok(Response::builder()
                        .status(200)
                        .body("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}".to_string().into())
                        .unwrap())
                }
            }
        });

        let transport = SseTransport::new(
            http_client,
            "http://localhost:3000/sse".to_string(),
            HashMap::default(),
            executor.clone(),
        );

        // Send a message (this will auto-connect)
        transport
            .send("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}".to_string())
            .await
            .unwrap();

        // The response should be available
        let mut receiver = transport.receive();
        let msg = receiver.next().await;
        assert!(msg.is_some());
    }
}