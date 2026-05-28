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
use crate::transport::service::{CatchError, InjectAnnotations};

// ============================================================================
// Shared helpers
// ============================================================================

/// Strip an optional UTF-8 BOM, then trim whitespace.
///
/// Windows tools sometimes prefix the first stdout line with a UTF-8 BOM
/// (`\u{feff}`). Without stripping it, the JSON parser sees an unexpected
/// character at offset 0 and rejects the whole message.
fn clean_input_line(line: &str) -> &str {
    line.strip_prefix('\u{feff}').unwrap_or(line).trim()
}

/// Build a JSON-RPC parse-error response from a parser/dispatch error message.
///
/// Per JSON-RPC 2.0, a parse error sets `code` to `-32700` and `id` to
/// `null` (the request id cannot be recovered from unparseable input).
/// Returning a single shared constructor keeps every stdio parse-error
/// path consistent and gives the wire-format tests in
/// [`tower_mcp_types::testing`] one stable surface to assert against.
pub(crate) fn parse_error_response(message: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse::error(None, crate::error::JsonRpcError::parse_error(message))
}

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
        ServerNotification::TaskStatusChanged(params) => {
            let notif = JsonRpcNotification::new(notifications::TASK_STATUS_CHANGED)
                .with_params(serde_json::to_value(params).unwrap_or_default());
            serde_json::to_string(&notif).ok()
        }
    }
}

/// Write a line to an async writer and flush.
async fn write_line_to_stdout<W>(stdout: &mut W, line: &str) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
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

    /// Apply a tower middleware layer to this transport.
    ///
    /// This converts the `StdioTransport` into a [`GenericStdioTransport`] with
    /// the middleware applied, while preserving notification forwarding.
    ///
    /// Use [`tower::ServiceBuilder`] to compose multiple layers:
    ///
    /// ```rust,no_run
    /// use std::time::Duration;
    /// use tower::ServiceBuilder;
    /// use tower::timeout::TimeoutLayer;
    /// use tower_mcp::{BoxError, McpRouter, StdioTransport};
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), BoxError> {
    ///     let router = McpRouter::new().server_info("my-server", "1.0.0");
    ///
    ///     let mut transport = StdioTransport::new(router)
    ///         .layer(
    ///             ServiceBuilder::new()
    ///                 .layer(TimeoutLayer::new(Duration::from_secs(5)))
    ///                 .concurrency_limit(10)
    ///                 .into_inner(),
    ///         );
    ///
    ///     transport.run().await?;
    ///     Ok(())
    /// }
    /// ```
    pub fn layer<L>(
        self,
        layer: L,
    ) -> GenericStdioTransport<InjectAnnotations<CatchError<L::Service>>>
    where
        L: tower::Layer<McpRouter>,
        L::Service: Service<RouterRequest, Response = RouterResponse> + Clone + Send + 'static,
        <L::Service as Service<RouterRequest>>::Error: std::fmt::Display + Send,
        <L::Service as Service<RouterRequest>>::Future: Send,
    {
        let annotations = self.router.tool_annotations_map();
        let wrapped = layer.layer(self.router);
        let service = InjectAnnotations::new(CatchError::new(wrapped), annotations);
        GenericStdioTransport::with_notifications(service, self.notification_rx)
    }

    /// Run the transport, processing messages until EOF or error
    ///
    /// This is a thin wrapper around [`Self::run_with_streams`] that wires up
    /// `tokio::io::stdin()` and `tokio::io::stdout()`. Most users want this
    /// method; use [`Self::run_with_streams`] only for in-process testing.
    pub async fn run(&mut self) -> Result<()> {
        self.run_with_streams(tokio::io::stdin(), tokio::io::stdout())
            .await
    }

    /// Run the transport, reading from `reader` and writing to `writer`.
    ///
    /// This is the streams-generic counterpart of [`Self::run`]. The default
    /// `run()` calls this with `tokio::io::stdin()` / `tokio::io::stdout()`.
    ///
    /// Exposing this lets tests drive the full read-eval-write loop with
    /// `tokio::io::duplex()` and assert end-to-end behavior (parse-error
    /// frames, loop continuation across bad input, EOF handling).
    pub async fn run_with_streams<R, W>(&mut self, reader: R, mut writer: W) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send,
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        let mut reader = BufReader::new(reader);

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

                    let trimmed = clean_input_line(&line);
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
                            write_line_to_stdout(&mut writer, &response_json).await?;
                        }
                        Ok(None) => {
                            // Notification, no response needed
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Error processing message");
                            let error_response = parse_error_response(e.to_string());
                            let response_json = serde_json::to_string(&error_response).map_err(|e| {
                                Error::Transport(format!("Failed to serialize error: {}", e))
                            })?;
                            write_line_to_stdout(&mut writer, &response_json).await?;
                        }
                    }
                }

                // Forward server notifications to stdout
                Some(notification) = self.notification_rx.recv() => {
                    if let Some(json) = serialize_notification(&notification) {
                        tracing::debug!(output = %json, "Sending notification");
                        write_line_to_stdout(&mut writer, &json).await?;
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
    ///
    /// Thin wrapper around [`Self::run_with_streams`] that wires up
    /// `tokio::io::stdin()` / `tokio::io::stdout()`.
    pub async fn run(&mut self) -> Result<()> {
        self.run_with_streams(tokio::io::stdin(), tokio::io::stdout())
            .await
    }

    /// Run the transport, reading from `reader` and writing to `writer`.
    ///
    /// Streams-generic counterpart of [`Self::run`]. Lets tests drive the
    /// read-eval-write loop with in-memory streams (e.g. `tokio::io::duplex()`).
    pub async fn run_with_streams<R, W>(&mut self, reader: R, mut writer: W) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send,
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        let mut reader = BufReader::new(reader);

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

                        Self::process_input(&mut self.service, &line, &mut writer).await?;
                    }

                    Some(notification) = notif_rx.recv() => {
                        if let Some(json) = serialize_notification(&notification) {
                            tracing::debug!(output = %json, "Sending notification");
                            write_line_to_stdout(&mut writer, &json).await?;
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

                Self::process_input(&mut self.service, &line, &mut writer).await?;
            }
        }

        Ok(())
    }

    async fn process_input<W>(
        service: &mut JsonRpcService<S>,
        line: &str,
        writer: &mut W,
    ) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        let trimmed = clean_input_line(line);
        if trimmed.is_empty() {
            return Ok(());
        }

        tracing::debug!(input = %trimmed, "Received message");

        // Check if it's a notification (no id field)
        let parsed: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                Self::write_error(writer, None, &e.to_string()).await?;
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
                Self::write_error(writer, None, &e.to_string()).await?;
                return Ok(());
            }
        };

        match service.call_message(message).await {
            Ok(response) => {
                let response_json = serde_json::to_string(&response).map_err(|e| {
                    Error::Transport(format!("Failed to serialize response: {}", e))
                })?;
                tracing::debug!(output = %response_json, "Sending response");
                write_line_to_stdout(writer, &response_json).await?;
            }
            Err(e) => {
                tracing::error!(error = %e, "Error processing message");
                Self::write_error(writer, None, &e.to_string()).await?;
            }
        }
        Ok(())
    }

    async fn write_error<W>(
        writer: &mut W,
        id: Option<crate::protocol::RequestId>,
        message: &str,
    ) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        // `id` is currently always `None` from every call site (parse-error
        // path), so use the shared helper; preserve the parameter for callers
        // that may want to surface a known-id error in future.
        let error_response = if let Some(id) = id {
            JsonRpcResponse::error(Some(id), crate::error::JsonRpcError::parse_error(message))
        } else {
            parse_error_response(message)
        };
        let response_json = serde_json::to_string(&error_response)
            .map_err(|e| Error::Transport(format!("Failed to serialize error: {}", e)))?;
        write_line_to_stdout(writer, &response_json).await
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

            let trimmed = clean_input_line(&line);
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
                    let error_response = parse_error_response(e.to_string());
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
    ///
    /// Thin wrapper around [`Self::run_with_streams`] that wires up
    /// `tokio::io::stdin()` / `tokio::io::stdout()`.
    pub async fn run(&mut self) -> Result<()> {
        self.run_with_streams(tokio::io::stdin(), tokio::io::stdout())
            .await
    }

    /// Run the transport, reading from `reader` and writing to `writer`.
    ///
    /// Streams-generic counterpart of [`Self::run`]. The writer is held
    /// behind an `Arc<Mutex<_>>` so the outgoing-request and notification
    /// paths can share it with the incoming-message branch -- the same
    /// concurrency model `run()` has always used, just with the streams
    /// supplied by the caller.
    pub async fn run_with_streams<R, W>(&mut self, reader: R, writer: W) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let writer = Arc::new(Mutex::new(writer));
        let mut reader = BufReader::new(reader);

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

                    let trimmed = clean_input_line(&line);
                    if trimmed.is_empty() {
                        continue;
                    }

                    self.handle_incoming_message(trimmed, writer.clone()).await?;
                }

                // Handle outgoing requests to send to the client
                Some(outgoing) = self.request_rx.recv() => {
                    self.send_outgoing_request(outgoing, writer.clone()).await?;
                }

                // Forward server notifications to the client
                Some(notification) = self.notification_rx.recv() => {
                    if let Some(json) = serialize_notification(&notification) {
                        tracing::debug!(output = %json, "Sending notification");
                        self.write_line(&json, writer.clone()).await?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle an incoming message from stdin
    async fn handle_incoming_message<W>(&mut self, line: &str, writer: Arc<Mutex<W>>) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        tracing::debug!(input = %line, "Received message");

        // Malformed JSON must produce a JSON-RPC parse error response, not
        // tear down the run loop. Per the spec, id is null when the request
        // can't be parsed at all.
        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "Malformed JSON on stdin");
                return self.write_parse_error(&e.to_string(), writer).await;
            }
        };

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

        // Process as a request. The shape parse can also fail (e.g. id of
        // wrong type); treat it the same way so the loop keeps running.
        let message: JsonRpcMessage = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "JSON did not match JSON-RPC request shape");
                return self.write_parse_error(&e.to_string(), writer).await;
            }
        };
        match self.service.call_message(message).await {
            Ok(response) => {
                let response_json = serde_json::to_string(&response).map_err(|e| {
                    Error::Transport(format!("Failed to serialize response: {}", e))
                })?;
                tracing::debug!(output = %response_json, "Sending response");
                self.write_line(&response_json, writer).await?;
            }
            Err(e) => {
                tracing::error!(error = %e, "Error processing message");
                let error_response = parse_error_response(e.to_string());
                let response_json = serde_json::to_string(&error_response)
                    .map_err(|e| Error::Transport(format!("Failed to serialize error: {}", e)))?;
                self.write_line(&response_json, writer).await?;
            }
        }

        Ok(())
    }

    async fn write_parse_error<W>(&self, message: &str, writer: Arc<Mutex<W>>) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        let error_response = parse_error_response(message);
        let response_json = serde_json::to_string(&error_response)
            .map_err(|e| Error::Transport(format!("Failed to serialize error: {}", e)))?;
        self.write_line(&response_json, writer).await
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
    async fn send_outgoing_request<W>(
        &mut self,
        outgoing: OutgoingRequest,
        writer: Arc<Mutex<W>>,
    ) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
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
        self.write_line(&request_json, writer).await?;

        Ok(())
    }

    /// Write a line to the shared writer
    async fn write_line<W>(&self, line: &str, writer: Arc<Mutex<W>>) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        let mut writer = writer.lock().await;
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| Error::Transport(format!("Failed to write to stdout: {}", e)))?;
        writer
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Transport(format!("Failed to write newline: {}", e)))?;
        writer
            .flush()
            .await
            .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ServerNotification;
    use crate::protocol::{
        LogLevel, LoggingMessageParams, ProgressParams, ProgressToken, TaskStatus, TaskStatusParams,
    };
    use tower_mcp_types::testing::assert_jsonrpc_error_response;

    // =========================================================================
    // parse_error_response tests -- wire-format invariants on the stdio
    // parse-error path (regression coverage for #802 / #803).
    // =========================================================================

    #[test]
    fn parse_error_response_has_null_id_and_code_neg_32700() {
        let resp = parse_error_response("expected value at line 1");
        let json = serde_json::to_value(&resp).unwrap();
        assert_jsonrpc_error_response(&json);
        assert!(
            json["id"].is_null(),
            "id must be null on parse error, got: {json}"
        );
        assert_eq!(json["error"]["code"].as_i64().unwrap(), -32700);
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("expected value"),
            "error.message should carry the parser detail, got: {json}"
        );
    }

    #[test]
    fn parse_error_response_serializes_to_single_line_json() {
        // The stdio loop writes responses line-delimited; the body itself
        // must not contain embedded newlines or it would split the frame.
        let resp = parse_error_response("oops\nstill oops");
        let s = serde_json::to_string(&resp).unwrap();
        assert!(
            !s.contains('\n'),
            "serialized parse-error response must be single-line, got: {s:?}"
        );
    }

    // =========================================================================
    // serialize_notification tests
    // =========================================================================

    #[test]
    fn test_serialize_progress_notification() {
        let notification = ServerNotification::Progress(ProgressParams {
            progress_token: ProgressToken::String("tok-1".to_string()),
            progress: 50.0,
            total: Some(100.0),
            message: Some("Halfway there".to_string()),
            meta: None,
        });
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "notifications/progress");
        assert_eq!(parsed["params"]["progressToken"], "tok-1");
        assert_eq!(parsed["params"]["progress"], 50.0);
        assert_eq!(parsed["params"]["total"], 100.0);
        assert!(parsed.get("id").is_none());
    }

    #[test]
    fn test_serialize_log_message_notification() {
        let notification = ServerNotification::LogMessage(LoggingMessageParams {
            level: LogLevel::Warning,
            logger: Some("test-logger".to_string()),
            data: serde_json::json!("something happened"),
            meta: None,
        });
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["method"], "notifications/message");
        assert_eq!(parsed["params"]["level"], "warning");
        assert_eq!(parsed["params"]["logger"], "test-logger");
    }

    #[test]
    fn test_serialize_resource_updated_notification() {
        let notification = ServerNotification::ResourceUpdated {
            uri: "file:///data.json".to_string(),
        };
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["method"], "notifications/resources/updated");
        assert_eq!(parsed["params"]["uri"], "file:///data.json");
    }

    #[test]
    fn test_serialize_resources_list_changed_notification() {
        let notification = ServerNotification::ResourcesListChanged;
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["method"], "notifications/resources/list_changed");
        assert!(parsed.get("params").is_none());
    }

    #[test]
    fn test_serialize_tools_list_changed_notification() {
        let notification = ServerNotification::ToolsListChanged;
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["method"], "notifications/tools/list_changed");
    }

    #[test]
    fn test_serialize_prompts_list_changed_notification() {
        let notification = ServerNotification::PromptsListChanged;
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["method"], "notifications/prompts/list_changed");
    }

    #[test]
    fn test_serialize_task_status_changed_notification() {
        let notification = ServerNotification::TaskStatusChanged(TaskStatusParams {
            task_id: "task-42".to_string(),
            status: TaskStatus::Working,
            status_message: Some("Processing...".to_string()),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            last_updated_at: "2025-01-01T00:01:00Z".to_string(),
            ttl: None,
            poll_interval: None,
            meta: None,
        });
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["method"], "notifications/tasks/status");
        assert_eq!(parsed["params"]["taskId"], "task-42");
        assert_eq!(parsed["params"]["status"], "working");
    }

    // =========================================================================
    // process_line tests
    // =========================================================================

    fn make_router() -> McpRouter {
        McpRouter::new().server_info("test-server", "1.0.0")
    }

    async fn init_service(router: &McpRouter) -> JsonRpcService<McpRouter> {
        let mut service = JsonRpcService::new(router.clone());

        // Initialize the session
        let init_msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "1.0.0" }
            }
        });
        let msg: JsonRpcMessage = serde_json::from_value(init_msg).unwrap();
        let _ = service.call_message(msg).await.unwrap();

        // Send initialized notification
        let notif_line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let notif = serde_json::from_str::<JsonRpcNotification>(notif_line).unwrap();
        handle_notification(router, notif).unwrap();

        service
    }

    #[tokio::test]
    async fn test_process_line_valid_request() {
        let router = make_router();
        let mut service = init_service(&router).await;

        let line = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let result = process_line(&mut service, &router, line).await;

        let response = result.unwrap().unwrap();
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 1);
        assert!(json.get("result").is_some());
    }

    #[tokio::test]
    async fn test_process_line_notification_returns_none() {
        let router = make_router();
        let mut service = init_service(&router).await;

        let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let result = process_line(&mut service, &router, line).await;

        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_process_line_malformed_json() {
        let router = make_router();
        let mut service = init_service(&router).await;

        let line = r#"not valid json at all"#;
        let result = process_line(&mut service, &router, line).await;

        assert!(result.is_err());
    }

    // =========================================================================
    // clean_input_line tests
    // =========================================================================

    #[test]
    fn test_clean_input_line_no_bom() {
        assert_eq!(
            clean_input_line(r#"{"jsonrpc":"2.0"}"#),
            r#"{"jsonrpc":"2.0"}"#
        );
    }

    #[test]
    fn test_clean_input_line_strips_leading_bom() {
        let with_bom = "\u{feff}{\"jsonrpc\":\"2.0\"}";
        assert_eq!(clean_input_line(with_bom), r#"{"jsonrpc":"2.0"}"#);
    }

    #[test]
    fn test_clean_input_line_strips_bom_then_trims() {
        // BOM, then whitespace, then content, then trailing newline.
        let input = "\u{feff}   {\"id\":1}\n";
        assert_eq!(clean_input_line(input), r#"{"id":1}"#);
    }

    #[test]
    fn test_clean_input_line_does_not_strip_internal_bom() {
        // Only a *leading* BOM is stripped; one inside the payload stays.
        let input = "{\"text\":\"hi\u{feff}there\"}";
        assert_eq!(clean_input_line(input), input);
    }

    #[test]
    fn test_clean_input_line_empty() {
        assert_eq!(clean_input_line(""), "");
        assert_eq!(clean_input_line("\u{feff}"), "");
        assert_eq!(clean_input_line("   \n\t"), "");
    }

    #[tokio::test]
    async fn test_process_line_with_bom_stripped_input_parses() {
        // After clean_input_line, a BOM-prefixed request should parse like
        // any other request and return a normal response.
        let router = make_router();
        let mut service = init_service(&router).await;

        let raw = "\u{feff}{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/list\",\"params\":{}}";
        let cleaned = clean_input_line(raw);
        let result = process_line(&mut service, &router, cleaned).await;

        let response = result.unwrap().unwrap();
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["id"], 7);
        assert!(json["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn test_process_line_tools_list() {
        let router = make_router();
        let mut service = init_service(&router).await;

        let line = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
        let result = process_line(&mut service, &router, line).await;

        let response = result.unwrap().unwrap();
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["id"], 2);
        assert!(json["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn test_process_line_unknown_method() {
        let router = make_router();
        let mut service = init_service(&router).await;

        let line = r#"{"jsonrpc":"2.0","id":3,"method":"nonexistent/method"}"#;
        let result = process_line(&mut service, &router, line).await;

        let response = result.unwrap().unwrap();
        let json = serde_json::to_value(&response).unwrap();
        assert!(json.get("error").is_some());
        assert_eq!(json["error"]["code"], -32601); // Method not found
    }

    // =========================================================================
    // handle_notification tests
    // =========================================================================

    #[test]
    fn test_handle_notification_initialized() {
        let router = make_router();
        let notif_json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let notif: JsonRpcNotification = serde_json::from_str(notif_json).unwrap();

        let result = handle_notification(&router, notif);
        assert!(result.is_ok());
    }

    #[test]
    fn test_handle_notification_cancelled() {
        let router = make_router();
        let notif_json = r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1,"reason":"timeout"}}"#;
        let notif: JsonRpcNotification = serde_json::from_str(notif_json).unwrap();

        let result = handle_notification(&router, notif);
        assert!(result.is_ok());
    }
}
