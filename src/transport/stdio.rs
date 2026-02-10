//! Stdio transport for MCP
//!
//! Reads JSON-RPC messages from stdin and writes responses to stdout.
//! Uses line-delimited JSON format.
//!
//! # Bidirectional Support
//!
//! The [`BidirectionalStdioTransport`] enables server-to-client requests like
//! sampling (LLM requests). It multiplexes the stdio streams to handle both
//! incoming requests and outgoing requests/responses.
//!
//! # Server Notifications
//!
//! [`StdioTransport`] and [`BidirectionalStdioTransport`] automatically set up
//! notification channels and forward server notifications (progress, logging,
//! resource/tool/prompt list changes) to stdout as JSON-RPC notifications.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot};

use crate::context::{
    ChannelClientRequester, ClientRequesterHandle, NotificationReceiver, OutgoingRequest,
    OutgoingRequestReceiver, ServerNotification, notification_channel, outgoing_request_channel,
};
use tower_service::Service;

use crate::error::{Error, Result};
use crate::jsonrpc::JsonRpcService;
use crate::protocol::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, JsonRpcResponseMessage,
    McpNotification, RequestId, notifications,
};
use crate::router::{McpRouter, RouterRequest, RouterResponse};

// ============================================================================
// Shared helpers
// ============================================================================

/// Process a single line of JSON-RPC input
///
/// Returns `Ok(Some(response))` for requests, `Ok(None)` for notifications.
async fn process_line(
    service: &mut JsonRpcService<McpRouter>,
    router: &McpRouter,
    line: &str,
) -> Result<Option<JsonRpcResponseMessage>> {
    // Check if it's a notification (no id field)
    let parsed: serde_json::Value = serde_json::from_str(line)?;
    if parsed.get("id").is_none()
        && let Ok(notification) = serde_json::from_str::<JsonRpcNotification>(line)
    {
        handle_notification(router, notification)?;
        return Ok(None);
    }

    // Parse and process as a request (single or batch)
    let message: JsonRpcMessage = serde_json::from_str(line)?;
    let response = service.call_message(message).await?;
    Ok(Some(response))
}

/// Handle a JSON-RPC notification
fn handle_notification(router: &McpRouter, notification: JsonRpcNotification) -> Result<()> {
    let mcp_notification = McpNotification::from_jsonrpc(&notification)?;
    router.handle_notification(mcp_notification);
    Ok(())
}

/// Serialize a server notification to a JSON-RPC notification string.
pub(crate) fn serialize_notification(notification: &ServerNotification) -> Option<String> {
    match notification {
        ServerNotification::Progress(params) => {
            let notif = JsonRpcNotification::new(notifications::PROGRESS)
                .with_params(serde_json::to_value(params).unwrap_or_default());
            serde_json::to_string(&notif).ok()
        }
        ServerNotification::LogMessage(params) => {
            let notif = JsonRpcNotification::new(notifications::MESSAGE)
                .with_params(serde_json::to_value(params).unwrap_or_default());
            serde_json::to_string(&notif).ok()
        }
        ServerNotification::ResourceUpdated { uri } => {
            let notif = JsonRpcNotification::new(notifications::RESOURCE_UPDATED)
                .with_params(serde_json::json!({ "uri": uri }));
            serde_json::to_string(&notif).ok()
        }
        ServerNotification::ResourcesListChanged => {
            let notif = JsonRpcNotification::new(notifications::RESOURCES_LIST_CHANGED);
            serde_json::to_string(&notif).ok()
        }
        ServerNotification::ToolsListChanged => {
            let notif = JsonRpcNotification::new(notifications::TOOLS_LIST_CHANGED);
            serde_json::to_string(&notif).ok()
        }
        ServerNotification::PromptsListChanged => {
            let notif = JsonRpcNotification::new(notifications::PROMPTS_LIST_CHANGED);
            serde_json::to_string(&notif).ok()
        }
    }
}

/// Write a line to an async stdout writer and flush.
async fn write_line_to_stdout(stdout: &mut tokio::io::Stdout, line: &str) -> Result<()> {
    stdout
        .write_all(line.as_bytes())
        .await
        .map_err(|e| Error::Transport(format!("Failed to write to stdout: {}", e)))?;
    stdout
        .write_all(b"\n")
        .await
        .map_err(|e| Error::Transport(format!("Failed to write newline: {}", e)))?;
    stdout
        .flush()
        .await
        .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
    Ok(())
}

// ============================================================================
// Async stdio transport
// ============================================================================

/// Stdio transport for MCP servers
///
/// Reads JSON-RPC messages from stdin and writes responses to stdout.
/// Supports both single requests and batch requests.
///
/// Server notifications (progress, logging, resource/tool/prompt list changes)
/// are automatically forwarded to stdout as JSON-RPC notifications.
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::{BoxError, McpRouter, StdioTransport};
///
/// #[tokio::main]
/// async fn main() -> Result<(), BoxError> {
///     let router = McpRouter::new()
///         .server_info("my-server", "1.0.0");
///
///     let mut transport = StdioTransport::new(router);
///     transport.run().await?;
///     Ok(())
/// }
/// ```
pub struct StdioTransport {
    service: JsonRpcService<McpRouter>,
    router: McpRouter,
    notification_rx: NotificationReceiver,
}

impl StdioTransport {
    /// Create a new stdio transport wrapping an MCP router
    pub fn new(router: McpRouter) -> Self {
        let (notif_tx, notification_rx) = notification_channel(256);
        let router = router.with_notification_sender(notif_tx);
        let service = JsonRpcService::new(router.clone());
        Self {
            service,
            router,
            notification_rx,
        }
    }

    /// Run the transport, processing messages until EOF or error
    pub async fn run(&mut self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut reader = BufReader::new(stdin);

        tracing::info!("Stdio transport started, waiting for input");

        loop {
            let mut line = String::new();

            tokio::select! {
                // Handle incoming messages from stdin
                result = reader.read_line(&mut line) => {
                    let bytes_read = result.map_err(|e| {
                        Error::Transport(format!("Failed to read from stdin: {}", e))
                    })?;

                    if bytes_read == 0 {
                        // EOF
                        tracing::info!("Stdin closed, shutting down");
                        break;
                    }

                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    tracing::debug!(input = %trimmed, "Received message");

                    match process_line(&mut self.service, &self.router, trimmed).await {
                        Ok(Some(response)) => {
                            let response_json = serde_json::to_string(&response).map_err(|e| {
                                Error::Transport(format!("Failed to serialize response: {}", e))
                            })?;
                            tracing::debug!(output = %response_json, "Sending response");
                            write_line_to_stdout(&mut stdout, &response_json).await?;
                        }
                        Ok(None) => {
                            // Notification, no response needed
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Error processing message");
                            let error_response = JsonRpcResponse::error(
                                None,
                                crate::error::JsonRpcError::parse_error(e.to_string()),
                            );
                            let response_json = serde_json::to_string(&error_response).map_err(|e| {
                                Error::Transport(format!("Failed to serialize error: {}", e))
                            })?;
                            write_line_to_stdout(&mut stdout, &response_json).await?;
                        }
                    }
                }

                // Forward server notifications to stdout
                Some(notification) = self.notification_rx.recv() => {
                    if let Some(json) = serialize_notification(&notification) {
                        tracing::debug!(output = %json, "Sending notification");
                        write_line_to_stdout(&mut stdout, &json).await?;
                    }
                }
            }
        }

        Ok(())
    }
}

// ============================================================================
// Generic stdio transport for middleware-wrapped services
// ============================================================================

/// Generic stdio transport that works with any tower service.
///
/// This transport accepts a middleware-wrapped service instead of an `McpRouter`
/// directly. Use this when you want to apply tower middleware layers like
/// rate limiting or bulkhead patterns.
///
/// # Server Notifications
///
/// Use [`GenericStdioTransport::with_notifications`] to enable server notification
/// forwarding. Without it, notifications from the router will not reach the client.
///
/// # Example
///
/// ```rust,no_run
/// use std::time::Duration;
/// use tower::ServiceBuilder;
/// use tower::timeout::TimeoutLayer;
/// use tower_mcp::{BoxError, CatchError, McpRouter, GenericStdioTransport};
/// use tower_mcp::context::notification_channel;
///
/// #[tokio::main]
/// async fn main() -> Result<(), BoxError> {
///     // Set up notification channel before wrapping in middleware
///     let (notif_tx, notif_rx) = notification_channel(256);
///     let router = McpRouter::new()
///         .server_info("my-server", "1.0.0")
///         .with_notification_sender(notif_tx);
///
///     let service = CatchError::new(
///         ServiceBuilder::new()
///             .layer(TimeoutLayer::new(Duration::from_secs(5)))
///             .concurrency_limit(10)
///             .service(router),
///     );
///
///     let mut transport = GenericStdioTransport::with_notifications(service, notif_rx);
///     transport.run().await?;
///     Ok(())
/// }
/// ```
pub struct GenericStdioTransport<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    service: JsonRpcService<S>,
    notification_rx: Option<NotificationReceiver>,
}

impl<S> GenericStdioTransport<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    /// Create a new generic stdio transport wrapping any compatible service.
    ///
    /// The service must implement `Service<RouterRequest, Response = RouterResponse>`.
    /// This is typically an `McpRouter` wrapped in tower middleware layers.
    ///
    /// **Note:** This constructor does not set up notification forwarding. Server
    /// notifications (progress, logging, list changes) will not reach the client.
    /// Use [`GenericStdioTransport::with_notifications`] instead to enable them.
    pub fn new(service: S) -> Self {
        Self {
            service: JsonRpcService::new(service),
            notification_rx: None,
        }
    }

    /// Create a new generic stdio transport with notification forwarding.
    ///
    /// Pass a `NotificationReceiver` from [`notification_channel()`] to enable
    /// server notifications. Make sure to also call
    /// `router.with_notification_sender(tx)` before wrapping the router in middleware.
    ///
    /// [`notification_channel()`]: crate::context::notification_channel
    pub fn with_notifications(service: S, notification_rx: NotificationReceiver) -> Self {
        Self {
            service: JsonRpcService::new(service),
            notification_rx: Some(notification_rx),
        }
    }

    /// Run the transport, processing messages until EOF or error.
    pub async fn run(&mut self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut reader = BufReader::new(stdin);

        tracing::info!("Generic stdio transport started, waiting for input");

        loop {
            let mut line = String::new();

            // Use select! if we have a notification receiver, otherwise just read
            if let Some(ref mut notif_rx) = self.notification_rx {
                tokio::select! {
                    result = reader.read_line(&mut line) => {
                        let bytes_read = result.map_err(|e| {
                            Error::Transport(format!("Failed to read from stdin: {}", e))
                        })?;

                        if bytes_read == 0 {
                            tracing::info!("Stdin closed, shutting down");
                            break;
                        }

                        self.process_input(&line, &mut stdout).await?;
                    }

                    Some(notification) = notif_rx.recv() => {
                        if let Some(json) = serialize_notification(&notification) {
                            tracing::debug!(output = %json, "Sending notification");
                            write_line_to_stdout(&mut stdout, &json).await?;
                        }
                    }
                }
            } else {
                let bytes_read = reader
                    .read_line(&mut line)
                    .await
                    .map_err(|e| Error::Transport(format!("Failed to read from stdin: {}", e)))?;

                if bytes_read == 0 {
                    tracing::info!("Stdin closed, shutting down");
                    break;
                }

                self.process_input(&line, &mut stdout).await?;
            }
        }

        Ok(())
    }

    async fn process_input(&mut self, line: &str, stdout: &mut tokio::io::Stdout) -> Result<()> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(());
        }

        tracing::debug!(input = %trimmed, "Received message");

        // Check if it's a notification (no id field)
        let parsed: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                self.write_error(stdout, None, &e.to_string()).await?;
                return Ok(());
            }
        };

        if parsed.get("id").is_none() {
            // Notification - log and ignore since we don't have router access
            tracing::debug!(
                method = parsed.get("method").and_then(|m| m.as_str()),
                "Received notification (ignored in generic transport)"
            );
            return Ok(());
        }

        // Parse and process as a request (single or batch)
        let message: JsonRpcMessage = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(e) => {
                self.write_error(stdout, None, &e.to_string()).await?;
                return Ok(());
            }
        };

        match self.service.call_message(message).await {
            Ok(response) => {
                let response_json = serde_json::to_string(&response).map_err(|e| {
                    Error::Transport(format!("Failed to serialize response: {}", e))
                })?;
                tracing::debug!(output = %response_json, "Sending response");
                write_line_to_stdout(stdout, &response_json).await?;
            }
            Err(e) => {
                tracing::error!(error = %e, "Error processing message");
                self.write_error(stdout, None, &e.to_string()).await?;
            }
        }
        Ok(())
    }

    async fn write_error(
        &self,
        stdout: &mut tokio::io::Stdout,
        id: Option<crate::protocol::RequestId>,
        message: &str,
    ) -> Result<()> {
        let error_response =
            JsonRpcResponse::error(id, crate::error::JsonRpcError::parse_error(message));
        let response_json = serde_json::to_string(&error_response)
            .map_err(|e| Error::Transport(format!("Failed to serialize error: {}", e)))?;
        write_line_to_stdout(stdout, &response_json).await
    }
}

// ============================================================================
// Synchronous stdio transport
// ============================================================================

/// Synchronous stdio transport for simpler use cases
///
/// This version uses blocking I/O and is suitable for simple CLI tools.
///
/// **Note:** This transport does not support server notification forwarding
/// (progress, logging, list changes) because it uses blocking I/O. Use
/// [`StdioTransport`] for full notification support.
pub struct SyncStdioTransport {
    service: JsonRpcService<McpRouter>,
    router: McpRouter,
}

impl SyncStdioTransport {
    /// Create a new synchronous stdio transport
    pub fn new(router: McpRouter) -> Self {
        let service = JsonRpcService::new(router.clone());
        Self { service, router }
    }

    /// Run the transport synchronously using a tokio runtime
    pub fn run_blocking(&mut self) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| Error::Transport(format!("Failed to create runtime: {}", e)))?;

        let stdin = io::stdin();
        let mut stdout = io::stdout();

        tracing::info!("Sync stdio transport started");

        for line in stdin.lock().lines() {
            let line =
                line.map_err(|e| Error::Transport(format!("Failed to read from stdin: {}", e)))?;

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            tracing::debug!(input = %trimmed, "Received message");

            match rt.block_on(process_line(&mut self.service, &self.router, trimmed)) {
                Ok(Some(response)) => {
                    let response_json = serde_json::to_string(&response).map_err(|e| {
                        Error::Transport(format!("Failed to serialize response: {}", e))
                    })?;
                    tracing::debug!(output = %response_json, "Sending response");
                    writeln!(stdout, "{}", response_json).map_err(|e| {
                        Error::Transport(format!("Failed to write to stdout: {}", e))
                    })?;
                    stdout
                        .flush()
                        .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
                }
                Ok(None) => {
                    // Notification, no response
                }
                Err(e) => {
                    tracing::error!(error = %e, "Error processing message");
                    let error_response = JsonRpcResponse::error(
                        None,
                        crate::error::JsonRpcError::parse_error(e.to_string()),
                    );
                    let response_json = serde_json::to_string(&error_response).map_err(|e| {
                        Error::Transport(format!("Failed to serialize error: {}", e))
                    })?;
                    writeln!(stdout, "{}", response_json)
                        .map_err(|e| Error::Transport(format!("Failed to write error: {}", e)))?;
                    stdout
                        .flush()
                        .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
                }
            }
        }

        tracing::info!("Stdin closed, shutting down");
        Ok(())
    }
}

// ============================================================================
// Bidirectional stdio transport (with sampling support)
// ============================================================================

/// Pending request waiting for a response
struct PendingRequest {
    response_tx: oneshot::Sender<Result<serde_json::Value>>,
}

/// Bidirectional stdio transport with sampling support
///
/// This transport supports both incoming requests from clients and outgoing
/// requests to clients (for sampling/LLM requests). It multiplexes stdin/stdout
/// to handle the bidirectional communication.
///
/// Server notifications (progress, logging, resource/tool/prompt list changes)
/// are automatically forwarded to stdout as JSON-RPC notifications.
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::{BoxError, McpRouter, ToolBuilder, CallToolResult};
/// use tower_mcp::transport::stdio::BidirectionalStdioTransport;
/// use tower_mcp::{CreateMessageParams, SamplingMessage};
/// use tower_mcp::extract::{Context, RawArgs};
///
/// #[tokio::main]
/// async fn main() -> Result<(), BoxError> {
///     let tool = ToolBuilder::new("ai-tool")
///         .description("A tool that uses LLM")
///         .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
///             // Request LLM completion from the client
///             let params = CreateMessageParams::new(
///                 vec![SamplingMessage::user("Help me with: ...")],
///                 500,
///             );
///             let result = ctx.sample(params).await?;
///             Ok(CallToolResult::text(format!("{:?}", result.content)))
///         })
///         .build();
///
///     let router = McpRouter::new()
///         .server_info("my-server", "1.0.0")
///         .tool(tool);
///
///     let mut transport = BidirectionalStdioTransport::new(router);
///     transport.run().await?;
///     Ok(())
/// }
/// ```
pub struct BidirectionalStdioTransport {
    service: JsonRpcService<McpRouter>,
    router: McpRouter,
    /// Channel for receiving outgoing requests to send to the client
    request_rx: OutgoingRequestReceiver,
    /// Handle for handlers to send requests to the client
    client_requester: ClientRequesterHandle,
    /// Pending requests waiting for responses
    pending_requests: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    /// Channel for receiving server notifications to forward to the client
    notification_rx: NotificationReceiver,
}

impl BidirectionalStdioTransport {
    /// Create a new bidirectional stdio transport
    pub fn new(router: McpRouter) -> Self {
        let (request_tx, request_rx) = outgoing_request_channel(32);
        let client_requester: ClientRequesterHandle =
            Arc::new(ChannelClientRequester::new(request_tx));

        let (notif_tx, notification_rx) = notification_channel(256);
        let router = router.with_notification_sender(notif_tx);

        let service = JsonRpcService::new(router.clone());

        Self {
            service,
            router,
            request_rx,
            client_requester,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            notification_rx,
        }
    }

    /// Get the client requester handle
    ///
    /// Use this to configure the router's request context to enable sampling.
    pub fn client_requester(&self) -> ClientRequesterHandle {
        self.client_requester.clone()
    }

    /// Run the transport, processing messages until EOF or error
    pub async fn run(&mut self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        let mut reader = BufReader::new(stdin);

        tracing::info!("Bidirectional stdio transport started, waiting for input");

        loop {
            let mut line = String::new();

            tokio::select! {
                // Handle incoming messages from stdin
                result = reader.read_line(&mut line) => {
                    let bytes_read = result.map_err(|e| {
                        Error::Transport(format!("Failed to read from stdin: {}", e))
                    })?;

                    if bytes_read == 0 {
                        tracing::info!("Stdin closed, shutting down");
                        break;
                    }

                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    self.handle_incoming_message(trimmed, stdout.clone()).await?;
                }

                // Handle outgoing requests to send to the client
                Some(outgoing) = self.request_rx.recv() => {
                    self.send_outgoing_request(outgoing, stdout.clone()).await?;
                }

                // Forward server notifications to the client
                Some(notification) = self.notification_rx.recv() => {
                    if let Some(json) = serialize_notification(&notification) {
                        tracing::debug!(output = %json, "Sending notification");
                        self.write_line(&json, stdout.clone()).await?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle an incoming message from stdin
    async fn handle_incoming_message(
        &mut self,
        line: &str,
        stdout: Arc<Mutex<tokio::io::Stdout>>,
    ) -> Result<()> {
        tracing::debug!(input = %line, "Received message");

        let parsed: serde_json::Value = serde_json::from_str(line)?;

        // Check if this is a response to one of our pending requests
        if parsed.get("method").is_none()
            && (parsed.get("result").is_some() || parsed.get("error").is_some())
        {
            return self.handle_response(&parsed).await;
        }

        // Check if it's a notification (no id field)
        if parsed.get("id").is_none() {
            if let Ok(notification) = serde_json::from_str::<JsonRpcNotification>(line) {
                handle_notification(&self.router, notification)?;
            }
            return Ok(());
        }

        // Process as a request
        let message: JsonRpcMessage = serde_json::from_str(line)?;
        match self.service.call_message(message).await {
            Ok(response) => {
                let response_json = serde_json::to_string(&response).map_err(|e| {
                    Error::Transport(format!("Failed to serialize response: {}", e))
                })?;
                tracing::debug!(output = %response_json, "Sending response");
                self.write_line(&response_json, stdout).await?;
            }
            Err(e) => {
                tracing::error!(error = %e, "Error processing message");
                let error_response = JsonRpcResponse::error(
                    None,
                    crate::error::JsonRpcError::parse_error(e.to_string()),
                );
                let response_json = serde_json::to_string(&error_response)
                    .map_err(|e| Error::Transport(format!("Failed to serialize error: {}", e)))?;
                self.write_line(&response_json, stdout).await?;
            }
        }

        Ok(())
    }

    /// Handle a response to one of our pending requests
    async fn handle_response(&self, parsed: &serde_json::Value) -> Result<()> {
        let id = match parsed.get("id") {
            Some(id) => {
                if let Some(n) = id.as_i64() {
                    RequestId::Number(n)
                } else if let Some(s) = id.as_str() {
                    RequestId::String(s.to_string())
                } else {
                    tracing::warn!("Response has invalid id type");
                    return Ok(());
                }
            }
            None => {
                tracing::warn!("Response missing id field");
                return Ok(());
            }
        };

        let pending = {
            let mut pending_requests = self.pending_requests.lock().await;
            pending_requests.remove(&id)
        };

        match pending {
            Some(pending) => {
                let result = if let Some(error) = parsed.get("error") {
                    let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                    let message = error
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("Unknown error");
                    Err(Error::Internal(format!(
                        "Client error ({}): {}",
                        code, message
                    )))
                } else if let Some(result) = parsed.get("result") {
                    Ok(result.clone())
                } else {
                    Err(Error::Internal(
                        "Response has neither result nor error".to_string(),
                    ))
                };

                // Send result to waiter (ignore if they've dropped the receiver)
                let _ = pending.response_tx.send(result);
            }
            None => {
                tracing::warn!(id = ?id, "Received response for unknown request");
            }
        }

        Ok(())
    }

    /// Send an outgoing request to the client
    async fn send_outgoing_request(
        &mut self,
        outgoing: OutgoingRequest,
        stdout: Arc<Mutex<tokio::io::Stdout>>,
    ) -> Result<()> {
        // Build JSON-RPC request
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: outgoing.id.clone(),
            method: outgoing.method,
            params: Some(outgoing.params),
        };

        let request_json = serde_json::to_string(&request)
            .map_err(|e| Error::Transport(format!("Failed to serialize request: {}", e)))?;

        tracing::debug!(output = %request_json, "Sending request to client");

        // Store pending request
        {
            let mut pending_requests = self.pending_requests.lock().await;
            pending_requests.insert(
                outgoing.id,
                PendingRequest {
                    response_tx: outgoing.response_tx,
                },
            );
        }

        // Send the request
        self.write_line(&request_json, stdout).await?;

        Ok(())
    }

    /// Write a line to stdout
    async fn write_line(&self, line: &str, stdout: Arc<Mutex<tokio::io::Stdout>>) -> Result<()> {
        let mut stdout = stdout.lock().await;
        stdout
            .write_all(line.as_bytes())
            .await
            .map_err(|e| Error::Transport(format!("Failed to write to stdout: {}", e)))?;
        stdout
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Transport(format!("Failed to write newline: {}", e)))?;
        stdout
            .flush()
            .await
            .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
        Ok(())
    }
}
