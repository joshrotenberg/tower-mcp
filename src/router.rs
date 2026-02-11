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

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

use crate::async_task::TaskStore;
use crate::context::{
    CancellationToken, ClientRequesterHandle, NotificationSender, RequestContext,
    ServerNotification,
};
use crate::error::{Error, JsonRpcError, Result};
use crate::filter::{PromptFilter, ResourceFilter, ToolFilter};
use crate::prompt::Prompt;
use crate::protocol::*;
#[cfg(feature = "dynamic-tools")]
use crate::registry::{DynamicToolRegistry, DynamicToolsInner};
use crate::resource::{Resource, ResourceTemplate};
use crate::session::SessionState;
use crate::tool::Tool;

/// Type alias for completion handler function
pub type CompletionHandler = Arc<
    dyn Fn(CompleteParams) -> Pin<Box<dyn Future<Output = Result<CompleteResult>> + Send>>
        + Send
        + Sync,
>;

/// Decode a pagination cursor into an offset.
///
/// Returns `Err` if the cursor is malformed.
fn decode_cursor(cursor: &str) -> Result<usize> {
    let bytes = BASE64
        .decode(cursor)
        .map_err(|_| Error::JsonRpc(JsonRpcError::invalid_params("Invalid pagination cursor")))?;
    let s = String::from_utf8(bytes)
        .map_err(|_| Error::JsonRpc(JsonRpcError::invalid_params("Invalid pagination cursor")))?;
    s.parse::<usize>()
        .map_err(|_| Error::JsonRpc(JsonRpcError::invalid_params("Invalid pagination cursor")))
}

/// Encode an offset into an opaque pagination cursor.
fn encode_cursor(offset: usize) -> String {
    BASE64.encode(offset.to_string())
}

/// Apply pagination to a collected list of items.
///
/// Returns the page of items and an optional `next_cursor`.
fn paginate<T>(
    items: Vec<T>,
    cursor: Option<&str>,
    page_size: Option<usize>,
) -> Result<(Vec<T>, Option<String>)> {
    let Some(page_size) = page_size else {
        return Ok((items, None));
    };

    let offset = match cursor {
        Some(c) => decode_cursor(c)?,
        None => 0,
    };

    if offset >= items.len() {
        return Ok((Vec::new(), None));
    }

    let end = (offset + page_size).min(items.len());
    let next_cursor = if end < items.len() {
        Some(encode_cursor(end))
    } else {
        None
    };

    let mut items = items;
    let page = items.drain(offset..end).collect();
    Ok((page, next_cursor))
}

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
///     .build();
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

/// Configuration for auto-generated instructions
#[derive(Clone, Debug)]
struct AutoInstructionsConfig {
    prefix: Option<String>,
    suffix: Option<String>,
}

/// Inner configuration that is shared across clones
#[derive(Clone)]
struct McpRouterInner {
    server_name: String,
    server_version: String,
    /// Human-readable title for the server
    server_title: Option<String>,
    /// Description of the server
    server_description: Option<String>,
    /// Icons for the server
    server_icons: Option<Vec<ToolIcon>>,
    /// URL of the server's website
    server_website_url: Option<String>,
    instructions: Option<String>,
    auto_instructions: Option<AutoInstructionsConfig>,
    tools: HashMap<String, Arc<Tool>>,
    resources: HashMap<String, Arc<Resource>>,
    /// Resource templates for dynamic resource matching (keyed by uri_template)
    resource_templates: Vec<Arc<ResourceTemplate>>,
    prompts: HashMap<String, Arc<Prompt>>,
    /// In-flight requests for cancellation tracking (shared across clones)
    in_flight: Arc<RwLock<HashMap<RequestId, CancellationToken>>>,
    /// Channel for sending notifications to connected clients
    notification_tx: Option<NotificationSender>,
    /// Handle for sending requests to the client (for sampling, etc.)
    client_requester: Option<ClientRequesterHandle>,
    /// Task store for async operations
    task_store: TaskStore,
    /// Subscribed resource URIs
    subscriptions: Arc<RwLock<HashSet<String>>>,
    /// Handler for completion requests
    completion_handler: Option<CompletionHandler>,
    /// Filter for tools based on session state
    tool_filter: Option<ToolFilter>,
    /// Filter for resources based on session state
    resource_filter: Option<ResourceFilter>,
    /// Filter for prompts based on session state
    prompt_filter: Option<PromptFilter>,
    /// Router-level extensions (for state and middleware data)
    extensions: Arc<crate::context::Extensions>,
    /// Minimum log level for filtering outgoing log notifications (set by client via logging/setLevel)
    min_log_level: Arc<RwLock<LogLevel>>,
    /// Page size for list method pagination (None = return all results)
    page_size: Option<usize>,
    /// Dynamic tools registry for runtime tool (de)registration
    #[cfg(feature = "dynamic-tools")]
    dynamic_tools: Option<Arc<DynamicToolsInner>>,
}

impl McpRouterInner {
    /// Generate instructions text from registered tools, resources, and prompts.
    fn generate_instructions(&self, config: &AutoInstructionsConfig) -> String {
        let mut parts = Vec::new();

        if let Some(prefix) = &config.prefix {
            parts.push(prefix.clone());
        }

        // Tools section
        if !self.tools.is_empty() {
            let mut lines = vec!["## Tools".to_string(), String::new()];
            let mut tools: Vec<_> = self.tools.values().collect();
            tools.sort_by(|a, b| a.name.cmp(&b.name));
            for tool in tools {
                let desc = tool.description.as_deref().unwrap_or("No description");
                let tags = annotation_tags(tool.annotations.as_ref());
                if tags.is_empty() {
                    lines.push(format!("- **{}**: {}", tool.name, desc));
                } else {
                    lines.push(format!("- **{}**: {} [{}]", tool.name, desc, tags));
                }
            }
            parts.push(lines.join("\n"));
        }

        // Resources section
        if !self.resources.is_empty() || !self.resource_templates.is_empty() {
            let mut lines = vec!["## Resources".to_string(), String::new()];
            let mut resources: Vec<_> = self.resources.values().collect();
            resources.sort_by(|a, b| a.uri.cmp(&b.uri));
            for resource in resources {
                let desc = resource.description.as_deref().unwrap_or("No description");
                lines.push(format!("- **{}**: {}", resource.uri, desc));
            }
            let mut templates: Vec<_> = self.resource_templates.iter().collect();
            templates.sort_by(|a, b| a.uri_template.cmp(&b.uri_template));
            for template in templates {
                let desc = template.description.as_deref().unwrap_or("No description");
                lines.push(format!("- **{}**: {}", template.uri_template, desc));
            }
            parts.push(lines.join("\n"));
        }

        // Prompts section
        if !self.prompts.is_empty() {
            let mut lines = vec!["## Prompts".to_string(), String::new()];
            let mut prompts: Vec<_> = self.prompts.values().collect();
            prompts.sort_by(|a, b| a.name.cmp(&b.name));
            for prompt in prompts {
                let desc = prompt.description.as_deref().unwrap_or("No description");
                lines.push(format!("- **{}**: {}", prompt.name, desc));
            }
            parts.push(lines.join("\n"));
        }

        if let Some(suffix) = &config.suffix {
            parts.push(suffix.clone());
        }

        parts.join("\n\n")
    }
}

/// Build annotation tags like "read-only, idempotent" from tool annotations.
///
/// Only includes tags that differ from the MCP spec defaults
/// (read-only=false, idempotent=false). The destructive and open-world
/// hints are omitted because they match the default assumptions.
fn annotation_tags(annotations: Option<&crate::protocol::ToolAnnotations>) -> String {
    let Some(ann) = annotations else {
        return String::new();
    };
    let mut tags = Vec::new();
    if ann.is_read_only() {
        tags.push("read-only");
    }
    if ann.is_idempotent() {
        tags.push("idempotent");
    }
    tags.join(", ")
}

impl McpRouter {
    /// Create a new MCP router
    pub fn new() -> Self {
        Self {
            inner: Arc::new(McpRouterInner {
                server_name: "tower-mcp".to_string(),
                server_version: env!("CARGO_PKG_VERSION").to_string(),
                server_title: None,
                server_description: None,
                server_icons: None,
                server_website_url: None,
                instructions: None,
                auto_instructions: None,
                tools: HashMap::new(),
                resources: HashMap::new(),
                resource_templates: Vec::new(),
                prompts: HashMap::new(),
                in_flight: Arc::new(RwLock::new(HashMap::new())),
                notification_tx: None,
                client_requester: None,
                task_store: TaskStore::new(),
                subscriptions: Arc::new(RwLock::new(HashSet::new())),
                extensions: Arc::new(crate::context::Extensions::new()),
                completion_handler: None,
                tool_filter: None,
                resource_filter: None,
                prompt_filter: None,
                min_log_level: Arc::new(RwLock::new(LogLevel::Debug)),
                page_size: None,
                #[cfg(feature = "dynamic-tools")]
                dynamic_tools: None,
            }),
            session: SessionState::new(),
        }
    }

    /// Create a clone with fresh session state.
    ///
    /// Use this when creating a new logical session (e.g., per HTTP connection).
    /// The router configuration (tools, resources, prompts) is shared, but the
    /// session state (phase, extensions) is independent.
    ///
    /// This is typically called by transports when establishing a new client session.
    pub fn with_fresh_session(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            session: SessionState::new(),
        }
    }

    /// Get access to the task store for async operations
    pub fn task_store(&self) -> &TaskStore {
        &self.inner.task_store
    }

    /// Enable dynamic tool registration and return a registry handle.
    ///
    /// The returned [`DynamicToolRegistry`] can be used to add and remove tools
    /// at runtime. Dynamic tools are merged with static tools when handling
    /// `tools/list` and `tools/call` requests. Static tools take precedence
    /// over dynamic tools when names collide.
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
    /// let (router, registry) = McpRouter::new()
    ///     .server_info("my-server", "1.0.0")
    ///     .with_dynamic_tools();
    ///
    /// // Register a tool at runtime
    /// let tool = ToolBuilder::new("echo")
    ///     .description("Echo input")
    ///     .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///     .build();
    ///
    /// registry.register(tool);
    /// ```
    #[cfg(feature = "dynamic-tools")]
    pub fn with_dynamic_tools(mut self) -> (Self, DynamicToolRegistry) {
        let inner_dyn = Arc::new(DynamicToolsInner::new());
        Arc::make_mut(&mut self.inner).dynamic_tools = Some(inner_dyn.clone());
        (self, DynamicToolRegistry::new(inner_dyn))
    }

    /// Set the notification sender for progress reporting
    ///
    /// This is typically called by the transport layer to receive notifications.
    pub fn with_notification_sender(mut self, tx: NotificationSender) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        // Also register the sender with the dynamic tools registry so it can
        // broadcast ToolsListChanged notifications to this session.
        #[cfg(feature = "dynamic-tools")]
        if let Some(ref dynamic_tools) = inner.dynamic_tools {
            dynamic_tools.add_notification_sender(tx.clone());
        }
        inner.notification_tx = Some(tx);
        self
    }

    /// Get the notification sender (if configured)
    pub fn notification_sender(&self) -> Option<&NotificationSender> {
        self.inner.notification_tx.as_ref()
    }

    /// Set the client requester for server-to-client requests (sampling, etc.)
    ///
    /// This is typically called by bidirectional transports (WebSocket, stdio)
    /// to enable tool handlers to send requests to the client.
    pub fn with_client_requester(mut self, requester: ClientRequesterHandle) -> Self {
        Arc::make_mut(&mut self.inner).client_requester = Some(requester);
        self
    }

    /// Get the client requester (if configured)
    pub fn client_requester(&self) -> Option<&ClientRequesterHandle> {
        self.inner.client_requester.as_ref()
    }

    /// Add router-level state that handlers can access via the `Extension<T>` extractor.
    ///
    /// This is the recommended way to share state across all tools, resources, and prompts
    /// in a router. The state is available to handlers via the [`crate::extract::Extension`]
    /// extractor.
    ///
    /// # Example
    ///
    /// ```rust
    /// use std::sync::Arc;
    /// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
    /// use tower_mcp::extract::{Extension, Json};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Clone)]
    /// struct AppState {
    ///     db_url: String,
    /// }
    ///
    /// #[derive(Deserialize, JsonSchema)]
    /// struct QueryInput {
    ///     sql: String,
    /// }
    ///
    /// let state = Arc::new(AppState { db_url: "postgres://...".into() });
    ///
    /// // Tool extracts state via Extension<T>
    /// let query_tool = ToolBuilder::new("query")
    ///     .description("Run a database query")
    ///     .extractor_handler(
    ///         (),
    ///         |Extension(state): Extension<Arc<AppState>>, Json(input): Json<QueryInput>| async move {
    ///             Ok(CallToolResult::text(format!("Query on {}: {}", state.db_url, input.sql)))
    ///         },
    ///     )
    ///     .build();
    ///
    /// let router = McpRouter::new()
    ///     .with_state(state)  // State is now available to all handlers
    ///     .tool(query_tool);
    /// ```
    pub fn with_state<T: Clone + Send + Sync + 'static>(mut self, state: T) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        Arc::make_mut(&mut inner.extensions).insert(state);
        self
    }

    /// Add an extension value that handlers can access via the `Extension<T>` extractor.
    ///
    /// This is a more general form of `with_state()` for when you need multiple
    /// typed values available to handlers.
    pub fn with_extension<T: Clone + Send + Sync + 'static>(self, value: T) -> Self {
        self.with_state(value)
    }

    /// Get the router's extensions.
    pub fn extensions(&self) -> &crate::context::Extensions {
        &self.inner.extensions
    }

    /// Create a request context for tracking a request
    ///
    /// This registers the request for cancellation tracking and sets up
    /// progress reporting, client requests, and router extensions if configured.
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

        // Set up client requester if configured (for sampling support)
        let ctx = if let Some(requester) = &self.inner.client_requester {
            ctx.with_client_requester(requester.clone())
        } else {
            ctx
        };

        // Include router extensions (for with_state() and middleware data)
        let ctx = ctx.with_extensions(self.inner.extensions.clone());

        // Set up log level filtering
        let ctx = ctx.with_min_log_level(self.inner.min_log_level.clone());

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
        let Ok(in_flight) = self.inner.in_flight.read() else {
            return false;
        };
        let Some(token) = in_flight.get(request_id) else {
            return false;
        };
        token.cancel();
        true
    }

    /// Set server info
    pub fn server_info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        inner.server_name = name.into();
        inner.server_version = version.into();
        self
    }

    /// Set the page size for list method pagination.
    ///
    /// When set, list methods (`tools/list`, `resources/list`, etc.) will return
    /// at most `page_size` items per response, with a `next_cursor` for fetching
    /// subsequent pages. When `None` (the default), all items are returned in a
    /// single response.
    pub fn page_size(mut self, size: usize) -> Self {
        Arc::make_mut(&mut self.inner).page_size = Some(size);
        self
    }

    /// Set instructions for LLMs describing how to use this server
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).instructions = Some(instructions.into());
        self
    }

    /// Auto-generate instructions from registered tool, resource, and prompt descriptions.
    ///
    /// The instructions are generated lazily at initialization time, so this can be
    /// called at any point in the builder chain regardless of when tools, resources,
    /// and prompts are registered.
    ///
    /// If both `instructions()` and `auto_instructions()` are set, the auto-generated
    /// instructions take precedence.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct QueryInput { sql: String }
    ///
    /// let query_tool = ToolBuilder::new("query")
    ///     .description("Execute a read-only SQL query")
    ///     .read_only()
    ///     .handler(|input: QueryInput| async move {
    ///         Ok(CallToolResult::text("result"))
    ///     })
    ///     .build();
    ///
    /// let router = McpRouter::new()
    ///     .auto_instructions()
    ///     .tool(query_tool);
    /// ```
    pub fn auto_instructions(mut self) -> Self {
        Arc::make_mut(&mut self.inner).auto_instructions = Some(AutoInstructionsConfig {
            prefix: None,
            suffix: None,
        });
        self
    }

    /// Auto-generate instructions with custom prefix and/or suffix text.
    ///
    /// The prefix is prepended and suffix appended to the generated instructions.
    /// See [`auto_instructions`](Self::auto_instructions) for details.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::McpRouter;
    ///
    /// let router = McpRouter::new()
    ///     .auto_instructions_with(
    ///         Some("This server provides database tools."),
    ///         Some("Use 'query' for read operations and 'insert' for writes."),
    ///     );
    /// ```
    pub fn auto_instructions_with(
        mut self,
        prefix: Option<impl Into<String>>,
        suffix: Option<impl Into<String>>,
    ) -> Self {
        Arc::make_mut(&mut self.inner).auto_instructions = Some(AutoInstructionsConfig {
            prefix: prefix.map(Into::into),
            suffix: suffix.map(Into::into),
        });
        self
    }

    /// Set a human-readable title for the server
    pub fn server_title(mut self, title: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).server_title = Some(title.into());
        self
    }

    /// Set the server description
    pub fn server_description(mut self, description: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).server_description = Some(description.into());
        self
    }

    /// Set icons for the server
    pub fn server_icons(mut self, icons: Vec<ToolIcon>) -> Self {
        Arc::make_mut(&mut self.inner).server_icons = Some(icons);
        self
    }

    /// Set the server's website URL
    pub fn server_website_url(mut self, url: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).server_website_url = Some(url.into());
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

    /// Register multiple tools at once.
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
    /// let tools = vec![
    ///     ToolBuilder::new("a")
    ///         .description("Tool A")
    ///         .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///         .build(),
    ///     ToolBuilder::new("b")
    ///         .description("Tool B")
    ///         .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///         .build(),
    /// ];
    ///
    /// let router = McpRouter::new().tools(tools);
    /// ```
    pub fn tools(self, tools: impl IntoIterator<Item = Tool>) -> Self {
        tools
            .into_iter()
            .fold(self, |router, tool| router.tool(tool))
    }

    /// Register multiple resources at once.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ResourceBuilder};
    ///
    /// let resources = vec![
    ///     ResourceBuilder::new("file:///a.txt")
    ///         .name("File A")
    ///         .text("contents a"),
    ///     ResourceBuilder::new("file:///b.txt")
    ///         .name("File B")
    ///         .text("contents b"),
    /// ];
    ///
    /// let router = McpRouter::new().resources(resources);
    /// ```
    pub fn resources(self, resources: impl IntoIterator<Item = Resource>) -> Self {
        resources
            .into_iter()
            .fold(self, |router, resource| router.resource(resource))
    }

    /// Register multiple prompts at once.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, PromptBuilder};
    ///
    /// let prompts = vec![
    ///     PromptBuilder::new("greet")
    ///         .description("Greet someone")
    ///         .user_message("Hello!"),
    ///     PromptBuilder::new("farewell")
    ///         .description("Say goodbye")
    ///         .user_message("Goodbye!"),
    /// ];
    ///
    /// let router = McpRouter::new().prompts(prompts);
    /// ```
    pub fn prompts(self, prompts: impl IntoIterator<Item = Prompt>) -> Self {
        prompts
            .into_iter()
            .fold(self, |router, prompt| router.prompt(prompt))
    }

    /// Merge another router's capabilities into this one.
    ///
    /// This combines all tools, resources, resource templates, and prompts from
    /// the other router into this router. Uses "last wins" semantics for conflicts,
    /// meaning if both routers have a tool/resource/prompt with the same name,
    /// the one from `other` will replace the one in `self`.
    ///
    /// Server info, instructions, filters, and other router-level configuration
    /// are NOT merged - only the root router's settings are used.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult, ResourceBuilder};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct Input { value: String }
    ///
    /// // Create a router with database tools
    /// let db_tools = McpRouter::new()
    ///     .tool(
    ///         ToolBuilder::new("query")
    ///             .description("Query the database")
    ///             .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///             .build()
    ///     );
    ///
    /// // Create a router with API tools
    /// let api_tools = McpRouter::new()
    ///     .tool(
    ///         ToolBuilder::new("fetch")
    ///             .description("Fetch from API")
    ///             .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///             .build()
    ///     );
    ///
    /// // Merge them together
    /// let router = McpRouter::new()
    ///     .server_info("combined", "1.0")
    ///     .merge(db_tools)
    ///     .merge(api_tools);
    /// ```
    pub fn merge(mut self, other: McpRouter) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        let other_inner = other.inner;

        // Merge tools (last wins)
        for (name, tool) in &other_inner.tools {
            inner.tools.insert(name.clone(), tool.clone());
        }

        // Merge resources (last wins)
        for (uri, resource) in &other_inner.resources {
            inner.resources.insert(uri.clone(), resource.clone());
        }

        // Merge resource templates (append - no deduplication since templates
        // can have complex matching behavior)
        for template in &other_inner.resource_templates {
            inner.resource_templates.push(template.clone());
        }

        // Merge prompts (last wins)
        for (name, prompt) in &other_inner.prompts {
            inner.prompts.insert(name.clone(), prompt.clone());
        }

        self
    }

    /// Nest another router's capabilities under a prefix.
    ///
    /// This is similar to `merge()`, but all tool names from the nested router
    /// are prefixed with the given string and a dot separator. For example,
    /// nesting with prefix "db" will turn a tool named "query" into "db.query".
    ///
    /// Resources, resource templates, and prompts are merged without modification
    /// since they use URIs rather than simple names for identification.
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
    /// // Create a router with database tools
    /// let db_tools = McpRouter::new()
    ///     .tool(
    ///         ToolBuilder::new("query")
    ///             .description("Query the database")
    ///             .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///             .build()
    ///     )
    ///     .tool(
    ///         ToolBuilder::new("insert")
    ///             .description("Insert into database")
    ///             .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///             .build()
    ///     );
    ///
    /// // Nest under "db" prefix - tools become "db.query" and "db.insert"
    /// let router = McpRouter::new()
    ///     .server_info("combined", "1.0")
    ///     .nest("db", db_tools);
    /// ```
    pub fn nest(mut self, prefix: impl Into<String>, other: McpRouter) -> Self {
        let prefix = prefix.into();
        let inner = Arc::make_mut(&mut self.inner);
        let other_inner = other.inner;

        // Nest tools with prefix
        for tool in other_inner.tools.values() {
            let prefixed_tool = tool.with_name_prefix(&prefix);
            inner
                .tools
                .insert(prefixed_tool.name.clone(), Arc::new(prefixed_tool));
        }

        // Merge resources (no prefix - URIs are already namespaced)
        for (uri, resource) in &other_inner.resources {
            inner.resources.insert(uri.clone(), resource.clone());
        }

        // Merge resource templates (no prefix)
        for template in &other_inner.resource_templates {
            inner.resource_templates.push(template.clone());
        }

        // Merge prompts (no prefix - could be added in future if needed)
        for (name, prompt) in &other_inner.prompts {
            inner.prompts.insert(name.clone(), prompt.clone());
        }

        self
    }

    /// Register a completion handler for `completion/complete` requests.
    ///
    /// The handler receives `CompleteParams` containing the reference (prompt or resource)
    /// and the argument being completed, and should return completion suggestions.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, CompleteResult};
    /// use tower_mcp::protocol::{CompleteParams, CompletionReference};
    ///
    /// let router = McpRouter::new()
    ///     .completion_handler(|params: CompleteParams| async move {
    ///         // Provide completions based on the reference and argument
    ///         match params.reference {
    ///             CompletionReference::Prompt { name } => {
    ///                 // Return prompt argument completions
    ///                 Ok(CompleteResult::new(vec!["option1".to_string(), "option2".to_string()]))
    ///             }
    ///             CompletionReference::Resource { uri } => {
    ///                 // Return resource URI completions
    ///                 Ok(CompleteResult::new(vec![]))
    ///             }
    ///         }
    ///     });
    /// ```
    pub fn completion_handler<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(CompleteParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CompleteResult>> + Send + 'static,
    {
        Arc::make_mut(&mut self.inner).completion_handler =
            Some(Arc::new(move |params| Box::pin(handler(params))));
        self
    }

    /// Set a filter for tools based on session state.
    ///
    /// The filter determines which tools are visible to each session. Tools that
    /// don't pass the filter will not appear in `tools/list` responses and will
    /// return an error if called directly.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ToolBuilder, CallToolResult, CapabilityFilter, Tool, Filterable};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct Input { value: String }
    ///
    /// let public_tool = ToolBuilder::new("public")
    ///     .description("Available to everyone")
    ///     .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///     .build();
    ///
    /// let admin_tool = ToolBuilder::new("admin")
    ///     .description("Admin only")
    ///     .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///     .build();
    ///
    /// let router = McpRouter::new()
    ///     .tool(public_tool)
    ///     .tool(admin_tool)
    ///     .tool_filter(CapabilityFilter::new(|_session, tool: &Tool| {
    ///         // In real code, check session.extensions() for auth claims
    ///         tool.name() != "admin"
    ///     }));
    /// ```
    pub fn tool_filter(mut self, filter: ToolFilter) -> Self {
        Arc::make_mut(&mut self.inner).tool_filter = Some(filter);
        self
    }

    /// Set a filter for resources based on session state.
    ///
    /// The filter receives the current session state and each resource, returning
    /// `true` if the resource should be visible to this session. Resources that
    /// don't pass the filter will not appear in `resources/list` responses and will
    /// return an error if read directly.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ResourceBuilder, ReadResourceResult, CapabilityFilter, Resource, Filterable};
    ///
    /// let public_resource = ResourceBuilder::new("file:///public.txt")
    ///     .name("Public File")
    ///     .description("Available to everyone")
    ///     .text("public content");
    ///
    /// let secret_resource = ResourceBuilder::new("file:///secret.txt")
    ///     .name("Secret File")
    ///     .description("Admin only")
    ///     .text("secret content");
    ///
    /// let router = McpRouter::new()
    ///     .resource(public_resource)
    ///     .resource(secret_resource)
    ///     .resource_filter(CapabilityFilter::new(|_session, resource: &Resource| {
    ///         // In real code, check session.extensions() for auth claims
    ///         !resource.name().contains("Secret")
    ///     }));
    /// ```
    pub fn resource_filter(mut self, filter: ResourceFilter) -> Self {
        Arc::make_mut(&mut self.inner).resource_filter = Some(filter);
        self
    }

    /// Set a filter for prompts based on session state.
    ///
    /// The filter receives the current session state and each prompt, returning
    /// `true` if the prompt should be visible to this session. Prompts that
    /// don't pass the filter will not appear in `prompts/list` responses and will
    /// return an error if accessed directly.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, PromptBuilder, CapabilityFilter, Prompt, Filterable};
    ///
    /// let public_prompt = PromptBuilder::new("greeting")
    ///     .description("A friendly greeting")
    ///     .user_message("Hello!");
    ///
    /// let admin_prompt = PromptBuilder::new("system_debug")
    ///     .description("Admin debugging prompt")
    ///     .user_message("Debug info");
    ///
    /// let router = McpRouter::new()
    ///     .prompt(public_prompt)
    ///     .prompt(admin_prompt)
    ///     .prompt_filter(CapabilityFilter::new(|_session, prompt: &Prompt| {
    ///         // In real code, check session.extensions() for auth claims
    ///         !prompt.name().contains("system")
    ///     }));
    /// ```
    pub fn prompt_filter(mut self, filter: PromptFilter) -> Self {
        Arc::make_mut(&mut self.inner).prompt_filter = Some(filter);
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
    /// router.log(LoggingMessageParams::new(LogLevel::Info,
    ///     serde_json::json!({"message": "Operation completed"})
    /// ));
    ///
    /// // Error with logger name
    /// router.log(LoggingMessageParams::new(LogLevel::Error,
    ///     serde_json::json!({"error": "Connection failed"}))
    ///     .with_logger("database"));
    /// ```
    pub fn log(&self, params: LoggingMessageParams) -> bool {
        let Some(tx) = &self.inner.notification_tx else {
            return false;
        };
        tx.try_send(ServerNotification::LogMessage(params)).is_ok()
    }

    /// Send an info-level log message
    ///
    /// Convenience method for sending an info log with a message string.
    pub fn log_info(&self, message: &str) -> bool {
        self.log(LoggingMessageParams::new(
            LogLevel::Info,
            serde_json::json!({ "message": message }),
        ))
    }

    /// Send a warning-level log message
    pub fn log_warning(&self, message: &str) -> bool {
        self.log(LoggingMessageParams::new(
            LogLevel::Warning,
            serde_json::json!({ "message": message }),
        ))
    }

    /// Send an error-level log message
    pub fn log_error(&self, message: &str) -> bool {
        self.log(LoggingMessageParams::new(
            LogLevel::Error,
            serde_json::json!({ "message": message }),
        ))
    }

    /// Send a debug-level log message
    pub fn log_debug(&self, message: &str) -> bool {
        self.log(LoggingMessageParams::new(
            LogLevel::Debug,
            serde_json::json!({ "message": message }),
        ))
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

        let Some(tx) = &self.inner.notification_tx else {
            return false;
        };
        tx.try_send(ServerNotification::ResourceUpdated {
            uri: uri.to_string(),
        })
        .is_ok()
    }

    /// Notify clients that the list of available resources has changed
    ///
    /// Returns `true` if the notification was sent.
    pub fn notify_resources_list_changed(&self) -> bool {
        let Some(tx) = &self.inner.notification_tx else {
            return false;
        };
        tx.try_send(ServerNotification::ResourcesListChanged)
            .is_ok()
    }

    /// Notify clients that the list of available tools has changed
    ///
    /// Returns `true` if the notification was sent.
    pub fn notify_tools_list_changed(&self) -> bool {
        let Some(tx) = &self.inner.notification_tx else {
            return false;
        };
        tx.try_send(ServerNotification::ToolsListChanged).is_ok()
    }

    /// Notify clients that the list of available prompts has changed
    ///
    /// Returns `true` if the notification was sent.
    pub fn notify_prompts_list_changed(&self) -> bool {
        let Some(tx) = &self.inner.notification_tx else {
            return false;
        };
        tx.try_send(ServerNotification::PromptsListChanged).is_ok()
    }

    /// Get server capabilities based on registered handlers
    fn capabilities(&self) -> ServerCapabilities {
        let has_resources =
            !self.inner.resources.is_empty() || !self.inner.resource_templates.is_empty();
        let has_notifications = self.inner.notification_tx.is_some();

        #[cfg(feature = "dynamic-tools")]
        let has_dynamic_tools = self.inner.dynamic_tools.is_some();
        #[cfg(not(feature = "dynamic-tools"))]
        let has_dynamic_tools = false;

        ServerCapabilities {
            tools: if self.inner.tools.is_empty() && !has_dynamic_tools {
                None
            } else {
                Some(ToolsCapability {
                    list_changed: has_notifications,
                })
            },
            resources: if has_resources {
                Some(ResourcesCapability {
                    subscribe: true,
                    list_changed: has_notifications,
                })
            } else {
                None
            },
            prompts: if self.inner.prompts.is_empty() {
                None
            } else {
                Some(PromptsCapability {
                    list_changed: has_notifications,
                })
            },
            // Always advertise logging capability when notification channel is configured
            logging: if self.inner.notification_tx.is_some() {
                Some(LoggingCapability::default())
            } else {
                None
            },
            // Tasks capability is advertised if any tool supports tasks
            tasks: {
                let has_task_support = self
                    .inner
                    .tools
                    .values()
                    .any(|t| !matches!(t.task_support, TaskSupportMode::Forbidden));
                if has_task_support {
                    Some(TasksCapability {
                        list: Some(TasksListCapability {}),
                        cancel: Some(TasksCancelCapability {}),
                        requests: Some(TasksRequestsCapability {
                            tools: Some(TasksToolsCallCapability {}),
                        }),
                    })
                } else {
                    None
                }
            },
            // Completions capability when a handler is registered
            completions: if self.inner.completion_handler.is_some() {
                Some(CompletionsCapability::default())
            } else {
                None
            },
            experimental: None,
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
                        title: self.inner.server_title.clone(),
                        description: self.inner.server_description.clone(),
                        icons: self.inner.server_icons.clone(),
                        website_url: self.inner.server_website_url.clone(),
                    },
                    instructions: if let Some(config) = &self.inner.auto_instructions {
                        Some(self.inner.generate_instructions(config))
                    } else {
                        self.inner.instructions.clone()
                    },
                }))
            }

            McpRequest::ListTools(params) => {
                let filter = self.inner.tool_filter.as_ref();
                let is_visible = |t: &Tool| {
                    filter
                        .map(|f| f.is_visible(&self.session, t))
                        .unwrap_or(true)
                };

                // Collect static tools
                let mut tools: Vec<ToolDefinition> = self
                    .inner
                    .tools
                    .values()
                    .filter(|t| is_visible(t))
                    .map(|t| t.definition())
                    .collect();

                // Merge dynamic tools (static tools win on name collision)
                #[cfg(feature = "dynamic-tools")]
                if let Some(ref dynamic) = self.inner.dynamic_tools {
                    let static_names: HashSet<String> =
                        tools.iter().map(|t| t.name.clone()).collect();
                    for t in dynamic.list() {
                        if !static_names.contains(&t.name) && is_visible(&t) {
                            tools.push(t.definition());
                        }
                    }
                }

                tools.sort_by(|a, b| a.name.cmp(&b.name));

                let (tools, next_cursor) =
                    paginate(tools, params.cursor.as_deref(), self.inner.page_size)?;

                Ok(McpResponse::ListTools(ListToolsResult {
                    tools,
                    next_cursor,
                }))
            }

            McpRequest::CallTool(params) => {
                // Look up static tools first, then dynamic
                let tool = self.inner.tools.get(&params.name).cloned();
                #[cfg(feature = "dynamic-tools")]
                let tool = tool.or_else(|| {
                    self.inner
                        .dynamic_tools
                        .as_ref()
                        .and_then(|d| d.get(&params.name))
                });

                let tool = tool
                    .ok_or_else(|| Error::JsonRpc(JsonRpcError::method_not_found(&params.name)))?;

                // Check tool filter if configured
                if let Some(filter) = &self.inner.tool_filter
                    && !filter.is_visible(&self.session, &tool)
                {
                    return Err(filter.denial_error(&params.name));
                }

                if let Some(task_params) = params.task {
                    // Task-augmented request: validate task_support != Forbidden
                    if matches!(tool.task_support, TaskSupportMode::Forbidden) {
                        return Err(Error::JsonRpc(JsonRpcError::invalid_params(format!(
                            "Tool '{}' does not support async tasks",
                            params.name
                        ))));
                    }

                    // Create the task
                    let (task_id, cancellation_token) = self.inner.task_store.create_task(
                        &params.name,
                        params.arguments.clone(),
                        task_params.ttl,
                    );

                    tracing::info!(task_id = %task_id, tool = %params.name, "Created async task");

                    // Create a context for the async task execution
                    let progress_token = params.meta.and_then(|m| m.progress_token);
                    let ctx = self.create_context(request_id, progress_token);

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
                        let result = tool.call_with_context(ctx, arguments).await;

                        if cancellation_token.is_cancelled() {
                            tracing::debug!(task_id = %task_id_clone, "Task cancelled during execution");
                        } else if result.is_error {
                            // Tool returned an error result
                            let error_msg = result.first_text().unwrap_or("Tool execution failed");
                            task_store.fail_task(&task_id_clone, error_msg);
                            tracing::warn!(task_id = %task_id_clone, error = %error_msg, "Task failed");
                        } else {
                            task_store.complete_task(&task_id_clone, result);
                            tracing::debug!(task_id = %task_id_clone, "Task completed successfully");
                        }
                    });

                    let task = self.inner.task_store.get_task(&task_id).ok_or_else(|| {
                        Error::JsonRpc(JsonRpcError::internal_error(
                            "Failed to retrieve created task",
                        ))
                    })?;

                    Ok(McpResponse::CreateTask(CreateTaskResult { task }))
                } else {
                    // Synchronous request: validate task_support != Required
                    if matches!(tool.task_support, TaskSupportMode::Required) {
                        return Err(Error::JsonRpc(JsonRpcError::invalid_params(format!(
                            "Tool '{}' requires async task execution (include 'task' in params)",
                            params.name
                        ))));
                    }

                    // Extract progress token from request metadata
                    let progress_token = params.meta.and_then(|m| m.progress_token);
                    let ctx = self.create_context(request_id, progress_token);

                    tracing::debug!(tool = %params.name, "Calling tool");
                    let result = tool.call_with_context(ctx, params.arguments).await;

                    Ok(McpResponse::CallTool(result))
                }
            }

            McpRequest::ListResources(params) => {
                let mut resources: Vec<ResourceDefinition> = self
                    .inner
                    .resources
                    .values()
                    .filter(|r| {
                        // Apply resource filter if configured
                        self.inner
                            .resource_filter
                            .as_ref()
                            .map(|f| f.is_visible(&self.session, r))
                            .unwrap_or(true)
                    })
                    .map(|r| r.definition())
                    .collect();
                resources.sort_by(|a, b| a.uri.cmp(&b.uri));

                let (resources, next_cursor) =
                    paginate(resources, params.cursor.as_deref(), self.inner.page_size)?;

                Ok(McpResponse::ListResources(ListResourcesResult {
                    resources,
                    next_cursor,
                }))
            }

            McpRequest::ListResourceTemplates(params) => {
                let mut resource_templates: Vec<ResourceTemplateDefinition> = self
                    .inner
                    .resource_templates
                    .iter()
                    .map(|t| t.definition())
                    .collect();
                resource_templates.sort_by(|a, b| a.uri_template.cmp(&b.uri_template));

                let (resource_templates, next_cursor) = paginate(
                    resource_templates,
                    params.cursor.as_deref(),
                    self.inner.page_size,
                )?;

                Ok(McpResponse::ListResourceTemplates(
                    ListResourceTemplatesResult {
                        resource_templates,
                        next_cursor,
                    },
                ))
            }

            McpRequest::ReadResource(params) => {
                // First, try to find a static resource
                if let Some(resource) = self.inner.resources.get(&params.uri) {
                    // Check resource filter if configured
                    if let Some(filter) = &self.inner.resource_filter
                        && !filter.is_visible(&self.session, resource)
                    {
                        return Err(filter.denial_error(&params.uri));
                    }

                    tracing::debug!(uri = %params.uri, "Reading static resource");
                    let result = resource.read().await;
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

            McpRequest::ListPrompts(params) => {
                let mut prompts: Vec<PromptDefinition> = self
                    .inner
                    .prompts
                    .values()
                    .filter(|p| {
                        // Apply prompt filter if configured
                        self.inner
                            .prompt_filter
                            .as_ref()
                            .map(|f| f.is_visible(&self.session, p))
                            .unwrap_or(true)
                    })
                    .map(|p| p.definition())
                    .collect();
                prompts.sort_by(|a, b| a.name.cmp(&b.name));

                let (prompts, next_cursor) =
                    paginate(prompts, params.cursor.as_deref(), self.inner.page_size)?;

                Ok(McpResponse::ListPrompts(ListPromptsResult {
                    prompts,
                    next_cursor,
                }))
            }

            McpRequest::GetPrompt(params) => {
                let prompt = self.inner.prompts.get(&params.name).ok_or_else(|| {
                    Error::JsonRpc(JsonRpcError::method_not_found(&format!(
                        "Prompt not found: {}",
                        params.name
                    )))
                })?;

                // Check prompt filter if configured
                if let Some(filter) = &self.inner.prompt_filter
                    && !filter.is_visible(&self.session, prompt)
                {
                    return Err(filter.denial_error(&params.name));
                }

                tracing::debug!(name = %params.name, "Getting prompt");
                let result = prompt.get(params.arguments).await?;

                Ok(McpResponse::GetPrompt(result))
            }

            McpRequest::Ping => Ok(McpResponse::Pong(EmptyResult {})),

            McpRequest::ListTasks(params) => {
                let tasks = self.inner.task_store.list_tasks(params.status);

                let (tasks, next_cursor) =
                    paginate(tasks, params.cursor.as_deref(), self.inner.page_size)?;

                Ok(McpResponse::ListTasks(ListTasksResult {
                    tasks,
                    next_cursor,
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
                // Wait for task to reach terminal state (blocks if still running)
                let (task_obj, result, error) = self
                    .inner
                    .task_store
                    .wait_for_completion(&params.task_id)
                    .await
                    .ok_or_else(|| {
                        Error::JsonRpc(JsonRpcError::invalid_params(format!(
                            "Task not found: {}",
                            params.task_id
                        )))
                    })?;

                // Build _meta with related-task reference
                let meta = serde_json::json!({
                    "io.modelcontextprotocol/related-task": task_obj
                });

                match task_obj.status {
                    TaskStatus::Cancelled => Err(Error::JsonRpc(JsonRpcError::invalid_params(
                        format!("Task {} was cancelled", params.task_id),
                    ))),
                    TaskStatus::Failed => {
                        let mut call_result = CallToolResult::error(
                            error.unwrap_or_else(|| "Task failed".to_string()),
                        );
                        call_result.meta = Some(meta);
                        Ok(McpResponse::GetTaskResult(call_result))
                    }
                    _ => {
                        let mut call_result = result.unwrap_or_else(|| CallToolResult::text(""));
                        call_result.meta = Some(meta);
                        Ok(McpResponse::GetTaskResult(call_result))
                    }
                }
            }

            McpRequest::CancelTask(params) => {
                // First check if the task exists and is not already terminal
                let current = self
                    .inner
                    .task_store
                    .get_task(&params.task_id)
                    .ok_or_else(|| {
                        Error::JsonRpc(JsonRpcError::invalid_params(format!(
                            "Task not found: {}",
                            params.task_id
                        )))
                    })?;

                if current.status.is_terminal() {
                    return Err(Error::JsonRpc(JsonRpcError::invalid_params(format!(
                        "Task {} is already in terminal state: {}",
                        params.task_id, current.status
                    ))));
                }

                let task_obj = self
                    .inner
                    .task_store
                    .cancel_task(&params.task_id, params.reason.as_deref())
                    .ok_or_else(|| {
                        Error::JsonRpc(JsonRpcError::invalid_params(format!(
                            "Task not found: {}",
                            params.task_id
                        )))
                    })?;

                Ok(McpResponse::CancelTask(task_obj))
            }

            McpRequest::SetLoggingLevel(params) => {
                tracing::debug!(level = ?params.level, "Client set logging level");
                if let Ok(mut level) = self.inner.min_log_level.write() {
                    *level = params.level;
                }
                Ok(McpResponse::SetLoggingLevel(EmptyResult {}))
            }

            McpRequest::Complete(params) => {
                tracing::debug!(
                    reference = ?params.reference,
                    argument = %params.argument.name,
                    "Completion request"
                );

                // Delegate to registered completion handler if available
                if let Some(ref handler) = self.inner.completion_handler {
                    let result = handler(params).await?;
                    Ok(McpResponse::Complete(result))
                } else {
                    // No completion handler registered, return empty completions
                    Ok(McpResponse::Complete(CompleteResult::new(vec![])))
                }
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
                if let Some(ref request_id) = params.request_id {
                    if self.cancel_request(request_id) {
                        tracing::info!(
                            request_id = ?request_id,
                            reason = ?params.reason,
                            "Request cancelled"
                        );
                    } else {
                        tracing::debug!(
                            request_id = ?request_id,
                            reason = ?params.reason,
                            "Cancellation requested for unknown request"
                        );
                    }
                } else {
                    tracing::debug!(
                        reason = ?params.reason,
                        "Cancellation notification received without request_id"
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
            McpNotification::RootsListChanged => {
                tracing::info!("Client roots list changed");
                // Server should re-request roots if needed
                // This is handled by the application layer
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

// Re-export Extensions from context for backwards compatibility
pub use crate::context::Extensions;

/// Request type for the tower Service implementation
#[derive(Debug, Clone)]
pub struct RouterRequest {
    pub id: RequestId,
    pub inner: McpRequest,
    /// Type-map for passing data (e.g., `TokenClaims`) through middleware.
    pub extensions: Extensions,
}

/// Response type for the tower Service implementation
#[derive(Debug, Clone)]
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
                // Map tower-mcp errors to JSON-RPC errors:
                // - Error::JsonRpc: forwarded as-is (preserves original code)
                // - Error::Tool: mapped to -32603 (Internal Error)
                // - All others: mapped to -32603 (Internal Error)
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
    use crate::extract::{Context, Json};
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
                protocol_version: "2025-11-25".to_string(),
                capabilities: ClientCapabilities {
                    roots: None,
                    sampling: None,
                    elicitation: None,
                    tasks: None,
                    experimental: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
            }),
            extensions: Extensions::new(),
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
            .build();

        let mut router = McpRouter::new().tool(add_tool);

        // Initialize session first
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
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
            .build();

        let mut router = McpRouter::new().tool(add_tool);

        // Initialize session first
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "add".to_string(),
                arguments: serde_json::json!({"a": 2, "b": 3}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
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
            "protocolVersion": "2025-11-25",
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
            .build();

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
            .build();

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
        use crate::context::{ServerNotification, notification_channel};
        use crate::protocol::ProgressToken;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        // Track whether progress was reported
        let progress_reported = Arc::new(AtomicBool::new(false));
        let progress_ref = progress_reported.clone();

        // Create a tool that reports progress
        let tool = ToolBuilder::new("progress_tool")
            .description("Tool that reports progress")
            .extractor_handler((), move |ctx: Context, Json(_input): Json<AddInput>| {
                let reported = progress_ref.clone();
                async move {
                    // Report progress - this should work if token was extracted
                    ctx.report_progress(50.0, Some(100.0), Some("Halfway"))
                        .await;
                    reported.store(true, Ordering::SeqCst);
                    Ok(CallToolResult::text("done"))
                }
            })
            .build();

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
        use crate::context::notification_channel;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let progress_attempted = Arc::new(AtomicBool::new(false));
        let progress_ref = progress_attempted.clone();

        let tool = ToolBuilder::new("no_token_tool")
            .description("Tool that tries to report progress without token")
            .extractor_handler((), move |ctx: Context, Json(_input): Json<AddInput>| {
                let attempted = progress_ref.clone();
                async move {
                    // Try to report progress - should be a no-op without token
                    ctx.report_progress(50.0, Some(100.0), None).await;
                    attempted.store(true, Ordering::SeqCst);
                    Ok(CallToolResult::text("done"))
                }
            })
            .build();

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
            .build();

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
            extensions: Extensions::new(),
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
            extensions: Extensions::new(),
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
            extensions: Extensions::new(),
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
            extensions: Extensions::new(),
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
                protocol_version: "2025-11-25".to_string(),
                capabilities: ClientCapabilities {
                    roots: None,
                    sampling: None,
                    elicitation: None,
                    tasks: None,
                    experimental: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
            }),
            extensions: Extensions::new(),
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
                let data = params.data;
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
        let params = LoggingMessageParams::new(
            LogLevel::Error,
            serde_json::json!({
                "error": "Connection failed",
                "host": "localhost"
            }),
        )
        .with_logger("database");

        let sent = router.log(params);
        assert!(sent);

        let notification = rx.try_recv().unwrap();
        match notification {
            ServerNotification::LogMessage(params) => {
                assert_eq!(params.level, LogLevel::Error);
                assert_eq!(params.logger.as_deref(), Some("database"));
                let data = params.data;
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
                protocol_version: "2025-11-25".to_string(),
                capabilities: ClientCapabilities {
                    roots: None,
                    sampling: None,
                    elicitation: None,
                    tasks: None,
                    experimental: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
            }),
            extensions: Extensions::new(),
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
                protocol_version: "2025-11-25".to_string(),
                capabilities: ClientCapabilities {
                    roots: None,
                    sampling: None,
                    elicitation: None,
                    tasks: None,
                    experimental: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
            }),
            extensions: Extensions::new(),
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
    async fn test_create_task_via_call_tool() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .task_support(TaskSupportMode::Optional)
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();

        let mut router = McpRouter::new().tool(add_tool);
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "add".to_string(),
                arguments: serde_json::json!({"a": 5, "b": 10}),
                meta: None,
                task: Some(TaskRequestParams { ttl: None }),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::CreateTask(result)) => {
                assert!(result.task.task_id.starts_with("task-"));
                assert_eq!(result.task.status, TaskStatus::Working);
            }
            _ => panic!("Expected CreateTask response"),
        }
    }

    #[tokio::test]
    async fn test_list_tasks_empty() {
        let mut router = McpRouter::new();
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTasks(ListTasksParams::default()),
            extensions: Extensions::new(),
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
            .task_support(TaskSupportMode::Optional)
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();

        let mut router = McpRouter::new().tool(add_tool);
        init_router(&mut router).await;

        // Create task via tools/call with task params
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "add".to_string(),
                arguments: serde_json::json!({"a": 7, "b": 8}),
                meta: None,
                task: Some(TaskRequestParams { ttl: None }),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        let task_id = match resp.inner {
            Ok(McpResponse::CreateTask(result)) => result.task.task_id,
            _ => panic!("Expected CreateTask response"),
        };

        // Wait for task to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Get task result
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::GetTaskResult(GetTaskResultParams {
                task_id: task_id.clone(),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::GetTaskResult(result)) => {
                // Result should have _meta with related-task
                assert!(result.meta.is_some());
                // Check the result content
                match &result.content[0] {
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
            .task_support(TaskSupportMode::Optional)
            .handler(|_input: serde_json::Value| async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                Ok(CallToolResult::text("done"))
            })
            .build();

        let mut router = McpRouter::new().tool(slow_tool);
        init_router(&mut router).await;

        // Create task
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "slow".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: Some(TaskRequestParams { ttl: None }),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        let task_id = match resp.inner {
            Ok(McpResponse::CreateTask(result)) => result.task.task_id,
            _ => panic!("Expected CreateTask response"),
        };

        // Cancel the task
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::CancelTask(CancelTaskParams {
                task_id: task_id.clone(),
                reason: Some("Test cancellation".to_string()),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::CancelTask(task_obj)) => {
                assert_eq!(task_obj.status, TaskStatus::Cancelled);
            }
            _ => panic!("Expected CancelTask response"),
        }
    }

    #[tokio::test]
    async fn test_get_task_info() {
        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .task_support(TaskSupportMode::Optional)
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();

        let mut router = McpRouter::new().tool(add_tool);
        init_router(&mut router).await;

        // Create task with TTL
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "add".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                meta: None,
                task: Some(TaskRequestParams { ttl: Some(600_000) }),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        let task_id = match resp.inner {
            Ok(McpResponse::CreateTask(result)) => result.task.task_id,
            _ => panic!("Expected CreateTask response"),
        };

        // Get task info
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: task_id.clone(),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::GetTaskInfo(info)) => {
                assert_eq!(info.task_id, task_id);
                assert!(info.created_at.contains('T')); // ISO 8601
                assert_eq!(info.ttl, Some(600_000));
            }
            _ => panic!("Expected GetTaskInfo response"),
        }
    }

    #[tokio::test]
    async fn test_task_forbidden_tool_rejects_task_params() {
        let tool = ToolBuilder::new("sync_only")
            .description("Sync only tool")
            .handler(|_input: serde_json::Value| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new().tool(tool);
        init_router(&mut router).await;

        // Try to create task on a tool with Forbidden task support
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "sync_only".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: Some(TaskRequestParams { ttl: None }),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Err(e) => {
                assert!(e.message.contains("does not support async tasks"));
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
            extensions: Extensions::new(),
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
            extensions: Extensions::new(),
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
            extensions: Extensions::new(),
        };
        let _ = router.ready().await.unwrap().call(req).await.unwrap();
        assert!(router.is_subscribed("file:///test.txt"));

        // Now unsubscribe
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::UnsubscribeResource(UnsubscribeResourceParams {
                uri: "file:///test.txt".to_string(),
            }),
            extensions: Extensions::new(),
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
            extensions: Extensions::new(),
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
                protocol_version: "2025-11-25".to_string(),
                capabilities: ClientCapabilities {
                    roots: None,
                    sampling: None,
                    elicitation: None,
                    tasks: None,
                    experimental: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
            }),
            extensions: Extensions::new(),
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

    #[tokio::test]
    async fn test_completion_handler() {
        let router = McpRouter::new()
            .server_info("test", "1.0")
            .completion_handler(|params: CompleteParams| async move {
                // Return suggestions based on the argument value
                let prefix = &params.argument.value;
                let suggestions: Vec<String> = vec!["alpha", "beta", "gamma"]
                    .into_iter()
                    .filter(|s| s.starts_with(prefix))
                    .map(String::from)
                    .collect();
                Ok(CompleteResult::new(suggestions))
            });

        // Initialize
        let init_req = RouterRequest {
            id: RequestId::Number(0),
            inner: McpRequest::Initialize(InitializeParams {
                protocol_version: "2025-11-25".to_string(),
                capabilities: ClientCapabilities::default(),
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
            }),
            extensions: Extensions::new(),
        };
        let resp = router
            .clone()
            .ready()
            .await
            .unwrap()
            .call(init_req)
            .await
            .unwrap();

        // Check that completions capability is advertised
        match resp.inner {
            Ok(McpResponse::Initialize(result)) => {
                assert!(result.capabilities.completions.is_some());
            }
            _ => panic!("Expected Initialize response"),
        }

        // Send initialized notification
        router.handle_notification(McpNotification::Initialized);

        // Test completion request
        let complete_req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::Complete(CompleteParams {
                reference: CompletionReference::prompt("test-prompt"),
                argument: CompletionArgument::new("query", "al"),
                context: None,
            }),
            extensions: Extensions::new(),
        };
        let resp = router
            .clone()
            .ready()
            .await
            .unwrap()
            .call(complete_req)
            .await
            .unwrap();

        match resp.inner {
            Ok(McpResponse::Complete(result)) => {
                assert_eq!(result.completion.values, vec!["alpha"]);
            }
            _ => panic!("Expected Complete response"),
        }
    }

    #[tokio::test]
    async fn test_completion_without_handler_returns_empty() {
        let router = McpRouter::new().server_info("test", "1.0");

        // Initialize
        let init_req = RouterRequest {
            id: RequestId::Number(0),
            inner: McpRequest::Initialize(InitializeParams {
                protocol_version: "2025-11-25".to_string(),
                capabilities: ClientCapabilities::default(),
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
            }),
            extensions: Extensions::new(),
        };
        let resp = router
            .clone()
            .ready()
            .await
            .unwrap()
            .call(init_req)
            .await
            .unwrap();

        // Check that completions capability is NOT advertised
        match resp.inner {
            Ok(McpResponse::Initialize(result)) => {
                assert!(result.capabilities.completions.is_none());
            }
            _ => panic!("Expected Initialize response"),
        }

        // Send initialized notification
        router.handle_notification(McpNotification::Initialized);

        // Test completion request still works but returns empty
        let complete_req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::Complete(CompleteParams {
                reference: CompletionReference::prompt("test-prompt"),
                argument: CompletionArgument::new("query", "al"),
                context: None,
            }),
            extensions: Extensions::new(),
        };
        let resp = router
            .clone()
            .ready()
            .await
            .unwrap()
            .call(complete_req)
            .await
            .unwrap();

        match resp.inner {
            Ok(McpResponse::Complete(result)) => {
                assert!(result.completion.values.is_empty());
            }
            _ => panic!("Expected Complete response"),
        }
    }

    #[tokio::test]
    async fn test_tool_filter_list() {
        use crate::filter::CapabilityFilter;
        use crate::tool::Tool;

        let public_tool = ToolBuilder::new("public")
            .description("Public tool")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("public")) })
            .build();

        let admin_tool = ToolBuilder::new("admin")
            .description("Admin tool")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("admin")) })
            .build();

        let mut router = McpRouter::new()
            .tool(public_tool)
            .tool(admin_tool)
            .tool_filter(CapabilityFilter::new(|_, tool: &Tool| tool.name != "admin"));

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                // Only public tool should be visible
                assert_eq!(result.tools.len(), 1);
                assert_eq!(result.tools[0].name, "public");
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_tool_filter_call_denied() {
        use crate::filter::CapabilityFilter;
        use crate::tool::Tool;

        let admin_tool = ToolBuilder::new("admin")
            .description("Admin tool")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("admin")) })
            .build();

        let mut router = McpRouter::new()
            .tool(admin_tool)
            .tool_filter(CapabilityFilter::new(|_, _: &Tool| false)); // Deny all

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "admin".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        // Should get method not found error (default denial behavior)
        match resp.inner {
            Err(e) => {
                assert_eq!(e.code, -32601); // Method not found
            }
            _ => panic!("Expected JsonRpc error"),
        }
    }

    #[tokio::test]
    async fn test_tool_filter_call_allowed() {
        use crate::filter::CapabilityFilter;
        use crate::tool::Tool;

        let public_tool = ToolBuilder::new("public")
            .description("Public tool")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();

        let mut router = McpRouter::new()
            .tool(public_tool)
            .tool_filter(CapabilityFilter::new(|_, _: &Tool| true)); // Allow all

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "public".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::CallTool(result)) => {
                assert!(!result.is_error);
            }
            _ => panic!("Expected CallTool response"),
        }
    }

    #[tokio::test]
    async fn test_tool_filter_custom_denial() {
        use crate::filter::{CapabilityFilter, DenialBehavior};
        use crate::tool::Tool;

        let admin_tool = ToolBuilder::new("admin")
            .description("Admin tool")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("admin")) })
            .build();

        let mut router = McpRouter::new().tool(admin_tool).tool_filter(
            CapabilityFilter::new(|_, _: &Tool| false)
                .denial_behavior(DenialBehavior::Unauthorized),
        );

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "admin".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        // Should get forbidden error
        match resp.inner {
            Err(e) => {
                assert_eq!(e.code, -32007); // Forbidden
                assert!(e.message.contains("Unauthorized"));
            }
            _ => panic!("Expected JsonRpc error"),
        }
    }

    #[tokio::test]
    async fn test_resource_filter_list() {
        use crate::filter::CapabilityFilter;
        use crate::resource::{Resource, ResourceBuilder};

        let public_resource = ResourceBuilder::new("file:///public.txt")
            .name("Public File")
            .text("public content");

        let secret_resource = ResourceBuilder::new("file:///secret.txt")
            .name("Secret File")
            .text("secret content");

        let mut router = McpRouter::new()
            .resource(public_resource)
            .resource(secret_resource)
            .resource_filter(CapabilityFilter::new(|_, r: &Resource| {
                !r.name.contains("Secret")
            }));

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListResources(ListResourcesParams::default()),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListResources(result)) => {
                // Should only see public resource
                assert_eq!(result.resources.len(), 1);
                assert_eq!(result.resources[0].name, "Public File");
            }
            _ => panic!("Expected ListResources response"),
        }
    }

    #[tokio::test]
    async fn test_resource_filter_read_denied() {
        use crate::filter::CapabilityFilter;
        use crate::resource::{Resource, ResourceBuilder};

        let secret_resource = ResourceBuilder::new("file:///secret.txt")
            .name("Secret File")
            .text("secret content");

        let mut router = McpRouter::new()
            .resource(secret_resource)
            .resource_filter(CapabilityFilter::new(|_, _: &Resource| false)); // Deny all

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ReadResource(ReadResourceParams {
                uri: "file:///secret.txt".to_string(),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        // Should get method not found error (default denial behavior)
        match resp.inner {
            Err(e) => {
                assert_eq!(e.code, -32601); // Method not found
            }
            _ => panic!("Expected JsonRpc error"),
        }
    }

    #[tokio::test]
    async fn test_resource_filter_read_allowed() {
        use crate::filter::CapabilityFilter;
        use crate::resource::{Resource, ResourceBuilder};

        let public_resource = ResourceBuilder::new("file:///public.txt")
            .name("Public File")
            .text("public content");

        let mut router = McpRouter::new()
            .resource(public_resource)
            .resource_filter(CapabilityFilter::new(|_, _: &Resource| true)); // Allow all

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ReadResource(ReadResourceParams {
                uri: "file:///public.txt".to_string(),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ReadResource(result)) => {
                assert_eq!(result.contents.len(), 1);
                assert_eq!(result.contents[0].text.as_deref(), Some("public content"));
            }
            _ => panic!("Expected ReadResource response"),
        }
    }

    #[tokio::test]
    async fn test_resource_filter_custom_denial() {
        use crate::filter::{CapabilityFilter, DenialBehavior};
        use crate::resource::{Resource, ResourceBuilder};

        let secret_resource = ResourceBuilder::new("file:///secret.txt")
            .name("Secret File")
            .text("secret content");

        let mut router = McpRouter::new().resource(secret_resource).resource_filter(
            CapabilityFilter::new(|_, _: &Resource| false)
                .denial_behavior(DenialBehavior::Unauthorized),
        );

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ReadResource(ReadResourceParams {
                uri: "file:///secret.txt".to_string(),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        // Should get forbidden error
        match resp.inner {
            Err(e) => {
                assert_eq!(e.code, -32007); // Forbidden
                assert!(e.message.contains("Unauthorized"));
            }
            _ => panic!("Expected JsonRpc error"),
        }
    }

    #[tokio::test]
    async fn test_prompt_filter_list() {
        use crate::filter::CapabilityFilter;
        use crate::prompt::{Prompt, PromptBuilder};

        let public_prompt = PromptBuilder::new("greeting")
            .description("A greeting")
            .user_message("Hello!");

        let admin_prompt = PromptBuilder::new("system_debug")
            .description("Admin prompt")
            .user_message("Debug");

        let mut router = McpRouter::new()
            .prompt(public_prompt)
            .prompt(admin_prompt)
            .prompt_filter(CapabilityFilter::new(|_, p: &Prompt| {
                !p.name.contains("system")
            }));

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListPrompts(ListPromptsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListPrompts(result)) => {
                // Should only see public prompt
                assert_eq!(result.prompts.len(), 1);
                assert_eq!(result.prompts[0].name, "greeting");
            }
            _ => panic!("Expected ListPrompts response"),
        }
    }

    #[tokio::test]
    async fn test_prompt_filter_get_denied() {
        use crate::filter::CapabilityFilter;
        use crate::prompt::{Prompt, PromptBuilder};
        use std::collections::HashMap;

        let admin_prompt = PromptBuilder::new("system_debug")
            .description("Admin prompt")
            .user_message("Debug");

        let mut router = McpRouter::new()
            .prompt(admin_prompt)
            .prompt_filter(CapabilityFilter::new(|_, _: &Prompt| false)); // Deny all

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::GetPrompt(GetPromptParams {
                name: "system_debug".to_string(),
                arguments: HashMap::new(),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        // Should get method not found error (default denial behavior)
        match resp.inner {
            Err(e) => {
                assert_eq!(e.code, -32601); // Method not found
            }
            _ => panic!("Expected JsonRpc error"),
        }
    }

    #[tokio::test]
    async fn test_prompt_filter_get_allowed() {
        use crate::filter::CapabilityFilter;
        use crate::prompt::{Prompt, PromptBuilder};
        use std::collections::HashMap;

        let public_prompt = PromptBuilder::new("greeting")
            .description("A greeting")
            .user_message("Hello!");

        let mut router = McpRouter::new()
            .prompt(public_prompt)
            .prompt_filter(CapabilityFilter::new(|_, _: &Prompt| true)); // Allow all

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::GetPrompt(GetPromptParams {
                name: "greeting".to_string(),
                arguments: HashMap::new(),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::GetPrompt(result)) => {
                assert_eq!(result.messages.len(), 1);
            }
            _ => panic!("Expected GetPrompt response"),
        }
    }

    #[tokio::test]
    async fn test_prompt_filter_custom_denial() {
        use crate::filter::{CapabilityFilter, DenialBehavior};
        use crate::prompt::{Prompt, PromptBuilder};
        use std::collections::HashMap;

        let admin_prompt = PromptBuilder::new("system_debug")
            .description("Admin prompt")
            .user_message("Debug");

        let mut router = McpRouter::new().prompt(admin_prompt).prompt_filter(
            CapabilityFilter::new(|_, _: &Prompt| false)
                .denial_behavior(DenialBehavior::Unauthorized),
        );

        // Initialize session
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::GetPrompt(GetPromptParams {
                name: "system_debug".to_string(),
                arguments: HashMap::new(),
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        // Should get forbidden error
        match resp.inner {
            Err(e) => {
                assert_eq!(e.code, -32007); // Forbidden
                assert!(e.message.contains("Unauthorized"));
            }
            _ => panic!("Expected JsonRpc error"),
        }
    }

    // =========================================================================
    // Router Composition Tests (merge/nest)
    // =========================================================================

    #[derive(Debug, Deserialize, JsonSchema)]
    struct StringInput {
        value: String,
    }

    #[tokio::test]
    async fn test_router_merge_tools() {
        // Create first router with a tool
        let tool_a = ToolBuilder::new("tool_a")
            .description("Tool A")
            .handler(|_: StringInput| async move { Ok(CallToolResult::text("A")) })
            .build();

        let router_a = McpRouter::new().tool(tool_a);

        // Create second router with different tools
        let tool_b = ToolBuilder::new("tool_b")
            .description("Tool B")
            .handler(|_: StringInput| async move { Ok(CallToolResult::text("B")) })
            .build();
        let tool_c = ToolBuilder::new("tool_c")
            .description("Tool C")
            .handler(|_: StringInput| async move { Ok(CallToolResult::text("C")) })
            .build();

        let router_b = McpRouter::new().tool(tool_b).tool(tool_c);

        // Merge them
        let mut merged = McpRouter::new()
            .server_info("merged", "1.0")
            .merge(router_a)
            .merge(router_b);

        init_router(&mut merged).await;

        // List tools
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = merged.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 3);
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(names.contains(&"tool_a"));
                assert!(names.contains(&"tool_b"));
                assert!(names.contains(&"tool_c"));
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_router_merge_overwrites_duplicates() {
        // Create first router with a tool
        let tool_v1 = ToolBuilder::new("shared")
            .description("Version 1")
            .handler(|_: StringInput| async move { Ok(CallToolResult::text("v1")) })
            .build();

        let router_a = McpRouter::new().tool(tool_v1);

        // Create second router with same tool name but different description
        let tool_v2 = ToolBuilder::new("shared")
            .description("Version 2")
            .handler(|_: StringInput| async move { Ok(CallToolResult::text("v2")) })
            .build();

        let router_b = McpRouter::new().tool(tool_v2);

        // Merge - second should win
        let mut merged = McpRouter::new().merge(router_a).merge(router_b);

        init_router(&mut merged).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = merged.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 1);
                assert_eq!(result.tools[0].name, "shared");
                assert_eq!(result.tools[0].description.as_deref(), Some("Version 2"));
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_router_merge_resources() {
        use crate::resource::ResourceBuilder;

        // Create routers with different resources
        let router_a = McpRouter::new().resource(
            ResourceBuilder::new("file:///a.txt")
                .name("File A")
                .text("content a"),
        );

        let router_b = McpRouter::new().resource(
            ResourceBuilder::new("file:///b.txt")
                .name("File B")
                .text("content b"),
        );

        let mut merged = McpRouter::new().merge(router_a).merge(router_b);

        init_router(&mut merged).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListResources(ListResourcesParams::default()),
            extensions: Extensions::new(),
        };

        let resp = merged.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListResources(result)) => {
                assert_eq!(result.resources.len(), 2);
                let uris: Vec<&str> = result.resources.iter().map(|r| r.uri.as_str()).collect();
                assert!(uris.contains(&"file:///a.txt"));
                assert!(uris.contains(&"file:///b.txt"));
            }
            _ => panic!("Expected ListResources response"),
        }
    }

    #[tokio::test]
    async fn test_router_merge_prompts() {
        use crate::prompt::PromptBuilder;

        let router_a =
            McpRouter::new().prompt(PromptBuilder::new("prompt_a").user_message("Hello A"));

        let router_b =
            McpRouter::new().prompt(PromptBuilder::new("prompt_b").user_message("Hello B"));

        let mut merged = McpRouter::new().merge(router_a).merge(router_b);

        init_router(&mut merged).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListPrompts(ListPromptsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = merged.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListPrompts(result)) => {
                assert_eq!(result.prompts.len(), 2);
                let names: Vec<&str> = result.prompts.iter().map(|p| p.name.as_str()).collect();
                assert!(names.contains(&"prompt_a"));
                assert!(names.contains(&"prompt_b"));
            }
            _ => panic!("Expected ListPrompts response"),
        }
    }

    #[tokio::test]
    async fn test_router_nest_prefixes_tools() {
        // Create a router with tools
        let tool_query = ToolBuilder::new("query")
            .description("Query the database")
            .handler(|_: StringInput| async move { Ok(CallToolResult::text("query result")) })
            .build();
        let tool_insert = ToolBuilder::new("insert")
            .description("Insert into database")
            .handler(|_: StringInput| async move { Ok(CallToolResult::text("insert result")) })
            .build();

        let db_router = McpRouter::new().tool(tool_query).tool(tool_insert);

        // Nest under "db" prefix
        let mut router = McpRouter::new()
            .server_info("nested", "1.0")
            .nest("db", db_router);

        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 2);
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(names.contains(&"db.query"));
                assert!(names.contains(&"db.insert"));
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_router_nest_call_prefixed_tool() {
        let tool = ToolBuilder::new("echo")
            .description("Echo input")
            .handler(|input: StringInput| async move { Ok(CallToolResult::text(&input.value)) })
            .build();

        let nested_router = McpRouter::new().tool(tool);

        let mut router = McpRouter::new().nest("api", nested_router);

        init_router(&mut router).await;

        // Call the prefixed tool
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "api.echo".to_string(),
                arguments: serde_json::json!({"value": "hello world"}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::CallTool(result)) => {
                assert!(!result.is_error);
                match &result.content[0] {
                    Content::Text { text, .. } => assert_eq!(text, "hello world"),
                    _ => panic!("Expected text content"),
                }
            }
            _ => panic!("Expected CallTool response"),
        }
    }

    #[tokio::test]
    async fn test_router_multiple_nests() {
        let db_tool = ToolBuilder::new("query")
            .description("Database query")
            .handler(|_: StringInput| async move { Ok(CallToolResult::text("db")) })
            .build();

        let api_tool = ToolBuilder::new("fetch")
            .description("API fetch")
            .handler(|_: StringInput| async move { Ok(CallToolResult::text("api")) })
            .build();

        let db_router = McpRouter::new().tool(db_tool);
        let api_router = McpRouter::new().tool(api_tool);

        let mut router = McpRouter::new()
            .nest("db", db_router)
            .nest("api", api_router);

        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 2);
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(names.contains(&"db.query"));
                assert!(names.contains(&"api.fetch"));
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_router_merge_and_nest_combined() {
        // Test combining merge and nest
        let tool_a = ToolBuilder::new("local")
            .description("Local tool")
            .handler(|_: StringInput| async move { Ok(CallToolResult::text("local")) })
            .build();

        let nested_tool = ToolBuilder::new("remote")
            .description("Remote tool")
            .handler(|_: StringInput| async move { Ok(CallToolResult::text("remote")) })
            .build();

        let nested_router = McpRouter::new().tool(nested_tool);

        let mut router = McpRouter::new()
            .tool(tool_a)
            .nest("external", nested_router);

        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 2);
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(names.contains(&"local"));
                assert!(names.contains(&"external.remote"));
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_router_merge_preserves_server_info() {
        let child_router = McpRouter::new()
            .server_info("child", "2.0")
            .instructions("Child instructions");

        let mut router = McpRouter::new()
            .server_info("parent", "1.0")
            .instructions("Parent instructions")
            .merge(child_router);

        init_router(&mut router).await;

        // Initialize response should have parent's server info
        let init_req = RouterRequest {
            id: RequestId::Number(99),
            inner: McpRequest::Initialize(InitializeParams {
                protocol_version: "2025-11-25".to_string(),
                capabilities: ClientCapabilities::default(),
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
            }),
            extensions: Extensions::new(),
        };

        // Create fresh router for this test since we need to call initialize
        let child_router2 = McpRouter::new().server_info("child", "2.0");
        let mut fresh_router = McpRouter::new()
            .server_info("parent", "1.0")
            .merge(child_router2);

        let resp = fresh_router
            .ready()
            .await
            .unwrap()
            .call(init_req)
            .await
            .unwrap();

        match resp.inner {
            Ok(McpResponse::Initialize(result)) => {
                assert_eq!(result.server_info.name, "parent");
                assert_eq!(result.server_info.version, "1.0");
            }
            _ => panic!("Expected Initialize response"),
        }
    }

    // =========================================================================
    // Auto-instructions tests
    // =========================================================================

    #[tokio::test]
    async fn test_auto_instructions_tools_only() {
        let tool_a = ToolBuilder::new("alpha")
            .description("Alpha tool")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();
        let tool_b = ToolBuilder::new("beta")
            .description("Beta tool")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new()
            .auto_instructions()
            .tool(tool_a)
            .tool(tool_b);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.expect("should have instructions");

        assert!(instructions.contains("## Tools"));
        assert!(instructions.contains("- **alpha**: Alpha tool"));
        assert!(instructions.contains("- **beta**: Beta tool"));
        // No resources or prompts sections
        assert!(!instructions.contains("## Resources"));
        assert!(!instructions.contains("## Prompts"));
    }

    #[tokio::test]
    async fn test_auto_instructions_with_annotations() {
        let read_only_tool = ToolBuilder::new("query")
            .description("Run a query")
            .read_only()
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();
        let destructive_tool = ToolBuilder::new("delete")
            .description("Delete a record")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();
        let idempotent_tool = ToolBuilder::new("upsert")
            .description("Upsert a record")
            .non_destructive()
            .idempotent()
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new()
            .auto_instructions()
            .tool(read_only_tool)
            .tool(destructive_tool)
            .tool(idempotent_tool);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        assert!(instructions.contains("- **query**: Run a query [read-only]"));
        // delete has no annotations set via builder, so no tags
        assert!(instructions.contains("- **delete**: Delete a record\n"));
        assert!(instructions.contains("- **upsert**: Upsert a record [idempotent]"));
    }

    #[tokio::test]
    async fn test_auto_instructions_with_resources() {
        use crate::resource::ResourceBuilder;

        let resource = ResourceBuilder::new("file:///schema.sql")
            .name("Schema")
            .description("Database schema")
            .text("CREATE TABLE ...");

        let mut router = McpRouter::new().auto_instructions().resource(resource);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        assert!(instructions.contains("## Resources"));
        assert!(instructions.contains("- **file:///schema.sql**: Database schema"));
        assert!(!instructions.contains("## Tools"));
    }

    #[tokio::test]
    async fn test_auto_instructions_with_resource_templates() {
        use crate::resource::ResourceTemplateBuilder;

        let template = ResourceTemplateBuilder::new("file:///{path}")
            .name("File")
            .description("Read a file by path")
            .handler(
                |_uri: String, _vars: std::collections::HashMap<String, String>| async move {
                    Ok(crate::ReadResourceResult::text("content", "text/plain"))
                },
            );

        let mut router = McpRouter::new()
            .auto_instructions()
            .resource_template(template);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        assert!(instructions.contains("## Resources"));
        assert!(instructions.contains("- **file:///{path}**: Read a file by path"));
    }

    #[tokio::test]
    async fn test_auto_instructions_with_prompts() {
        use crate::prompt::PromptBuilder;

        let prompt = PromptBuilder::new("write_query")
            .description("Help write a SQL query")
            .user_message("Write a query for: {task}");

        let mut router = McpRouter::new().auto_instructions().prompt(prompt);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        assert!(instructions.contains("## Prompts"));
        assert!(instructions.contains("- **write_query**: Help write a SQL query"));
        assert!(!instructions.contains("## Tools"));
    }

    #[tokio::test]
    async fn test_auto_instructions_all_sections() {
        use crate::prompt::PromptBuilder;
        use crate::resource::ResourceBuilder;

        let tool = ToolBuilder::new("query")
            .description("Execute SQL")
            .read_only()
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();
        let resource = ResourceBuilder::new("db://schema")
            .name("Schema")
            .description("Full database schema")
            .text("schema");
        let prompt = PromptBuilder::new("write_query")
            .description("Help write a SQL query")
            .user_message("Write a query");

        let mut router = McpRouter::new()
            .auto_instructions()
            .tool(tool)
            .resource(resource)
            .prompt(prompt);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        // All three sections present
        assert!(instructions.contains("## Tools"));
        assert!(instructions.contains("## Resources"));
        assert!(instructions.contains("## Prompts"));

        // Sections appear in order: Tools, Resources, Prompts
        let tools_pos = instructions.find("## Tools").unwrap();
        let resources_pos = instructions.find("## Resources").unwrap();
        let prompts_pos = instructions.find("## Prompts").unwrap();
        assert!(tools_pos < resources_pos);
        assert!(resources_pos < prompts_pos);
    }

    #[tokio::test]
    async fn test_auto_instructions_with_prefix_and_suffix() {
        let tool = ToolBuilder::new("echo")
            .description("Echo input")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new()
            .auto_instructions_with(
                Some("This server provides echo capabilities."),
                Some("Contact admin@example.com for support."),
            )
            .tool(tool);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        assert!(instructions.starts_with("This server provides echo capabilities."));
        assert!(instructions.ends_with("Contact admin@example.com for support."));
        assert!(instructions.contains("## Tools"));
        assert!(instructions.contains("- **echo**: Echo input"));
    }

    #[tokio::test]
    async fn test_auto_instructions_prefix_only() {
        let tool = ToolBuilder::new("echo")
            .description("Echo input")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new()
            .auto_instructions_with(Some("My server intro."), None::<String>)
            .tool(tool);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        assert!(instructions.starts_with("My server intro."));
        assert!(instructions.contains("- **echo**: Echo input"));
    }

    #[tokio::test]
    async fn test_auto_instructions_empty_router() {
        let mut router = McpRouter::new().auto_instructions();

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.expect("should have instructions");

        // No sections when nothing is registered
        assert!(!instructions.contains("## Tools"));
        assert!(!instructions.contains("## Resources"));
        assert!(!instructions.contains("## Prompts"));
        assert!(instructions.is_empty());
    }

    #[tokio::test]
    async fn test_auto_instructions_overrides_manual() {
        let tool = ToolBuilder::new("echo")
            .description("Echo input")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new()
            .instructions("This will be overridden")
            .auto_instructions()
            .tool(tool);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        assert!(!instructions.contains("This will be overridden"));
        assert!(instructions.contains("- **echo**: Echo input"));
    }

    #[tokio::test]
    async fn test_no_auto_instructions_returns_manual() {
        let tool = ToolBuilder::new("echo")
            .description("Echo input")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new()
            .instructions("Manual instructions here")
            .tool(tool);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        assert_eq!(instructions, "Manual instructions here");
    }

    #[tokio::test]
    async fn test_auto_instructions_no_description_fallback() {
        let tool = ToolBuilder::new("mystery")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new().auto_instructions().tool(tool);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        assert!(instructions.contains("- **mystery**: No description"));
    }

    #[tokio::test]
    async fn test_auto_instructions_sorted_alphabetically() {
        let tool_z = ToolBuilder::new("zebra")
            .description("Z tool")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();
        let tool_a = ToolBuilder::new("alpha")
            .description("A tool")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();
        let tool_m = ToolBuilder::new("middle")
            .description("M tool")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new()
            .auto_instructions()
            .tool(tool_z)
            .tool(tool_a)
            .tool(tool_m);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        let alpha_pos = instructions.find("**alpha**").unwrap();
        let middle_pos = instructions.find("**middle**").unwrap();
        let zebra_pos = instructions.find("**zebra**").unwrap();
        assert!(alpha_pos < middle_pos);
        assert!(middle_pos < zebra_pos);
    }

    #[tokio::test]
    async fn test_auto_instructions_read_only_and_idempotent_tags() {
        let tool = ToolBuilder::new("safe_update")
            .description("Safe update operation")
            .idempotent()
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new().auto_instructions().tool(tool);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        assert!(
            instructions.contains("[idempotent]"),
            "got: {}",
            instructions
        );
    }

    #[tokio::test]
    async fn test_auto_instructions_lazy_generation() {
        // auto_instructions() is called BEFORE tools are registered
        // but instructions should still include tools
        let mut router = McpRouter::new().auto_instructions();

        let tool = ToolBuilder::new("late_tool")
            .description("Added after auto_instructions")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();

        router = router.tool(tool);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        assert!(instructions.contains("- **late_tool**: Added after auto_instructions"));
    }

    #[tokio::test]
    async fn test_auto_instructions_multiple_annotation_tags() {
        let tool = ToolBuilder::new("update")
            .description("Update a record")
            .annotations(ToolAnnotations {
                read_only_hint: true,
                idempotent_hint: true,
                ..Default::default()
            })
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new().auto_instructions().tool(tool);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        assert!(
            instructions.contains("[read-only, idempotent]"),
            "got: {}",
            instructions
        );
    }

    #[tokio::test]
    async fn test_auto_instructions_no_annotations_no_tags() {
        // Tools without annotations should have no tags at all
        let tool = ToolBuilder::new("fetch")
            .description("Fetch data")
            .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new().auto_instructions().tool(tool);

        let resp = send_initialize(&mut router).await;
        let instructions = resp.instructions.unwrap();

        // No bracket tags
        assert!(
            !instructions.contains('['),
            "should have no tags, got: {}",
            instructions
        );
        assert!(instructions.contains("- **fetch**: Fetch data"));
    }

    /// Helper to send an Initialize request and return the result
    async fn send_initialize(router: &mut McpRouter) -> InitializeResult {
        let init_req = RouterRequest {
            id: RequestId::Number(0),
            inner: McpRequest::Initialize(InitializeParams {
                protocol_version: "2025-11-25".to_string(),
                capabilities: ClientCapabilities {
                    roots: None,
                    sampling: None,
                    elicitation: None,
                    tasks: None,
                    experimental: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
            }),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(init_req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::Initialize(result)) => result,
            other => panic!("Expected Initialize response, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_notify_tools_list_changed() {
        let (tx, mut rx) = crate::context::notification_channel(16);

        let router = McpRouter::new()
            .server_info("test", "1.0")
            .with_notification_sender(tx);

        assert!(router.notify_tools_list_changed());

        let notification = rx.recv().await.unwrap();
        assert!(matches!(notification, ServerNotification::ToolsListChanged));
    }

    #[tokio::test]
    async fn test_notify_prompts_list_changed() {
        let (tx, mut rx) = crate::context::notification_channel(16);

        let router = McpRouter::new()
            .server_info("test", "1.0")
            .with_notification_sender(tx);

        assert!(router.notify_prompts_list_changed());

        let notification = rx.recv().await.unwrap();
        assert!(matches!(
            notification,
            ServerNotification::PromptsListChanged
        ));
    }

    #[tokio::test]
    async fn test_notify_without_sender_returns_false() {
        let router = McpRouter::new().server_info("test", "1.0");

        assert!(!router.notify_tools_list_changed());
        assert!(!router.notify_prompts_list_changed());
        assert!(!router.notify_resources_list_changed());
    }

    #[tokio::test]
    async fn test_list_changed_capabilities_with_notification_sender() {
        let (tx, _rx) = crate::context::notification_channel(16);
        let tool = ToolBuilder::new("test")
            .description("test")
            .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new()
            .server_info("test", "1.0")
            .tool(tool)
            .with_notification_sender(tx);

        init_router(&mut router).await;

        let caps = router.capabilities();
        let tools_cap = caps.tools.expect("tools capability should be present");
        assert!(
            tools_cap.list_changed,
            "tools.listChanged should be true when notification sender is configured"
        );
    }

    #[tokio::test]
    async fn test_list_changed_capabilities_without_notification_sender() {
        let tool = ToolBuilder::new("test")
            .description("test")
            .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new().server_info("test", "1.0").tool(tool);

        init_router(&mut router).await;

        let caps = router.capabilities();
        let tools_cap = caps.tools.expect("tools capability should be present");
        assert!(
            !tools_cap.list_changed,
            "tools.listChanged should be false without notification sender"
        );
    }

    #[tokio::test]
    async fn test_set_logging_level_filters_messages() {
        let (tx, mut rx) = crate::context::notification_channel(16);

        let mut router = McpRouter::new()
            .server_info("test", "1.0")
            .with_notification_sender(tx);

        init_router(&mut router).await;

        // Set logging level to Warning
        let set_level_req = RouterRequest {
            id: RequestId::Number(99),
            inner: McpRequest::SetLoggingLevel(SetLogLevelParams {
                level: LogLevel::Warning,
            }),
            extensions: crate::context::Extensions::new(),
        };
        let resp = router
            .ready()
            .await
            .unwrap()
            .call(set_level_req)
            .await
            .unwrap();
        assert!(matches!(resp.inner, Ok(McpResponse::SetLoggingLevel(_))));

        // Create a context from the router (simulating a handler)
        let ctx = router.create_context(RequestId::Number(100), None);

        // Error (more severe than Warning) should pass through
        ctx.send_log(LoggingMessageParams::new(
            LogLevel::Error,
            serde_json::Value::Null,
        ));
        assert!(
            rx.try_recv().is_ok(),
            "Error should pass through Warning filter"
        );

        // Info (less severe than Warning) should be filtered
        ctx.send_log(LoggingMessageParams::new(
            LogLevel::Info,
            serde_json::Value::Null,
        ));
        assert!(
            rx.try_recv().is_err(),
            "Info should be filtered at Warning level"
        );
    }

    #[test]
    fn test_paginate_no_page_size() {
        let items = vec![1, 2, 3, 4, 5];
        let (page, cursor) = paginate(items.clone(), None, None).unwrap();
        assert_eq!(page, items);
        assert!(cursor.is_none());
    }

    #[test]
    fn test_paginate_first_page() {
        let items = vec![1, 2, 3, 4, 5];
        let (page, cursor) = paginate(items, None, Some(2)).unwrap();
        assert_eq!(page, vec![1, 2]);
        assert!(cursor.is_some());
    }

    #[test]
    fn test_paginate_middle_page() {
        let items = vec![1, 2, 3, 4, 5];
        let (page1, cursor1) = paginate(items.clone(), None, Some(2)).unwrap();
        assert_eq!(page1, vec![1, 2]);

        let (page2, cursor2) = paginate(items, cursor1.as_deref(), Some(2)).unwrap();
        assert_eq!(page2, vec![3, 4]);
        assert!(cursor2.is_some());
    }

    #[test]
    fn test_paginate_last_page() {
        let items = vec![1, 2, 3, 4, 5];
        // Skip to offset 4 (last item)
        let cursor = encode_cursor(4);
        let (page, next) = paginate(items, Some(&cursor), Some(2)).unwrap();
        assert_eq!(page, vec![5]);
        assert!(next.is_none());
    }

    #[test]
    fn test_paginate_exact_boundary() {
        let items = vec![1, 2, 3, 4];
        let (page, cursor) = paginate(items, None, Some(4)).unwrap();
        assert_eq!(page, vec![1, 2, 3, 4]);
        assert!(cursor.is_none());
    }

    #[test]
    fn test_paginate_invalid_cursor() {
        let items = vec![1, 2, 3];
        let result = paginate(items, Some("not-valid-base64!@#$"), Some(2));
        assert!(result.is_err());
    }

    #[test]
    fn test_cursor_round_trip() {
        let offset = 42;
        let encoded = encode_cursor(offset);
        let decoded = decode_cursor(&encoded).unwrap();
        assert_eq!(decoded, offset);
    }

    #[tokio::test]
    async fn test_list_tools_pagination() {
        let tool_a = ToolBuilder::new("alpha")
            .description("a")
            .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();
        let tool_b = ToolBuilder::new("beta")
            .description("b")
            .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();
        let tool_c = ToolBuilder::new("gamma")
            .description("c")
            .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new()
            .server_info("test", "1.0")
            .page_size(2)
            .tool(tool_a)
            .tool(tool_b)
            .tool(tool_c);

        init_router(&mut router).await;

        // First page
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams { cursor: None }),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        let (tools, next_cursor) = match resp.inner {
            Ok(McpResponse::ListTools(result)) => (result.tools, result.next_cursor),
            other => panic!("Expected ListTools, got {:?}", other),
        };
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "alpha");
        assert_eq!(tools[1].name, "beta");
        assert!(next_cursor.is_some());

        // Second page
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::ListTools(ListToolsParams {
                cursor: next_cursor,
            }),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        let (tools, next_cursor) = match resp.inner {
            Ok(McpResponse::ListTools(result)) => (result.tools, result.next_cursor),
            other => panic!("Expected ListTools, got {:?}", other),
        };
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "gamma");
        assert!(next_cursor.is_none());
    }

    #[tokio::test]
    async fn test_list_tools_no_pagination_by_default() {
        let tool_a = ToolBuilder::new("alpha")
            .description("a")
            .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();
        let tool_b = ToolBuilder::new("beta")
            .description("b")
            .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new()
            .server_info("test", "1.0")
            .tool(tool_a)
            .tool(tool_b);

        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams { cursor: None }),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 2);
                assert!(result.next_cursor.is_none());
            }
            other => panic!("Expected ListTools, got {:?}", other),
        }
    }

    // =========================================================================
    // Dynamic Tool Registry Tests
    // =========================================================================

    #[cfg(feature = "dynamic-tools")]
    mod dynamic_tools_tests {
        use super::*;

        #[tokio::test]
        async fn test_dynamic_tools_register_and_list() {
            let (router, registry) = McpRouter::new()
                .server_info("test", "1.0")
                .with_dynamic_tools();

            let tool = ToolBuilder::new("dynamic_echo")
                .description("Dynamic echo")
                .handler(|input: AddInput| async move {
                    Ok(CallToolResult::text(format!("{}", input.a)))
                })
                .build();

            registry.register(tool);

            let mut router = router;
            init_router(&mut router).await;

            let req = RouterRequest {
                id: RequestId::Number(1),
                inner: McpRequest::ListTools(ListToolsParams::default()),
                extensions: Extensions::new(),
            };

            let resp = router.ready().await.unwrap().call(req).await.unwrap();
            match resp.inner {
                Ok(McpResponse::ListTools(result)) => {
                    assert_eq!(result.tools.len(), 1);
                    assert_eq!(result.tools[0].name, "dynamic_echo");
                }
                _ => panic!("Expected ListTools response"),
            }
        }

        #[tokio::test]
        async fn test_dynamic_tools_unregister() {
            let (router, registry) = McpRouter::new()
                .server_info("test", "1.0")
                .with_dynamic_tools();

            let tool = ToolBuilder::new("temp")
                .description("Temporary")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
                .build();

            registry.register(tool);
            assert!(registry.contains("temp"));

            let removed = registry.unregister("temp");
            assert!(removed);
            assert!(!registry.contains("temp"));

            // Unregistering again returns false
            assert!(!registry.unregister("temp"));

            let mut router = router;
            init_router(&mut router).await;

            let req = RouterRequest {
                id: RequestId::Number(1),
                inner: McpRequest::ListTools(ListToolsParams::default()),
                extensions: Extensions::new(),
            };

            let resp = router.ready().await.unwrap().call(req).await.unwrap();
            match resp.inner {
                Ok(McpResponse::ListTools(result)) => {
                    assert_eq!(result.tools.len(), 0);
                }
                _ => panic!("Expected ListTools response"),
            }
        }

        #[tokio::test]
        async fn test_dynamic_tools_merged_with_static() {
            let static_tool = ToolBuilder::new("static_tool")
                .description("Static")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("static")) })
                .build();

            let (router, registry) = McpRouter::new()
                .server_info("test", "1.0")
                .tool(static_tool)
                .with_dynamic_tools();

            let dynamic_tool = ToolBuilder::new("dynamic_tool")
                .description("Dynamic")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("dynamic")) })
                .build();

            registry.register(dynamic_tool);

            let mut router = router;
            init_router(&mut router).await;

            let req = RouterRequest {
                id: RequestId::Number(1),
                inner: McpRequest::ListTools(ListToolsParams::default()),
                extensions: Extensions::new(),
            };

            let resp = router.ready().await.unwrap().call(req).await.unwrap();
            match resp.inner {
                Ok(McpResponse::ListTools(result)) => {
                    assert_eq!(result.tools.len(), 2);
                    let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                    assert!(names.contains(&"static_tool"));
                    assert!(names.contains(&"dynamic_tool"));
                }
                _ => panic!("Expected ListTools response"),
            }
        }

        #[tokio::test]
        async fn test_static_tools_shadow_dynamic() {
            let static_tool = ToolBuilder::new("shared")
                .description("Static version")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("static")) })
                .build();

            let (router, registry) = McpRouter::new()
                .server_info("test", "1.0")
                .tool(static_tool)
                .with_dynamic_tools();

            let dynamic_tool = ToolBuilder::new("shared")
                .description("Dynamic version")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("dynamic")) })
                .build();

            registry.register(dynamic_tool);

            let mut router = router;
            init_router(&mut router).await;

            // List should only show the static version
            let req = RouterRequest {
                id: RequestId::Number(1),
                inner: McpRequest::ListTools(ListToolsParams::default()),
                extensions: Extensions::new(),
            };

            let resp = router.ready().await.unwrap().call(req).await.unwrap();
            match resp.inner {
                Ok(McpResponse::ListTools(result)) => {
                    assert_eq!(result.tools.len(), 1);
                    assert_eq!(result.tools[0].name, "shared");
                    assert_eq!(
                        result.tools[0].description.as_deref(),
                        Some("Static version")
                    );
                }
                _ => panic!("Expected ListTools response"),
            }

            // Call should dispatch to the static tool
            let req = RouterRequest {
                id: RequestId::Number(2),
                inner: McpRequest::CallTool(CallToolParams {
                    name: "shared".to_string(),
                    arguments: serde_json::json!({"a": 1, "b": 2}),
                    meta: None,
                    task: None,
                }),
                extensions: Extensions::new(),
            };

            let resp = router.ready().await.unwrap().call(req).await.unwrap();
            match resp.inner {
                Ok(McpResponse::CallTool(result)) => {
                    assert!(!result.is_error);
                    match &result.content[0] {
                        Content::Text { text, .. } => assert_eq!(text, "static"),
                        _ => panic!("Expected text content"),
                    }
                }
                _ => panic!("Expected CallTool response"),
            }
        }

        #[tokio::test]
        async fn test_dynamic_tools_call() {
            let (router, registry) = McpRouter::new()
                .server_info("test", "1.0")
                .with_dynamic_tools();

            let tool = ToolBuilder::new("add")
                .description("Add two numbers")
                .handler(|input: AddInput| async move {
                    Ok(CallToolResult::text(format!("{}", input.a + input.b)))
                })
                .build();

            registry.register(tool);

            let mut router = router;
            init_router(&mut router).await;

            let req = RouterRequest {
                id: RequestId::Number(1),
                inner: McpRequest::CallTool(CallToolParams {
                    name: "add".to_string(),
                    arguments: serde_json::json!({"a": 3, "b": 4}),
                    meta: None,
                    task: None,
                }),
                extensions: Extensions::new(),
            };

            let resp = router.ready().await.unwrap().call(req).await.unwrap();
            match resp.inner {
                Ok(McpResponse::CallTool(result)) => {
                    assert!(!result.is_error);
                    match &result.content[0] {
                        Content::Text { text, .. } => assert_eq!(text, "7"),
                        _ => panic!("Expected text content"),
                    }
                }
                _ => panic!("Expected CallTool response"),
            }
        }

        #[tokio::test]
        async fn test_dynamic_tools_notification_on_register() {
            let (tx, mut rx) = crate::context::notification_channel(16);
            let (router, registry) = McpRouter::new()
                .server_info("test", "1.0")
                .with_dynamic_tools();
            let _router = router.with_notification_sender(tx);

            let tool = ToolBuilder::new("notified")
                .description("Test")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
                .build();

            registry.register(tool);

            let notification = rx.recv().await.unwrap();
            assert!(matches!(notification, ServerNotification::ToolsListChanged));
        }

        #[tokio::test]
        async fn test_dynamic_tools_notification_on_unregister() {
            let (tx, mut rx) = crate::context::notification_channel(16);
            let (router, registry) = McpRouter::new()
                .server_info("test", "1.0")
                .with_dynamic_tools();
            let _router = router.with_notification_sender(tx);

            let tool = ToolBuilder::new("notified")
                .description("Test")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
                .build();

            registry.register(tool);
            // Consume the register notification
            let _ = rx.recv().await.unwrap();

            registry.unregister("notified");
            let notification = rx.recv().await.unwrap();
            assert!(matches!(notification, ServerNotification::ToolsListChanged));
        }

        #[tokio::test]
        async fn test_dynamic_tools_no_notification_on_empty_unregister() {
            let (tx, mut rx) = crate::context::notification_channel(16);
            let (router, registry) = McpRouter::new()
                .server_info("test", "1.0")
                .with_dynamic_tools();
            let _router = router.with_notification_sender(tx);

            // Unregister a tool that doesn't exist — should NOT send notification
            assert!(!registry.unregister("nonexistent"));

            // Channel should be empty
            assert!(rx.try_recv().is_err());
        }

        #[tokio::test]
        async fn test_dynamic_tools_filter_applies() {
            use crate::filter::CapabilityFilter;

            let (router, registry) = McpRouter::new()
                .server_info("test", "1.0")
                .tool_filter(CapabilityFilter::new(|_, tool: &Tool| {
                    tool.name != "hidden"
                }))
                .with_dynamic_tools();

            let visible = ToolBuilder::new("visible")
                .description("Visible")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
                .build();

            let hidden = ToolBuilder::new("hidden")
                .description("Hidden")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
                .build();

            registry.register(visible);
            registry.register(hidden);

            let mut router = router;
            init_router(&mut router).await;

            // List should only show visible tool
            let req = RouterRequest {
                id: RequestId::Number(1),
                inner: McpRequest::ListTools(ListToolsParams::default()),
                extensions: Extensions::new(),
            };

            let resp = router.ready().await.unwrap().call(req).await.unwrap();
            match resp.inner {
                Ok(McpResponse::ListTools(result)) => {
                    assert_eq!(result.tools.len(), 1);
                    assert_eq!(result.tools[0].name, "visible");
                }
                _ => panic!("Expected ListTools response"),
            }

            // Call to hidden tool should be denied
            let req = RouterRequest {
                id: RequestId::Number(2),
                inner: McpRequest::CallTool(CallToolParams {
                    name: "hidden".to_string(),
                    arguments: serde_json::json!({"a": 1, "b": 2}),
                    meta: None,
                    task: None,
                }),
                extensions: Extensions::new(),
            };

            let resp = router.ready().await.unwrap().call(req).await.unwrap();
            match resp.inner {
                Err(e) => {
                    assert_eq!(e.code, -32601); // Method not found
                }
                _ => panic!("Expected JsonRpc error"),
            }
        }

        #[tokio::test]
        async fn test_dynamic_tools_capabilities_advertised() {
            // No static tools, but dynamic tools enabled — should advertise tools capability
            let (mut router, _registry) = McpRouter::new()
                .server_info("test", "1.0")
                .with_dynamic_tools();

            let init_req = RouterRequest {
                id: RequestId::Number(1),
                inner: McpRequest::Initialize(InitializeParams {
                    protocol_version: "2025-11-25".to_string(),
                    capabilities: ClientCapabilities::default(),
                    client_info: Implementation {
                        name: "test".to_string(),
                        version: "1.0".to_string(),
                        ..Default::default()
                    },
                }),
                extensions: Extensions::new(),
            };

            let resp = router.ready().await.unwrap().call(init_req).await.unwrap();
            match resp.inner {
                Ok(McpResponse::Initialize(result)) => {
                    assert!(result.capabilities.tools.is_some());
                }
                _ => panic!("Expected Initialize response"),
            }
        }

        #[tokio::test]
        async fn test_dynamic_tools_multi_session_notification() {
            let (tx1, mut rx1) = crate::context::notification_channel(16);
            let (tx2, mut rx2) = crate::context::notification_channel(16);

            let (router, registry) = McpRouter::new()
                .server_info("test", "1.0")
                .with_dynamic_tools();

            // Simulate two sessions by calling with_notification_sender on two clones
            let _session1 = router.clone().with_notification_sender(tx1);
            let _session2 = router.clone().with_notification_sender(tx2);

            let tool = ToolBuilder::new("broadcast")
                .description("Test")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
                .build();

            registry.register(tool);

            // Both sessions should receive the notification
            let n1 = rx1.recv().await.unwrap();
            let n2 = rx2.recv().await.unwrap();
            assert!(matches!(n1, ServerNotification::ToolsListChanged));
            assert!(matches!(n2, ServerNotification::ToolsListChanged));
        }

        #[tokio::test]
        async fn test_dynamic_tools_call_not_found() {
            let (router, _registry) = McpRouter::new()
                .server_info("test", "1.0")
                .with_dynamic_tools();

            let mut router = router;
            init_router(&mut router).await;

            let req = RouterRequest {
                id: RequestId::Number(1),
                inner: McpRequest::CallTool(CallToolParams {
                    name: "nonexistent".to_string(),
                    arguments: serde_json::json!({}),
                    meta: None,
                    task: None,
                }),
                extensions: Extensions::new(),
            };

            let resp = router.ready().await.unwrap().call(req).await.unwrap();
            match resp.inner {
                Err(e) => {
                    assert_eq!(e.code, -32601);
                }
                _ => panic!("Expected method not found error"),
            }
        }

        #[tokio::test]
        async fn test_dynamic_tools_registry_list() {
            let (_, registry) = McpRouter::new()
                .server_info("test", "1.0")
                .with_dynamic_tools();

            assert!(registry.list().is_empty());

            let tool = ToolBuilder::new("tool_a")
                .description("A")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
                .build();
            registry.register(tool);

            let tool = ToolBuilder::new("tool_b")
                .description("B")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
                .build();
            registry.register(tool);

            let tools = registry.list();
            assert_eq!(tools.len(), 2);
            let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"tool_a"));
            assert!(names.contains(&"tool_b"));
        }
    } // mod dynamic_tools_tests
}
