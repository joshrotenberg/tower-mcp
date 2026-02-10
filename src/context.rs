//! Request context for MCP handlers
//!
//! Provides progress reporting, cancellation support, and client request capabilities
//! for long-running operations.
//!
//! # Example
//!
//! ```rust,ignore
//! use tower_mcp::context::RequestContext;
//!
//! async fn long_running_tool(ctx: RequestContext, input: MyInput) -> Result<CallToolResult> {
//!     for i in 0..100 {
//!         // Check if cancelled
//!         if ctx.is_cancelled() {
//!             return Err(Error::tool("Operation cancelled"));
//!         }
//!
//!         // Report progress
//!         ctx.report_progress(i as f64, Some(100.0), Some("Processing...")).await;
//!
//!         do_work(i).await;
//!     }
//!     Ok(CallToolResult::text("Done!"))
//! }
//! ```
//!
//! # Sampling (LLM requests to client)
//!
//! ```rust,ignore
//! use tower_mcp::context::RequestContext;
//! use tower_mcp::{CreateMessageParams, SamplingMessage};
//!
//! async fn ai_tool(ctx: RequestContext, input: MyInput) -> Result<CallToolResult> {
//!     // Request LLM completion from the client
//!     let params = CreateMessageParams::new(
//!         vec![SamplingMessage::user("Summarize this text...")],
//!         500,
//!     );
//!
//!     let result = ctx.sample(params).await?;
//!     Ok(CallToolResult::text(format!("Summary: {:?}", result.content)))
//! }
//! ```
//!
//! # Elicitation (requesting user input)
//!
//! ```rust,ignore
//! use tower_mcp::context::RequestContext;
//! use tower_mcp::{ElicitFormParams, ElicitFormSchema, ElicitMode, ElicitAction};
//!
//! async fn interactive_tool(ctx: RequestContext, input: MyInput) -> Result<CallToolResult> {
//!     // Request user input via form
//!     let params = ElicitFormParams {
//!         mode: ElicitMode::Form,
//!         message: "Please provide additional details".to_string(),
//!         requested_schema: ElicitFormSchema::new()
//!             .string_field("name", Some("Your name"), true)
//!             .number_field("age", Some("Your age"), false),
//!         meta: None,
//!     };
//!
//!     let result = ctx.elicit_form(params).await?;
//!     if result.action == ElicitAction::Accept {
//!         // Use the form data
//!         Ok(CallToolResult::text(format!("Got: {:?}", result.content)))
//!     } else {
//!         Ok(CallToolResult::text("User declined"))
//!     }
//! }
//! ```

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::protocol::{
    CreateMessageParams, CreateMessageResult, ElicitFormParams, ElicitRequestParams, ElicitResult,
    ElicitUrlParams, LogLevel, LoggingMessageParams, ProgressParams, ProgressToken, RequestId,
};

/// A notification to be sent to the client
#[derive(Debug, Clone)]
pub enum ServerNotification {
    /// Progress update for a request
    Progress(ProgressParams),
    /// Log message notification
    LogMessage(LoggingMessageParams),
    /// A subscribed resource has been updated
    ResourceUpdated {
        /// The URI of the updated resource
        uri: String,
    },
    /// The list of available resources has changed
    ResourcesListChanged,
    /// The list of available tools has changed
    ToolsListChanged,
    /// The list of available prompts has changed
    PromptsListChanged,
}

/// Sender for server notifications
pub type NotificationSender = mpsc::Sender<ServerNotification>;

/// Receiver for server notifications
pub type NotificationReceiver = mpsc::Receiver<ServerNotification>;

/// Create a new notification channel
pub fn notification_channel(buffer: usize) -> (NotificationSender, NotificationReceiver) {
    mpsc::channel(buffer)
}

// =============================================================================
// Client Requests (Server -> Client)
// =============================================================================

/// Trait for sending requests from server to client
///
/// This enables bidirectional communication where the server can request
/// actions from the client, such as sampling (LLM requests) and elicitation
/// (user input requests).
#[async_trait]
pub trait ClientRequester: Send + Sync {
    /// Send a sampling request to the client
    ///
    /// Returns the LLM completion result from the client.
    async fn sample(&self, params: CreateMessageParams) -> Result<CreateMessageResult>;

    /// Send an elicitation request to the client
    ///
    /// This requests user input from the client. The request can be either
    /// form-based (structured input) or URL-based (redirect to external URL).
    ///
    /// Returns the elicitation result with the user's action and any submitted data.
    async fn elicit(&self, params: ElicitRequestParams) -> Result<ElicitResult>;
}

/// A clonable handle to a client requester
pub type ClientRequesterHandle = Arc<dyn ClientRequester>;

/// Outgoing request to be sent to the client
#[derive(Debug)]
pub struct OutgoingRequest {
    /// The JSON-RPC request ID
    pub id: RequestId,
    /// The method name
    pub method: String,
    /// The request parameters as JSON
    pub params: serde_json::Value,
    /// Channel to send the response back
    pub response_tx: tokio::sync::oneshot::Sender<Result<serde_json::Value>>,
}

/// Sender for outgoing requests to the client
pub type OutgoingRequestSender = mpsc::Sender<OutgoingRequest>;

/// Receiver for outgoing requests (used by transport)
pub type OutgoingRequestReceiver = mpsc::Receiver<OutgoingRequest>;

/// Create a new outgoing request channel
pub fn outgoing_request_channel(buffer: usize) -> (OutgoingRequestSender, OutgoingRequestReceiver) {
    mpsc::channel(buffer)
}

/// A client requester implementation that sends requests through a channel
#[derive(Clone)]
pub struct ChannelClientRequester {
    request_tx: OutgoingRequestSender,
    next_id: Arc<AtomicI64>,
}

impl ChannelClientRequester {
    /// Create a new channel-based client requester
    pub fn new(request_tx: OutgoingRequestSender) -> Self {
        Self {
            request_tx,
            next_id: Arc::new(AtomicI64::new(1)),
        }
    }

    fn next_request_id(&self) -> RequestId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        RequestId::Number(id)
    }
}

#[async_trait]
impl ClientRequester for ChannelClientRequester {
    async fn sample(&self, params: CreateMessageParams) -> Result<CreateMessageResult> {
        let id = self.next_request_id();
        let params_json = serde_json::to_value(&params)
            .map_err(|e| Error::Internal(format!("Failed to serialize params: {}", e)))?;

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        let request = OutgoingRequest {
            id: id.clone(),
            method: "sampling/createMessage".to_string(),
            params: params_json,
            response_tx,
        };

        self.request_tx
            .send(request)
            .await
            .map_err(|_| Error::Internal("Failed to send request: channel closed".to_string()))?;

        let response = response_rx.await.map_err(|_| {
            Error::Internal("Failed to receive response: channel closed".to_string())
        })??;

        serde_json::from_value(response)
            .map_err(|e| Error::Internal(format!("Failed to deserialize response: {}", e)))
    }

    async fn elicit(&self, params: ElicitRequestParams) -> Result<ElicitResult> {
        let id = self.next_request_id();
        let params_json = serde_json::to_value(&params)
            .map_err(|e| Error::Internal(format!("Failed to serialize params: {}", e)))?;

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        let request = OutgoingRequest {
            id: id.clone(),
            method: "elicitation/create".to_string(),
            params: params_json,
            response_tx,
        };

        self.request_tx
            .send(request)
            .await
            .map_err(|_| Error::Internal("Failed to send request: channel closed".to_string()))?;

        let response = response_rx.await.map_err(|_| {
            Error::Internal("Failed to receive response: channel closed".to_string())
        })??;

        serde_json::from_value(response)
            .map_err(|e| Error::Internal(format!("Failed to deserialize response: {}", e)))
    }
}

/// Context for a request, providing progress, cancellation, and client request support
#[derive(Clone)]
pub struct RequestContext {
    /// The request ID
    request_id: RequestId,
    /// Progress token (if provided by client)
    progress_token: Option<ProgressToken>,
    /// Cancellation flag
    cancelled: Arc<AtomicBool>,
    /// Channel for sending notifications
    notification_tx: Option<NotificationSender>,
    /// Handle for sending requests to the client (for sampling, etc.)
    client_requester: Option<ClientRequesterHandle>,
    /// Extensions for passing data from router/middleware to handlers
    extensions: Arc<Extensions>,
    /// Minimum log level set by the client (shared with router for dynamic updates)
    min_log_level: Option<Arc<RwLock<LogLevel>>>,
}

/// Type-erased extensions map for passing data to handlers.
///
/// Extensions allow router-level state and middleware-injected data to flow
/// to tool handlers via the `Extension<T>` extractor.
#[derive(Clone, Default)]
pub struct Extensions {
    map: std::collections::HashMap<std::any::TypeId, Arc<dyn std::any::Any + Send + Sync>>,
}

impl Extensions {
    /// Create an empty extensions map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a value into the extensions map.
    ///
    /// If a value of the same type already exists, it is replaced.
    pub fn insert<T: Send + Sync + 'static>(&mut self, val: T) {
        self.map.insert(std::any::TypeId::of::<T>(), Arc::new(val));
    }

    /// Get a reference to a value in the extensions map.
    ///
    /// Returns `None` if no value of the given type has been inserted.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.map
            .get(&std::any::TypeId::of::<T>())
            .and_then(|val| val.downcast_ref::<T>())
    }

    /// Check if the extensions map contains a value of the given type.
    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.map.contains_key(&std::any::TypeId::of::<T>())
    }

    /// Merge another extensions map into this one.
    ///
    /// Values from `other` will overwrite existing values of the same type.
    pub fn merge(&mut self, other: &Extensions) {
        for (k, v) in &other.map {
            self.map.insert(*k, v.clone());
        }
    }
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.map.len())
            .finish()
    }
}

impl std::fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestContext")
            .field("request_id", &self.request_id)
            .field("progress_token", &self.progress_token)
            .field("cancelled", &self.cancelled.load(Ordering::Relaxed))
            .finish()
    }
}

impl RequestContext {
    /// Create a new request context
    pub fn new(request_id: RequestId) -> Self {
        Self {
            request_id,
            progress_token: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            notification_tx: None,
            client_requester: None,
            extensions: Arc::new(Extensions::new()),
            min_log_level: None,
        }
    }

    /// Set the progress token
    pub fn with_progress_token(mut self, token: ProgressToken) -> Self {
        self.progress_token = Some(token);
        self
    }

    /// Set the notification sender
    pub fn with_notification_sender(mut self, tx: NotificationSender) -> Self {
        self.notification_tx = Some(tx);
        self
    }

    /// Set the minimum log level for filtering outgoing log notifications
    ///
    /// This is shared with the router so that `logging/setLevel` updates
    /// are immediately visible to all request contexts.
    pub fn with_min_log_level(mut self, level: Arc<RwLock<LogLevel>>) -> Self {
        self.min_log_level = Some(level);
        self
    }

    /// Set the client requester for server-to-client requests
    pub fn with_client_requester(mut self, requester: ClientRequesterHandle) -> Self {
        self.client_requester = Some(requester);
        self
    }

    /// Set the extensions for this request context.
    ///
    /// Extensions allow router-level state and middleware data to flow to handlers.
    pub fn with_extensions(mut self, extensions: Arc<Extensions>) -> Self {
        self.extensions = extensions;
        self
    }

    /// Get a reference to a value from the extensions map.
    ///
    /// Returns `None` if no value of the given type has been inserted.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// #[derive(Clone)]
    /// struct CurrentUser { id: String }
    ///
    /// // In a handler:
    /// if let Some(user) = ctx.extension::<CurrentUser>() {
    ///     println!("User: {}", user.id);
    /// }
    /// ```
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.extensions.get::<T>()
    }

    /// Get a mutable reference to the extensions.
    ///
    /// This allows middleware to insert data that handlers can access via
    /// the `Extension<T>` extractor.
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        Arc::make_mut(&mut self.extensions)
    }

    /// Get a reference to the extensions.
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Get the request ID
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    /// Get the progress token (if any)
    pub fn progress_token(&self) -> Option<&ProgressToken> {
        self.progress_token.as_ref()
    }

    /// Check if the request has been cancelled
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Mark the request as cancelled
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Get a cancellation token that can be shared
    pub fn cancellation_token(&self) -> CancellationToken {
        CancellationToken {
            cancelled: self.cancelled.clone(),
        }
    }

    /// Report progress to the client
    ///
    /// This is a no-op if no progress token was provided or no notification sender is configured.
    pub async fn report_progress(&self, progress: f64, total: Option<f64>, message: Option<&str>) {
        let Some(token) = &self.progress_token else {
            return;
        };
        let Some(tx) = &self.notification_tx else {
            return;
        };

        let params = ProgressParams {
            progress_token: token.clone(),
            progress,
            total,
            message: message.map(|s| s.to_string()),
        };

        // Best effort - don't block if channel is full
        let _ = tx.try_send(ServerNotification::Progress(params));
    }

    /// Report progress synchronously (non-async version)
    ///
    /// This is a no-op if no progress token was provided or no notification sender is configured.
    pub fn report_progress_sync(&self, progress: f64, total: Option<f64>, message: Option<&str>) {
        let Some(token) = &self.progress_token else {
            return;
        };
        let Some(tx) = &self.notification_tx else {
            return;
        };

        let params = ProgressParams {
            progress_token: token.clone(),
            progress,
            total,
            message: message.map(|s| s.to_string()),
        };

        let _ = tx.try_send(ServerNotification::Progress(params));
    }

    /// Send a log message notification to the client
    ///
    /// This is a no-op if no notification sender is configured.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tower_mcp::protocol::{LoggingMessageParams, LogLevel};
    ///
    /// async fn my_tool(ctx: RequestContext) {
    ///     ctx.send_log(
    ///         LoggingMessageParams::new(LogLevel::Info)
    ///             .with_logger("my-tool")
    ///             .with_data(serde_json::json!("Processing..."))
    ///     );
    /// }
    /// ```
    pub fn send_log(&self, params: LoggingMessageParams) {
        let Some(tx) = &self.notification_tx else {
            return;
        };

        // Filter by minimum log level set via logging/setLevel
        // LogLevel derives Ord with Emergency < Alert < ... < Debug,
        // so a message passes if its severity is at least the minimum
        // (i.e., its ordinal is <= the minimum level's ordinal).
        if let Some(min_level) = &self.min_log_level
            && let Ok(min) = min_level.read()
            && params.level > *min
        {
            return;
        }

        let _ = tx.try_send(ServerNotification::LogMessage(params));
    }

    /// Check if sampling is available
    ///
    /// Returns true if a client requester is configured and the transport
    /// supports bidirectional communication.
    pub fn can_sample(&self) -> bool {
        self.client_requester.is_some()
    }

    /// Request an LLM completion from the client
    ///
    /// This sends a `sampling/createMessage` request to the client and waits
    /// for the response. The client is expected to forward this to an LLM
    /// and return the result.
    ///
    /// Returns an error if sampling is not available (no client requester configured).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tower_mcp::{CreateMessageParams, SamplingMessage};
    ///
    /// async fn my_tool(ctx: RequestContext, input: MyInput) -> Result<CallToolResult> {
    ///     let params = CreateMessageParams::new(
    ///         vec![SamplingMessage::user("Summarize: ...")],
    ///         500,
    ///     );
    ///
    ///     let result = ctx.sample(params).await?;
    ///     Ok(CallToolResult::text(format!("{:?}", result.content)))
    /// }
    /// ```
    pub async fn sample(&self, params: CreateMessageParams) -> Result<CreateMessageResult> {
        let requester = self.client_requester.as_ref().ok_or_else(|| {
            Error::Internal("Sampling not available: no client requester configured".to_string())
        })?;

        requester.sample(params).await
    }

    /// Check if elicitation is available
    ///
    /// Returns true if a client requester is configured and the transport
    /// supports bidirectional communication. Note that this only checks if
    /// the mechanism is available, not whether the client supports elicitation.
    pub fn can_elicit(&self) -> bool {
        self.client_requester.is_some()
    }

    /// Request user input via a form from the client
    ///
    /// This sends an `elicitation/create` request to the client with a form schema.
    /// The client renders the form to the user and returns their response.
    ///
    /// Returns an error if elicitation is not available (no client requester configured).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tower_mcp::{ElicitFormParams, ElicitFormSchema, ElicitMode, ElicitAction};
    ///
    /// async fn my_tool(ctx: RequestContext, input: MyInput) -> Result<CallToolResult> {
    ///     let params = ElicitFormParams {
    ///         mode: ElicitMode::Form,
    ///         message: "Please enter your details".to_string(),
    ///         requested_schema: ElicitFormSchema::new()
    ///             .string_field("name", Some("Your name"), true),
    ///         meta: None,
    ///     };
    ///
    ///     let result = ctx.elicit_form(params).await?;
    ///     match result.action {
    ///         ElicitAction::Accept => {
    ///             // Use result.content
    ///             Ok(CallToolResult::text("Got your input!"))
    ///         }
    ///         _ => Ok(CallToolResult::text("User declined"))
    ///     }
    /// }
    /// ```
    pub async fn elicit_form(&self, params: ElicitFormParams) -> Result<ElicitResult> {
        let requester = self.client_requester.as_ref().ok_or_else(|| {
            Error::Internal("Elicitation not available: no client requester configured".to_string())
        })?;

        requester.elicit(ElicitRequestParams::Form(params)).await
    }

    /// Request user input via URL redirect from the client
    ///
    /// This sends an `elicitation/create` request to the client with a URL.
    /// The client directs the user to the URL for out-of-band input collection.
    /// The server receives the result via a callback notification.
    ///
    /// Returns an error if elicitation is not available (no client requester configured).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tower_mcp::{ElicitUrlParams, ElicitMode, ElicitAction};
    ///
    /// async fn my_tool(ctx: RequestContext, input: MyInput) -> Result<CallToolResult> {
    ///     let params = ElicitUrlParams {
    ///         mode: ElicitMode::Url,
    ///         elicitation_id: "unique-id-123".to_string(),
    ///         message: "Please authorize via the link".to_string(),
    ///         url: "https://example.com/auth?id=unique-id-123".to_string(),
    ///         meta: None,
    ///     };
    ///
    ///     let result = ctx.elicit_url(params).await?;
    ///     match result.action {
    ///         ElicitAction::Accept => Ok(CallToolResult::text("Authorization complete!")),
    ///         _ => Ok(CallToolResult::text("Authorization cancelled"))
    ///     }
    /// }
    /// ```
    pub async fn elicit_url(&self, params: ElicitUrlParams) -> Result<ElicitResult> {
        let requester = self.client_requester.as_ref().ok_or_else(|| {
            Error::Internal("Elicitation not available: no client requester configured".to_string())
        })?;

        requester.elicit(ElicitRequestParams::Url(params)).await
    }

    /// Request simple confirmation from the user.
    ///
    /// This is a convenience method for simple yes/no confirmation dialogs.
    /// It creates an elicitation form with a single boolean "confirm" field
    /// and returns `true` if the user accepts, `false` otherwise.
    ///
    /// Returns an error if elicitation is not available (no client requester configured).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tower_mcp::{RequestContext, CallToolResult};
    ///
    /// async fn delete_item(ctx: RequestContext) -> Result<CallToolResult> {
    ///     let confirmed = ctx.confirm("Are you sure you want to delete this item?").await?;
    ///     if confirmed {
    ///         // Perform deletion
    ///         Ok(CallToolResult::text("Item deleted"))
    ///     } else {
    ///         Ok(CallToolResult::text("Deletion cancelled"))
    ///     }
    /// }
    /// ```
    pub async fn confirm(&self, message: impl Into<String>) -> Result<bool> {
        use crate::protocol::{ElicitAction, ElicitFormParams, ElicitFormSchema, ElicitMode};

        let params = ElicitFormParams {
            mode: ElicitMode::Form,
            message: message.into(),
            requested_schema: ElicitFormSchema::new().boolean_field_with_default(
                "confirm",
                Some("Confirm this action"),
                true,
                false,
            ),
            meta: None,
        };

        let result = self.elicit_form(params).await?;
        Ok(result.action == ElicitAction::Accept)
    }
}

/// A token that can be used to check for cancellation
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Check if cancellation has been requested
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Request cancellation
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

/// Builder for creating request contexts
#[derive(Default)]
pub struct RequestContextBuilder {
    request_id: Option<RequestId>,
    progress_token: Option<ProgressToken>,
    notification_tx: Option<NotificationSender>,
    client_requester: Option<ClientRequesterHandle>,
    min_log_level: Option<Arc<RwLock<LogLevel>>>,
}

impl RequestContextBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the request ID
    pub fn request_id(mut self, id: RequestId) -> Self {
        self.request_id = Some(id);
        self
    }

    /// Set the progress token
    pub fn progress_token(mut self, token: ProgressToken) -> Self {
        self.progress_token = Some(token);
        self
    }

    /// Set the notification sender
    pub fn notification_sender(mut self, tx: NotificationSender) -> Self {
        self.notification_tx = Some(tx);
        self
    }

    /// Set the client requester for server-to-client requests
    pub fn client_requester(mut self, requester: ClientRequesterHandle) -> Self {
        self.client_requester = Some(requester);
        self
    }

    /// Set the minimum log level for filtering
    pub fn min_log_level(mut self, level: Arc<RwLock<LogLevel>>) -> Self {
        self.min_log_level = Some(level);
        self
    }

    /// Build the request context
    ///
    /// Panics if request_id is not set.
    pub fn build(self) -> RequestContext {
        let mut ctx = RequestContext::new(self.request_id.expect("request_id is required"));
        if let Some(token) = self.progress_token {
            ctx = ctx.with_progress_token(token);
        }
        if let Some(tx) = self.notification_tx {
            ctx = ctx.with_notification_sender(tx);
        }
        if let Some(requester) = self.client_requester {
            ctx = ctx.with_client_requester(requester);
        }
        if let Some(level) = self.min_log_level {
            ctx = ctx.with_min_log_level(level);
        }
        ctx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cancellation() {
        let ctx = RequestContext::new(RequestId::Number(1));
        assert!(!ctx.is_cancelled());

        let token = ctx.cancellation_token();
        assert!(!token.is_cancelled());

        ctx.cancel();
        assert!(ctx.is_cancelled());
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn test_progress_reporting() {
        let (tx, mut rx) = notification_channel(10);

        let ctx = RequestContext::new(RequestId::Number(1))
            .with_progress_token(ProgressToken::Number(42))
            .with_notification_sender(tx);

        ctx.report_progress(50.0, Some(100.0), Some("Halfway"))
            .await;

        let notification = rx.recv().await.unwrap();
        match notification {
            ServerNotification::Progress(params) => {
                assert_eq!(params.progress, 50.0);
                assert_eq!(params.total, Some(100.0));
                assert_eq!(params.message.as_deref(), Some("Halfway"));
            }
            _ => panic!("Expected Progress notification"),
        }
    }

    #[tokio::test]
    async fn test_progress_no_token() {
        let (tx, mut rx) = notification_channel(10);

        // No progress token - should be a no-op
        let ctx = RequestContext::new(RequestId::Number(1)).with_notification_sender(tx);

        ctx.report_progress(50.0, Some(100.0), None).await;

        // Channel should be empty
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_builder() {
        let (tx, _rx) = notification_channel(10);

        let ctx = RequestContextBuilder::new()
            .request_id(RequestId::String("req-1".to_string()))
            .progress_token(ProgressToken::String("prog-1".to_string()))
            .notification_sender(tx)
            .build();

        assert_eq!(ctx.request_id(), &RequestId::String("req-1".to_string()));
        assert!(ctx.progress_token().is_some());
    }

    #[test]
    fn test_can_sample_without_requester() {
        let ctx = RequestContext::new(RequestId::Number(1));
        assert!(!ctx.can_sample());
    }

    #[test]
    fn test_can_sample_with_requester() {
        let (request_tx, _rx) = outgoing_request_channel(10);
        let requester: ClientRequesterHandle = Arc::new(ChannelClientRequester::new(request_tx));

        let ctx = RequestContext::new(RequestId::Number(1)).with_client_requester(requester);
        assert!(ctx.can_sample());
    }

    #[tokio::test]
    async fn test_sample_without_requester_fails() {
        use crate::protocol::{CreateMessageParams, SamplingMessage};

        let ctx = RequestContext::new(RequestId::Number(1));
        let params = CreateMessageParams::new(vec![SamplingMessage::user("test")], 100);

        let result = ctx.sample(params).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Sampling not available")
        );
    }

    #[test]
    fn test_builder_with_client_requester() {
        let (request_tx, _rx) = outgoing_request_channel(10);
        let requester: ClientRequesterHandle = Arc::new(ChannelClientRequester::new(request_tx));

        let ctx = RequestContextBuilder::new()
            .request_id(RequestId::Number(1))
            .client_requester(requester)
            .build();

        assert!(ctx.can_sample());
    }

    #[test]
    fn test_can_elicit_without_requester() {
        let ctx = RequestContext::new(RequestId::Number(1));
        assert!(!ctx.can_elicit());
    }

    #[test]
    fn test_can_elicit_with_requester() {
        let (request_tx, _rx) = outgoing_request_channel(10);
        let requester: ClientRequesterHandle = Arc::new(ChannelClientRequester::new(request_tx));

        let ctx = RequestContext::new(RequestId::Number(1)).with_client_requester(requester);
        assert!(ctx.can_elicit());
    }

    #[tokio::test]
    async fn test_elicit_form_without_requester_fails() {
        use crate::protocol::{ElicitFormSchema, ElicitMode};

        let ctx = RequestContext::new(RequestId::Number(1));
        let params = ElicitFormParams {
            mode: ElicitMode::Form,
            message: "Enter details".to_string(),
            requested_schema: ElicitFormSchema::new().string_field("name", None, true),
            meta: None,
        };

        let result = ctx.elicit_form(params).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Elicitation not available")
        );
    }

    #[tokio::test]
    async fn test_elicit_url_without_requester_fails() {
        use crate::protocol::ElicitMode;

        let ctx = RequestContext::new(RequestId::Number(1));
        let params = ElicitUrlParams {
            mode: ElicitMode::Url,
            elicitation_id: "test-123".to_string(),
            message: "Please authorize".to_string(),
            url: "https://example.com/auth".to_string(),
            meta: None,
        };

        let result = ctx.elicit_url(params).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Elicitation not available")
        );
    }

    #[tokio::test]
    async fn test_confirm_without_requester_fails() {
        let ctx = RequestContext::new(RequestId::Number(1));

        let result = ctx.confirm("Are you sure?").await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Elicitation not available")
        );
    }

    #[tokio::test]
    async fn test_send_log_filtered_by_level() {
        let (tx, mut rx) = notification_channel(10);
        let min_level = Arc::new(RwLock::new(LogLevel::Warning));

        let ctx = RequestContext::new(RequestId::Number(1))
            .with_notification_sender(tx)
            .with_min_log_level(min_level.clone());

        // Error is more severe than Warning — should pass through
        ctx.send_log(LoggingMessageParams::new(LogLevel::Error));
        let msg = rx.try_recv();
        assert!(msg.is_ok(), "Error should pass through Warning filter");

        // Warning is equal to min level — should pass through
        ctx.send_log(LoggingMessageParams::new(LogLevel::Warning));
        let msg = rx.try_recv();
        assert!(msg.is_ok(), "Warning should pass through Warning filter");

        // Info is less severe than Warning — should be filtered
        ctx.send_log(LoggingMessageParams::new(LogLevel::Info));
        let msg = rx.try_recv();
        assert!(msg.is_err(), "Info should be filtered by Warning filter");

        // Debug is less severe than Warning — should be filtered
        ctx.send_log(LoggingMessageParams::new(LogLevel::Debug));
        let msg = rx.try_recv();
        assert!(msg.is_err(), "Debug should be filtered by Warning filter");
    }

    #[tokio::test]
    async fn test_send_log_level_updates_dynamically() {
        let (tx, mut rx) = notification_channel(10);
        let min_level = Arc::new(RwLock::new(LogLevel::Error));

        let ctx = RequestContext::new(RequestId::Number(1))
            .with_notification_sender(tx)
            .with_min_log_level(min_level.clone());

        // Info should be filtered at Error level
        ctx.send_log(LoggingMessageParams::new(LogLevel::Info));
        assert!(
            rx.try_recv().is_err(),
            "Info should be filtered at Error level"
        );

        // Dynamically update to Debug (most permissive)
        *min_level.write().unwrap() = LogLevel::Debug;

        // Now Info should pass through
        ctx.send_log(LoggingMessageParams::new(LogLevel::Info));
        assert!(
            rx.try_recv().is_ok(),
            "Info should pass through after level changed to Debug"
        );
    }

    #[tokio::test]
    async fn test_send_log_no_min_level_sends_all() {
        let (tx, mut rx) = notification_channel(10);

        // No min_log_level set — all messages should pass through
        let ctx = RequestContext::new(RequestId::Number(1)).with_notification_sender(tx);

        ctx.send_log(LoggingMessageParams::new(LogLevel::Debug));
        assert!(
            rx.try_recv().is_ok(),
            "Debug should pass when no min level is set"
        );
    }
}
