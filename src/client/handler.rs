//! Handler trait for server-initiated requests and notifications.
//!
//! The [`ClientHandler`] trait defines how the client responds to requests
//! and notifications sent by the server. All methods have default
//! implementations, so you only need to override the ones you care about.
//!
//! The unit type `()` implements this trait with all defaults, which is
//! used by [`McpClient::connect()`](super::McpClient::connect).
//!
//! # Example
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
    LoggingMessageParams, ProgressParams,
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
