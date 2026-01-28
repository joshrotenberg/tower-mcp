//! MCP Router - routes requests to tools, resources, and prompts
//!
//! The router implements Tower's `Service` trait, making it composable with
//! standard tower middleware.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};

use tower_service::Service;

use crate::async_task::TaskStore;
use crate::context::{CancellationToken, NotificationSender, RequestContext, ServerNotification};
use crate::error::{Error, JsonRpcError, Result};
use crate::prompt::Prompt;
use crate::protocol::*;
use crate::resource::{Resource, ResourceTemplate};
use crate::session::SessionState;
use crate::tool::Tool;

/// MCP Router that dispatches requests to registered handlers
///
/// Implements `tower::Service<McpRequest>` for middleware composition.
///
/// # Example
///
/// ```rust
/// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
/// use schemars::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(Debug, Deserialize, JsonSchema)]
/// struct Input { value: String }
///
/// let tool = ToolBuilder::new("echo")
///     .description("Echo input")
///     .handler(|i: Input| async move { Ok(CallToolResult::text(i.value)) })
///     .build()
///     .unwrap();
///
/// let router = McpRouter::new()
///     .server_info("my-server", "1.0.0")
///     .tool(tool);
/// ```
#[derive(Clone)]
pub struct McpRouter {
    inner: Arc<McpRouterInner>,
    session: SessionState,
}

impl std::fmt::Debug for McpRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRouter")
            .field("server_name", &self.inner.server_name)
            .field("server_version", &self.inner.server_version)
            .field("tools_count", &self.inner.tools.len())
            .field("resources_count", &self.inner.resources.len())
            .field("prompts_count", &self.inner.prompts.len())
            .field("session_phase", &self.session.phase())
            .finish()
    }
}

/// Inner configuration that is shared across clones
#[derive(Clone)]
struct McpRouterInner {
    server_name: String,
    server_version: String,
    instructions: Option<String>,
    tools: HashMap<String, Arc<Tool>>,
    resources: HashMap<String, Arc<Resource>>,
    /// Resource templates for dynamic resource matching (keyed by uri_template)
    resource_templates: Vec<Arc<ResourceTemplate>>,
    prompts: HashMap<String, Arc<Prompt>>,
    /// In-flight requests for cancellation tracking (shared across clones)
    in_flight: Arc<RwLock<HashMap<RequestId, CancellationToken>>>,
    /// Channel for sending notifications to connected clients
    notification_tx: Option<NotificationSender>,
    /// Task store for async operations
    task_store: TaskStore,
    /// Subscribed resource URIs
    subscriptions: Arc<RwLock<HashSet<String>>>,
}

impl McpRouter {
    /// Create a new MCP router
    pub fn new() -> Self {
        Self {
            inner: Arc::new(McpRouterInner {
                server_name: "tower-mcp".to_string(),
                server_version: env!("CARGO_PKG_VERSION").to_string(),
                instructions: None,
                tools: HashMap::new(),
                resources: HashMap::new(),
                resource_templates: Vec::new(),
                prompts: HashMap::new(),
                in_flight: Arc::new(RwLock::new(HashMap::new())),
                notification_tx: None,
                task_store: TaskStore::new(),
                subscriptions: Arc::new(RwLock::new(HashSet::new())),
            }),
            session: SessionState::new(),
        }
    }

    /// Get access to the task store for async operations
    pub fn task_store(&self) -> &TaskStore {
        &self.inner.task_store
    }

    /// Set the notification sender for progress reporting
    ///
    /// This is typically called by the transport layer to receive notifications.
    pub fn with_notification_sender(mut self, tx: NotificationSender) -> Self {
        Arc::make_mut(&mut self.inner).notification_tx = Some(tx);
        self
    }

    /// Get the notification sender (if configured)
    pub fn notification_sender(&self) -> Option<&NotificationSender> {
        self.inner.notification_tx.as_ref()
    }

    /// Create a request context for tracking a request
    ///
    /// This registers the request for cancellation tracking and sets up
    /// progress reporting if configured.
    pub fn create_context(
        &self,
        request_id: RequestId,
        progress_token: Option<ProgressToken>,
    ) -> RequestContext {
        let ctx = RequestContext::new(request_id.clone());

        // Set up progress token if provided
        let ctx = if let Some(token) = progress_token {
            ctx.with_progress_token(token)
        } else {
            ctx
        };

        // Set up notification sender if configured
        let ctx = if let Some(tx) = &self.inner.notification_tx {
            ctx.with_notification_sender(tx.clone())
        } else {
            ctx
        };

        // Register for cancellation tracking
        let token = ctx.cancellation_token();
        if let Ok(mut in_flight) = self.inner.in_flight.write() {
            in_flight.insert(request_id, token);
        }

        ctx
    }

    /// Remove a request from tracking (called when request completes)
    pub fn complete_request(&self, request_id: &RequestId) {
        if let Ok(mut in_flight) = self.inner.in_flight.write() {
            in_flight.remove(request_id);
        }
    }

    /// Cancel a tracked request
    fn cancel_request(&self, request_id: &RequestId) -> bool {
        if let Ok(in_flight) = self.inner.in_flight.read()
            && let Some(token) = in_flight.get(request_id)
        {
            token.cancel();
            return true;
        }
        false
    }

    /// Set server info
    pub fn server_info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        inner.server_name = name.into();
        inner.server_version = version.into();
        self
    }

    /// Set instructions for LLMs describing how to use this server
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).instructions = Some(instructions.into());
        self
    }

    /// Register a tool
    pub fn tool(mut self, tool: Tool) -> Self {
        Arc::make_mut(&mut self.inner)
            .tools
            .insert(tool.name.clone(), Arc::new(tool));
        self
    }

    /// Register a resource
    pub fn resource(mut self, resource: Resource) -> Self {
        Arc::make_mut(&mut self.inner)
            .resources
            .insert(resource.uri.clone(), Arc::new(resource));
        self
    }

    /// Register a resource template
    ///
    /// Resource templates allow dynamic resources to be matched by URI pattern.
    /// When a client requests a resource URI that doesn't match any static
    /// resource, the router tries to match it against registered templates.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ResourceTemplateBuilder};
    /// use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
    /// use std::collections::HashMap;
    ///
    /// let template = ResourceTemplateBuilder::new("file:///{path}")
    ///     .name("Project Files")
    ///     .handler(|uri: String, vars: HashMap<String, String>| async move {
    ///         let path = vars.get("path").unwrap_or(&String::new()).clone();
    ///         Ok(ReadResourceResult {
    ///             contents: vec![ResourceContent {
    ///                 uri,
    ///                 mime_type: Some("text/plain".to_string()),
    ///                 text: Some(format!("Contents of {}", path)),
    ///                 blob: None,
    ///             }],
    ///         })
    ///     });
    ///
    /// let router = McpRouter::new()
    ///     .resource_template(template);
    /// ```
    pub fn resource_template(mut self, template: ResourceTemplate) -> Self {
        Arc::make_mut(&mut self.inner)
            .resource_templates
            .push(Arc::new(template));
        self
    }

    /// Register a prompt
    pub fn prompt(mut self, prompt: Prompt) -> Self {
        Arc::make_mut(&mut self.inner)
            .prompts
            .insert(prompt.name.clone(), Arc::new(prompt));
        self
    }

    /// Get access to the session state
    pub fn session(&self) -> &SessionState {
        &self.session
    }

    /// Send a log message notification to the client
    ///
    /// This sends a `notifications/message` notification with the given parameters.
    /// Returns `true` if the notification was sent, `false` if no notification channel
    /// is configured.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tower_mcp::protocol::{LogLevel, LoggingMessageParams};
    ///
    /// // Simple info message
    /// router.log(LoggingMessageParams::new(LogLevel::Info).with_data(
    ///     serde_json::json!({"message": "Operation completed"})
    /// ));
    ///
    /// // Error with logger name
    /// router.log(LoggingMessageParams::new(LogLevel::Error)
    ///     .with_logger("database")
    ///     .with_data(serde_json::json!({"error": "Connection failed"})));
    /// ```
    pub fn log(&self, params: LoggingMessageParams) -> bool {
        if let Some(tx) = &self.inner.notification_tx
            && tx.try_send(ServerNotification::LogMessage(params)).is_ok()
        {
            return true;
        }
        false
    }

    /// Send an info-level log message
    ///
    /// Convenience method for sending an info log with optional data.
    pub fn log_info(&self, message: &str) -> bool {
        self.log(
            LoggingMessageParams::new(LogLevel::Info)
                .with_data(serde_json::json!({ "message": message })),
        )
    }

    /// Send a warning-level log message
    pub fn log_warning(&self, message: &str) -> bool {
        self.log(
            LoggingMessageParams::new(LogLevel::Warning)
                .with_data(serde_json::json!({ "message": message })),
        )
    }

    /// Send an error-level log message
    pub fn log_error(&self, message: &str) -> bool {
        self.log(
            LoggingMessageParams::new(LogLevel::Error)
                .with_data(serde_json::json!({ "message": message })),
        )
    }

    /// Send a debug-level log message
    pub fn log_debug(&self, message: &str) -> bool {
        self.log(
            LoggingMessageParams::new(LogLevel::Debug)
                .with_data(serde_json::json!({ "message": message })),
        )
    }

    /// Check if a resource URI is currently subscribed
    pub fn is_subscribed(&self, uri: &str) -> bool {
        if let Ok(subs) = self.inner.subscriptions.read() {
            return subs.contains(uri);
        }
        false
    }

    /// Get a list of all subscribed resource URIs
    pub fn subscribed_uris(&self) -> Vec<String> {
        if let Ok(subs) = self.inner.subscriptions.read() {
            return subs.iter().cloned().collect();
        }
        Vec::new()
    }

    /// Subscribe to a resource URI
    fn subscribe(&self, uri: &str) -> bool {
        if let Ok(mut subs) = self.inner.subscriptions.write() {
            return subs.insert(uri.to_string());
        }
        false
    }

    /// Unsubscribe from a resource URI
    fn unsubscribe(&self, uri: &str) -> bool {
        if let Ok(mut subs) = self.inner.subscriptions.write() {
            return subs.remove(uri);
        }
        false
    }

    /// Notify clients that a subscribed resource has been updated
    ///
    /// Only sends the notification if the resource is currently subscribed.
    /// Returns `true` if the notification was sent.
    pub fn notify_resource_updated(&self, uri: &str) -> bool {
        // Only notify if the resource is subscribed
        if !self.is_subscribed(uri) {
            return false;
        }

        if let Some(tx) = &self.inner.notification_tx
            && tx
                .try_send(ServerNotification::ResourceUpdated {
                    uri: uri.to_string(),
                })
                .is_ok()
        {
            return true;
        }
        false
    }

    /// Notify clients that the list of available resources has changed
    ///
    /// Returns `true` if the notification was sent.
    pub fn notify_resources_list_changed(&self) -> bool {
        if let Some(tx) = &self.inner.notification_tx
            && tx
                .try_send(ServerNotification::ResourcesListChanged)
                .is_ok()
        {
            return true;
        }
        false
    }

    /// Get server capabilities based on registered handlers
    fn capabilities(&self) -> ServerCapabilities {
        let has_resources =
            !self.inner.resources.is_empty() || !self.inner.resource_templates.is_empty();

        ServerCapabilities {
            tools: if self.inner.tools.is_empty() {
                None
            } else {
                Some(ToolsCapability::default())
            },
            resources: if has_resources {
                Some(ResourcesCapability {
                    subscribe: true,
                    ..Default::default()
                })
            } else {
                None
            },
            prompts: if self.inner.prompts.is_empty() {
                None
            } else {
                Some(PromptsCapability::default())
            },
            // Always advertise logging capability when notification channel is configured
            logging: if self.inner.notification_tx.is_some() {
                Some(LoggingCapability::default())
            } else {
                None
            },
            // Tasks capability is always available
            tasks: Some(TasksCapability::default()),
        }
    }

    /// Handle an MCP request
    async fn handle(&self, request_id: RequestId, request: McpRequest) -> Result<McpResponse> {
        // Enforce session state - reject requests before initialization
        let method = request.method_name();
        if !self.session.is_request_allowed(method) {
            tracing::warn!(
                method = %method,
                phase = ?self.session.phase(),
                "Request rejected: session not initialized"
            );
            return Err(Error::JsonRpc(JsonRpcError::invalid_request(format!(
                "Session not initialized. Only 'initialize' and 'ping' are allowed before initialization. Got: {}",
                method
            ))));
        }

        match request {
            McpRequest::Initialize(params) => {
                tracing::info!(
                    client = %params.client_info.name,
                    version = %params.client_info.version,
                    "Client initializing"
                );

                // Protocol version negotiation: respond with same version if supported,
                // otherwise respond with our latest supported version
                let protocol_version = if crate::protocol::SUPPORTED_PROTOCOL_VERSIONS
                    .contains(&params.protocol_version.as_str())
                {
                    params.protocol_version
                } else {
                    crate::protocol::LATEST_PROTOCOL_VERSION.to_string()
                };

                // Transition session state to Initializing
                self.session.mark_initializing();

                Ok(McpResponse::Initialize(InitializeResult {
                    protocol_version,
                    capabilities: self.capabilities(),
                    server_info: Implementation {
                        name: self.inner.server_name.clone(),
                        version: self.inner.server_version.clone(),
                    },
                    instructions: self.inner.instructions.clone(),
                }))
            }

            McpRequest::ListTools(_params) => {
                let tools: Vec<ToolDefinition> =
                    self.inner.tools.values().map(|t| t.definition()).collect();

                Ok(McpResponse::ListTools(ListToolsResult {
                    tools,
                    next_cursor: None,
                }))
            }

            McpRequest::CallTool(params) => {
                let tool =
                    self.inner.tools.get(&params.name).ok_or_else(|| {
                        Error::JsonRpc(JsonRpcError::method_not_found(&params.name))
                    })?;

                // Extract progress token from request metadata
                let progress_token = params.meta.and_then(|m| m.progress_token);
                let ctx = self.create_context(request_id, progress_token);

                tracing::debug!(tool = %params.name, "Calling tool");
                let result = tool.call_with_context(ctx, params.arguments).await?;

                Ok(McpResponse::CallTool(result))
            }

            McpRequest::ListResources(_params) => {
                let resources: Vec<ResourceDefinition> = self
                    .inner
                    .resources
                    .values()
                    .map(|r| r.definition())
                    .collect();

                Ok(McpResponse::ListResources(ListResourcesResult {
                    resources,
                    next_cursor: None,
                }))
            }

            McpRequest::ListResourceTemplates(_params) => {
                let resource_templates: Vec<ResourceTemplateDefinition> = self
                    .inner
                    .resource_templates
                    .iter()
                    .map(|t| t.definition())
                    .collect();

                Ok(McpResponse::ListResourceTemplates(
                    ListResourceTemplatesResult {
                        resource_templates,
                        next_cursor: None,
                    },
                ))
            }

            McpRequest::ReadResource(params) => {
                // First, try to find a static resource
                if let Some(resource) = self.inner.resources.get(&params.uri) {
                    tracing::debug!(uri = %params.uri, "Reading static resource");
                    let result = resource.read().await?;
                    return Ok(McpResponse::ReadResource(result));
                }

                // If no static resource found, try to match against templates
                for template in &self.inner.resource_templates {
                    if let Some(variables) = template.match_uri(&params.uri) {
                        tracing::debug!(
                            uri = %params.uri,
                            template = %template.uri_template,
                            "Reading resource via template"
                        );
                        let result = template.read(&params.uri, variables).await?;
                        return Ok(McpResponse::ReadResource(result));
                    }
                }

                // No match found
                Err(Error::JsonRpc(JsonRpcError::resource_not_found(
                    &params.uri,
                )))
            }

            McpRequest::SubscribeResource(params) => {
                // Verify the resource exists
                if !self.inner.resources.contains_key(&params.uri) {
                    return Err(Error::JsonRpc(JsonRpcError::resource_not_found(
                        &params.uri,
                    )));
                }

                tracing::debug!(uri = %params.uri, "Subscribing to resource");
                self.subscribe(&params.uri);

                Ok(McpResponse::SubscribeResource(EmptyResult {}))
            }

            McpRequest::UnsubscribeResource(params) => {
                // Verify the resource exists
                if !self.inner.resources.contains_key(&params.uri) {
                    return Err(Error::JsonRpc(JsonRpcError::resource_not_found(
                        &params.uri,
                    )));
                }

                tracing::debug!(uri = %params.uri, "Unsubscribing from resource");
                self.unsubscribe(&params.uri);

                Ok(McpResponse::UnsubscribeResource(EmptyResult {}))
            }

            McpRequest::ListPrompts(_params) => {
                let prompts: Vec<PromptDefinition> = self
                    .inner
                    .prompts
                    .values()
                    .map(|p| p.definition())
                    .collect();

                Ok(McpResponse::ListPrompts(ListPromptsResult {
                    prompts,
                    next_cursor: None,
                }))
            }

            McpRequest::GetPrompt(params) => {
                let prompt = self.inner.prompts.get(&params.name).ok_or_else(|| {
                    Error::JsonRpc(JsonRpcError::method_not_found(&format!(
                        "Prompt not found: {}",
                        params.name
                    )))
                })?;

                tracing::debug!(name = %params.name, "Getting prompt");
                let result = prompt.get(params.arguments).await?;

                Ok(McpResponse::GetPrompt(result))
            }

            McpRequest::Ping => Ok(McpResponse::Pong(EmptyResult {})),

            McpRequest::EnqueueTask(params) => {
                // Verify the tool exists
                let tool = self.inner.tools.get(&params.tool_name).ok_or_else(|| {
                    Error::JsonRpc(JsonRpcError::method_not_found(&format!(
                        "Tool not found: {}",
                        params.tool_name
                    )))
                })?;

                // Create the task
                let (task_id, cancellation_token) = self.inner.task_store.create_task(
                    &params.tool_name,
                    params.arguments.clone(),
                    params.ttl,
                );

                tracing::info!(task_id = %task_id, tool = %params.tool_name, "Enqueued async task");

                // Create a context for the async task execution
                let ctx = self.create_context(request_id, None);

                // Spawn the task execution in the background
                let task_store = self.inner.task_store.clone();
                let tool = tool.clone();
                let arguments = params.arguments;
                let task_id_clone = task_id.clone();

                tokio::spawn(async move {
                    // Check for cancellation before starting
                    if cancellation_token.is_cancelled() {
                        tracing::debug!(task_id = %task_id_clone, "Task cancelled before execution");
                        return;
                    }

                    // Execute the tool
                    match tool.call_with_context(ctx, arguments).await {
                        Ok(result) => {
                            if cancellation_token.is_cancelled() {
                                tracing::debug!(task_id = %task_id_clone, "Task cancelled during execution");
                            } else {
                                task_store.complete_task(&task_id_clone, result);
                                tracing::debug!(task_id = %task_id_clone, "Task completed successfully");
                            }
                        }
                        Err(e) => {
                            task_store.fail_task(&task_id_clone, &e.to_string());
                            tracing::warn!(task_id = %task_id_clone, error = %e, "Task failed");
                        }
                    }
                });

                Ok(McpResponse::EnqueueTask(EnqueueTaskResult {
                    task_id,
                    status: TaskStatus::Working,
                    poll_interval: Some(2),
                }))
            }

            McpRequest::ListTasks(params) => {
                let tasks = self.inner.task_store.list_tasks(params.status);

                Ok(McpResponse::ListTasks(ListTasksResult {
                    tasks,
                    next_cursor: None,
                }))
            }

            McpRequest::GetTaskInfo(params) => {
                let task = self
                    .inner
                    .task_store
                    .get_task(&params.task_id)
                    .ok_or_else(|| {
                        Error::JsonRpc(JsonRpcError::invalid_params(format!(
                            "Task not found: {}",
                            params.task_id
                        )))
                    })?;

                Ok(McpResponse::GetTaskInfo(task))
            }

            McpRequest::GetTaskResult(params) => {
                let (status, result, error) = self
                    .inner
                    .task_store
                    .get_task_full(&params.task_id)
                    .ok_or_else(|| {
                        Error::JsonRpc(JsonRpcError::invalid_params(format!(
                            "Task not found: {}",
                            params.task_id
                        )))
                    })?;

                Ok(McpResponse::GetTaskResult(GetTaskResultResult {
                    task_id: params.task_id,
                    status,
                    result,
                    error,
                }))
            }

            McpRequest::CancelTask(params) => {
                let status = self
                    .inner
                    .task_store
                    .cancel_task(&params.task_id, params.reason.as_deref())
                    .ok_or_else(|| {
                        Error::JsonRpc(JsonRpcError::invalid_params(format!(
                            "Task not found: {}",
                            params.task_id
                        )))
                    })?;

                let cancelled = status == TaskStatus::Cancelled;

                Ok(McpResponse::CancelTask(CancelTaskResult {
                    cancelled,
                    status,
                }))
            }

            McpRequest::SetLoggingLevel(params) => {
                // Store the log level for filtering outgoing log notifications
                // For now, we just accept the request - actual filtering would be
                // implemented in the notification sending logic
                tracing::debug!(level = ?params.level, "Client set logging level");
                Ok(McpResponse::SetLoggingLevel(EmptyResult {}))
            }

            McpRequest::Unknown { method, .. } => {
                Err(Error::JsonRpc(JsonRpcError::method_not_found(&method)))
            }
        }
    }

    /// Handle an MCP notification (no response expected)
    pub fn handle_notification(&self, notification: McpNotification) {
        match notification {
            McpNotification::Initialized => {
                if self.session.mark_initialized() {
                    tracing::info!("Session initialized, entering operation phase");
                } else {
                    tracing::warn!(
                        "Received initialized notification in unexpected state: {:?}",
                        self.session.phase()
                    );
                }
            }
            McpNotification::Cancelled(params) => {
                if self.cancel_request(&params.request_id) {
                    tracing::info!(
                        request_id = ?params.request_id,
                        reason = ?params.reason,
                        "Request cancelled"
                    );
                } else {
                    tracing::debug!(
                        request_id = ?params.request_id,
                        reason = ?params.reason,
                        "Cancellation requested for unknown request"
                    );
                }
            }
            McpNotification::Progress(params) => {
                tracing::trace!(
                    token = ?params.progress_token,
                    progress = params.progress,
                    total = ?params.total,
                    "Progress notification"
                );
                // Progress notifications from client are unusual but valid
            }
            McpNotification::Unknown { method, .. } => {
                tracing::debug!(method = %method, "Unknown notification received");
            }
        }
    }
}

impl Default for McpRouter {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Tower Service implementation
// =============================================================================

/// Request type for the tower Service implementation
#[derive(Debug)]
pub struct RouterRequest {
    pub id: RequestId,
    pub inner: McpRequest,
}

/// Response type for the tower Service implementation
#[derive(Debug)]
pub struct RouterResponse {
    pub id: RequestId,
    pub inner: std::result::Result<McpResponse, JsonRpcError>,
}

impl RouterResponse {
    /// Convert to JSON-RPC response
    pub fn into_jsonrpc(self) -> JsonRpcResponse {
        match self.inner {
            Ok(response) => match serde_json::to_value(response) {
                Ok(result) => JsonRpcResponse::result(self.id, result),
                Err(e) => {
                    tracing::error!(error = %e, "Failed to serialize response");
                    JsonRpcResponse::error(
                        Some(self.id),
                        JsonRpcError::internal_error(format!("Serialization error: {}", e)),
                    )
                }
            },
            Err(error) => JsonRpcResponse::error(Some(self.id), error),
        }
    }
}

impl Service<RouterRequest> for McpRouter {
    type Response = RouterResponse;
    type Error = std::convert::Infallible; // Errors are in the response
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let router = self.clone();
        let request_id = req.id.clone();
        Box::pin(async move {
            let result = router.handle(req.id, req.inner).await;
            // Clean up tracking after request completes
            router.complete_request(&request_id);
            Ok(RouterResponse {
                id: request_id,
                inner: result.map_err(|e| match e {
                    Error::JsonRpc(err) => err,
                    Error::Tool(err) => JsonRpcError::internal_error(err.to_string()),
                    e => JsonRpcError::internal_error(e.to_string()),
                }),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::JsonRpcService;
    use crate::tool::ToolBuilder;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use tower::ServiceExt;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct AddInput {
        a: i64,
        b: i64,
    }

    /// Helper to initialize a router for testing
    async fn init_router(router: &mut McpRouter) {
        // Send initialize request
        let init_req = RouterRequest {
            id: RequestId::Number(0),
            inner: McpRequest::Initialize(InitializeParams {
                protocol_version: "2025-03-26".to_string(),
                capabilities: ClientCapabilities {
                    roots: None,
                    sampling: None,
                    elicitation: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                },
            }),
        };
        let _ = router.ready().await.unwrap().call(init_req).await.unwrap();
        // Send initialized notification
        router.handle_notification(McpNotification::Initialized);
    }

    #[tokio::test]
    async fn test_router_list_tools() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build()
            .expect("valid tool name");

        let mut router = McpRouter::new().tool(add_tool);

        // Initialize session first
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 1);
                assert_eq!(result.tools[0].name, "add");
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_router_call_tool() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build()
            .expect("valid tool name");

        let mut router = McpRouter::new().tool(add_tool);

        // Initialize session first
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "add".to_string(),
                arguments: serde_json::json!({"a": 2, "b": 3}),
                meta: None,
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::CallTool(result)) => {
                assert!(!result.is_error);
                // Check the text content
                match &result.content[0] {
                    Content::Text { text, .. } => assert_eq!(text, "5"),
                    _ => panic!("Expected text content"),
                }
            }
            _ => panic!("Expected CallTool response"),
        }
    }

    /// Helper to initialize a JsonRpcService for testing
    async fn init_jsonrpc_service(service: &mut JsonRpcService<McpRouter>, router: &McpRouter) {
        let init_req = JsonRpcRequest::new(0, "initialize").with_params(serde_json::json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }));
        let _ = service.call_single(init_req).await.unwrap();
        router.handle_notification(McpNotification::Initialized);
    }

    #[tokio::test]
    async fn test_jsonrpc_service() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build()
            .expect("valid tool name");

        let router = McpRouter::new().tool(add_tool);
        let mut service = JsonRpcService::new(router.clone());

        // Initialize session first
        init_jsonrpc_service(&mut service, &router).await;

        let req = JsonRpcRequest::new(1, "tools/list");

        let resp = service.call_single(req).await.unwrap();

        match resp {
            JsonRpcResponse::Result(r) => {
                assert_eq!(r.id, RequestId::Number(1));
                let tools = r.result.get("tools").unwrap().as_array().unwrap();
                assert_eq!(tools.len(), 1);
            }
            JsonRpcResponse::Error(_) => panic!("Expected success response"),
        }
    }

    #[tokio::test]
    async fn test_batch_request() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build()
            .expect("valid tool name");

        let router = McpRouter::new().tool(add_tool);
        let mut service = JsonRpcService::new(router.clone());

        // Initialize session first
        init_jsonrpc_service(&mut service, &router).await;

        // Create a batch of requests
        let requests = vec![
            JsonRpcRequest::new(1, "tools/list"),
            JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
                "name": "add",
                "arguments": {"a": 10, "b": 20}
            })),
            JsonRpcRequest::new(3, "ping"),
        ];

        let responses = service.call_batch(requests).await.unwrap();

        assert_eq!(responses.len(), 3);

        // Check first response (tools/list)
        match &responses[0] {
            JsonRpcResponse::Result(r) => {
                assert_eq!(r.id, RequestId::Number(1));
                let tools = r.result.get("tools").unwrap().as_array().unwrap();
                assert_eq!(tools.len(), 1);
            }
            JsonRpcResponse::Error(_) => panic!("Expected success for tools/list"),
        }

        // Check second response (tools/call)
        match &responses[1] {
            JsonRpcResponse::Result(r) => {
                assert_eq!(r.id, RequestId::Number(2));
                let content = r.result.get("content").unwrap().as_array().unwrap();
                let text = content[0].get("text").unwrap().as_str().unwrap();
                assert_eq!(text, "30");
            }
            JsonRpcResponse::Error(_) => panic!("Expected success for tools/call"),
        }

        // Check third response (ping)
        match &responses[2] {
            JsonRpcResponse::Result(r) => {
                assert_eq!(r.id, RequestId::Number(3));
            }
            JsonRpcResponse::Error(_) => panic!("Expected success for ping"),
        }
    }

    #[tokio::test]
    async fn test_empty_batch_error() {
        let router = McpRouter::new();
        let mut service = JsonRpcService::new(router);

        let result = service.call_batch(vec![]).await;
        assert!(result.is_err());
    }

    // =========================================================================
    // Progress Token Tests
    // =========================================================================

    #[tokio::test]
    async fn test_progress_token_extraction() {
        use crate::context::{RequestContext, ServerNotification, notification_channel};
        use crate::protocol::ProgressToken;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        // Track whether progress was reported
        let progress_reported = Arc::new(AtomicBool::new(false));
        let progress_ref = progress_reported.clone();

        // Create a tool that reports progress
        let tool = ToolBuilder::new("progress_tool")
            .description("Tool that reports progress")
            .handler_with_context(move |ctx: RequestContext, _input: AddInput| {
                let reported = progress_ref.clone();
                async move {
                    // Report progress - this should work if token was extracted
                    ctx.report_progress(50.0, Some(100.0), Some("Halfway"))
                        .await;
                    reported.store(true, Ordering::SeqCst);
                    Ok(CallToolResult::text("done"))
                }
            })
            .build()
            .expect("valid tool name");

        // Set up notification channel
        let (tx, mut rx) = notification_channel(10);
        let router = McpRouter::new().with_notification_sender(tx).tool(tool);
        let mut service = JsonRpcService::new(router.clone());

        // Initialize
        init_jsonrpc_service(&mut service, &router).await;

        // Call tool WITH progress token in _meta
        let req = JsonRpcRequest::new(1, "tools/call").with_params(serde_json::json!({
            "name": "progress_tool",
            "arguments": {"a": 1, "b": 2},
            "_meta": {
                "progressToken": "test-token-123"
            }
        }));

        let resp = service.call_single(req).await.unwrap();

        // Verify the tool was called successfully
        match resp {
            JsonRpcResponse::Result(_) => {}
            JsonRpcResponse::Error(e) => panic!("Expected success, got error: {:?}", e),
        }

        // Verify progress was reported by handler
        assert!(progress_reported.load(Ordering::SeqCst));

        // Verify progress notification was sent through channel
        let notification = rx.try_recv().expect("Expected progress notification");
        match notification {
            ServerNotification::Progress(params) => {
                assert_eq!(
                    params.progress_token,
                    ProgressToken::String("test-token-123".to_string())
                );
                assert_eq!(params.progress, 50.0);
                assert_eq!(params.total, Some(100.0));
                assert_eq!(params.message.as_deref(), Some("Halfway"));
            }
            _ => panic!("Expected Progress notification"),
        }
    }

    #[tokio::test]
    async fn test_tool_call_without_progress_token() {
        use crate::context::{RequestContext, notification_channel};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let progress_attempted = Arc::new(AtomicBool::new(false));
        let progress_ref = progress_attempted.clone();

        let tool = ToolBuilder::new("no_token_tool")
            .description("Tool that tries to report progress without token")
            .handler_with_context(move |ctx: RequestContext, _input: AddInput| {
                let attempted = progress_ref.clone();
                async move {
                    // Try to report progress - should be a no-op without token
                    ctx.report_progress(50.0, Some(100.0), None).await;
                    attempted.store(true, Ordering::SeqCst);
                    Ok(CallToolResult::text("done"))
                }
            })
            .build()
            .expect("valid tool name");

        let (tx, mut rx) = notification_channel(10);
        let router = McpRouter::new().with_notification_sender(tx).tool(tool);
        let mut service = JsonRpcService::new(router.clone());

        init_jsonrpc_service(&mut service, &router).await;

        // Call tool WITHOUT progress token
        let req = JsonRpcRequest::new(1, "tools/call").with_params(serde_json::json!({
            "name": "no_token_tool",
            "arguments": {"a": 1, "b": 2}
        }));

        let resp = service.call_single(req).await.unwrap();
        assert!(matches!(resp, JsonRpcResponse::Result(_)));

        // Handler was called
        assert!(progress_attempted.load(Ordering::SeqCst));

        // But no notification was sent (no progress token)
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_batch_errors_returned_not_dropped() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build()
            .expect("valid tool name");

        let router = McpRouter::new().tool(add_tool);
        let mut service = JsonRpcService::new(router.clone());

        init_jsonrpc_service(&mut service, &router).await;

        // Create a batch with one valid and one invalid request
        let requests = vec![
            // Valid request
            JsonRpcRequest::new(1, "tools/call").with_params(serde_json::json!({
                "name": "add",
                "arguments": {"a": 10, "b": 20}
            })),
            // Invalid request - tool doesn't exist
            JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
                "name": "nonexistent_tool",
                "arguments": {}
            })),
            // Another valid request
            JsonRpcRequest::new(3, "ping"),
        ];

        let responses = service.call_batch(requests).await.unwrap();

        // All three requests should have responses (errors are not dropped)
        assert_eq!(responses.len(), 3);

        // First should be success
        match &responses[0] {
            JsonRpcResponse::Result(r) => {
                assert_eq!(r.id, RequestId::Number(1));
            }
            JsonRpcResponse::Error(_) => panic!("Expected success for first request"),
        }

        // Second should be an error (tool not found)
        match &responses[1] {
            JsonRpcResponse::Error(e) => {
                assert_eq!(e.id, Some(RequestId::Number(2)));
                // Error should indicate method not found
                assert!(e.error.message.contains("not found") || e.error.code == -32601);
            }
            JsonRpcResponse::Result(_) => panic!("Expected error for second request"),
        }

        // Third should be success
        match &responses[2] {
            JsonRpcResponse::Result(r) => {
                assert_eq!(r.id, RequestId::Number(3));
            }
            JsonRpcResponse::Error(_) => panic!("Expected success for third request"),
        }
    }

    // =========================================================================
    // Resource Template Tests
    // =========================================================================

    #[tokio::test]
    async fn test_list_resource_templates() {
        use crate::resource::ResourceTemplateBuilder;
        use std::collections::HashMap;

        let template = ResourceTemplateBuilder::new("file:///{path}")
            .name("Project Files")
            .description("Access project files")
            .handler(|uri: String, _vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: None,
                        text: None,
                        blob: None,
                    }],
                })
            });

        let mut router = McpRouter::new().resource_template(template);

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListResourceTemplates(ListResourceTemplatesParams::default()),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListResourceTemplates(result)) => {
                assert_eq!(result.resource_templates.len(), 1);
                assert_eq!(result.resource_templates[0].uri_template, "file:///{path}");
                assert_eq!(result.resource_templates[0].name, "Project Files");
            }
            _ => panic!("Expected ListResourceTemplates response"),
        }
    }

    #[tokio::test]
    async fn test_read_resource_via_template() {
        use crate::resource::ResourceTemplateBuilder;
        use std::collections::HashMap;

        let template = ResourceTemplateBuilder::new("db://users/{id}")
            .name("User Records")
            .handler(|uri: String, vars: HashMap<String, String>| async move {
                let id = vars.get("id").unwrap().clone();
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("application/json".to_string()),
                        text: Some(format!(r#"{{"id": "{}"}}"#, id)),
                        blob: None,
                    }],
                })
            });

        let mut router = McpRouter::new().resource_template(template);

        // Initialize session
        init_router(&mut router).await;

        // Read a resource that matches the template
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ReadResource(ReadResourceParams {
                uri: "db://users/123".to_string(),
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ReadResource(result)) => {
                assert_eq!(result.contents.len(), 1);
                assert_eq!(result.contents[0].uri, "db://users/123");
                assert!(result.contents[0].text.as_ref().unwrap().contains("123"));
            }
            _ => panic!("Expected ReadResource response"),
        }
    }

    #[tokio::test]
    async fn test_static_resource_takes_precedence_over_template() {
        use crate::resource::{ResourceBuilder, ResourceTemplateBuilder};
        use std::collections::HashMap;

        // Template that would match the same URI
        let template = ResourceTemplateBuilder::new("file:///{path}")
            .name("Files Template")
            .handler(|uri: String, _vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: None,
                        text: Some("from template".to_string()),
                        blob: None,
                    }],
                })
            });

        // Static resource with exact URI
        let static_resource = ResourceBuilder::new("file:///README.md")
            .name("README")
            .text("from static resource");

        let mut router = McpRouter::new()
            .resource_template(template)
            .resource(static_resource);

        // Initialize session
        init_router(&mut router).await;

        // Read the static resource - should NOT go through template
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ReadResource(ReadResourceParams {
                uri: "file:///README.md".to_string(),
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ReadResource(result)) => {
                // Should get static resource, not template
                assert_eq!(
                    result.contents[0].text.as_deref(),
                    Some("from static resource")
                );
            }
            _ => panic!("Expected ReadResource response"),
        }
    }

    #[tokio::test]
    async fn test_resource_not_found_when_no_match() {
        use crate::resource::ResourceTemplateBuilder;
        use std::collections::HashMap;

        let template = ResourceTemplateBuilder::new("db://users/{id}")
            .name("Users")
            .handler(|uri: String, _vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: None,
                        text: None,
                        blob: None,
                    }],
                })
            });

        let mut router = McpRouter::new().resource_template(template);

        // Initialize session
        init_router(&mut router).await;

        // Try to read a URI that doesn't match any resource or template
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ReadResource(ReadResourceParams {
                uri: "db://posts/123".to_string(),
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Err(err) => {
                assert!(err.message.contains("not found"));
            }
            Ok(_) => panic!("Expected error for non-matching URI"),
        }
    }

    #[tokio::test]
    async fn test_capabilities_include_resources_with_only_templates() {
        use crate::resource::ResourceTemplateBuilder;
        use std::collections::HashMap;

        let template = ResourceTemplateBuilder::new("file:///{path}")
            .name("Files")
            .handler(|uri: String, _vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: None,
                        text: None,
                        blob: None,
                    }],
                })
            });

        let mut router = McpRouter::new().resource_template(template);

        // Send initialize request and check capabilities
        let init_req = RouterRequest {
            id: RequestId::Number(0),
            inner: McpRequest::Initialize(InitializeParams {
                protocol_version: "2025-03-26".to_string(),
                capabilities: ClientCapabilities {
                    roots: None,
                    sampling: None,
                    elicitation: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                },
            }),
        };
        let resp = router.ready().await.unwrap().call(init_req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::Initialize(result)) => {
                // Should have resources capability even though only templates registered
                assert!(result.capabilities.resources.is_some());
            }
            _ => panic!("Expected Initialize response"),
        }
    }

    // =========================================================================
    // Logging Notification Tests
    // =========================================================================

    #[tokio::test]
    async fn test_log_sends_notification() {
        use crate::context::notification_channel;

        let (tx, mut rx) = notification_channel(10);
        let router = McpRouter::new().with_notification_sender(tx);

        // Send an info log
        let sent = router.log_info("Test message");
        assert!(sent);

        // Should receive the notification
        let notification = rx.try_recv().unwrap();
        match notification {
            ServerNotification::LogMessage(params) => {
                assert_eq!(params.level, LogLevel::Info);
                let data = params.data.unwrap();
                assert_eq!(
                    data.get("message").unwrap().as_str().unwrap(),
                    "Test message"
                );
            }
            _ => panic!("Expected LogMessage notification"),
        }
    }

    #[tokio::test]
    async fn test_log_with_custom_params() {
        use crate::context::notification_channel;

        let (tx, mut rx) = notification_channel(10);
        let router = McpRouter::new().with_notification_sender(tx);

        // Send a custom log message
        let params = LoggingMessageParams::new(LogLevel::Error)
            .with_logger("database")
            .with_data(serde_json::json!({
                "error": "Connection failed",
                "host": "localhost"
            }));

        let sent = router.log(params);
        assert!(sent);

        let notification = rx.try_recv().unwrap();
        match notification {
            ServerNotification::LogMessage(params) => {
                assert_eq!(params.level, LogLevel::Error);
                assert_eq!(params.logger.as_deref(), Some("database"));
                let data = params.data.unwrap();
                assert_eq!(
                    data.get("error").unwrap().as_str().unwrap(),
                    "Connection failed"
                );
            }
            _ => panic!("Expected LogMessage notification"),
        }
    }

    #[tokio::test]
    async fn test_log_without_channel_returns_false() {
        // Router without notification channel
        let router = McpRouter::new();

        // Should return false when no channel configured
        assert!(!router.log_info("Test"));
        assert!(!router.log_warning("Test"));
        assert!(!router.log_error("Test"));
        assert!(!router.log_debug("Test"));
    }

    #[tokio::test]
    async fn test_logging_capability_with_channel() {
        use crate::context::notification_channel;

        let (tx, _rx) = notification_channel(10);
        let mut router = McpRouter::new().with_notification_sender(tx);

        // Initialize and check capabilities
        let init_req = RouterRequest {
            id: RequestId::Number(0),
            inner: McpRequest::Initialize(InitializeParams {
                protocol_version: "2025-03-26".to_string(),
                capabilities: ClientCapabilities {
                    roots: None,
                    sampling: None,
                    elicitation: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                },
            }),
        };
        let resp = router.ready().await.unwrap().call(init_req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::Initialize(result)) => {
                // Should have logging capability when notification channel is set
                assert!(result.capabilities.logging.is_some());
            }
            _ => panic!("Expected Initialize response"),
        }
    }

    #[tokio::test]
    async fn test_no_logging_capability_without_channel() {
        let mut router = McpRouter::new();

        // Initialize and check capabilities
        let init_req = RouterRequest {
            id: RequestId::Number(0),
            inner: McpRequest::Initialize(InitializeParams {
                protocol_version: "2025-03-26".to_string(),
                capabilities: ClientCapabilities {
                    roots: None,
                    sampling: None,
                    elicitation: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                },
            }),
        };
        let resp = router.ready().await.unwrap().call(init_req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::Initialize(result)) => {
                // Should NOT have logging capability without notification channel
                assert!(result.capabilities.logging.is_none());
            }
            _ => panic!("Expected Initialize response"),
        }
    }

    // =========================================================================
    // Task Lifecycle Tests
    // =========================================================================

    #[tokio::test]
    async fn test_enqueue_task() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build()
            .expect("valid tool name");

        let mut router = McpRouter::new().tool(add_tool);
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::EnqueueTask(EnqueueTaskParams {
                tool_name: "add".to_string(),
                arguments: serde_json::json!({"a": 5, "b": 10}),
                ttl: None,
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::EnqueueTask(result)) => {
                assert!(result.task_id.starts_with("task-"));
                assert_eq!(result.status, TaskStatus::Working);
            }
            _ => panic!("Expected EnqueueTask response"),
        }
    }

    #[tokio::test]
    async fn test_list_tasks_empty() {
        let mut router = McpRouter::new();
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTasks(ListTasksParams::default()),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListTasks(result)) => {
                assert!(result.tasks.is_empty());
            }
            _ => panic!("Expected ListTasks response"),
        }
    }

    #[tokio::test]
    async fn test_task_lifecycle_complete() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build()
            .expect("valid tool name");

        let mut router = McpRouter::new().tool(add_tool);
        init_router(&mut router).await;

        // Enqueue task
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::EnqueueTask(EnqueueTaskParams {
                tool_name: "add".to_string(),
                arguments: serde_json::json!({"a": 7, "b": 8}),
                ttl: None,
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        let task_id = match resp.inner {
            Ok(McpResponse::EnqueueTask(result)) => result.task_id,
            _ => panic!("Expected EnqueueTask response"),
        };

        // Wait for task to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Get task result
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::GetTaskResult(GetTaskResultParams {
                task_id: task_id.clone(),
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::GetTaskResult(result)) => {
                assert_eq!(result.task_id, task_id);
                assert_eq!(result.status, TaskStatus::Completed);
                assert!(result.result.is_some());
                assert!(result.error.is_none());

                // Check the result content
                let tool_result = result.result.unwrap();
                match &tool_result.content[0] {
                    Content::Text { text, .. } => assert_eq!(text, "15"),
                    _ => panic!("Expected text content"),
                }
            }
            _ => panic!("Expected GetTaskResult response"),
        }
    }

    #[tokio::test]
    async fn test_task_cancellation() {
        // Use a slow tool to test cancellation
        let slow_tool = ToolBuilder::new("slow")
            .description("Slow tool")
            .handler(|_input: serde_json::Value| async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                Ok(CallToolResult::text("done"))
            })
            .build()
            .expect("valid tool name");

        let mut router = McpRouter::new().tool(slow_tool);
        init_router(&mut router).await;

        // Enqueue task
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::EnqueueTask(EnqueueTaskParams {
                tool_name: "slow".to_string(),
                arguments: serde_json::json!({}),
                ttl: None,
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        let task_id = match resp.inner {
            Ok(McpResponse::EnqueueTask(result)) => result.task_id,
            _ => panic!("Expected EnqueueTask response"),
        };

        // Cancel the task
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::CancelTask(CancelTaskParams {
                task_id: task_id.clone(),
                reason: Some("Test cancellation".to_string()),
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::CancelTask(result)) => {
                assert!(result.cancelled);
                assert_eq!(result.status, TaskStatus::Cancelled);
            }
            _ => panic!("Expected CancelTask response"),
        }
    }

    #[tokio::test]
    async fn test_get_task_info() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build()
            .expect("valid tool name");

        let mut router = McpRouter::new().tool(add_tool);
        init_router(&mut router).await;

        // Enqueue task
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::EnqueueTask(EnqueueTaskParams {
                tool_name: "add".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                ttl: Some(600),
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        let task_id = match resp.inner {
            Ok(McpResponse::EnqueueTask(result)) => result.task_id,
            _ => panic!("Expected EnqueueTask response"),
        };

        // Get task info
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: task_id.clone(),
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::GetTaskInfo(info)) => {
                assert_eq!(info.task_id, task_id);
                assert!(info.created_at.contains('T')); // ISO 8601
                assert_eq!(info.ttl, Some(600));
            }
            _ => panic!("Expected GetTaskInfo response"),
        }
    }

    #[tokio::test]
    async fn test_enqueue_nonexistent_tool() {
        let mut router = McpRouter::new();
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::EnqueueTask(EnqueueTaskParams {
                tool_name: "nonexistent".to_string(),
                arguments: serde_json::json!({}),
                ttl: None,
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Err(e) => {
                assert!(e.message.contains("not found"));
            }
            _ => panic!("Expected error response"),
        }
    }

    #[tokio::test]
    async fn test_get_nonexistent_task() {
        let mut router = McpRouter::new();
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: "task-999".to_string(),
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Err(e) => {
                assert!(e.message.contains("not found"));
            }
            _ => panic!("Expected error response"),
        }
    }

    // =========================================================================
    // Resource Subscription Tests
    // =========================================================================

    #[tokio::test]
    async fn test_subscribe_to_resource() {
        use crate::resource::ResourceBuilder;

        let resource = ResourceBuilder::new("file:///test.txt")
            .name("Test File")
            .text("Hello");

        let mut router = McpRouter::new().resource(resource);
        init_router(&mut router).await;

        // Subscribe to the resource
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::SubscribeResource(SubscribeResourceParams {
                uri: "file:///test.txt".to_string(),
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::SubscribeResource(_)) => {
                // Should be subscribed now
                assert!(router.is_subscribed("file:///test.txt"));
            }
            _ => panic!("Expected SubscribeResource response"),
        }
    }

    #[tokio::test]
    async fn test_unsubscribe_from_resource() {
        use crate::resource::ResourceBuilder;

        let resource = ResourceBuilder::new("file:///test.txt")
            .name("Test File")
            .text("Hello");

        let mut router = McpRouter::new().resource(resource);
        init_router(&mut router).await;

        // Subscribe first
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::SubscribeResource(SubscribeResourceParams {
                uri: "file:///test.txt".to_string(),
            }),
        };
        let _ = router.ready().await.unwrap().call(req).await.unwrap();
        assert!(router.is_subscribed("file:///test.txt"));

        // Now unsubscribe
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::UnsubscribeResource(UnsubscribeResourceParams {
                uri: "file:///test.txt".to_string(),
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::UnsubscribeResource(_)) => {
                // Should no longer be subscribed
                assert!(!router.is_subscribed("file:///test.txt"));
            }
            _ => panic!("Expected UnsubscribeResource response"),
        }
    }

    #[tokio::test]
    async fn test_subscribe_nonexistent_resource() {
        let mut router = McpRouter::new();
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::SubscribeResource(SubscribeResourceParams {
                uri: "file:///nonexistent.txt".to_string(),
            }),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Err(e) => {
                assert!(e.message.contains("not found"));
            }
            _ => panic!("Expected error response"),
        }
    }

    #[tokio::test]
    async fn test_notify_resource_updated() {
        use crate::context::notification_channel;
        use crate::resource::ResourceBuilder;

        let (tx, mut rx) = notification_channel(10);

        let resource = ResourceBuilder::new("file:///test.txt")
            .name("Test File")
            .text("Hello");

        let router = McpRouter::new()
            .resource(resource)
            .with_notification_sender(tx);

        // First, manually subscribe (simulate subscription)
        router.subscribe("file:///test.txt");

        // Now notify
        let sent = router.notify_resource_updated("file:///test.txt");
        assert!(sent);

        // Check the notification was sent
        let notification = rx.try_recv().unwrap();
        match notification {
            ServerNotification::ResourceUpdated { uri } => {
                assert_eq!(uri, "file:///test.txt");
            }
            _ => panic!("Expected ResourceUpdated notification"),
        }
    }

    #[tokio::test]
    async fn test_notify_resource_updated_not_subscribed() {
        use crate::context::notification_channel;
        use crate::resource::ResourceBuilder;

        let (tx, mut rx) = notification_channel(10);

        let resource = ResourceBuilder::new("file:///test.txt")
            .name("Test File")
            .text("Hello");

        let router = McpRouter::new()
            .resource(resource)
            .with_notification_sender(tx);

        // Try to notify without subscribing
        let sent = router.notify_resource_updated("file:///test.txt");
        assert!(!sent); // Should not send because not subscribed

        // Channel should be empty
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_notify_resources_list_changed() {
        use crate::context::notification_channel;

        let (tx, mut rx) = notification_channel(10);
        let router = McpRouter::new().with_notification_sender(tx);

        let sent = router.notify_resources_list_changed();
        assert!(sent);

        let notification = rx.try_recv().unwrap();
        match notification {
            ServerNotification::ResourcesListChanged => {}
            _ => panic!("Expected ResourcesListChanged notification"),
        }
    }

    #[tokio::test]
    async fn test_subscribed_uris() {
        use crate::resource::ResourceBuilder;

        let resource1 = ResourceBuilder::new("file:///a.txt").name("A").text("A");

        let resource2 = ResourceBuilder::new("file:///b.txt").name("B").text("B");

        let router = McpRouter::new().resource(resource1).resource(resource2);

        // Subscribe to both
        router.subscribe("file:///a.txt");
        router.subscribe("file:///b.txt");

        let uris = router.subscribed_uris();
        assert_eq!(uris.len(), 2);
        assert!(uris.contains(&"file:///a.txt".to_string()));
        assert!(uris.contains(&"file:///b.txt".to_string()));
    }

    #[tokio::test]
    async fn test_subscription_capability_advertised() {
        use crate::resource::ResourceBuilder;

        let resource = ResourceBuilder::new("file:///test.txt")
            .name("Test")
            .text("Hello");

        let mut router = McpRouter::new().resource(resource);

        // Initialize and check capabilities
        let init_req = RouterRequest {
            id: RequestId::Number(0),
            inner: McpRequest::Initialize(InitializeParams {
                protocol_version: "2025-03-26".to_string(),
                capabilities: ClientCapabilities {
                    roots: None,
                    sampling: None,
                    elicitation: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                },
            }),
        };
        let resp = router.ready().await.unwrap().call(init_req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::Initialize(result)) => {
                // Should have resources capability with subscribe enabled
                let resources_cap = result.capabilities.resources.unwrap();
                assert!(resources_cap.subscribe);
            }
            _ => panic!("Expected Initialize response"),
        }
    }
}
