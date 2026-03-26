//! MCP Client with bidirectional communication support.
//!
//! Provides [`McpClient`] for connecting to MCP servers over any
//! [`ClientTransport`]. The client runs a background message loop that
//! handles request/response correlation, server-initiated requests
//! (sampling, elicitation, roots), and notifications.
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::client::{McpClient, StdioClientTransport};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), tower_mcp::BoxError> {
//!     let transport = StdioClientTransport::spawn("my-mcp-server", &["--flag"]).await?;
//!     let client = McpClient::connect(transport).await?;
//!
//!     let server_info = client.initialize("my-client", "1.0.0").await?;
//!     println!("Connected to: {}", server_info.server_info.name);
//!
//!     let tools = client.list_tools().await?;
//!     for tool in &tools.tools {
//!         println!("Tool: {}", tool.name);
//!     }
//!
//!     let result = client.call_tool("my-tool", serde_json::json!({"arg": "value"})).await?;
//!     println!("Result: {:?}", result);
//!
//!     Ok(())
//! }
//! ```

mod channel;
mod handler;
#[cfg(feature = "http-client")]
mod http;
#[cfg(feature = "oauth-client")]
mod oauth;
#[cfg(feature = "oauth-client")]
mod oauth_authcode;
mod stdio;
mod transport;

pub use channel::ChannelTransport;
pub use handler::{ClientHandler, NotificationHandler, ServerNotification};
#[cfg(feature = "http-client")]
pub use http::{HttpClientConfig, HttpClientTransport};
#[cfg(feature = "oauth-client")]
pub use oauth::{
    OAuthClientCredentials, OAuthClientCredentialsBuilder, OAuthClientError, TokenProvider,
};
#[cfg(feature = "oauth-client")]
pub use oauth_authcode::{OAuthAuthCodeConfig, OAuthAuthorizationCode};
pub use stdio::StdioClientTransport;
pub use transport::ClientTransport;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::protocol::{
    CallToolParams, CallToolResult, ClientCapabilities, CompleteParams, CompleteResult,
    CompletionArgument, CompletionReference, ElicitationCapability, GetPromptParams,
    GetPromptResult, Implementation, InitializeParams, InitializeResult, JsonRpcNotification,
    JsonRpcRequest, ListPromptsParams, ListPromptsResult, ListResourceTemplatesParams,
    ListResourceTemplatesResult, ListResourcesParams, ListResourcesResult, ListRootsResult,
    ListToolsParams, ListToolsResult, PromptDefinition, ReadResourceParams, ReadResourceResult,
    RequestId, ResourceDefinition, ResourceTemplateDefinition, Root, RootsCapability,
    SamplingCapability, ToolDefinition, notifications,
};
use tower_mcp_types::JsonRpcError;

/// Internal command sent from McpClient methods to the background loop.
enum LoopCommand {
    /// Send a JSON-RPC request and await a response.
    Request {
        method: String,
        params: serde_json::Value,
        response_tx: oneshot::Sender<Result<serde_json::Value>>,
    },
    /// Send a JSON-RPC notification (no response expected).
    Notify {
        method: String,
        params: serde_json::Value,
    },
    /// Reset the transport's session state for re-initialization.
    ResetSession { done_tx: oneshot::Sender<()> },
    /// Graceful shutdown.
    Shutdown,
}

/// MCP client with a background message loop.
///
/// Unlike previous versions, this type is not generic over the transport.
/// The transport is consumed during [`connect()`](Self::connect) and moved
/// into a background Tokio task that handles message multiplexing.
///
/// All public methods take `&self`, enabling concurrent use from multiple
/// tasks.
///
/// # Construction
///
/// ```rust,no_run
/// use tower_mcp::client::{McpClient, StdioClientTransport};
///
/// # async fn example() -> Result<(), tower_mcp::BoxError> {
/// // Simple: no handler for server-initiated requests
/// let transport = StdioClientTransport::spawn("server", &[]).await?;
/// let client = McpClient::connect(transport).await?;
///
/// // With configuration
/// use tower_mcp::protocol::Root;
/// let transport = StdioClientTransport::spawn("server", &[]).await?;
/// let client = McpClient::builder()
///     .with_roots(vec![Root::new("file:///project")])
///     .connect_simple(transport)
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct McpClient {
    /// Channel to send commands to the background loop.
    command_tx: mpsc::Sender<LoopCommand>,
    /// Background task handle.
    task: Option<JoinHandle<()>>,
    /// Whether `initialize()` has been called successfully.
    initialized: AtomicBool,
    /// Server info (set after successful initialization).
    server_info: RwLock<Option<InitializeResult>>,
    /// Client capabilities declared during initialization.
    capabilities: ClientCapabilities,
    /// Current roots (shared with the loop for roots/list responses).
    roots: Arc<RwLock<Vec<Root>>>,
    /// Whether the transport is still connected.
    connected: Arc<AtomicBool>,
    /// Whether the transport supports session recovery.
    supports_session_recovery: bool,
    /// Stored init params for session recovery re-initialization.
    init_params: RwLock<Option<(String, String)>>,
    /// Lock to prevent concurrent session recovery attempts.
    recovery_lock: Mutex<()>,
}

/// Builder for configuring and connecting an [`McpClient`].
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::client::{McpClient, StdioClientTransport};
/// use tower_mcp::protocol::Root;
///
/// # async fn example() -> Result<(), tower_mcp::BoxError> {
/// let transport = StdioClientTransport::spawn("server", &[]).await?;
/// let handler = (); // Use a real ClientHandler for bidirectional support
/// let client = McpClient::builder()
///     .with_roots(vec![Root::new("file:///project")])
///     .with_sampling()
///     .connect(transport, handler)
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct McpClientBuilder {
    capabilities: ClientCapabilities,
    roots: Vec<Root>,
}

impl McpClientBuilder {
    /// Create a new builder with default settings.
    pub fn new() -> Self {
        Self {
            capabilities: ClientCapabilities::default(),
            roots: Vec::new(),
        }
    }

    /// Configure roots for this client.
    ///
    /// The client will declare roots support during initialization and
    /// respond to `roots/list` requests with these roots.
    pub fn with_roots(mut self, roots: Vec<Root>) -> Self {
        self.roots = roots;
        self.capabilities.roots = Some(RootsCapability { list_changed: true });
        self
    }

    /// Configure custom capabilities for this client.
    pub fn with_capabilities(mut self, capabilities: ClientCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Declare sampling support.
    ///
    /// Sets the sampling capability so the server knows this client can
    /// handle `sampling/createMessage` requests. The handler passed to
    /// [`connect()`](Self::connect) should override
    /// [`handle_create_message()`](ClientHandler::handle_create_message).
    pub fn with_sampling(mut self) -> Self {
        self.capabilities.sampling = Some(SamplingCapability::default());
        self
    }

    /// Declare elicitation support.
    ///
    /// Sets the elicitation capability so the server knows this client can
    /// handle `elicitation/create` requests. The handler passed to
    /// [`connect()`](Self::connect) should override
    /// [`handle_elicit()`](ClientHandler::handle_elicit).
    pub fn with_elicitation(mut self) -> Self {
        self.capabilities.elicitation = Some(ElicitationCapability::default());
        self
    }

    /// Connect to a server using the given transport and handler.
    ///
    /// Spawns a background task to handle message I/O. The transport is
    /// consumed and owned by the background task.
    pub async fn connect<T, H>(self, transport: T, handler: H) -> Result<McpClient>
    where
        T: ClientTransport,
        H: ClientHandler,
    {
        McpClient::connect_inner(transport, handler, self.capabilities, self.roots).await
    }

    /// Connect to a server without a handler.
    ///
    /// All server-initiated requests will be rejected with `method_not_found`.
    pub async fn connect_simple<T: ClientTransport>(self, transport: T) -> Result<McpClient> {
        self.connect(transport, ()).await
    }
}

impl Default for McpClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl McpClient {
    /// Connect with default settings and no handler.
    ///
    /// Shorthand for `McpClient::builder().connect_simple(transport)`.
    pub async fn connect<T: ClientTransport>(transport: T) -> Result<Self> {
        McpClientBuilder::new().connect_simple(transport).await
    }

    /// Connect with a handler for server-initiated requests.
    pub async fn connect_with_handler<T, H>(transport: T, handler: H) -> Result<Self>
    where
        T: ClientTransport,
        H: ClientHandler,
    {
        McpClientBuilder::new().connect(transport, handler).await
    }

    /// Create a builder for advanced configuration.
    pub fn builder() -> McpClientBuilder {
        McpClientBuilder::new()
    }

    /// Internal connect implementation.
    async fn connect_inner<T, H>(
        transport: T,
        handler: H,
        capabilities: ClientCapabilities,
        roots: Vec<Root>,
    ) -> Result<Self>
    where
        T: ClientTransport,
        H: ClientHandler,
    {
        let supports_session_recovery = transport.supports_session_recovery();
        let (command_tx, command_rx) = mpsc::channel::<LoopCommand>(64);
        let connected = Arc::new(AtomicBool::new(true));
        let roots = Arc::new(RwLock::new(roots));

        let loop_connected = connected.clone();
        let loop_roots = roots.clone();

        let task = tokio::spawn(async move {
            message_loop(transport, handler, command_rx, loop_connected, loop_roots).await;
        });

        Ok(Self {
            command_tx,
            task: Some(task),
            initialized: AtomicBool::new(false),
            server_info: RwLock::new(None),
            capabilities,
            roots,
            connected,
            supports_session_recovery,
            init_params: RwLock::new(None),
            recovery_lock: Mutex::new(()),
        })
    }

    /// Check if the client has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Check if the transport is still connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Get the server info (available after initialization).
    pub async fn server_info(&self) -> Option<InitializeResult> {
        self.server_info.read().await.clone()
    }

    /// Get the server info synchronously (best-effort, non-blocking).
    ///
    /// Returns `None` if the lock is currently held by a writer or if
    /// initialization hasn't completed. Prefer [`server_info()`](Self::server_info)
    /// in async contexts.
    pub fn server_info_blocking(&self) -> Option<InitializeResult> {
        self.server_info.try_read().ok()?.clone()
    }

    /// Initialize the MCP connection.
    ///
    /// Sends the `initialize` request and `notifications/initialized` notification.
    /// Must be called before any other operations.
    pub async fn initialize(
        &self,
        client_name: &str,
        client_version: &str,
    ) -> Result<InitializeResult> {
        let params = InitializeParams {
            protocol_version: crate::protocol::LATEST_PROTOCOL_VERSION.to_string(),
            capabilities: self.capabilities.clone(),
            client_info: Implementation {
                name: client_name.to_string(),
                version: client_version.to_string(),
                ..Default::default()
            },
            meta: None,
        };

        let result: InitializeResult = self.send_request("initialize", &params).await?;
        *self.server_info.write().await = Some(result.clone());

        // Store init params for potential session recovery
        *self.init_params.write().await =
            Some((client_name.to_string(), client_version.to_string()));

        // Send initialized notification
        self.send_notification("notifications/initialized", &serde_json::json!({}))
            .await?;
        self.initialized.store(true, Ordering::Release);

        Ok(result)
    }

    /// List available tools.
    pub async fn list_tools(&self) -> Result<ListToolsResult> {
        self.ensure_initialized()?;
        self.send_request(
            "tools/list",
            &ListToolsParams {
                cursor: None,
                meta: None,
            },
        )
        .await
    }

    /// Call a tool.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult> {
        self.ensure_initialized()?;
        let params = CallToolParams {
            name: name.to_string(),
            arguments,
            meta: None,
            task: None,
        };
        self.send_request("tools/call", &params).await
    }

    /// List available resources.
    pub async fn list_resources(&self) -> Result<ListResourcesResult> {
        self.ensure_initialized()?;
        self.send_request(
            "resources/list",
            &ListResourcesParams {
                cursor: None,
                meta: None,
            },
        )
        .await
    }

    /// Read a resource.
    pub async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult> {
        self.ensure_initialized()?;
        let params = ReadResourceParams {
            uri: uri.to_string(),
            meta: None,
        };
        self.send_request("resources/read", &params).await
    }

    /// List available prompts.
    pub async fn list_prompts(&self) -> Result<ListPromptsResult> {
        self.ensure_initialized()?;
        self.send_request(
            "prompts/list",
            &ListPromptsParams {
                cursor: None,
                meta: None,
            },
        )
        .await
    }

    /// List tools with an optional pagination cursor.
    pub async fn list_tools_with_cursor(&self, cursor: Option<String>) -> Result<ListToolsResult> {
        self.ensure_initialized()?;
        self.send_request("tools/list", &ListToolsParams { cursor, meta: None })
            .await
    }

    /// List resources with an optional pagination cursor.
    pub async fn list_resources_with_cursor(
        &self,
        cursor: Option<String>,
    ) -> Result<ListResourcesResult> {
        self.ensure_initialized()?;
        self.send_request(
            "resources/list",
            &ListResourcesParams { cursor, meta: None },
        )
        .await
    }

    /// List resource templates.
    pub async fn list_resource_templates(&self) -> Result<ListResourceTemplatesResult> {
        self.ensure_initialized()?;
        self.send_request(
            "resources/templates/list",
            &ListResourceTemplatesParams {
                cursor: None,
                meta: None,
            },
        )
        .await
    }

    /// List resource templates with an optional pagination cursor.
    pub async fn list_resource_templates_with_cursor(
        &self,
        cursor: Option<String>,
    ) -> Result<ListResourceTemplatesResult> {
        self.ensure_initialized()?;
        self.send_request(
            "resources/templates/list",
            &ListResourceTemplatesParams { cursor, meta: None },
        )
        .await
    }

    /// List prompts with an optional pagination cursor.
    pub async fn list_prompts_with_cursor(
        &self,
        cursor: Option<String>,
    ) -> Result<ListPromptsResult> {
        self.ensure_initialized()?;
        self.send_request("prompts/list", &ListPromptsParams { cursor, meta: None })
            .await
    }

    /// List all tools, following pagination cursors until exhausted.
    pub async fn list_all_tools(&self) -> Result<Vec<ToolDefinition>> {
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let result = self.list_tools_with_cursor(cursor).await?;
            all.extend(result.tools);
            match result.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(all)
    }

    /// List all resources, following pagination cursors until exhausted.
    pub async fn list_all_resources(&self) -> Result<Vec<ResourceDefinition>> {
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let result = self.list_resources_with_cursor(cursor).await?;
            all.extend(result.resources);
            match result.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(all)
    }

    /// List all resource templates, following pagination cursors until exhausted.
    pub async fn list_all_resource_templates(&self) -> Result<Vec<ResourceTemplateDefinition>> {
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let result = self.list_resource_templates_with_cursor(cursor).await?;
            all.extend(result.resource_templates);
            match result.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(all)
    }

    /// List all prompts, following pagination cursors until exhausted.
    pub async fn list_all_prompts(&self) -> Result<Vec<PromptDefinition>> {
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let result = self.list_prompts_with_cursor(cursor).await?;
            all.extend(result.prompts);
            match result.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(all)
    }

    /// Call a tool and return the concatenated text content.
    ///
    /// Returns the text from all [`Text`](crate::protocol::Content::Text) items joined together.
    /// If the tool result indicates an error (`is_error` is true), returns
    /// an error with the text content as the message.
    ///
    /// For more control over the result, use [`call_tool()`](Self::call_tool).
    pub async fn call_tool_text(&self, name: &str, arguments: serde_json::Value) -> Result<String> {
        let result = self.call_tool(name, arguments).await?;
        if result.is_error {
            return Err(Error::Internal(result.all_text()));
        }
        Ok(result.all_text())
    }

    /// Get a prompt.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> Result<GetPromptResult> {
        self.ensure_initialized()?;
        let params = GetPromptParams {
            name: name.to_string(),
            arguments: arguments.unwrap_or_default(),
            meta: None,
        };
        self.send_request("prompts/get", &params).await
    }

    /// Ping the server.
    pub async fn ping(&self) -> Result<()> {
        let _: serde_json::Value = self.send_request("ping", &serde_json::json!({})).await?;
        Ok(())
    }

    /// Request completion suggestions from the server.
    pub async fn complete(
        &self,
        reference: CompletionReference,
        argument_name: &str,
        argument_value: &str,
    ) -> Result<CompleteResult> {
        self.ensure_initialized()?;
        let params = CompleteParams {
            reference,
            argument: CompletionArgument::new(argument_name, argument_value),
            context: None,
            meta: None,
        };
        self.send_request("completion/complete", &params).await
    }

    /// Request completion for a prompt argument.
    pub async fn complete_prompt_arg(
        &self,
        prompt_name: &str,
        argument_name: &str,
        argument_value: &str,
    ) -> Result<CompleteResult> {
        self.complete(
            CompletionReference::prompt(prompt_name),
            argument_name,
            argument_value,
        )
        .await
    }

    /// Request completion for a resource URI.
    pub async fn complete_resource_uri(
        &self,
        resource_uri: &str,
        argument_name: &str,
        argument_value: &str,
    ) -> Result<CompleteResult> {
        self.complete(
            CompletionReference::resource(resource_uri),
            argument_name,
            argument_value,
        )
        .await
    }

    /// Send a raw typed request to the server.
    pub async fn request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        self.send_request(method, params).await
    }

    /// Send a raw typed notification to the server.
    pub async fn notify<P: serde::Serialize>(&self, method: &str, params: &P) -> Result<()> {
        self.send_notification(method, params).await
    }

    /// Get the current roots.
    pub async fn roots(&self) -> Vec<Root> {
        self.roots.read().await.clone()
    }

    /// Set roots and notify the server if initialized.
    pub async fn set_roots(&self, roots: Vec<Root>) -> Result<()> {
        *self.roots.write().await = roots;
        if self.is_initialized() {
            self.send_notification(notifications::ROOTS_LIST_CHANGED, &serde_json::json!({}))
                .await?;
        }
        Ok(())
    }

    /// Add a root and notify the server if initialized.
    pub async fn add_root(&self, root: Root) -> Result<()> {
        self.roots.write().await.push(root);
        if self.is_initialized() {
            self.send_notification(notifications::ROOTS_LIST_CHANGED, &serde_json::json!({}))
                .await?;
        }
        Ok(())
    }

    /// Remove a root by URI and notify the server if initialized.
    pub async fn remove_root(&self, uri: &str) -> Result<bool> {
        let mut roots = self.roots.write().await;
        let initial_len = roots.len();
        roots.retain(|r| r.uri != uri);
        let removed = roots.len() < initial_len;
        drop(roots);

        if removed && self.is_initialized() {
            self.send_notification(notifications::ROOTS_LIST_CHANGED, &serde_json::json!({}))
                .await?;
        }
        Ok(removed)
    }

    /// Get the roots list result (for responding to server's roots/list request).
    pub async fn list_roots(&self) -> ListRootsResult {
        ListRootsResult {
            roots: self.roots.read().await.clone(),
            meta: None,
        }
    }

    /// Gracefully shut down the client and close the transport.
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.command_tx.send(LoopCommand::Shutdown).await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        Ok(())
    }

    // --- Internal helpers ---

    async fn send_request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        match self.send_request_once(method, params).await {
            Err(Error::SessionExpired)
                if self.supports_session_recovery && method != "initialize" =>
            {
                tracing::info!(method = %method, "Session expired, attempting recovery");
                self.recover_session().await?;
                self.send_request_once(method, params).await
            }
            other => other,
        }
    }

    async fn send_request_once<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        self.ensure_connected()?;
        let params_value = serde_json::to_value(params)
            .map_err(|e| Error::Transport(format!("Failed to serialize params: {}", e)))?;

        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(LoopCommand::Request {
                method: method.to_string(),
                params: params_value,
                response_tx,
            })
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))?;

        let result = response_rx
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))??;

        serde_json::from_value(result)
            .map_err(|e| Error::Transport(format!("Failed to deserialize response: {}", e)))
    }

    /// Recover from a session expiry by resetting the transport and re-initializing.
    async fn recover_session(&self) -> Result<()> {
        // Serialize recovery attempts
        let _guard = self.recovery_lock.lock().await;

        // Check if another task already recovered while we waited
        // (the init_params being present means we were initialized before)
        let init_params = self.init_params.read().await.clone();
        let (client_name, client_version) = match init_params {
            Some(params) => params,
            None => {
                return Err(Error::Transport(
                    "Cannot recover: never initialized".to_string(),
                ));
            }
        };

        // Tell the message loop to reset the transport
        let (done_tx, done_rx) = oneshot::channel();
        self.command_tx
            .send(LoopCommand::ResetSession { done_tx })
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))?;
        done_rx
            .await
            .map_err(|_| Error::Transport("Connection closed during recovery".to_string()))?;

        // Clear initialized state
        self.initialized.store(false, Ordering::Release);
        *self.server_info.write().await = None;

        // Re-initialize (using send_request_once to avoid recursion)
        tracing::info!("Re-initializing session after expiry");
        let params = InitializeParams {
            protocol_version: crate::protocol::LATEST_PROTOCOL_VERSION.to_string(),
            capabilities: self.capabilities.clone(),
            client_info: Implementation {
                name: client_name,
                version: client_version,
                ..Default::default()
            },
            meta: None,
        };

        let result: InitializeResult = self.send_request_once("initialize", &params).await?;
        *self.server_info.write().await = Some(result);

        self.send_notification("notifications/initialized", &serde_json::json!({}))
            .await?;
        self.initialized.store(true, Ordering::Release);

        Ok(())
    }

    async fn send_notification<P: serde::Serialize>(&self, method: &str, params: &P) -> Result<()> {
        self.ensure_connected()?;
        let params_value = serde_json::to_value(params)
            .map_err(|e| Error::Transport(format!("Failed to serialize params: {}", e)))?;

        self.command_tx
            .send(LoopCommand::Notify {
                method: method.to_string(),
                params: params_value,
            })
            .await
            .map_err(|_| Error::Transport("Connection closed".to_string()))?;

        Ok(())
    }

    fn ensure_connected(&self) -> Result<()> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(Error::Transport("Connection closed".to_string()));
        }
        Ok(())
    }

    fn ensure_initialized(&self) -> Result<()> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(Error::Transport("Client not initialized".to_string()));
        }
        Ok(())
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// =============================================================================
// Background Message Loop
// =============================================================================

/// A pending request waiting for a response from the server.
struct PendingRequest {
    response_tx: oneshot::Sender<Result<serde_json::Value>>,
}

/// Background message loop that multiplexes incoming/outgoing messages.
async fn message_loop<T: ClientTransport, H: ClientHandler>(
    mut transport: T,
    handler: H,
    mut command_rx: mpsc::Receiver<LoopCommand>,
    connected: Arc<AtomicBool>,
    roots: Arc<RwLock<Vec<Root>>>,
) {
    let handler = Arc::new(handler);
    let mut pending_requests: HashMap<RequestId, PendingRequest> = HashMap::new();
    let next_id = AtomicI64::new(1);

    loop {
        tokio::select! {
            // Commands from McpClient methods
            command = command_rx.recv() => {
                match command {
                    Some(LoopCommand::Request { method, params, response_tx }) => {
                        let id = RequestId::Number(next_id.fetch_add(1, Ordering::Relaxed));

                        let request = JsonRpcRequest::new(id.clone(), &method)
                            .with_params(params);
                        let json = match serde_json::to_string(&request) {
                            Ok(j) => j,
                            Err(e) => {
                                let _ = response_tx.send(Err(Error::Transport(
                                    format!("Serialization failed: {}", e)
                                )));
                                continue;
                            }
                        };

                        tracing::debug!(method = %method, id = ?id, "Sending request");
                        pending_requests.insert(id, PendingRequest { response_tx });

                        if let Err(e) = transport.send(&json).await {
                            tracing::error!(error = %e, "Transport send error");
                            fail_all_pending(&mut pending_requests, &format!("Transport error: {}", e));
                            break;
                        }
                    }
                    Some(LoopCommand::Notify { method, params }) => {
                        let notification = JsonRpcNotification::new(&method)
                            .with_params(params);
                        if let Ok(json) = serde_json::to_string(&notification) {
                            tracing::debug!(method = %method, "Sending notification");
                            let _ = transport.send(&json).await;
                        }
                    }
                    Some(LoopCommand::ResetSession { done_tx }) => {
                        tracing::info!("Resetting transport session for re-initialization");
                        transport.reset_session().await;
                        // Fail any pending requests with session expired
                        for (_, pending) in pending_requests.drain() {
                            let _ = pending.response_tx.send(Err(Error::SessionExpired));
                        }
                        let _ = done_tx.send(());
                    }
                    Some(LoopCommand::Shutdown) | None => {
                        tracing::debug!("Message loop shutting down");
                        break;
                    }
                }
            }

            // Incoming messages from the server
            result = transport.recv() => {
                match result {
                    Ok(Some(line)) => {
                        handle_incoming(
                            &line,
                            &mut pending_requests,
                            &handler,
                            &roots,
                            &mut transport,
                        ).await;
                    }
                    Ok(None) => {
                        tracing::info!("Transport closed (EOF)");
                        break;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Transport receive error");
                        break;
                    }
                }
            }
        }
    }

    // Cleanup
    connected.store(false, Ordering::Release);
    fail_all_pending(&mut pending_requests, "Connection closed");
    let _ = transport.close().await;
}

/// Handle a single incoming message from the server.
async fn handle_incoming<T: ClientTransport, H: ClientHandler>(
    line: &str,
    pending_requests: &mut HashMap<RequestId, PendingRequest>,
    handler: &Arc<H>,
    roots: &Arc<RwLock<Vec<Root>>>,
    transport: &mut T,
) {
    let parsed: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to parse incoming message");
            return;
        }
    };

    // Case 1: Response to one of our pending requests (has result or error, no method)
    if parsed.get("method").is_none()
        && (parsed.get("result").is_some() || parsed.get("error").is_some())
    {
        // Check for session-level errors (id: null with -32005) that affect
        // all pending requests, not just a specific one.
        if let Some(error) = parsed.get("error") {
            let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0) as i32;
            let id_missing_or_null = parsed.get("id").is_none_or(|id| id.is_null());
            if code == -32005 && id_missing_or_null {
                tracing::warn!(
                    "Session expired (-32005 with null id), failing all pending requests"
                );
                for (_, pending) in pending_requests.drain() {
                    let _ = pending.response_tx.send(Err(Error::SessionExpired));
                }
                return;
            }
        }

        handle_response(&parsed, pending_requests);
        return;
    }

    // Case 2: Server-initiated request (has id + method)
    if parsed.get("id").is_some() && parsed.get("method").is_some() {
        let id = parse_request_id(&parsed);
        let method = parsed["method"].as_str().unwrap_or("");
        let params = parsed.get("params").cloned();

        let result = dispatch_server_request(handler, roots, method, params).await;

        // Send response back to the server
        let response = match result {
            Ok(value) => {
                if let Some(id) = id {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": value
                    })
                } else {
                    return;
                }
            }
            Err(error) => {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": error.code,
                        "message": error.message
                    }
                })
            }
        };

        if let Ok(json) = serde_json::to_string(&response) {
            let _ = transport.send(&json).await;
        }
        return;
    }

    // Case 3: Server notification (has method, no id)
    if parsed.get("method").is_some() && parsed.get("id").is_none() {
        let method = parsed["method"].as_str().unwrap_or("");
        let params = parsed.get("params").cloned();
        let notification = parse_server_notification(method, params);
        handler.on_notification(notification).await;
    }
}

/// Handle a JSON-RPC response by routing to the pending request.
fn handle_response(
    parsed: &serde_json::Value,
    pending_requests: &mut HashMap<RequestId, PendingRequest>,
) {
    let id = match parse_request_id(parsed) {
        Some(id) => id,
        None => {
            tracing::warn!("Response without id");
            return;
        }
    };

    let pending = match pending_requests.remove(&id) {
        Some(p) => p,
        None => {
            tracing::warn!(id = ?id, "Response for unknown request");
            return;
        }
    };

    tracing::debug!(id = ?id, "Received response");

    if let Some(error) = parsed.get("error") {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-1) as i32;
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown error")
            .to_string();
        let data = error.get("data").cloned();

        // -32005 = SessionNotFound: signal session expiry for recovery
        if code == -32005 {
            let _ = pending.response_tx.send(Err(Error::SessionExpired));
            return;
        }

        let json_rpc_error = JsonRpcError {
            code,
            message,
            data,
        };
        let _ = pending
            .response_tx
            .send(Err(Error::JsonRpc(json_rpc_error)));
    } else if let Some(result) = parsed.get("result") {
        let _ = pending.response_tx.send(Ok(result.clone()));
    } else {
        let _ = pending
            .response_tx
            .send(Err(Error::Transport("Invalid response".to_string())));
    }
}

/// Dispatch a server-initiated request to the handler.
async fn dispatch_server_request<H: ClientHandler>(
    handler: &Arc<H>,
    roots: &Arc<RwLock<Vec<Root>>>,
    method: &str,
    params: Option<serde_json::Value>,
) -> std::result::Result<serde_json::Value, JsonRpcError> {
    match method {
        "sampling/createMessage" => {
            let p = serde_json::from_value(params.unwrap_or_default())
                .map_err(|e| JsonRpcError::invalid_params(e.to_string()))?;
            let result = handler.handle_create_message(p).await?;
            serde_json::to_value(result).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        }
        "elicitation/create" => {
            let p = serde_json::from_value(params.unwrap_or_default())
                .map_err(|e| JsonRpcError::invalid_params(e.to_string()))?;
            let result = handler.handle_elicit(p).await?;
            serde_json::to_value(result).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        }
        "roots/list" => {
            // Use client-configured roots if available, otherwise delegate to handler
            let roots_list = roots.read().await;
            if !roots_list.is_empty() {
                let result = ListRootsResult {
                    roots: roots_list.clone(),
                    meta: None,
                };
                return serde_json::to_value(result)
                    .map_err(|e| JsonRpcError::internal_error(e.to_string()));
            }
            drop(roots_list);

            let result = handler.handle_list_roots().await?;
            serde_json::to_value(result).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        }
        "ping" => Ok(serde_json::json!({})),
        _ => Err(JsonRpcError::method_not_found(method)),
    }
}

/// Parse a request ID from a JSON-RPC message.
fn parse_request_id(parsed: &serde_json::Value) -> Option<RequestId> {
    parsed.get("id").and_then(|id| {
        if let Some(n) = id.as_i64() {
            Some(RequestId::Number(n))
        } else {
            id.as_str().map(|s| RequestId::String(s.to_string()))
        }
    })
}

/// Parse a server notification into the typed enum.
fn parse_server_notification(
    method: &str,
    params: Option<serde_json::Value>,
) -> ServerNotification {
    match method {
        notifications::PROGRESS => {
            if let Some(params) = params
                && let Ok(p) = serde_json::from_value(params)
            {
                return ServerNotification::Progress(p);
            }
            ServerNotification::Unknown {
                method: method.to_string(),
                params: None,
            }
        }
        notifications::MESSAGE => {
            if let Some(params) = params
                && let Ok(p) = serde_json::from_value(params)
            {
                return ServerNotification::LogMessage(p);
            }
            ServerNotification::Unknown {
                method: method.to_string(),
                params: None,
            }
        }
        notifications::RESOURCE_UPDATED => {
            if let Some(params) = &params
                && let Some(uri) = params.get("uri").and_then(|u| u.as_str())
            {
                return ServerNotification::ResourceUpdated {
                    uri: uri.to_string(),
                };
            }
            ServerNotification::Unknown {
                method: method.to_string(),
                params,
            }
        }
        notifications::RESOURCES_LIST_CHANGED => ServerNotification::ResourcesListChanged,
        notifications::TOOLS_LIST_CHANGED => ServerNotification::ToolsListChanged,
        notifications::PROMPTS_LIST_CHANGED => ServerNotification::PromptsListChanged,
        _ => ServerNotification::Unknown {
            method: method.to_string(),
            params,
        },
    }
}

/// Fail all pending requests with the given error message.
fn fail_all_pending(pending: &mut HashMap<RequestId, PendingRequest>, reason: &str) {
    for (_, req) in pending.drain() {
        let _ = req
            .response_tx
            .send(Err(Error::Transport(reason.to_string())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Mock transport for testing that auto-responds to requests.
    ///
    /// When the client sends a request via `send()`, the mock extracts the
    /// request ID, pairs it with the next preconfigured response, and feeds
    /// it back through a channel that `recv()` awaits on. This ensures
    /// `recv()` blocks when no messages are available (instead of returning
    /// EOF), keeping the background message loop alive.
    struct MockTransport {
        /// Pre-configured response payloads (result values, not full envelopes).
        responses: Arc<Mutex<Vec<serde_json::Value>>>,
        /// Index of the next response to use.
        response_idx: Arc<std::sync::atomic::AtomicUsize>,
        /// Channel sender for feeding responses back to `recv()`.
        incoming_tx: mpsc::Sender<String>,
        /// Channel receiver for `recv()` to await on.
        incoming_rx: mpsc::Receiver<String>,
        /// Collected outgoing messages from `send()`.
        outgoing: Arc<Mutex<Vec<String>>>,
        connected: Arc<AtomicBool>,
    }

    #[allow(dead_code)]
    impl MockTransport {
        fn new() -> Self {
            let (tx, rx) = mpsc::channel(32);
            Self {
                responses: Arc::new(Mutex::new(Vec::new())),
                response_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                incoming_tx: tx,
                incoming_rx: rx,
                outgoing: Arc::new(Mutex::new(Vec::new())),
                connected: Arc::new(AtomicBool::new(true)),
            }
        }

        /// Create a mock that auto-responds with the given result payloads.
        ///
        /// When `send()` receives a JSON-RPC request, it extracts the request
        /// ID and pairs it with the next response from this list, sending the
        /// complete JSON-RPC response through the channel for `recv()`.
        fn with_responses(responses: Vec<serde_json::Value>) -> Self {
            let (tx, rx) = mpsc::channel(32);
            Self {
                responses: Arc::new(Mutex::new(responses)),
                response_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                incoming_tx: tx,
                incoming_rx: rx,
                outgoing: Arc::new(Mutex::new(Vec::new())),
                connected: Arc::new(AtomicBool::new(true)),
            }
        }
    }

    #[async_trait]
    impl ClientTransport for MockTransport {
        async fn send(&mut self, message: &str) -> Result<()> {
            self.outgoing.lock().unwrap().push(message.to_string());

            // Parse the outgoing message to extract the request ID
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(message) {
                // Only respond to requests (messages with an id and method)
                if let Some(id) = parsed.get("id") {
                    let idx = self
                        .response_idx
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let responses = self.responses.lock().unwrap();
                    if let Some(result) = responses.get(idx) {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": result
                        });
                        let _ = self.incoming_tx.try_send(response.to_string());
                    }
                }
            }

            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<String>> {
            // Await on the channel -- blocks until a message is available
            // or the sender is dropped (returns None = EOF).
            match self.incoming_rx.recv().await {
                Some(msg) => Ok(Some(msg)),
                None => Ok(None),
            }
        }

        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::Relaxed)
        }

        async fn close(&mut self) -> Result<()> {
            self.connected.store(false, Ordering::Relaxed);
            Ok(())
        }
    }

    fn mock_initialize_response() -> serde_json::Value {
        serde_json::json!({
            "protocolVersion": "2025-11-25",
            "serverInfo": {
                "name": "test-server",
                "version": "1.0.0"
            },
            "capabilities": {
                "tools": {}
            }
        })
    }

    #[tokio::test]
    async fn test_client_not_initialized() {
        let client = McpClient::connect(MockTransport::with_responses(vec![]))
            .await
            .unwrap();

        let result = client.list_tools().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[tokio::test]
    async fn test_client_initialize() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
        ]))
        .await
        .unwrap();

        assert!(!client.is_initialized());

        let result = client.initialize("test-client", "1.0.0").await;
        assert!(result.is_ok());
        assert!(client.is_initialized());

        let server_info = client.server_info().await.unwrap();
        assert_eq!(server_info.server_info.name, "test-server");
    }

    #[tokio::test]
    async fn test_list_tools() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "tools": [
                    {
                        "name": "test_tool",
                        "description": "A test tool",
                        "inputSchema": {
                            "type": "object",
                            "properties": {}
                        }
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let tools = client.list_tools().await.unwrap();

        assert_eq!(tools.tools.len(), 1);
        assert_eq!(tools.tools[0].name, "test_tool");
    }

    #[tokio::test]
    async fn test_call_tool() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "content": [
                    {
                        "type": "text",
                        "text": "Tool result"
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client
            .call_tool("test_tool", serde_json::json!({"arg": "value"}))
            .await
            .unwrap();

        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn test_list_resources() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "resources": [
                    {
                        "uri": "file://test.txt",
                        "name": "Test File"
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let resources = client.list_resources().await.unwrap();

        assert_eq!(resources.resources.len(), 1);
        assert_eq!(resources.resources[0].uri, "file://test.txt");
    }

    #[tokio::test]
    async fn test_read_resource() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "contents": [
                    {
                        "uri": "file://test.txt",
                        "text": "File contents"
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client.read_resource("file://test.txt").await.unwrap();

        assert_eq!(result.contents.len(), 1);
        assert_eq!(result.contents[0].text.as_deref(), Some("File contents"));
    }

    #[tokio::test]
    async fn test_list_prompts() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "prompts": [
                    {
                        "name": "test_prompt",
                        "description": "A test prompt"
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let prompts = client.list_prompts().await.unwrap();

        assert_eq!(prompts.prompts.len(), 1);
        assert_eq!(prompts.prompts[0].name, "test_prompt");
    }

    #[tokio::test]
    async fn test_get_prompt() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "messages": [
                    {
                        "role": "user",
                        "content": {
                            "type": "text",
                            "text": "Prompt message"
                        }
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client.get_prompt("test_prompt", None).await.unwrap();

        assert_eq!(result.messages.len(), 1);
    }

    #[tokio::test]
    async fn test_ping() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({}),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client.ping().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_with_roots() {
        let roots = vec![Root::new("file:///test")];
        let client = McpClient::builder()
            .with_roots(roots)
            .connect_simple(MockTransport::with_responses(vec![]))
            .await
            .unwrap();

        let current_roots = client.roots().await;
        assert_eq!(current_roots.len(), 1);
    }

    #[tokio::test]
    async fn test_roots_management() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
        ]))
        .await
        .unwrap();

        // Initially no roots
        assert!(client.roots().await.is_empty());

        // Add a root before initialization (no notification sent)
        client.add_root(Root::new("file:///project")).await.unwrap();
        assert_eq!(client.roots().await.len(), 1);

        // Initialize
        client.initialize("test-client", "1.0.0").await.unwrap();

        // Remove a root
        let removed = client.remove_root("file:///project").await.unwrap();
        assert!(removed);
        assert!(client.roots().await.is_empty());

        // Try to remove non-existent root
        let not_removed = client.remove_root("file:///nonexistent").await.unwrap();
        assert!(!not_removed);
    }

    #[tokio::test]
    async fn test_list_roots() {
        let roots = vec![
            Root::new("file:///project1"),
            Root::with_name("file:///project2", "Project 2"),
        ];
        let client = McpClient::builder()
            .with_roots(roots)
            .connect_simple(MockTransport::with_responses(vec![]))
            .await
            .unwrap();

        let result = client.list_roots().await;
        assert_eq!(result.roots.len(), 2);
        assert_eq!(result.roots[1].name, Some("Project 2".to_string()));
    }

    #[test]
    fn test_builder_with_sampling() {
        let builder = McpClientBuilder::new().with_sampling();
        assert!(builder.capabilities.sampling.is_some());
    }

    #[test]
    fn test_builder_with_elicitation() {
        let builder = McpClientBuilder::new().with_elicitation();
        assert!(builder.capabilities.elicitation.is_some());
    }

    #[test]
    fn test_builder_chaining() {
        let builder = McpClientBuilder::new()
            .with_sampling()
            .with_elicitation()
            .with_roots(vec![Root::new("file:///project")]);
        assert!(builder.capabilities.sampling.is_some());
        assert!(builder.capabilities.elicitation.is_some());
        assert!(builder.capabilities.roots.is_some());
    }

    #[tokio::test]
    async fn test_bidirectional_sampling_round_trip() {
        use crate::protocol::{
            ContentRole, CreateMessageParams, CreateMessageResult, SamplingContent,
            SamplingContentOrArray,
        };

        // A handler that records whether handle_create_message was called
        struct RecordingHandler {
            called: Arc<AtomicBool>,
        }

        #[async_trait]
        impl ClientHandler for RecordingHandler {
            async fn handle_create_message(
                &self,
                _params: CreateMessageParams,
            ) -> std::result::Result<CreateMessageResult, tower_mcp_types::JsonRpcError>
            {
                self.called.store(true, Ordering::SeqCst);
                Ok(CreateMessageResult {
                    content: SamplingContentOrArray::Single(SamplingContent::Text {
                        text: "test response".to_string(),
                        annotations: None,
                        meta: None,
                    }),
                    model: "test-model".to_string(),
                    role: ContentRole::Assistant,
                    stop_reason: Some("end_turn".to_string()),
                    meta: None,
                })
            }
        }

        let called = Arc::new(AtomicBool::new(false));
        let handler = RecordingHandler {
            called: called.clone(),
        };

        // Build a mock transport, keeping a clone of incoming_tx so we can
        // inject a server-initiated request after the transport is consumed.
        let (inject_tx, rx) = mpsc::channel::<String>(32);
        let responses = vec![mock_initialize_response()];
        let inject_tx_clone = inject_tx.clone();

        let transport = MockTransport {
            responses: Arc::new(Mutex::new(responses)),
            response_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            incoming_tx: inject_tx,
            incoming_rx: rx,
            outgoing: Arc::new(Mutex::new(Vec::new())),
            connected: Arc::new(AtomicBool::new(true)),
        };

        let client = McpClient::builder()
            .with_sampling()
            .connect(transport, handler)
            .await
            .unwrap();

        // Initialize the client (this sends initialize request + notification)
        client.initialize("test-client", "1.0.0").await.unwrap();

        // Inject a server-initiated sampling/createMessage request
        let sampling_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "sampling/createMessage",
            "params": {
                "messages": [
                    {
                        "role": "user",
                        "content": {
                            "type": "text",
                            "text": "Hello"
                        }
                    }
                ],
                "maxTokens": 100
            }
        });
        inject_tx_clone
            .send(sampling_request.to_string())
            .await
            .unwrap();

        // Give the background loop time to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify the handler was called
        assert!(
            called.load(Ordering::SeqCst),
            "handle_create_message should have been called"
        );
    }

    #[tokio::test]
    async fn test_list_resource_templates() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "resourceTemplates": [
                    {
                        "uriTemplate": "file:///{path}",
                        "name": "File Template",
                        "description": "A file template"
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client.list_resource_templates().await.unwrap();

        assert_eq!(result.resource_templates.len(), 1);
        assert_eq!(result.resource_templates[0].name, "File Template");
    }

    #[tokio::test]
    async fn test_list_all_tools_single_page() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "tools": [
                    {
                        "name": "tool_a",
                        "description": "Tool A",
                        "inputSchema": { "type": "object", "properties": {} }
                    },
                    {
                        "name": "tool_b",
                        "description": "Tool B",
                        "inputSchema": { "type": "object", "properties": {} }
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let tools = client.list_all_tools().await.unwrap();

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "tool_a");
        assert_eq!(tools[1].name, "tool_b");
    }

    #[tokio::test]
    async fn test_list_all_tools_paginated() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            // First page with a next_cursor
            serde_json::json!({
                "tools": [
                    {
                        "name": "tool_a",
                        "description": "Tool A",
                        "inputSchema": { "type": "object", "properties": {} }
                    }
                ],
                "nextCursor": "page2"
            }),
            // Second page with no next_cursor
            serde_json::json!({
                "tools": [
                    {
                        "name": "tool_b",
                        "description": "Tool B",
                        "inputSchema": { "type": "object", "properties": {} }
                    }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let tools = client.list_all_tools().await.unwrap();

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "tool_a");
        assert_eq!(tools[1].name, "tool_b");
    }

    #[tokio::test]
    async fn test_call_tool_text_success() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "content": [
                    { "type": "text", "text": "Hello " },
                    { "type": "text", "text": "World" }
                ]
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let text = client
            .call_tool_text("test_tool", serde_json::json!({}))
            .await
            .unwrap();

        assert_eq!(text, "Hello World");
    }

    #[tokio::test]
    async fn test_call_tool_text_error() {
        let client = McpClient::connect(MockTransport::with_responses(vec![
            mock_initialize_response(),
            serde_json::json!({
                "content": [
                    { "type": "text", "text": "something went wrong" }
                ],
                "isError": true
            }),
        ]))
        .await
        .unwrap();

        client.initialize("test-client", "1.0.0").await.unwrap();
        let result = client
            .call_tool_text("test_tool", serde_json::json!({}))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("something went wrong"),
            "Error message should contain tool error text, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_server_notification_parsing() {
        let notification = parse_server_notification("notifications/tools/list_changed", None);
        assert!(matches!(notification, ServerNotification::ToolsListChanged));

        let notification = parse_server_notification("notifications/resources/list_changed", None);
        assert!(matches!(
            notification,
            ServerNotification::ResourcesListChanged
        ));

        let notification = parse_server_notification(
            "notifications/resources/updated",
            Some(serde_json::json!({"uri": "file:///test"})),
        );
        match notification {
            ServerNotification::ResourceUpdated { uri } => {
                assert_eq!(uri, "file:///test");
            }
            _ => panic!("Expected ResourceUpdated"),
        }

        let notification =
            parse_server_notification("custom/notification", Some(serde_json::json!({"data": 42})));
        match notification {
            ServerNotification::Unknown { method, params } => {
                assert_eq!(method, "custom/notification");
                assert!(params.is_some());
            }
            _ => panic!("Expected Unknown"),
        }
    }
}
