//! Handler trait for server-initiated requests and notifications.
//!
//! The [`ClientHandler`] trait defines how the client responds to requests
//! and notifications sent by the server. All methods have default
//! implementations, so you only need to override the ones you care about.
//!
//! The unit type `()` implements this trait with all defaults, which is
//! used by [`McpClient::connect()`](super::McpClient::connect).
//!
//! # Notification Handler
//!
//! For notification-only use cases, [`NotificationHandler`] provides a
//! builder-based alternative to implementing the full trait:
//!
//! ```rust
//! use tower_mcp::client::NotificationHandler;
//!
//! let handler = NotificationHandler::new()
//!     .on_tools_changed(|| {
//!         println!("Tools changed, re-fetching...");
//!     })
//!     .on_log_message(|msg| {
//!         println!("[{}] {}", msg.level, msg.data);
//!     });
//! ```
//!
//! For forwarding MCP log messages to the [`tracing`] crate:
//!
//! ```rust
//! use tower_mcp::client::NotificationHandler;
//!
//! let handler = NotificationHandler::with_log_forwarding();
//! ```
//!
//! # Custom Handler
//!
//! ```rust,ignore
//! use async_trait::async_trait;
//! use tower_mcp::client::ClientHandler;
//! use tower_mcp::protocol::{CreateMessageParams, CreateMessageResult};
//! use tower_mcp_types::JsonRpcError;
//!
//! struct MySamplingHandler;
//!
//! #[async_trait]
//! impl ClientHandler for MySamplingHandler {
//!     async fn handle_create_message(
//!         &self,
//!         params: CreateMessageParams,
//!     ) -> Result<CreateMessageResult, JsonRpcError> {
//!         // Forward to your LLM and return the result
//!         todo!()
//!     }
//! }
//! ```

use async_trait::async_trait;

use crate::protocol::{
    CreateMessageParams, CreateMessageResult, ElicitRequestParams, ElicitResult, ListRootsResult,
    LogLevel, LoggingMessageParams, ProgressParams,
};
use tower_mcp_types::JsonRpcError;

/// Notification sent from the server to the client.
///
/// These correspond to the `notifications/` methods defined in the MCP spec
/// that flow from server to client.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ServerNotification {
    /// Progress update for a request (`notifications/progress`).
    Progress(ProgressParams),
    /// Log message (`notifications/message`).
    LogMessage(LoggingMessageParams),
    /// A subscribed resource has been updated (`notifications/resources/updated`).
    ResourceUpdated {
        /// The URI of the updated resource.
        uri: String,
    },
    /// The list of available resources has changed.
    ResourcesListChanged,
    /// The list of available tools has changed.
    ToolsListChanged,
    /// The list of available prompts has changed.
    PromptsListChanged,
    /// An unknown or unrecognized notification.
    Unknown {
        /// The notification method name.
        method: String,
        /// The notification parameters, if any.
        params: Option<serde_json::Value>,
    },
}

/// Handler for server-initiated requests and notifications.
///
/// Implement this trait to handle sampling requests, elicitation requests,
/// roots listing, and server notifications. All methods have default
/// implementations that either return sensible defaults or reject with
/// `method_not_found`.
///
/// The unit type `()` implements this trait with all defaults, which is
/// used by [`McpClient::connect()`](super::McpClient::connect).
#[async_trait]
pub trait ClientHandler: Send + Sync + 'static {
    /// Handle a `sampling/createMessage` request from the server.
    ///
    /// The server is asking the client to perform LLM inference. Override
    /// this to forward the request to your LLM provider.
    ///
    /// Default: returns `method_not_found` error.
    async fn handle_create_message(
        &self,
        _params: CreateMessageParams,
    ) -> Result<CreateMessageResult, JsonRpcError> {
        Err(JsonRpcError::method_not_found("sampling/createMessage"))
    }

    /// Handle an `elicitation/create` request from the server.
    ///
    /// The server is asking the client for user input (form data or URL).
    ///
    /// Default: returns `method_not_found` error.
    async fn handle_elicit(
        &self,
        _params: ElicitRequestParams,
    ) -> Result<ElicitResult, JsonRpcError> {
        Err(JsonRpcError::method_not_found("elicitation/create"))
    }

    /// Handle a `roots/list` request from the server.
    ///
    /// The server is asking which filesystem roots the client has access to.
    /// If roots were configured on the [`McpClient`](super::McpClient) via
    /// the builder, those are returned automatically before this method
    /// is called.
    ///
    /// Default: returns an empty list.
    async fn handle_list_roots(&self) -> Result<ListRootsResult, JsonRpcError> {
        Ok(ListRootsResult {
            roots: vec![],
            meta: None,
        })
    }

    /// Called when the server sends a notification.
    ///
    /// Override to handle progress updates, log messages, resource changes, etc.
    ///
    /// Default: no-op.
    async fn on_notification(&self, _notification: ServerNotification) {}
}

/// Unit type implements [`ClientHandler`] with all defaults.
#[async_trait]
impl ClientHandler for () {}

// Type aliases for notification callback boxes.
type ProgressCallback = Box<dyn Fn(ProgressParams) + Send + Sync>;
type LogMessageCallback = Box<dyn Fn(LoggingMessageParams) + Send + Sync>;
type ResourceUpdatedCallback = Box<dyn Fn(String) + Send + Sync>;
type SimpleCallback = Box<dyn Fn() + Send + Sync>;

/// Callback-based handler for server notifications.
///
/// Provides typed callback registration for each notification type,
/// without requiring a full [`ClientHandler`] trait implementation.
/// Server-initiated requests (sampling, elicitation, roots) are
/// rejected with `method_not_found`.
///
/// # Example
///
/// ```rust
/// use tower_mcp::client::NotificationHandler;
///
/// let handler = NotificationHandler::new()
///     .on_progress(|p| {
///         println!("Progress: {}/{}", p.progress, p.total.unwrap_or(1.0));
///     })
///     .on_tools_changed(|| {
///         println!("Server tools changed!");
///     });
/// ```
pub struct NotificationHandler {
    on_progress: Option<ProgressCallback>,
    on_log_message: Option<LogMessageCallback>,
    on_resource_updated: Option<ResourceUpdatedCallback>,
    on_resources_changed: Option<SimpleCallback>,
    on_tools_changed: Option<SimpleCallback>,
    on_prompts_changed: Option<SimpleCallback>,
}

impl NotificationHandler {
    /// Create a new handler with no callbacks registered.
    pub fn new() -> Self {
        Self {
            on_progress: None,
            on_log_message: None,
            on_resource_updated: None,
            on_resources_changed: None,
            on_tools_changed: None,
            on_prompts_changed: None,
        }
    }

    /// Create a handler that forwards MCP log messages to [`tracing`].
    ///
    /// Maps MCP log levels to tracing levels:
    /// - Emergency, Alert, Critical -> `error!`
    /// - Error -> `error!`
    /// - Warning -> `warn!`
    /// - Notice, Info -> `info!`
    /// - Debug -> `debug!`
    pub fn with_log_forwarding() -> Self {
        Self::new().on_log_message(|msg| {
            let logger = msg.logger.as_deref().unwrap_or("mcp");
            match msg.level {
                LogLevel::Emergency | LogLevel::Alert | LogLevel::Critical | LogLevel::Error => {
                    tracing::error!(logger = logger, "{}", msg.data);
                }
                LogLevel::Warning => {
                    tracing::warn!(logger = logger, "{}", msg.data);
                }
                LogLevel::Notice | LogLevel::Info => {
                    tracing::info!(logger = logger, "{}", msg.data);
                }
                LogLevel::Debug => {
                    tracing::debug!(logger = logger, "{}", msg.data);
                }
                _ => {
                    tracing::trace!(logger = logger, "{}", msg.data);
                }
            }
        })
    }

    /// Register a callback for progress notifications.
    pub fn on_progress(mut self, f: impl Fn(ProgressParams) + Send + Sync + 'static) -> Self {
        self.on_progress = Some(Box::new(f));
        self
    }

    /// Register a callback for log message notifications.
    pub fn on_log_message(
        mut self,
        f: impl Fn(LoggingMessageParams) + Send + Sync + 'static,
    ) -> Self {
        self.on_log_message = Some(Box::new(f));
        self
    }

    /// Register a callback for resource updated notifications.
    ///
    /// The callback receives the URI of the updated resource.
    pub fn on_resource_updated(mut self, f: impl Fn(String) + Send + Sync + 'static) -> Self {
        self.on_resource_updated = Some(Box::new(f));
        self
    }

    /// Register a callback for resources list changed notifications.
    pub fn on_resources_changed(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_resources_changed = Some(Box::new(f));
        self
    }

    /// Register a callback for tools list changed notifications.
    pub fn on_tools_changed(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_tools_changed = Some(Box::new(f));
        self
    }

    /// Register a callback for prompts list changed notifications.
    pub fn on_prompts_changed(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_prompts_changed = Some(Box::new(f));
        self
    }
}

impl Default for NotificationHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for NotificationHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotificationHandler")
            .field("on_progress", &self.on_progress.is_some())
            .field("on_log_message", &self.on_log_message.is_some())
            .field("on_resource_updated", &self.on_resource_updated.is_some())
            .field("on_resources_changed", &self.on_resources_changed.is_some())
            .field("on_tools_changed", &self.on_tools_changed.is_some())
            .field("on_prompts_changed", &self.on_prompts_changed.is_some())
            .finish()
    }
}

#[async_trait]
impl ClientHandler for NotificationHandler {
    async fn on_notification(&self, notification: ServerNotification) {
        match notification {
            ServerNotification::Progress(params) => {
                if let Some(cb) = &self.on_progress {
                    cb(params);
                }
            }
            ServerNotification::LogMessage(params) => {
                if let Some(cb) = &self.on_log_message {
                    cb(params);
                }
            }
            ServerNotification::ResourceUpdated { uri } => {
                if let Some(cb) = &self.on_resource_updated {
                    cb(uri);
                }
            }
            ServerNotification::ResourcesListChanged => {
                if let Some(cb) = &self.on_resources_changed {
                    cb();
                }
            }
            ServerNotification::ToolsListChanged => {
                if let Some(cb) = &self.on_tools_changed {
                    cb();
                }
            }
            ServerNotification::PromptsListChanged => {
                if let Some(cb) = &self.on_prompts_changed {
                    cb();
                }
            }
            ServerNotification::Unknown { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[tokio::test]
    async fn test_notification_handler_progress() {
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();
        let handler = NotificationHandler::new().on_progress(move |p| {
            assert!((p.progress - 0.5).abs() < f64::EPSILON);
            called_clone.store(true, Ordering::SeqCst);
        });

        handler
            .on_notification(ServerNotification::Progress(ProgressParams {
                progress_token: crate::protocol::ProgressToken::String("t1".into()),
                progress: 0.5,
                total: Some(1.0),
                message: None,
                meta: None,
            }))
            .await;

        assert!(called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_notification_handler_log_message() {
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();
        let handler = NotificationHandler::new().on_log_message(move |msg| {
            assert_eq!(msg.level, LogLevel::Info);
            called_clone.store(true, Ordering::SeqCst);
        });

        handler
            .on_notification(ServerNotification::LogMessage(LoggingMessageParams {
                level: LogLevel::Info,
                logger: Some("test".into()),
                data: serde_json::json!("hello"),
                meta: None,
            }))
            .await;

        assert!(called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_notification_handler_resource_updated() {
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();
        let handler = NotificationHandler::new().on_resource_updated(move |uri| {
            assert_eq!(uri, "file:///test.txt");
            called_clone.store(true, Ordering::SeqCst);
        });

        handler
            .on_notification(ServerNotification::ResourceUpdated {
                uri: "file:///test.txt".to_string(),
            })
            .await;

        assert!(called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_notification_handler_list_changed() {
        let tools_count = Arc::new(AtomicUsize::new(0));
        let resources_count = Arc::new(AtomicUsize::new(0));
        let prompts_count = Arc::new(AtomicUsize::new(0));

        let tc = tools_count.clone();
        let rc = resources_count.clone();
        let pc = prompts_count.clone();

        let handler = NotificationHandler::new()
            .on_tools_changed(move || {
                tc.fetch_add(1, Ordering::SeqCst);
            })
            .on_resources_changed(move || {
                rc.fetch_add(1, Ordering::SeqCst);
            })
            .on_prompts_changed(move || {
                pc.fetch_add(1, Ordering::SeqCst);
            });

        handler
            .on_notification(ServerNotification::ToolsListChanged)
            .await;
        handler
            .on_notification(ServerNotification::ResourcesListChanged)
            .await;
        handler
            .on_notification(ServerNotification::PromptsListChanged)
            .await;

        assert_eq!(tools_count.load(Ordering::SeqCst), 1);
        assert_eq!(resources_count.load(Ordering::SeqCst), 1);
        assert_eq!(prompts_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_notification_handler_unset_callbacks_are_noop() {
        // Handler with no callbacks should not panic
        let handler = NotificationHandler::new();

        handler
            .on_notification(ServerNotification::ToolsListChanged)
            .await;
        handler
            .on_notification(ServerNotification::Progress(ProgressParams {
                progress_token: crate::protocol::ProgressToken::String("t".into()),
                progress: 1.0,
                total: None,
                message: None,
                meta: None,
            }))
            .await;
        handler
            .on_notification(ServerNotification::LogMessage(LoggingMessageParams {
                level: LogLevel::Debug,
                logger: None,
                data: serde_json::json!("test"),
                meta: None,
            }))
            .await;
        handler
            .on_notification(ServerNotification::Unknown {
                method: "custom/thing".into(),
                params: None,
            })
            .await;
    }

    #[tokio::test]
    async fn test_notification_handler_rejects_requests() {
        use crate::protocol::{ElicitFormParams, ElicitFormSchema};

        let handler = NotificationHandler::new();

        let params = serde_json::from_value::<CreateMessageParams>(serde_json::json!({
            "messages": [],
            "maxTokens": 100
        }))
        .unwrap();
        let err = handler.handle_create_message(params).await.unwrap_err();
        assert_eq!(err.code, -32601); // method_not_found

        let err = handler
            .handle_elicit(ElicitRequestParams::Form(ElicitFormParams {
                mode: None,
                message: "test".into(),
                requested_schema: ElicitFormSchema {
                    schema_type: "object".into(),
                    properties: Default::default(),
                    required: vec![],
                },
                meta: None,
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn test_notification_handler_debug() {
        let handler = NotificationHandler::new().on_progress(|_| {});
        let debug = format!("{:?}", handler);
        assert!(debug.contains("on_progress: true"));
        assert!(debug.contains("on_log_message: false"));
    }

    #[test]
    fn test_notification_handler_default() {
        let _handler = NotificationHandler::default();
    }
}
