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

use crate::async_task::{MemoryTaskStore, TaskStore, TaskStoreError};
use crate::context::{
    CancellationToken, ClientRequesterHandle, NotificationSender, RequestContext,
    ServerNotification,
};
use crate::error::{Error, JsonRpcError, Result};
use crate::filter::{PromptFilter, ResourceFilter, ToolFilter};
use crate::prompt::Prompt;
use crate::protocol::*;
#[cfg(feature = "dynamic-tools")]
use crate::registry::{
    DynamicPromptRegistry, DynamicPromptsInner, DynamicResourceRegistry,
    DynamicResourceTemplateRegistry, DynamicResourceTemplatesInner, DynamicResourcesInner,
    DynamicToolRegistry, DynamicToolsInner,
};
use crate::resource::{Resource, ResourceTemplate};
use crate::session::SessionState;
use crate::tool::Tool;

/// Type alias for completion handler function
pub(crate) type CompletionHandler = Arc<
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

/// Map a [`TaskStoreError`] to a JSON-RPC internal error.
fn task_store_error(e: TaskStoreError) -> Error {
    Error::JsonRpc(JsonRpcError::internal_error(format!(
        "Task store error: {}",
        e
    )))
}

/// Whether this request is using the final, stateless 2026-07-28 lifecycle.
///
/// Stable sessionful requests retain the crate's legacy task behavior; final
/// requests use extension negotiation and server-directed task creation.
#[cfg(feature = "stateless")]
fn is_final_protocol_request(extensions: &crate::context::Extensions) -> bool {
    extensions
        .get::<crate::stateless::StatelessRequestMeta>()
        .and_then(|meta| meta.protocol_version.as_deref())
        == Some(crate::protocol::PROTOCOL_VERSION_2026_07_28)
}

#[cfg(not(feature = "stateless"))]
fn is_final_protocol_request(_extensions: &crate::context::Extensions) -> bool {
    false
}

/// Whether this request's client declared the final Tasks extension.
///
/// Final requests carry client capabilities per request, so negotiation is
/// decided from the request itself rather than from session state.
#[cfg(feature = "stateless")]
fn client_declares_tasks(extensions: &crate::context::Extensions) -> bool {
    final_client_capabilities(extensions).is_some_and(|capabilities| {
        capabilities.extensions.as_ref().is_some_and(|declared| {
            declared.contains_key(tower_mcp_types::protocol::TASKS_EXTENSION_ID)
        })
    })
}

#[cfg(not(feature = "stateless"))]
fn client_declares_tasks(_extensions: &crate::context::Extensions) -> bool {
    false
}

/// Decode the wire `inputResponses` map into typed responses.
///
/// A key whose value does not match any known response shape is dropped here
/// rather than failing the request: the store treats an unmatched key as
/// ignorable, and SEP-2663 requires ignoring responses that do not correspond
/// to an outstanding request.
fn decode_input_responses(
    responses: &std::collections::HashMap<String, serde_json::Value>,
) -> crate::protocol::InputResponses {
    responses
        .iter()
        .filter_map(|(key, value)| {
            serde_json::from_value(value.clone())
                .ok()
                .map(|response| (key.clone(), response))
        })
        .collect()
}

/// The authenticated principal for this request, if any.
///
/// Sourced from the OAuth `sub` claim that the HTTP and WebSocket transports
/// bridge into MCP extensions. Without the `oauth` feature there is no
/// principal, so tasks are unowned and behave as they did before ownership
/// existed.
#[cfg(feature = "oauth")]
fn request_principal(extensions: &crate::context::Extensions) -> Option<String> {
    extensions
        .get::<crate::oauth::token::TokenClaims>()
        .and_then(|claims| claims.sub.clone())
}

#[cfg(not(feature = "oauth"))]
fn request_principal(_extensions: &crate::context::Extensions) -> Option<String> {
    None
}

/// Error for a task the server cannot serve.
///
/// Unknown and expired tasks are deliberately indistinguishable, so a caller
/// cannot probe for the existence of a task whose retention window closed.
fn unknown_task_error(task_id: &str) -> JsonRpcError {
    JsonRpcError::invalid_params(format!("Task not found: {task_id}"))
}

/// The client capability shape a server names in a `-32021` when it cannot
/// service a request without the Tasks extension.
pub(crate) fn tasks_client_capabilities() -> crate::protocol::ClientCapabilities {
    crate::protocol::ClientCapabilities {
        extensions: Some(
            [(
                tower_mcp_types::protocol::TASKS_EXTENSION_ID.to_string(),
                serde_json::json!({}),
            )]
            .into_iter()
            .collect(),
        ),
        ..Default::default()
    }
}

#[cfg(feature = "stateless")]
fn final_client_capabilities(
    extensions: &crate::context::Extensions,
) -> Option<&ClientCapabilities> {
    extensions
        .get::<crate::stateless::StatelessRequestMeta>()
        .and_then(|meta| meta.client_capabilities.as_ref())
}

#[cfg(not(feature = "stateless"))]
fn final_client_capabilities(
    _extensions: &crate::context::Extensions,
) -> Option<&ClientCapabilities> {
    None
}

/// Return whether `actual` contains every field and value in `required`.
///
/// Client capability objects are extensible, so extra advertised properties
/// must not cause a required-capability check to fail.
#[cfg(feature = "stateless")]
fn json_value_contains(actual: &serde_json::Value, required: &serde_json::Value) -> bool {
    match (actual, required) {
        (serde_json::Value::Object(actual), serde_json::Value::Object(required)) => {
            required.iter().all(|(key, value)| {
                actual
                    .get(key)
                    .is_some_and(|a| json_value_contains(a, value))
            })
        }
        _ => actual == required,
    }
}

#[cfg(feature = "stateless")]
fn client_capabilities_satisfy(actual: &ClientCapabilities, required: &ClientCapabilities) -> bool {
    let actual = serde_json::to_value(actual).expect("ClientCapabilities is always serializable");
    let mut required =
        serde_json::to_value(required).expect("ClientCapabilities is always serializable");
    // `roots.listChanged: false` means the optional notification capability
    // was not declared; it is not a requirement that the caller also set the
    // flag to false. Normalize it away before doing the structural subset
    // comparison so `{roots:{listChanged:true}}` satisfies plain `{roots:{}}`.
    if required.pointer("/roots/listChanged") == Some(&serde_json::Value::Bool(false))
        && let Some(roots) = required
            .get_mut("roots")
            .and_then(serde_json::Value::as_object_mut)
    {
        roots.remove("listChanged");
    }
    json_value_contains(&actual, &required)
}

#[cfg(feature = "stateless")]
fn validate_input_required_result(
    extensions: &crate::context::Extensions,
    result: &InputRequiredResult,
) -> Result<()> {
    result.validate().map_err(|message| {
        Error::invalid_params(format!("invalid InputRequiredResult: {message}"))
    })?;

    let meta = extensions
        .get::<crate::stateless::StatelessRequestMeta>()
        .filter(|meta| {
            meta.protocol_version.as_deref() == Some(crate::protocol::PROTOCOL_VERSION_2026_07_28)
        })
        .ok_or_else(|| {
            Error::invalid_params(
                "InputRequiredResult is only supported by the 2026-07-28 request lifecycle",
            )
        })?;
    let actual = meta.client_capabilities.as_ref().ok_or_else(|| {
        Error::invalid_params("clientCapabilities is required for InputRequiredResult")
    })?;

    if let Some(requests) = &result.input_requests {
        for request in requests.values() {
            let (supported, required) = match request {
                InputRequest::CreateMessage(params) => {
                    let requires_tools = params.tools.is_some();
                    let requires_context = params
                        .include_context
                        .is_some_and(|mode| mode != IncludeContext::None);
                    let required_sampling = SamplingCapability {
                        tools: requires_tools.then(SamplingToolsCapability::default),
                        context: requires_context.then(SamplingContextCapability::default),
                        ..SamplingCapability::default()
                    };
                    let supported = actual.sampling.as_ref().is_some_and(|sampling| {
                        (!requires_tools || sampling.tools.is_some())
                            && (!requires_context || sampling.context.is_some())
                    });
                    (
                        supported,
                        ClientCapabilities {
                            sampling: Some(required_sampling),
                            ..ClientCapabilities::default()
                        },
                    )
                }
                InputRequest::ListRoots(_) => (
                    actual.roots.is_some(),
                    ClientCapabilities {
                        roots: Some(RootsCapability::default()),
                        ..ClientCapabilities::default()
                    },
                ),
                InputRequest::Elicit(ElicitRequestParams::Form(_)) => {
                    let supported = actual.elicitation.as_ref().is_some_and(|elicitation| {
                        elicitation.form.is_some()
                            || (elicitation.form.is_none() && elicitation.url.is_none())
                    });
                    (
                        supported,
                        ClientCapabilities {
                            elicitation: Some(ElicitationCapability {
                                form: Some(ElicitationFormCapability::default()),
                                ..ElicitationCapability::default()
                            }),
                            ..ClientCapabilities::default()
                        },
                    )
                }
                InputRequest::Elicit(ElicitRequestParams::Url(_)) => (
                    actual
                        .elicitation
                        .as_ref()
                        .is_some_and(|elicitation| elicitation.url.is_some()),
                    ClientCapabilities {
                        elicitation: Some(ElicitationCapability {
                            url: Some(ElicitationUrlCapability::default()),
                            ..ElicitationCapability::default()
                        }),
                        ..ClientCapabilities::default()
                    },
                ),
                _ => {
                    return Err(Error::invalid_params(
                        "unsupported input request method in InputRequiredResult",
                    ));
                }
            };
            if !supported {
                return Err(Error::JsonRpc(
                    JsonRpcError::missing_required_client_capability(required),
                ));
            }
        }
    }
    Ok(())
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

#[cfg(all(feature = "http", feature = "stateless"))]
type ModernNotificationSink = Arc<dyn Fn(&ServerNotification) -> bool + Send + Sync + 'static>;

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
    /// Transport-lifetime sink for final HTTP subscription notifications.
    ///
    /// The lock is shared across router clones so an application-owned clone
    /// can publish after the transport attaches its subscription registry.
    #[cfg(all(feature = "http", feature = "stateless"))]
    modern_notification_sink: Arc<RwLock<Option<ModernNotificationSink>>>,
    /// Handle for sending requests to the client (for sampling, etc.)
    client_requester: Option<ClientRequesterHandle>,
    /// Task store for async operations
    task_store: Arc<dyn TaskStore>,
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
    /// Locally supported MCP protocol extensions and their server settings.
    protocol_extensions: HashMap<String, serde_json::Value>,
    /// Minimum log level for filtering outgoing log notifications (set by client via logging/setLevel)
    min_log_level: Arc<RwLock<LogLevel>>,
    /// Page size for list method pagination (None = return all results)
    page_size: Option<usize>,
    /// TTL hint for list responses in milliseconds (SEP-2549).
    /// When set, the value is returned as `ttlMs` in tools/list, resources/list,
    /// and prompts/list responses so clients can cache the list.
    list_ttl_ms: Option<u64>,
    /// Default TTL hint for resources/read responses in milliseconds
    /// (SEP-2549). Applied only when the resource handler did not set its
    /// own `ttl_ms` on the result.
    read_ttl_ms: Option<u64>,
    /// Cache scope for SEP-2549 hints on list and read responses. When a
    /// TTL is emitted and no scope is configured, `private` is used: it is
    /// the conservative choice (never shared across authorization
    /// contexts).
    cache_scope: Option<CacheScope>,
    /// Deprecation info for the logging capability (SEP-2577).
    /// When set, included in the `logging` capability in the initialize result.
    logging_deprecated: Option<tower_mcp_types::protocol::DeprecationInfo>,
    /// Names of tools that are currently disabled (hidden from list/call).
    disabled_tools: Arc<RwLock<HashSet<String>>>,
    /// URIs of resources that are currently disabled (hidden from list/read).
    disabled_resources: Arc<RwLock<HashSet<String>>>,
    /// Names of prompts that are currently disabled (hidden from list/get).
    disabled_prompts: Arc<RwLock<HashSet<String>>>,
    /// Dynamic tools registry for runtime tool (de)registration
    #[cfg(feature = "dynamic-tools")]
    dynamic_tools: Option<Arc<DynamicToolsInner>>,
    /// Dynamic prompts registry for runtime prompt (de)registration
    #[cfg(feature = "dynamic-tools")]
    dynamic_prompts: Option<Arc<DynamicPromptsInner>>,
    /// Dynamic resources registry for runtime resource (de)registration
    #[cfg(feature = "dynamic-tools")]
    dynamic_resources: Option<Arc<DynamicResourcesInner>>,
    /// Dynamic resource templates registry for runtime template (de)registration
    #[cfg(feature = "dynamic-tools")]
    dynamic_resource_templates: Option<Arc<DynamicResourceTemplatesInner>>,
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
                #[cfg(all(feature = "http", feature = "stateless"))]
                modern_notification_sink: Arc::new(RwLock::new(None)),
                client_requester: None,
                task_store: Arc::new(MemoryTaskStore::new()),
                subscriptions: Arc::new(RwLock::new(HashSet::new())),
                extensions: Arc::new(crate::context::Extensions::new()),
                protocol_extensions: HashMap::new(),
                completion_handler: None,
                tool_filter: None,
                resource_filter: None,
                prompt_filter: None,
                min_log_level: Arc::new(RwLock::new(LogLevel::Debug)),
                page_size: None,
                list_ttl_ms: None,
                read_ttl_ms: None,
                cache_scope: None,
                logging_deprecated: None,
                disabled_tools: Arc::new(RwLock::new(HashSet::new())),
                disabled_resources: Arc::new(RwLock::new(HashSet::new())),
                disabled_prompts: Arc::new(RwLock::new(HashSet::new())),
                #[cfg(feature = "dynamic-tools")]
                dynamic_tools: None,
                #[cfg(feature = "dynamic-tools")]
                dynamic_prompts: None,
                #[cfg(feature = "dynamic-tools")]
                dynamic_resources: None,
                #[cfg(feature = "dynamic-tools")]
                dynamic_resource_templates: None,
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

    /// Build a map of tool names to their annotations.
    ///
    /// The returned [`ToolAnnotationsMap`] includes annotations from all
    /// currently registered tools (both static and dynamic). Tools without
    /// annotations are omitted from the map.
    ///
    /// This is used internally by transports to inject annotations into
    /// request extensions, but can also be called directly for custom
    /// middleware setups.
    pub fn tool_annotations_map(&self) -> ToolAnnotationsMap {
        let disabled = self.inner.disabled_tools.read().unwrap();
        let mut map = HashMap::new();
        for (name, tool) in &self.inner.tools {
            if disabled.contains(name) {
                continue;
            }
            if let Some(annotations) = &tool.annotations {
                map.insert(name.clone(), annotations.clone());
            }
        }
        #[cfg(feature = "dynamic-tools")]
        if let Some(dynamic) = &self.inner.dynamic_tools {
            for tool in dynamic.list() {
                if disabled.contains(&tool.name) {
                    continue;
                }
                // Static tools take precedence
                if !map.contains_key(&tool.name)
                    && let Some(ref annotations) = tool.annotations
                {
                    map.insert(tool.name.clone(), annotations.clone());
                }
            }
        }
        ToolAnnotationsMap { map: Arc::new(map) }
    }

    /// Configure a pluggable [`TaskStore`] for async task state.
    ///
    /// The default is an in-process [`MemoryTaskStore`]. Supply an external
    /// store (Redis, Postgres, etc.) to share task state across server
    /// instances behind a load balancer, so `tasks/get` works regardless of
    /// which instance created the task (SEP-2663).
    ///
    /// # Example
    ///
    /// ```rust
    /// use std::sync::Arc;
    /// use tower_mcp::McpRouter;
    /// use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
    ///
    /// let store: Arc<dyn TaskStore> = Arc::new(MemoryTaskStore::new());
    /// let router = McpRouter::new().task_store(store);
    /// ```
    pub fn task_store(mut self, store: Arc<dyn TaskStore>) -> Self {
        Arc::make_mut(&mut self.inner).task_store = store;
        self
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

    /// Enable dynamic prompt registration and return a registry handle.
    ///
    /// The returned [`DynamicPromptRegistry`] can be used to add and remove
    /// prompts at runtime. Dynamic prompts are merged with static prompts
    /// when handling `prompts/list` and `prompts/get` requests. Static
    /// prompts take precedence over dynamic prompts when names collide.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, PromptBuilder};
    ///
    /// let (router, registry) = McpRouter::new()
    ///     .server_info("my-server", "1.0.0")
    ///     .with_dynamic_prompts();
    ///
    /// let prompt = PromptBuilder::new("greet")
    ///     .description("Greet someone")
    ///     .user_message("Hello!");
    ///
    /// registry.register(prompt);
    /// ```
    #[cfg(feature = "dynamic-tools")]
    pub fn with_dynamic_prompts(mut self) -> (Self, DynamicPromptRegistry) {
        let inner_dyn = Arc::new(DynamicPromptsInner::new());
        Arc::make_mut(&mut self.inner).dynamic_prompts = Some(inner_dyn.clone());
        (self, DynamicPromptRegistry::new(inner_dyn))
    }

    /// Enable dynamic resource registration and return a registry handle.
    ///
    /// The returned [`DynamicResourceRegistry`] can be used to add and remove
    /// resources at runtime. Dynamic resources are merged with static resources
    /// when handling `resources/list` and `resources/read` requests. Static
    /// resources take precedence over dynamic resources when URIs collide.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ResourceBuilder};
    ///
    /// let (router, registry) = McpRouter::new()
    ///     .server_info("my-server", "1.0.0")
    ///     .with_dynamic_resources();
    ///
    /// let resource = ResourceBuilder::new("file:///data.json")
    ///     .name("Data")
    ///     .text(r#"{"key": "value"}"#);
    ///
    /// registry.register(resource);
    /// ```
    #[cfg(feature = "dynamic-tools")]
    pub fn with_dynamic_resources(mut self) -> (Self, DynamicResourceRegistry) {
        let inner_dyn = Arc::new(DynamicResourcesInner::new());
        Arc::make_mut(&mut self.inner).dynamic_resources = Some(inner_dyn.clone());
        (self, DynamicResourceRegistry::new(inner_dyn))
    }

    /// Enable dynamic resource template registration and return a registry handle.
    ///
    /// The returned [`DynamicResourceTemplateRegistry`] can be used to add and
    /// remove resource templates at runtime. Dynamic templates are checked
    /// after static templates when handling `resources/read` requests.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tower_mcp::{McpRouter, ResourceTemplateBuilder};
    ///
    /// let (router, registry) = McpRouter::new()
    ///     .server_info("my-server", "1.0.0")
    ///     .with_dynamic_resource_templates();
    ///
    /// let template = ResourceTemplateBuilder::new("db://tables/{table}")
    ///     .name("Database Table")
    ///     .handler(|uri, vars| async move { /* ... */ });
    ///
    /// registry.register(template);
    /// ```
    #[cfg(feature = "dynamic-tools")]
    pub fn with_dynamic_resource_templates(mut self) -> (Self, DynamicResourceTemplateRegistry) {
        let inner_dyn = Arc::new(DynamicResourceTemplatesInner::new());
        Arc::make_mut(&mut self.inner).dynamic_resource_templates = Some(inner_dyn.clone());
        (self, DynamicResourceTemplateRegistry::new(inner_dyn))
    }

    /// Set the notification sender without registering it with the shared
    /// dynamic registries.
    ///
    /// Used by transports for per-request (sessionless) notification
    /// capture: the dynamic registries are long-lived and shared across
    /// router clones, so registering one sender per request would
    /// accumulate senders without bound.
    #[cfg(feature = "stateless")]
    #[cfg(feature = "http")]
    pub(crate) fn with_request_notification_sender(mut self, tx: NotificationSender) -> Self {
        Arc::make_mut(&mut self.inner).notification_tx = Some(tx);
        self
    }

    /// Set the notification sender for progress reporting
    ///
    /// This is typically called by the transport layer to receive notifications.
    pub fn with_notification_sender(mut self, tx: NotificationSender) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        // Also register the sender with dynamic registries so they can
        // broadcast list-changed notifications to this session.
        #[cfg(feature = "dynamic-tools")]
        if let Some(ref dynamic_tools) = inner.dynamic_tools {
            dynamic_tools.add_notification_sender(tx.clone());
        }
        #[cfg(feature = "dynamic-tools")]
        if let Some(ref dynamic_prompts) = inner.dynamic_prompts {
            dynamic_prompts.add_notification_sender(tx.clone());
        }
        #[cfg(feature = "dynamic-tools")]
        if let Some(ref dynamic_resources) = inner.dynamic_resources {
            dynamic_resources.add_notification_sender(tx.clone());
        }
        #[cfg(feature = "dynamic-tools")]
        if let Some(ref dynamic_resource_templates) = inner.dynamic_resource_templates {
            dynamic_resource_templates.add_notification_sender(tx.clone());
        }
        inner.notification_tx = Some(tx);
        self
    }

    /// Attach the transport-lifetime final subscription notification path.
    #[cfg(all(feature = "http", feature = "stateless"))]
    pub(crate) fn attach_modern_notification_sink(&self, sink: ModernNotificationSink) {
        if let Ok(mut active) = self.inner.modern_notification_sink.write() {
            *active = Some(sink);
        }
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

    /// Advertise one validated MCP protocol extension.
    ///
    /// This is separate from [`with_extension`](Self::with_extension), which
    /// stores process-local Rust values for handlers. Protocol extensions are
    /// advertised on the wire and become active only when the client declares
    /// the same identifier.
    pub fn with_protocol_extension(mut self, extension: crate::ExtensionDeclaration) -> Self {
        let (identifier, settings) = extension.into_parts();
        Arc::make_mut(&mut self.inner)
            .protocol_extensions
            .insert(identifier, settings);
        self
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
        self.create_context_with_extensions(request_id, progress_token, &Extensions::new())
    }

    /// Internal: build a `RequestContext` and additionally merge per-request
    /// extensions on top of the router's extensions. Used by [`Service::call`]
    /// to thread `RouterRequest.extensions` (e.g. SEP-2575 per-request
    /// `_meta`) through to handlers.
    pub(crate) fn create_context_with_extensions(
        &self,
        request_id: RequestId,
        progress_token: Option<ProgressToken>,
        per_request: &Extensions,
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

        // Start with router-level extensions, then layer per-request extensions
        // on top so they win on type collision. with_state() data stays
        // visible; per-request meta (SEP-2575) is now reachable too.
        let mut merged = (*self.inner.extensions).clone();
        merged.merge(per_request);
        let negotiated_extensions = if is_final_protocol_request(per_request) {
            let server_capabilities =
                self.capabilities_for_protocol(Some(crate::protocol::PROTOCOL_VERSION_2026_07_28));
            final_client_capabilities(per_request)
                .map(|client_capabilities| {
                    crate::NegotiatedExtensions::from_capabilities(
                        client_capabilities,
                        &server_capabilities,
                    )
                })
                .unwrap_or_default()
        } else {
            self.session
                .get::<crate::NegotiatedExtensions>()
                .unwrap_or_default()
        };
        merged.insert(negotiated_extensions);

        // The final protocol does not permit servers to initiate JSON-RPC
        // requests. Legacy transports may provide a requester scoped to the
        // originating request; prefer it over a transport-wide fallback so
        // restricted requests stay on their associated response channel.
        let ctx = if !is_final_protocol_request(per_request)
            && let Some(requester) = merged
                .get::<ClientRequesterHandle>()
                .cloned()
                .or_else(|| self.inner.client_requester.clone())
        {
            ctx.with_client_requester(requester)
        } else {
            ctx
        };

        // Adopt a transport-provided cancellation token (e.g. HTTP stateless
        // client disconnect) so `ctx.is_cancelled()` / `ctx.cancelled()` and
        // in-flight tracking observe the transport's signal.
        let ctx = if let Some(token) = merged.get::<CancellationToken>() {
            ctx.with_cancellation_token(token.clone())
        } else {
            ctx
        };

        let ctx = ctx.with_extensions(Arc::new(merged));

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

    /// Set a TTL hint on list responses (tools/list, resources/list, prompts/list).
    ///
    /// When set, the `ttlMs` field is included in list responses so clients can
    /// cache the list for up to this many milliseconds before re-fetching.
    /// Implements SEP-2549.
    pub fn list_ttl(mut self, ms: u64) -> Self {
        Arc::make_mut(&mut self.inner).list_ttl_ms = Some(ms);
        self
    }

    /// Set a default TTL hint on resources/read responses (SEP-2549).
    ///
    /// Applied only when the resource handler did not set its own `ttl_ms`
    /// on the [`ReadResourceResult`]. When any TTL is emitted without a
    /// configured [`cache_scope`](Self::cache_scope), the scope defaults to
    /// `private`.
    pub fn read_ttl(mut self, ms: u64) -> Self {
        Arc::make_mut(&mut self.inner).read_ttl_ms = Some(ms);
        self
    }

    /// Set the SEP-2549 cache scope emitted alongside TTL hints on list and
    /// resources/read responses.
    ///
    /// `CacheScope::Public` allows any client, gateway, or proxy to reuse
    /// the cached result across authorization contexts; `CacheScope::Private`
    /// restricts reuse to the same authorization context. When a TTL is
    /// emitted and no scope is configured, `private` is used as the
    /// conservative default.
    pub fn cache_scope(mut self, scope: CacheScope) -> Self {
        Arc::make_mut(&mut self.inner).cache_scope = Some(scope);
        self
    }

    /// Mark the logging capability as deprecated in the server's initialize result.
    ///
    /// When set, the `deprecated` object is included in the `logging` capability
    /// in the `initialize` response, signalling to clients that logging notifications
    /// are being phased out. Implements SEP-2577.
    pub fn logging_deprecated(mut self, info: tower_mcp_types::protocol::DeprecationInfo) -> Self {
        Arc::make_mut(&mut self.inner).logging_deprecated = Some(info);
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

    /// Conditionally register a tool.
    ///
    /// Registers the tool only if `condition` is `true`. This keeps fluent
    /// builder chains intact when tools are conditionally enabled.
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
    /// let enable_admin = false;
    ///
    /// let admin_tool = ToolBuilder::new("admin")
    ///     .description("Admin tool")
    ///     .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///     .build();
    ///
    /// let router = McpRouter::new()
    ///     .tool_if(enable_admin, admin_tool);
    /// ```
    pub fn tool_if(self, condition: bool, tool: Tool) -> Self {
        if condition { self.tool(tool) } else { self }
    }

    /// Register a resource
    pub fn resource(mut self, resource: Resource) -> Self {
        Arc::make_mut(&mut self.inner)
            .resources
            .insert(resource.uri.clone(), Arc::new(resource));
        self
    }

    /// Conditionally register a resource.
    ///
    /// Registers the resource only if `condition` is `true`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ResourceBuilder};
    ///
    /// let enable_config = false;
    ///
    /// let config = ResourceBuilder::new("config://system")
    ///     .name("config")
    ///     .text("secret=xxx");
    ///
    /// let router = McpRouter::new()
    ///     .resource_if(enable_config, config);
    /// ```
    pub fn resource_if(self, condition: bool, resource: Resource) -> Self {
        if condition {
            self.resource(resource)
        } else {
            self
        }
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
    ///                 meta: None,
    ///             }],
    ///             meta: None,
    ///             ..Default::default()
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

    /// Conditionally register a prompt.
    ///
    /// Registers the prompt only if `condition` is `true`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, PromptBuilder};
    ///
    /// let enable_debug = false;
    ///
    /// let debug_prompt = PromptBuilder::new("debug")
    ///     .description("Debug prompt")
    ///     .user_message("Debug mode enabled");
    ///
    /// let router = McpRouter::new()
    ///     .prompt_if(enable_debug, debug_prompt);
    /// ```
    pub fn prompt_if(self, condition: bool, prompt: Prompt) -> Self {
        if condition { self.prompt(prompt) } else { self }
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

    /// Conditionally register multiple tools at once.
    ///
    /// Registers all tools only if `condition` is `true`.
    pub fn tools_if(self, condition: bool, tools: impl IntoIterator<Item = Tool>) -> Self {
        if condition { self.tools(tools) } else { self }
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

    /// Conditionally register multiple resources at once.
    ///
    /// Registers all resources only if `condition` is `true`.
    pub fn resources_if(
        self,
        condition: bool,
        resources: impl IntoIterator<Item = Resource>,
    ) -> Self {
        if condition {
            self.resources(resources)
        } else {
            self
        }
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

    /// Conditionally register multiple prompts at once.
    ///
    /// Registers all prompts only if `condition` is `true`.
    pub fn prompts_if(self, condition: bool, prompts: impl IntoIterator<Item = Prompt>) -> Self {
        if condition {
            self.prompts(prompts)
        } else {
            self
        }
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

        // Merge protocol extension declarations (last wins).
        for (identifier, settings) in &other_inner.protocol_extensions {
            inner
                .protocol_extensions
                .insert(identifier.clone(), settings.clone());
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

        // Protocol extensions are server-wide declarations and are not
        // namespace-prefixed. Nested declarations use last-write-wins.
        for (identifier, settings) in &other_inner.protocol_extensions {
            inner
                .protocol_extensions
                .insert(identifier.clone(), settings.clone());
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
    ///             _ => Ok(CompleteResult::new(vec![])),
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
    /// Legacy sessions receive the notification only after
    /// `resources/subscribe`. Final HTTP listeners are filtered by their
    /// `subscriptions/listen` registration.
    /// Returns `true` if the notification was sent.
    pub fn notify_resource_updated(&self, uri: &str) -> bool {
        let notification = ServerNotification::ResourceUpdated {
            uri: uri.to_string(),
        };
        let mut sent = false;

        if self.is_subscribed(uri)
            && let Some(tx) = &self.inner.notification_tx
        {
            sent |= tx.try_send(notification.clone()).is_ok();
        }

        #[cfg(all(feature = "http", feature = "stateless"))]
        if let Ok(active) = self.inner.modern_notification_sink.read()
            && let Some(sink) = active.as_ref()
        {
            sent |= sink(&notification);
        }

        sent
    }

    /// Push a task's current state to subscribed `subscriptions/listen`
    /// streams as a `notifications/tasks` notification.
    ///
    /// The router already announces the transitions it drives: completion,
    /// failure, cancellation, and the resumption that follows a
    /// `tasks/update`. Call this after driving a transition yourself, most
    /// commonly [`TaskStore::require_input`], which a tool handler invokes on
    /// the store directly.
    ///
    /// Announcing task creation is deliberately left out. A client learns the
    /// task ID from the `tools/call` result, so it cannot have subscribed to a
    /// task before that result reaches it.
    ///
    /// [`TaskStore::require_input`]: crate::async_task::TaskStore::require_input
    pub async fn notify_task_status_changed(&self, task_id: &str) {
        self.notify_task_state(task_id).await;
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

    /// Disable a tool by name. Disabled tools are hidden from `tools/list`
    /// and return a method-not-found error from `tools/call`, but the tool
    /// definition stays attached to the router and can be flipped back on
    /// with [`enable_tool`](Self::enable_tool).
    ///
    /// State is shared across all clones produced by
    /// [`with_fresh_session`](Self::with_fresh_session), so flipping it once
    /// affects every connected session at the next request boundary. Call
    /// [`notify_tools_list_changed`](Self::notify_tools_list_changed) to nudge
    /// clients to re-fetch.
    pub fn disable_tool(&self, name: impl Into<String>) {
        let mut set = self.inner.disabled_tools.write().unwrap();
        set.insert(name.into());
    }

    /// Re-enable a previously disabled tool. No-op if the tool was not
    /// disabled.
    pub fn enable_tool(&self, name: &str) {
        let mut set = self.inner.disabled_tools.write().unwrap();
        set.remove(name);
    }

    /// Returns `true` if the named tool is currently enabled (i.e. not in
    /// the disabled set). Returns `true` even for unknown tool names; this
    /// only reports disable state, not registration.
    pub fn is_tool_enabled(&self, name: &str) -> bool {
        !self.inner.disabled_tools.read().unwrap().contains(name)
    }

    /// Disable a resource by URI. Disabled resources are hidden from
    /// `resources/list` and return a not-found error from `resources/read`.
    pub fn disable_resource(&self, uri: impl Into<String>) {
        let mut set = self.inner.disabled_resources.write().unwrap();
        set.insert(uri.into());
    }

    /// Re-enable a previously disabled resource.
    pub fn enable_resource(&self, uri: &str) {
        let mut set = self.inner.disabled_resources.write().unwrap();
        set.remove(uri);
    }

    /// Returns `true` if the resource at this URI is currently enabled.
    pub fn is_resource_enabled(&self, uri: &str) -> bool {
        !self.inner.disabled_resources.read().unwrap().contains(uri)
    }

    /// Disable a prompt by name. Disabled prompts are hidden from
    /// `prompts/list` and return a method-not-found error from `prompts/get`.
    pub fn disable_prompt(&self, name: impl Into<String>) {
        let mut set = self.inner.disabled_prompts.write().unwrap();
        set.insert(name.into());
    }

    /// Re-enable a previously disabled prompt.
    pub fn enable_prompt(&self, name: &str) {
        let mut set = self.inner.disabled_prompts.write().unwrap();
        set.remove(name);
    }

    /// Returns `true` if the named prompt is currently enabled.
    pub fn is_prompt_enabled(&self, name: &str) -> bool {
        !self.inner.disabled_prompts.read().unwrap().contains(name)
    }

    /// Get server capabilities based on registered handlers
    /// The server's identity, as configured via `.server_info()` and the
    /// related `.server_title()` / `.server_description()` / etc. builders.
    ///
    /// Shared by the `initialize` and `server/discover` handlers, and by the
    /// 2026-07-28 stateless HTTP dispatch (SEP-2575's "servers SHOULD
    /// identify themselves in each result's `_meta`") since that path calls
    /// in from outside this module and has no other way to read identity
    /// off a router wrapped behind arbitrary `.layer()` middleware.
    pub(crate) fn implementation(&self) -> Implementation {
        Implementation {
            name: self.inner.server_name.clone(),
            version: self.inner.server_version.clone(),
            title: self.inner.server_title.clone(),
            description: self.inner.server_description.clone(),
            icons: self.inner.server_icons.clone(),
            website_url: self.inner.server_website_url.clone(),
            meta: None,
        }
    }

    /// Return a snapshot of a registered tool's input schema.
    ///
    /// HTTP transport validation uses this before dispatch to enforce
    /// SEP-2243 `x-mcp-header` mappings. Static tools take precedence over
    /// dynamic tools, matching `tools/list` and `tools/call`.
    #[cfg(feature = "http")]
    pub(crate) fn tool_input_schema(&self, name: &str) -> Option<serde_json::Value> {
        if let Some(tool) = self.inner.tools.get(name) {
            return Some(tool.input_schema.clone());
        }
        #[cfg(feature = "dynamic-tools")]
        if let Some(tool) = self
            .inner
            .dynamic_tools
            .as_ref()
            .and_then(|tools| tools.get(name))
        {
            return Some(tool.input_schema.clone());
        }
        None
    }

    fn capabilities(&self) -> ServerCapabilities {
        let has_resources =
            !self.inner.resources.is_empty() || !self.inner.resource_templates.is_empty();
        let has_notifications = self.inner.notification_tx.is_some();

        #[cfg(feature = "dynamic-tools")]
        let has_dynamic_tools = self.inner.dynamic_tools.is_some();
        #[cfg(not(feature = "dynamic-tools"))]
        let has_dynamic_tools = false;

        #[cfg(feature = "dynamic-tools")]
        let has_dynamic_prompts = self.inner.dynamic_prompts.is_some();
        #[cfg(not(feature = "dynamic-tools"))]
        let has_dynamic_prompts = false;

        #[cfg(feature = "dynamic-tools")]
        let has_dynamic_resources = self.inner.dynamic_resources.is_some()
            || self.inner.dynamic_resource_templates.is_some();
        #[cfg(not(feature = "dynamic-tools"))]
        let has_dynamic_resources = false;

        ServerCapabilities {
            tools: if self.inner.tools.is_empty() && !has_dynamic_tools {
                None
            } else {
                Some(ToolsCapability {
                    list_changed: has_notifications,
                })
            },
            resources: if has_resources || has_dynamic_resources {
                Some(ResourcesCapability {
                    subscribe: true,
                    list_changed: has_notifications,
                })
            } else {
                None
            },
            prompts: if self.inner.prompts.is_empty() && !has_dynamic_prompts {
                None
            } else {
                Some(PromptsCapability {
                    list_changed: has_notifications,
                })
            },
            // Always advertise logging capability when notification channel is configured
            logging: if self.inner.notification_tx.is_some() {
                Some(LoggingCapability {
                    deprecated: self.inner.logging_deprecated.clone(),
                })
            } else {
                None
            },
            // Tasks capability is advertised if any tool supports tasks.
            // SEP-2663 moves the declaration to `capabilities.extensions`
            // under the reverse-DNS key `io.modelcontextprotocol/tasks`; we
            // continue to set the legacy top-level `tasks` field for back-compat
            // with 2025-11-25 clients that key off it.
            tasks: {
                let has_task_support = self
                    .inner
                    .tools
                    .values()
                    .any(|t| !matches!(t.task_support, TaskSupportMode::Forbidden));
                if has_task_support {
                    Some(TasksCapability {
                        // `list` is intentionally not advertised: final
                        // SEP-2663 removes `tasks/list` and this router
                        // answers MethodNotFound for it.
                        list: None,
                        cancel: Some(TasksCancelCapability {}),
                        requests: Some(TasksRequestsCapability {
                            tools: Some(TasksToolsRequestsCapability {
                                call: Some(TasksToolsCallCapability {}),
                            }),
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
            extensions: {
                let mut map = self.inner.protocol_extensions.clone();
                let has_task_support = self
                    .inner
                    .tools
                    .values()
                    .any(|t| !matches!(t.task_support, TaskSupportMode::Forbidden));
                if has_task_support {
                    map.insert(
                        tower_mcp_types::protocol::TASKS_EXTENSION_ID.to_string(),
                        serde_json::json!({}),
                    );
                }
                (!map.is_empty()).then_some(map)
            },
        }
    }

    /// Return the capability surface appropriate for a protocol version.
    ///
    /// `capabilities.tasks` is the legacy 2025-11-25 shape and is never
    /// advertised on the final path. The final extension is advertised only
    /// when the server opted in via [`McpRouter::with_tasks`]; merely
    /// registering task-capable tools does not advertise it, so a server that
    /// has not opted in presents no Tasks surface to a 2026-07-28 client.
    fn capabilities_for_protocol(&self, protocol_version: Option<&str>) -> ServerCapabilities {
        let mut capabilities = self.capabilities();
        if protocol_version == Some(crate::protocol::PROTOCOL_VERSION_2026_07_28) {
            capabilities.tasks = None;
            if !self.final_tasks_enabled()
                && let Some(extensions) = capabilities.extensions.as_mut()
            {
                extensions.remove(tower_mcp_types::protocol::TASKS_EXTENSION_ID);
                if extensions.is_empty() {
                    capabilities.extensions = None;
                }
            }
        }
        capabilities
    }

    /// Whether this server opted into the final Tasks extension.
    ///
    /// Distinct from the synthesized advertisement in [`Self::capabilities`],
    /// which reflects registered tools rather than an explicit choice.
    pub(crate) fn final_tasks_enabled(&self) -> bool {
        self.inner
            .protocol_extensions
            .contains_key(tower_mcp_types::protocol::TASKS_EXTENSION_ID)
    }

    /// Reject a final task method that was not negotiated by both peers.
    ///
    /// An unnegotiated method is reported as absent rather than forbidden:
    /// the server genuinely does not serve it for this client.
    fn require_negotiated_tasks(
        &self,
        extensions: &crate::context::Extensions,
        method: &str,
    ) -> Result<()> {
        if self.final_tasks_enabled() && client_declares_tasks(extensions) {
            Ok(())
        } else {
            Err(Error::JsonRpc(JsonRpcError::method_not_found(method)))
        }
    }

    /// Verify the caller may act on this task.
    ///
    /// A task the caller does not own is reported exactly as an unknown task.
    /// Distinguishing the two would confirm that an ID is real, which is the
    /// thing unguessable IDs exist to prevent.
    async fn authorize_task(
        &self,
        task_id: &str,
        extensions: &crate::context::Extensions,
    ) -> Result<()> {
        let owner = self
            .inner
            .task_store
            .task_owner(task_id)
            .await
            .map_err(task_store_error)?
            .ok_or_else(|| Error::JsonRpc(unknown_task_error(task_id)))?;

        if crate::async_task::owner_matches(&owner, request_principal(extensions).as_deref()) {
            Ok(())
        } else {
            tracing::debug!(
                target: "mcp::tasks",
                task_id = %task_id,
                "task operation refused: principal does not own the task"
            );
            Err(Error::JsonRpc(unknown_task_error(task_id)))
        }
    }

    /// Serve a final `tasks/get` as a status-discriminated `DetailedTask`.
    async fn final_get_task(&self, task_id: &str) -> Result<McpResponse> {
        let detailed = self.detailed_task(task_id).await?;
        Ok(McpResponse::FinalGetTask(crate::tasks::GetTaskResult::new(
            detailed,
        )))
    }

    /// Build the complete status-discriminated view of a task.
    ///
    /// Both `tasks/get` and `notifications/tasks` render a task through this
    /// one path, which is what makes a pushed notification identical to the
    /// poll response a client would have received at that moment.
    async fn detailed_task(&self, task_id: &str) -> Result<crate::tasks::DetailedTask> {
        let (task, result, error) = self
            .inner
            .task_store
            .get_task_result(task_id)
            .await
            .map_err(task_store_error)?
            .ok_or_else(|| Error::JsonRpc(unknown_task_error(task_id)))?;

        let mut metadata = crate::tasks::TaskMetadata::new(
            task.task_id.clone(),
            task.created_at.clone(),
            task.last_updated_at.clone(),
            task.ttl,
        );
        metadata.status_message = task.status_message.clone();
        metadata.poll_interval_ms = task.poll_interval;

        Ok(match task.status {
            TaskStatus::Working => crate::tasks::DetailedTask::working(metadata),
            TaskStatus::InputRequired => {
                // Every request still awaiting a response, not just the most
                // recent one.
                let outstanding = self
                    .inner
                    .task_store
                    .outstanding_input_requests(task_id)
                    .await
                    .map_err(task_store_error)?
                    .unwrap_or_default();
                crate::tasks::DetailedTask::input_required(metadata, outstanding)
            }
            TaskStatus::Completed => {
                // The exact object the synchronous call would have returned,
                // including `isError: true` results.
                let mut object = result
                    .map(serde_json::to_value)
                    .transpose()
                    .map_err(|e| {
                        Error::JsonRpc(JsonRpcError::internal_error(format!(
                            "failed to encode task result: {e}"
                        )))
                    })?
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                // This object is nested inside tasks/get, so it does not pass
                // through the JSON-RPC response stamper that adds the final
                // protocol's required complete discriminator.
                object.insert(
                    "resultType".to_string(),
                    serde_json::Value::String("complete".to_string()),
                );
                crate::tasks::DetailedTask::completed(metadata, object)
            }
            TaskStatus::Failed => crate::tasks::DetailedTask::failed(
                metadata,
                error.unwrap_or_else(|| JsonRpcError::internal_error("Task failed")),
            ),
            TaskStatus::Cancelled => crate::tasks::DetailedTask::cancelled(metadata),
            // `TaskStatus` is non_exhaustive. Report an unrecognized status as
            // working rather than inventing a terminal state.
            _ => crate::tasks::DetailedTask::working(metadata),
        })
    }

    /// Push the current state of a task to subscribed listen streams.
    ///
    /// Best effort by design. A task outlives the request that created it, so
    /// there may be no subscriber at all, and SEP-2663 keeps `tasks/get`
    /// authoritative precisely so a dropped notification costs a client
    /// nothing beyond a slower poll. A failure to read the task back is
    /// therefore logged rather than propagated: the caller has already
    /// committed the state change this announces.
    async fn notify_task_state(&self, task_id: &str) {
        if !self.final_tasks_enabled() {
            return;
        }

        let detailed = match self.detailed_task(task_id).await {
            Ok(detailed) => detailed,
            Err(error) => {
                tracing::debug!(
                    target: "mcp::tasks",
                    task_id = %task_id,
                    %error,
                    "skipping task notification: task state unavailable"
                );
                return;
            }
        };

        let notification = ServerNotification::FinalTaskStatusChanged(
            crate::tasks::TaskStatusNotificationParams {
                task: detailed,
                meta: None,
            },
        );

        // Delivery goes through the transport-lifetime sink rather than the
        // originating request's sender: the `tools/call` that created the task
        // has usually completed by the time a terminal transition happens, and
        // its stream is gone.
        #[cfg(all(feature = "http", feature = "stateless"))]
        if let Ok(active) = self.inner.modern_notification_sink.read()
            && let Some(sink) = active.as_ref()
        {
            sink(&notification);
            return;
        }

        if let Some(tx) = &self.inner.notification_tx {
            let _ = tx.try_send(notification);
        }
    }

    /// Effective SEP-2549 cache scope to emit alongside a TTL hint.
    ///
    /// Returns the configured scope, or `private` (the conservative choice)
    /// when a TTL is being emitted without an explicit scope. Returns `None`
    /// when no TTL is emitted and no scope is configured, so responses
    /// without hints stay hint-free.
    fn effective_cache_scope(&self, ttl_ms: Option<u64>) -> Option<CacheScope> {
        self.inner
            .cache_scope
            .or_else(|| ttl_ms.map(|_| CacheScope::Private))
    }

    /// Fill in SEP-2549 caching hints on a resources/read result.
    ///
    /// Handler-set values win; the router-level `read_ttl` and `cache_scope`
    /// configuration only fills fields the handler left unset.
    fn apply_read_cache_hints(&self, mut result: ReadResourceResult) -> ReadResourceResult {
        if result.ttl_ms.is_none() {
            result.ttl_ms = self.inner.read_ttl_ms;
        }
        if result.cache_scope.is_none() {
            result.cache_scope = self.effective_cache_scope(result.ttl_ms);
        }
        result
    }

    /// Handle an MCP request
    async fn handle(
        &self,
        request_id: RequestId,
        request: McpRequest,
        extensions: Extensions,
    ) -> Result<McpResponse> {
        // Enforce session state - reject requests before initialization
        let method = request.method_name();
        if !is_final_protocol_request(&extensions) && !self.session.is_request_allowed(method) {
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

                // HTTP and other configurable transports inject their exact
                // runtime allow-list. Direct router use retains the stable
                // default policy.
                let protocol_support = extensions.get::<crate::ProtocolSupport>();
                let requested_is_legacy = crate::protocol::SUPPORTED_PROTOCOL_VERSIONS
                    .contains(&params.protocol_version.as_str());
                let requested_is_supported = requested_is_legacy
                    && protocol_support
                        .is_none_or(|support| support.contains(&params.protocol_version));
                let protocol_version = if requested_is_supported {
                    params.protocol_version
                } else {
                    match protocol_support {
                        None => crate::protocol::LATEST_PROTOCOL_VERSION.to_string(),
                        Some(support) => support
                            .versions()
                            .iter()
                            .find(|version| {
                                crate::protocol::SUPPORTED_PROTOCOL_VERSIONS
                                    .contains(&version.as_str())
                            })
                            .cloned()
                            .ok_or_else(|| {
                                Error::JsonRpc(JsonRpcError::unsupported_protocol_version(
                                    params.protocol_version,
                                    support.versions().iter().map(String::as_str),
                                ))
                            })?,
                    }
                };

                // Transition session state to Initializing
                self.session.mark_initializing();
                let capabilities = self.capabilities_for_protocol(Some(&protocol_version));
                self.session.insert(params.capabilities.clone());
                self.session
                    .insert(crate::NegotiatedExtensions::from_capabilities(
                        &params.capabilities,
                        &capabilities,
                    ));

                Ok(McpResponse::Initialize(InitializeResult {
                    protocol_version,
                    capabilities,
                    server_info: self.implementation(),
                    instructions: if let Some(config) = &self.inner.auto_instructions {
                        Some(self.inner.generate_instructions(config))
                    } else {
                        self.inner.instructions.clone()
                    },
                    meta: None,
                }))
            }

            McpRequest::Discover(_) => {
                // SEP-2575 server/discover -- stateless capability advertisement.
                // Unlike initialize, this does NOT transition session state and
                // does not require a session at all. Returns the same capability
                // surface plus the full set of protocol versions we can speak,
                // so clients can pick one and signal it via MCP-Protocol-Version
                // on subsequent requests.
                tracing::debug!("Stateless server/discover request");
                let server_info = self.implementation();
                let supported_versions = extensions.get::<crate::ProtocolSupport>().map_or_else(
                    || {
                        crate::protocol::SUPPORTED_PROTOCOL_VERSIONS
                            .iter()
                            .map(|version| (*version).to_string())
                            .collect()
                    },
                    |support| support.versions().to_vec(),
                );
                // server/discover is itself the entry point for the final
                // stateless lifecycle, so its advertised surface must be safe
                // even when this router is invoked directly without transport
                // metadata.
                let capabilities = self
                    .capabilities_for_protocol(Some(crate::protocol::PROTOCOL_VERSION_2026_07_28));
                Ok(McpResponse::Discover(DiscoverResult {
                    supported_versions,
                    capabilities,
                    ttl_ms: None,
                    cache_scope: None,
                    instructions: if let Some(config) = &self.inner.auto_instructions {
                        Some(self.inner.generate_instructions(config))
                    } else {
                        self.inner.instructions.clone()
                    },
                    meta: Some(crate::protocol::ResultMeta {
                        server_info: Some(server_info),
                    }),
                }))
            }

            McpRequest::ListTools(params) => {
                let final_protocol = is_final_protocol_request(&extensions);
                let final_tasks_negotiated = final_protocol
                    && self.final_tasks_enabled()
                    && client_declares_tasks(&extensions);
                let filter = self.inner.tool_filter.as_ref();
                let disabled = self.inner.disabled_tools.read().unwrap().clone();
                let is_visible = |t: &Tool| {
                    !disabled.contains(&t.name)
                        && !(final_protocol
                            && matches!(t.task_support, TaskSupportMode::Required)
                            && !final_tasks_negotiated)
                        && filter
                            .map(|f| f.is_visible(&self.session, t))
                            .unwrap_or(true)
                };
                let definition = |t: &Tool| {
                    let mut definition = t.definition();
                    if final_protocol {
                        definition.execution = None;
                    }
                    definition
                };

                // Collect static tools
                let mut tools: Vec<ToolDefinition> = self
                    .inner
                    .tools
                    .values()
                    .filter(|t| is_visible(t))
                    .map(|t| definition(t))
                    .collect();

                // Merge dynamic tools (static tools win on name collision)
                #[cfg(feature = "dynamic-tools")]
                if let Some(ref dynamic) = self.inner.dynamic_tools {
                    let static_names: HashSet<String> =
                        tools.iter().map(|t| t.name.clone()).collect();
                    for t in dynamic.list() {
                        if !static_names.contains(&t.name) && is_visible(&t) {
                            tools.push(definition(&t));
                        }
                    }
                }

                tools.sort_by(|a, b| a.name.cmp(&b.name));

                let (tools, next_cursor) =
                    paginate(tools, params.cursor.as_deref(), self.inner.page_size)?;

                Ok(McpResponse::ListTools(ListToolsResult {
                    tools,
                    next_cursor,
                    ttl_ms: self.inner.list_ttl_ms,
                    cache_scope: self.effective_cache_scope(self.inner.list_ttl_ms),
                    meta: None,
                }))
            }

            McpRequest::CallTool(params) => {
                // Disabled tools are reported as if they don't exist.
                if self
                    .inner
                    .disabled_tools
                    .read()
                    .unwrap()
                    .contains(&params.name)
                {
                    tracing::info!(
                        target: "mcp::tools",
                        tool = %params.name,
                        status = "disabled",
                        "tool call completed"
                    );
                    return Err(Error::JsonRpc(JsonRpcError::method_not_found(&params.name)));
                }

                // Look up static tools first, then dynamic
                let tool = self.inner.tools.get(&params.name).cloned();
                #[cfg(feature = "dynamic-tools")]
                let tool = tool.or_else(|| {
                    self.inner
                        .dynamic_tools
                        .as_ref()
                        .and_then(|d| d.get(&params.name))
                });

                let tool = match tool {
                    Some(t) => t,
                    None => {
                        tracing::info!(
                            target: "mcp::tools",
                            tool = %params.name,
                            status = "not_found",
                            "tool call completed"
                        );
                        return Err(Error::JsonRpc(JsonRpcError::method_not_found(&params.name)));
                    }
                };

                // Check tool filter if configured
                if let Some(filter) = &self.inner.tool_filter
                    && !filter.is_visible(&self.session, &tool)
                {
                    tracing::info!(
                        target: "mcp::tools",
                        tool = %params.name,
                        status = "denied",
                        "tool call completed"
                    );
                    return Err(filter.denial_error(&params.name));
                }

                // Task creation is client-directed on the legacy protocol and
                // server-directed on the final protocol. `Some(None)` means
                // create a task using the server-selected TTL.
                let final_protocol = is_final_protocol_request(&extensions);
                let task_ttl = if final_protocol {
                    if params.task.is_some() {
                        return Err(Error::JsonRpc(JsonRpcError::invalid_params(
                            "The final Tasks extension does not allow a 'task' request parameter",
                        )));
                    }

                    let server_enabled = self.final_tasks_enabled();
                    let tasks_negotiated = server_enabled && client_declares_tasks(&extensions);
                    match tool.task_support {
                        TaskSupportMode::Required if !server_enabled => {
                            // Match tools/list: a final-only task tool is not
                            // part of this server's surface until it opts in.
                            return Err(Error::JsonRpc(JsonRpcError::method_not_found(
                                &params.name,
                            )));
                        }
                        TaskSupportMode::Required if !tasks_negotiated => {
                            return Err(Error::JsonRpc(
                                JsonRpcError::missing_required_client_capability(
                                    tasks_client_capabilities(),
                                ),
                            ));
                        }
                        TaskSupportMode::Required | TaskSupportMode::Optional
                            if tasks_negotiated =>
                        {
                            Some(None)
                        }
                        _ => None,
                    }
                } else {
                    match (&params.task, tool.task_support) {
                        (Some(_), TaskSupportMode::Forbidden) => {
                            return Err(Error::JsonRpc(JsonRpcError::invalid_params(format!(
                                "Tool '{}' does not support async tasks",
                                params.name
                            ))));
                        }
                        (None, TaskSupportMode::Required) => {
                            return Err(Error::JsonRpc(JsonRpcError::invalid_params(format!(
                                "Tool '{}' requires async task execution (include 'task' in params)",
                                params.name
                            ))));
                        }
                        (Some(task), _) => Some(task.ttl),
                        (None, _) => None,
                    }
                };

                // Final 2026-07-28 requests declare client capabilities on
                // every request. Reject a tool before any handler work begins
                // when its declared requirement is not present.
                #[cfg(feature = "stateless")]
                if let Some(required) = tool.required_client_capabilities()
                    && let Some(meta) = extensions.get::<crate::stateless::StatelessRequestMeta>()
                    && meta.protocol_version.as_deref()
                        == Some(crate::protocol::PROTOCOL_VERSION_2026_07_28)
                    && !meta
                        .client_capabilities
                        .as_ref()
                        .is_some_and(|actual| client_capabilities_satisfy(actual, required))
                {
                    return Err(Error::JsonRpc(
                        JsonRpcError::missing_required_client_capability(required.clone()),
                    ));
                }

                if let Some(task_ttl) = task_ttl {
                    // Create the task
                    let (task_id, cancellation_token) = self
                        .inner
                        .task_store
                        .create_task(
                            &params.name,
                            params.arguments.clone(),
                            task_ttl,
                            request_principal(&extensions),
                        )
                        .await
                        .map_err(task_store_error)?;

                    tracing::info!(task_id = %task_id, tool = %params.name, "Created async task");

                    // Create a context for the async task execution
                    let progress_token = params.meta.and_then(|m| m.progress_token);
                    let ctx = self.create_context_with_extensions(
                        request_id,
                        progress_token,
                        &extensions,
                    );

                    // Spawn the task execution in the background
                    let task_store = self.inner.task_store.clone();
                    let tool = tool.clone();
                    let arguments = params.arguments;
                    let task_id_clone = task_id.clone();

                    let tool_name = params.name.clone();
                    let notifier = self.clone();
                    tokio::spawn(async move {
                        // Check for cancellation before starting
                        if cancellation_token.is_cancelled() {
                            tracing::debug!(task_id = %task_id_clone, "Task cancelled before execution");
                            notifier.notify_task_state(&task_id_clone).await;
                            return;
                        }

                        // Execute the tool
                        let start = std::time::Instant::now();
                        let result = tool.call_with_context(ctx, arguments).await;
                        let duration_ms = start.elapsed().as_secs_f64() * 1000.0;

                        if cancellation_token.is_cancelled() {
                            tracing::debug!(task_id = %task_id_clone, "Task cancelled during execution");
                            notifier.notify_task_state(&task_id_clone).await;
                        } else {
                            // A tool result carrying `isError: true` completes
                            // the task: the tool ran and produced a domain
                            // error. SEP-2663 reserves `failed` for execution
                            // failures, which surface as a JSON-RPC error.
                            let status = if result.is_error { "error" } else { "success" };
                            let error_msg = result
                                .is_error
                                .then(|| result.first_text().unwrap_or("Tool execution failed"))
                                .map(str::to_string);
                            if let Err(e) = task_store.complete_task(&task_id_clone, result).await {
                                tracing::warn!(task_id = %task_id_clone, error = %e, "failed to record task completion");
                            }
                            tracing::info!(
                                target: "mcp::tools",
                                tool = %tool_name,
                                task_id = %task_id_clone,
                                duration_ms,
                                status,
                                error = error_msg.as_deref().unwrap_or_default(),
                                "tool call completed"
                            );
                            notifier.notify_task_state(&task_id_clone).await;
                        }
                    });

                    let task = self
                        .inner
                        .task_store
                        .get_task(&task_id)
                        .await
                        .map_err(task_store_error)?
                        .ok_or_else(|| {
                            Error::JsonRpc(JsonRpcError::internal_error(
                                "Failed to retrieve created task",
                            ))
                        })?;

                    // The final wire is flat with `resultType: "task"`; the
                    // legacy shape nests a `task` compatibility mirror. Pick
                    // by protocol version rather than emitting a hybrid.
                    if is_final_protocol_request(&extensions) {
                        let mut metadata = crate::tasks::TaskMetadata::new(
                            task.task_id.clone(),
                            task.created_at.clone(),
                            task.last_updated_at.clone(),
                            task.ttl,
                        );
                        metadata.status_message = task.status_message.clone();
                        metadata.poll_interval_ms = task.poll_interval;
                        return Ok(McpResponse::FinalCreateTask(
                            crate::tasks::CreateTaskResult::new(crate::tasks::Task::new(
                                metadata,
                                task.status,
                            )),
                        ));
                    }
                    Ok(McpResponse::CreateTask(CreateTaskResult::new(task)))
                } else {
                    // Extract progress token from request metadata
                    let progress_token = params.meta.and_then(|m| m.progress_token);
                    let ctx = self.create_context_with_extensions(
                        request_id,
                        progress_token,
                        &extensions,
                    );
                    #[cfg(feature = "stateless")]
                    let ctx = {
                        let mut ctx = ctx;
                        ctx.extensions_mut().insert(crate::mrtr::MrtrRequest::new(
                            params.input_responses,
                            params.request_state,
                        ));
                        ctx
                    };

                    let start = std::time::Instant::now();
                    let outcome = tool
                        .call_outcome_with_context(ctx, params.arguments)
                        .await?;
                    let duration_ms = start.elapsed().as_secs_f64() * 1000.0;

                    match outcome {
                        RequestOutcome::Complete(result) => {
                            let status = if result.is_error { "error" } else { "success" };
                            tracing::info!(
                                target: "mcp::tools",
                                tool = %params.name,
                                duration_ms,
                                status,
                                "tool call completed"
                            );
                            Ok(McpResponse::CallTool(result))
                        }
                        RequestOutcome::InputRequired(result) => {
                            #[cfg(feature = "stateless")]
                            {
                                validate_input_required_result(&extensions, &result)?;
                                tracing::info!(
                                    target: "mcp::tools",
                                    tool = %params.name,
                                    duration_ms,
                                    status = "input_required",
                                    "tool call requires client input"
                                );
                                Ok(McpResponse::InputRequired(result))
                            }
                            #[cfg(not(feature = "stateless"))]
                            {
                                let _ = result;
                                Err(Error::invalid_params(
                                    "InputRequiredResult support was not compiled",
                                ))
                            }
                        }
                    }
                }
            }

            McpRequest::ListResources(params) => {
                let disabled = self.inner.disabled_resources.read().unwrap().clone();
                let is_visible = |r: &Resource| -> bool {
                    !disabled.contains(&r.uri)
                        && self
                            .inner
                            .resource_filter
                            .as_ref()
                            .map(|f| f.is_visible(&self.session, r))
                            .unwrap_or(true)
                };

                let mut resources: Vec<ResourceDefinition> = self
                    .inner
                    .resources
                    .values()
                    .filter(|r| is_visible(r))
                    .map(|r| r.definition())
                    .collect();

                // Merge dynamic resources (static resources win on URI collision)
                #[cfg(feature = "dynamic-tools")]
                if let Some(ref dynamic) = self.inner.dynamic_resources {
                    let static_uris: HashSet<String> =
                        resources.iter().map(|r| r.uri.clone()).collect();
                    for r in dynamic.list() {
                        if !static_uris.contains(&r.uri) && is_visible(&r) {
                            resources.push(r.definition());
                        }
                    }
                }

                resources.sort_by(|a, b| a.uri.cmp(&b.uri));

                let (resources, next_cursor) =
                    paginate(resources, params.cursor.as_deref(), self.inner.page_size)?;

                Ok(McpResponse::ListResources(ListResourcesResult {
                    resources,
                    next_cursor,
                    ttl_ms: self.inner.list_ttl_ms,
                    cache_scope: self.effective_cache_scope(self.inner.list_ttl_ms),
                    meta: None,
                }))
            }

            McpRequest::ListResourceTemplates(params) => {
                let mut resource_templates: Vec<ResourceTemplateDefinition> = self
                    .inner
                    .resource_templates
                    .iter()
                    .map(|t| t.definition())
                    .collect();

                // Merge dynamic resource templates (static win on collision)
                #[cfg(feature = "dynamic-tools")]
                if let Some(ref dynamic) = self.inner.dynamic_resource_templates {
                    let static_patterns: HashSet<String> = resource_templates
                        .iter()
                        .map(|t| t.uri_template.clone())
                        .collect();
                    for t in dynamic.list() {
                        if !static_patterns.contains(&t.uri_template) {
                            resource_templates.push(t.definition());
                        }
                    }
                }

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
                        ttl_ms: self.inner.list_ttl_ms,
                        cache_scope: self.effective_cache_scope(self.inner.list_ttl_ms),
                        meta: None,
                    },
                ))
            }

            McpRequest::ReadResource(params) => {
                // Disabled resources are reported as if they don't exist.
                if self
                    .inner
                    .disabled_resources
                    .read()
                    .unwrap()
                    .contains(&params.uri)
                {
                    return Err(Error::JsonRpc(JsonRpcError::resource_not_found(
                        &params.uri,
                    )));
                }

                // First, try to find a static resource
                if let Some(resource) = self.inner.resources.get(&params.uri) {
                    // Check resource filter if configured
                    if let Some(filter) = &self.inner.resource_filter
                        && !filter.is_visible(&self.session, resource)
                    {
                        return Err(filter.denial_error(&params.uri));
                    }

                    tracing::debug!(uri = %params.uri, "Reading static resource");
                    let ctx = self.create_context_with_extensions(request_id, None, &extensions);
                    #[cfg(feature = "stateless")]
                    let ctx = {
                        let mut ctx = ctx;
                        ctx.extensions_mut().insert(crate::mrtr::MrtrRequest::new(
                            params.input_responses.clone(),
                            params.request_state.clone(),
                        ));
                        ctx
                    };
                    return match resource.read_outcome_with_context(ctx).await? {
                        RequestOutcome::Complete(result) => Ok(McpResponse::ReadResource(
                            self.apply_read_cache_hints(result),
                        )),
                        RequestOutcome::InputRequired(result) => {
                            #[cfg(feature = "stateless")]
                            {
                                validate_input_required_result(&extensions, &result)?;
                                Ok(McpResponse::InputRequired(result))
                            }
                            #[cfg(not(feature = "stateless"))]
                            {
                                let _ = result;
                                Err(Error::invalid_params(
                                    "InputRequiredResult support was not compiled",
                                ))
                            }
                        }
                    };
                }

                // Try dynamic resources
                #[cfg(feature = "dynamic-tools")]
                #[allow(clippy::collapsible_if)]
                if let Some(ref dynamic) = self.inner.dynamic_resources {
                    if let Some(resource) = dynamic.get(&params.uri) {
                        if let Some(filter) = &self.inner.resource_filter
                            && !filter.is_visible(&self.session, &resource)
                        {
                            return Err(filter.denial_error(&params.uri));
                        }
                        tracing::debug!(uri = %params.uri, "Reading dynamic resource");
                        let ctx =
                            self.create_context_with_extensions(request_id, None, &extensions);
                        #[cfg(feature = "stateless")]
                        let ctx = {
                            let mut ctx = ctx;
                            ctx.extensions_mut().insert(crate::mrtr::MrtrRequest::new(
                                params.input_responses.clone(),
                                params.request_state.clone(),
                            ));
                            ctx
                        };
                        return match resource.read_outcome_with_context(ctx).await? {
                            RequestOutcome::Complete(result) => Ok(McpResponse::ReadResource(
                                self.apply_read_cache_hints(result),
                            )),
                            RequestOutcome::InputRequired(result) => {
                                #[cfg(feature = "stateless")]
                                {
                                    validate_input_required_result(&extensions, &result)?;
                                    Ok(McpResponse::InputRequired(result))
                                }
                                #[cfg(not(feature = "stateless"))]
                                {
                                    let _ = result;
                                    Err(Error::invalid_params(
                                        "InputRequiredResult support was not compiled",
                                    ))
                                }
                            }
                        };
                    }
                }

                // Try static templates
                for template in &self.inner.resource_templates {
                    if let Some(variables) = template.match_uri(&params.uri) {
                        tracing::debug!(
                            uri = %params.uri,
                            template = %template.uri_template,
                            "Reading resource via template"
                        );
                        let ctx =
                            self.create_context_with_extensions(request_id, None, &extensions);
                        #[cfg(feature = "stateless")]
                        let ctx = {
                            let mut ctx = ctx;
                            ctx.extensions_mut().insert(crate::mrtr::MrtrRequest::new(
                                params.input_responses.clone(),
                                params.request_state.clone(),
                            ));
                            ctx
                        };
                        return match template
                            .read_outcome_with_context(ctx, &params.uri, variables)
                            .await?
                        {
                            RequestOutcome::Complete(result) => Ok(McpResponse::ReadResource(
                                self.apply_read_cache_hints(result),
                            )),
                            RequestOutcome::InputRequired(result) => {
                                #[cfg(feature = "stateless")]
                                {
                                    validate_input_required_result(&extensions, &result)?;
                                    Ok(McpResponse::InputRequired(result))
                                }
                                #[cfg(not(feature = "stateless"))]
                                {
                                    let _ = result;
                                    Err(Error::invalid_params(
                                        "InputRequiredResult support was not compiled",
                                    ))
                                }
                            }
                        };
                    }
                }

                // Try dynamic templates
                #[cfg(feature = "dynamic-tools")]
                #[allow(clippy::collapsible_if)]
                if let Some(ref dynamic) = self.inner.dynamic_resource_templates {
                    if let Some((template, variables)) = dynamic.match_uri(&params.uri) {
                        tracing::debug!(
                            uri = %params.uri,
                            template = %template.uri_template,
                            "Reading resource via dynamic template"
                        );
                        let ctx =
                            self.create_context_with_extensions(request_id, None, &extensions);
                        #[cfg(feature = "stateless")]
                        let ctx = {
                            let mut ctx = ctx;
                            ctx.extensions_mut().insert(crate::mrtr::MrtrRequest::new(
                                params.input_responses.clone(),
                                params.request_state.clone(),
                            ));
                            ctx
                        };
                        return match template
                            .read_outcome_with_context(ctx, &params.uri, variables)
                            .await?
                        {
                            RequestOutcome::Complete(result) => Ok(McpResponse::ReadResource(
                                self.apply_read_cache_hints(result),
                            )),
                            RequestOutcome::InputRequired(result) => {
                                #[cfg(feature = "stateless")]
                                {
                                    validate_input_required_result(&extensions, &result)?;
                                    Ok(McpResponse::InputRequired(result))
                                }
                                #[cfg(not(feature = "stateless"))]
                                {
                                    let _ = result;
                                    Err(Error::invalid_params(
                                        "InputRequiredResult support was not compiled",
                                    ))
                                }
                            }
                        };
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
                let disabled = self.inner.disabled_prompts.read().unwrap().clone();
                let is_visible = |p: &Prompt| -> bool {
                    !disabled.contains(&p.name)
                        && self
                            .inner
                            .prompt_filter
                            .as_ref()
                            .map(|f| f.is_visible(&self.session, p))
                            .unwrap_or(true)
                };

                let mut prompts: Vec<PromptDefinition> = self
                    .inner
                    .prompts
                    .values()
                    .filter(|p| is_visible(p))
                    .map(|p| p.definition())
                    .collect();

                // Merge dynamic prompts (static prompts win on name collision)
                #[cfg(feature = "dynamic-tools")]
                if let Some(ref dynamic) = self.inner.dynamic_prompts {
                    let static_names: HashSet<String> =
                        prompts.iter().map(|p| p.name.clone()).collect();
                    for p in dynamic.list() {
                        if !static_names.contains(&p.name) && is_visible(&p) {
                            prompts.push(p.definition());
                        }
                    }
                }

                prompts.sort_by(|a, b| a.name.cmp(&b.name));

                let (prompts, next_cursor) =
                    paginate(prompts, params.cursor.as_deref(), self.inner.page_size)?;

                Ok(McpResponse::ListPrompts(ListPromptsResult {
                    prompts,
                    next_cursor,
                    ttl_ms: self.inner.list_ttl_ms,
                    cache_scope: self.effective_cache_scope(self.inner.list_ttl_ms),
                    meta: None,
                }))
            }

            McpRequest::GetPrompt(params) => {
                // Disabled prompts are reported as if they don't exist.
                if self
                    .inner
                    .disabled_prompts
                    .read()
                    .unwrap()
                    .contains(&params.name)
                {
                    return Err(Error::JsonRpc(JsonRpcError::method_not_found(&format!(
                        "Prompt not found: {}",
                        params.name
                    ))));
                }

                // Look up static prompts first, then dynamic
                let prompt = self.inner.prompts.get(&params.name).cloned();
                #[cfg(feature = "dynamic-tools")]
                let prompt = prompt.or_else(|| {
                    self.inner
                        .dynamic_prompts
                        .as_ref()
                        .and_then(|d| d.get(&params.name))
                });
                let prompt = prompt.ok_or_else(|| {
                    Error::JsonRpc(JsonRpcError::method_not_found(&format!(
                        "Prompt not found: {}",
                        params.name
                    )))
                })?;

                // Check prompt filter if configured
                if let Some(filter) = &self.inner.prompt_filter
                    && !filter.is_visible(&self.session, &prompt)
                {
                    return Err(filter.denial_error(&params.name));
                }

                tracing::debug!(name = %params.name, "Getting prompt");
                let ctx = self.create_context_with_extensions(request_id, None, &extensions);
                #[cfg(feature = "stateless")]
                let ctx = {
                    let mut ctx = ctx;
                    ctx.extensions_mut().insert(crate::mrtr::MrtrRequest::new(
                        params.input_responses,
                        params.request_state,
                    ));
                    ctx
                };
                let outcome = prompt
                    .get_outcome_with_context(ctx, params.arguments)
                    .await?;

                match outcome {
                    RequestOutcome::Complete(result) => Ok(McpResponse::GetPrompt(result)),
                    RequestOutcome::InputRequired(result) => {
                        #[cfg(feature = "stateless")]
                        {
                            validate_input_required_result(&extensions, &result)?;
                            Ok(McpResponse::InputRequired(result))
                        }
                        #[cfg(not(feature = "stateless"))]
                        {
                            let _ = result;
                            Err(Error::invalid_params(
                                "InputRequiredResult support was not compiled",
                            ))
                        }
                    }
                }
            }

            McpRequest::Ping => Ok(McpResponse::Pong(EmptyResult {})),

            McpRequest::GetTaskInfo(params) => {
                if is_final_protocol_request(&extensions) {
                    self.require_negotiated_tasks(&extensions, "tasks/get")?;
                    self.authorize_task(&params.task_id, &extensions).await?;
                    return self.final_get_task(&params.task_id).await;
                }
                self.authorize_task(&params.task_id, &extensions).await?;

                // SEP-2663 DetailedTask: `tasks/get` carries the
                // status-discriminated payload inline. `completed` includes
                // the result the synchronous request would have returned;
                // `failed` includes the JSON-RPC error. This replaced the
                // removed blocking `tasks/result` method as the way clients
                // retrieve a task's outcome.
                let (mut task, result, error) = self
                    .inner
                    .task_store
                    .get_task_result(&params.task_id)
                    .await
                    .map_err(task_store_error)?
                    .ok_or_else(|| {
                        Error::JsonRpc(JsonRpcError::invalid_params(format!(
                            "Task not found: {}",
                            params.task_id
                        )))
                    })?;

                match task.status {
                    TaskStatus::Completed => task.result = result,
                    TaskStatus::Failed => {
                        // The store preserves the structured error, so the
                        // original code and data survive to the client instead
                        // of being flattened into an internal-error message.
                        task.error = Some(
                            error.unwrap_or_else(|| JsonRpcError::internal_error("Task failed")),
                        );
                    }
                    _ => {}
                }

                Ok(McpResponse::GetTaskInfo(task))
            }

            McpRequest::UpdateTask(params) => {
                if is_final_protocol_request(&extensions) {
                    self.require_negotiated_tasks(&extensions, "tasks/update")?;
                    self.authorize_task(&params.task_id, &extensions).await?;
                    // Partial responses are the normal case: the store
                    // consumes what matches an outstanding request and ignores
                    // unknown, already-answered, and superseded keys.
                    self.inner
                        .task_store
                        .apply_input_responses(
                            &params.task_id,
                            decode_input_responses(&params.input_responses),
                        )
                        .await
                        .map_err(task_store_error)?
                        .ok_or_else(|| Error::JsonRpc(unknown_task_error(&params.task_id)))?;
                    // Answering the last outstanding request resumes the task,
                    // so the status a subscriber sees changes here even though
                    // the ack itself is empty.
                    self.notify_task_state(&params.task_id).await;
                    return Ok(McpResponse::FinalTaskAck(
                        crate::tasks::TaskAcknowledgement::new(),
                    ));
                }

                self.authorize_task(&params.task_id, &extensions).await?;

                // SEP-2663 `tasks/update`: validate the task exists and
                // acknowledge with an empty result. tower-mcp does not yet
                // model server-initiated `inputRequests` for tasks (that's a
                // future MRTR-flavored feature), so we currently treat any
                // submitted `inputResponses` as ignorable per spec ("A server
                // SHOULD ignore any inputResponses mapped to a key that is
                // not currently outstanding").
                let _ = self
                    .inner
                    .task_store
                    .get_task(&params.task_id)
                    .await
                    .map_err(task_store_error)?
                    .ok_or_else(|| {
                        Error::JsonRpc(JsonRpcError::invalid_params(format!(
                            "Task not found: {}",
                            params.task_id
                        )))
                    })?;
                Ok(McpResponse::UpdateTask(EmptyResult {}))
            }

            McpRequest::CancelTask(params) => {
                if is_final_protocol_request(&extensions) {
                    self.require_negotiated_tasks(&extensions, "tasks/cancel")?;
                    self.authorize_task(&params.task_id, &extensions).await?;
                    // The final ack does not require a terminal transition:
                    // cancelling an already-terminal task is acknowledged, and
                    // the observable status is polled via `tasks/get`.
                    self.inner
                        .task_store
                        .cancel_task(&params.task_id, params.reason.as_deref())
                        .await
                        .map_err(task_store_error)?
                        .ok_or_else(|| Error::JsonRpc(unknown_task_error(&params.task_id)))?;
                    self.notify_task_state(&params.task_id).await;
                    return Ok(McpResponse::FinalTaskAck(
                        crate::tasks::TaskAcknowledgement::new(),
                    ));
                }

                self.authorize_task(&params.task_id, &extensions).await?;

                // First check if the task exists and is not already terminal
                let current = self
                    .inner
                    .task_store
                    .get_task(&params.task_id)
                    .await
                    .map_err(task_store_error)?
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

                self.inner
                    .task_store
                    .cancel_task(&params.task_id, params.reason.as_deref())
                    .await
                    .map_err(task_store_error)?
                    .ok_or_else(|| {
                        Error::JsonRpc(JsonRpcError::invalid_params(format!(
                            "Task not found: {}",
                            params.task_id
                        )))
                    })?;

                // SEP-2663 (final): the cancel acknowledgment MUST be an empty
                // result. The observable status is polled via `tasks/get` and
                // may remain non-terminal after this ack.
                Ok(McpResponse::CancelTask(EmptyResult {}))
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
            _ => Err(Error::JsonRpc(JsonRpcError::method_not_found(
                "unknown method",
            ))),
        }
    }

    /// Handle an MCP notification (no response expected)
    pub fn handle_notification(&self, notification: McpNotification) {
        match notification {
            McpNotification::Initialized => {
                let phase_before = self.session.phase();
                if self.session.mark_initialized() {
                    if phase_before == crate::session::SessionPhase::Uninitialized {
                        tracing::info!(
                            "Session initialized from uninitialized state (race resolved)"
                        );
                    } else {
                        tracing::info!("Session initialized, entering operation phase");
                    }
                } else {
                    tracing::warn!(
                        phase = ?self.session.phase(),
                        "Received initialized notification in unexpected state"
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
                // Client-to-server progress notifications are unusual but
                // valid through 2025-11-25. The final 2026-07-28 schema
                // removes ProgressNotification from ClientNotification
                // entirely -- clients no longer send this. Notifications are
                // fire-and-forget with no response to reject with, so an
                // off-spec one arriving here is simply logged and ignored
                // rather than rejected, regardless of negotiated version.
            }
            McpNotification::RootsListChanged => {
                tracing::info!("Client roots list changed");
                // Server should re-request roots if needed
                // This is handled by the application layer
            }
            McpNotification::Unknown { method, .. } => {
                tracing::debug!(method = %method, "Unknown notification received");
            }
            _ => {
                tracing::debug!("Unrecognized notification variant received");
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

/// A map of tool names to their annotations, for use by middleware.
///
/// This is automatically inserted into [`RouterRequest::extensions`] for
/// `tools/call` requests, allowing middleware to inspect tool safety hints
/// (e.g., `read_only_hint`, `destructive_hint`) without needing direct
/// access to the router's tool registry.
///
/// # Example
///
/// ```rust,ignore
/// use tower_mcp::router::ToolAnnotationsMap;
/// use tower_mcp::protocol::McpRequest;
///
/// // In a middleware Service::call():
/// fn call(&mut self, req: RouterRequest) -> Self::Future {
///     if let McpRequest::CallTool(params) = &req.inner {
///         if let Some(map) = req.extensions.get::<ToolAnnotationsMap>() {
///             let annotations = map.get(&params.name);
///             // Check annotations.read_only_hint, destructive_hint, etc.
///         }
///     }
///     self.inner.call(req)
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ToolAnnotationsMap {
    map: Arc<HashMap<String, ToolAnnotations>>,
}

impl ToolAnnotationsMap {
    /// Look up annotations for a tool by name.
    ///
    /// Returns `None` if the tool has no annotations or doesn't exist.
    pub fn get(&self, tool_name: &str) -> Option<&ToolAnnotations> {
        self.map.get(tool_name)
    }

    /// Check if a tool is read-only (does not modify state).
    ///
    /// Returns `false` if the tool has no annotations or doesn't exist
    /// (the MCP spec default for `readOnlyHint` is `false`).
    pub fn is_read_only(&self, tool_name: &str) -> bool {
        self.map.get(tool_name).is_some_and(|a| a.read_only_hint)
    }

    /// Check if a tool may have destructive effects.
    ///
    /// Returns `true` if the tool has no annotations or doesn't exist
    /// (the MCP spec default for `destructiveHint` is `true`).
    pub fn is_destructive(&self, tool_name: &str) -> bool {
        self.map.get(tool_name).is_none_or(|a| a.destructive_hint)
    }

    /// Check if a tool is idempotent.
    ///
    /// Returns `false` if the tool has no annotations or doesn't exist
    /// (the MCP spec default for `idempotentHint` is `false`).
    pub fn is_idempotent(&self, tool_name: &str) -> bool {
        self.map.get(tool_name).is_some_and(|a| a.idempotent_hint)
    }
}

/// Request type for the tower Service implementation.
///
/// # Preserving extensions in middleware
///
/// When rewriting a request in middleware, use [`with_inner`](Self::with_inner)
/// or [`clone_with_inner`](Self::clone_with_inner) instead of constructing a
/// new `RouterRequest` directly. Constructing with `Extensions::new()` will
/// silently drop extensions set by earlier middleware layers (token claims,
/// RBAC context, etc.).
///
/// ```rust,ignore
/// // WRONG: drops extensions from earlier middleware
/// let rewritten = RouterRequest {
///     id: req.id.clone(),
///     inner: new_inner,
///     extensions: Extensions::new(),
/// };
///
/// // RIGHT: preserves extensions
/// let rewritten = req.with_inner(new_inner);
/// ```
#[derive(Debug, Clone)]
pub struct RouterRequest {
    /// The JSON-RPC request ID.
    pub id: RequestId,
    /// The parsed MCP request.
    pub inner: McpRequest,
    /// Type-map for passing data (e.g., `TokenClaims`) through middleware.
    pub extensions: Extensions,
}

impl RouterRequest {
    /// Create a new `RouterRequest` with empty extensions.
    pub fn new(id: RequestId, inner: McpRequest) -> Self {
        Self {
            id,
            inner,
            extensions: Extensions::new(),
        }
    }

    /// Replace the inner MCP request, preserving the id and extensions.
    ///
    /// This is the recommended way to rewrite requests in middleware,
    /// as it ensures extensions set by earlier middleware layers
    /// (e.g., token claims, RBAC context) are not lost.
    pub fn with_inner(self, inner: McpRequest) -> Self {
        Self {
            id: self.id,
            inner,
            extensions: self.extensions,
        }
    }

    /// Replace both the id and inner MCP request, preserving extensions.
    ///
    /// Useful when middleware needs to assign a new request id
    /// (e.g., for fan-out or request duplication) while keeping
    /// the extensions from the original request.
    pub fn with_id_and_inner(self, id: RequestId, inner: McpRequest) -> Self {
        Self {
            id,
            inner,
            extensions: self.extensions,
        }
    }

    /// Create a copy of this request with a different inner request,
    /// cloning the id and extensions from the original.
    ///
    /// Unlike [`with_inner`](Self::with_inner), this borrows `self`,
    /// which is useful when the original request is still needed
    /// (e.g., for traffic mirroring where you send the request to
    /// two backends).
    pub fn clone_with_inner(&self, inner: McpRequest) -> Self {
        Self {
            id: self.id.clone(),
            inner,
            extensions: self.extensions.clone(),
        }
    }
}

/// Response type for the tower Service implementation
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RouterResponse {
    /// The JSON-RPC request ID this response corresponds to.
    pub id: RequestId,
    /// The MCP response or JSON-RPC error.
    pub inner: std::result::Result<McpResponse, JsonRpcError>,
}

impl RouterResponse {
    /// Returns `true` if the response contains a JSON-RPC error.
    ///
    /// Since tower-mcp services use `Error = Infallible` (errors are carried
    /// inside the response, not in the `Result`), this method is useful for
    /// middleware that needs to inspect whether a request failed -- for example,
    /// retry or circuit breaker middleware.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Response-based retry predicate for tower-resilience or similar
    /// fn is_retriable(response: &RouterResponse) -> bool {
    ///     response.is_error()
    /// }
    /// ```
    pub fn is_error(&self) -> bool {
        self.inner.is_err()
    }

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
            let result = router.handle(req.id, req.inner, req.extensions).await;
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

    #[cfg(feature = "stateless")]
    fn final_extensions(client_capabilities: ClientCapabilities) -> Extensions {
        let mut extensions = Extensions::new();
        extensions.insert(crate::stateless::StatelessRequestMeta {
            protocol_version: Some(PROTOCOL_VERSION_2026_07_28.to_string()),
            client_capabilities: Some(client_capabilities),
            ..Default::default()
        });
        extensions
    }

    #[cfg(feature = "stateless")]
    fn tasks_client_extensions() -> Extensions {
        final_extensions(ClientCapabilities {
            extensions: Some(
                [(TASKS_EXTENSION_ID.to_string(), serde_json::json!({}))]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        })
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn final_tasks_require_server_opt_in_and_client_declaration() {
        let tool = || {
            ToolBuilder::new("optional_task")
                .task_support(TaskSupportMode::Optional)
                .handler(|input: AddInput| async move {
                    Ok(CallToolResult::text(format!("{}", input.a + input.b)))
                })
                .build()
        };
        let task_params = |task| CallToolParams {
            name: "optional_task".to_string(),
            arguments: serde_json::json!({"a": 1, "b": 2}),
            input_responses: None,
            request_state: None,
            meta: None,
            task,
        };

        // Registering a task-capable tool is not an opt-in: a server that
        // never called `with_tasks` advertises nothing on the final path and
        // still refuses the augmentation even to a declaring client.
        let implicit = McpRouter::new().tool(tool());
        let McpResponse::Discover(result) = implicit
            .handle(
                RequestId::Number(1),
                McpRequest::Discover(DiscoverParams::default()),
                Extensions::new(),
            )
            .await
            .unwrap()
        else {
            panic!("Expected Discover response");
        };
        assert!(
            result
                .capabilities
                .extensions
                .as_ref()
                .is_none_or(|extensions| !extensions.contains_key(TASKS_EXTENSION_ID))
        );
        let error = implicit
            .handle(
                RequestId::Number(2),
                McpRequest::CallTool(task_params(Some(TaskRequestParams { ttl: None }))),
                tasks_client_extensions(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::JsonRpc(e) if e.code == -32602));

        // Opting in advertises the extension.
        let router = McpRouter::new().tool(tool()).with_tasks();
        let McpResponse::Discover(result) = router
            .handle(
                RequestId::Number(3),
                McpRequest::Discover(DiscoverParams::default()),
                Extensions::new(),
            )
            .await
            .unwrap()
        else {
            panic!("Expected Discover response");
        };
        assert!(
            result
                .capabilities
                .extensions
                .as_ref()
                .is_some_and(|extensions| extensions.contains_key(TASKS_EXTENSION_ID)),
            "with_tasks() must advertise the extension on the final path"
        );
        assert!(
            result.capabilities.tasks.is_none(),
            "the legacy capability shape is never advertised on the final path"
        );

        // A client that did not declare the extension gets the synchronous
        // form of an optional tool.
        let response = router
            .handle(
                RequestId::Number(4),
                McpRequest::CallTool(task_params(None)),
                final_extensions(ClientCapabilities::default()),
            )
            .await
            .unwrap();
        assert!(matches!(response, McpResponse::CallTool(_)));

        // Both sides declared: the server elects a task from an ordinary
        // tools/call request.
        let response = router
            .handle(
                RequestId::Number(5),
                McpRequest::CallTool(task_params(None)),
                tasks_client_extensions(),
            )
            .await
            .unwrap();
        assert!(
            matches!(response, McpResponse::FinalCreateTask(_)),
            "a negotiated request must receive a task, got {response:?}"
        );

        // The removed legacy request flag is invalid even when the extension
        // was negotiated.
        let error = router
            .handle(
                RequestId::Number(6),
                McpRequest::CallTool(task_params(Some(TaskRequestParams { ttl: None }))),
                tasks_client_extensions(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::JsonRpc(e) if e.code == -32602));
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn final_task_methods_serve_the_negotiated_wire_shapes() {
        let router = McpRouter::new()
            .tool(
                ToolBuilder::new("optional_task")
                    .task_support(TaskSupportMode::Optional)
                    .handler(|input: AddInput| async move {
                        Ok(CallToolResult::text(format!("{}", input.a + input.b)))
                    })
                    .build(),
            )
            .with_tasks();

        let McpResponse::FinalCreateTask(created) = router
            .handle(
                RequestId::Number(1),
                McpRequest::CallTool(CallToolParams {
                    name: "optional_task".to_string(),
                    arguments: serde_json::json!({"a": 1, "b": 2}),
                    input_responses: None,
                    request_state: None,
                    meta: None,
                    task: None,
                }),
                tasks_client_extensions(),
            )
            .await
            .unwrap()
        else {
            panic!("Expected a final create-task response");
        };

        // Flat, with no legacy nested mirror.
        let wire = serde_json::to_value(&created).unwrap();
        assert_eq!(wire["resultType"], "task");
        assert!(wire.get("task").is_none(), "final results are flat: {wire}");
        assert!(wire["ttlMs"].is_number() || wire["ttlMs"].is_null());
        assert!(wire.get("ttl").is_none(), "legacy field name leaked");
        let task_id = created.task.metadata.task_id.clone();

        // tasks/get returns a status-discriminated DetailedTask.
        let McpResponse::FinalGetTask(fetched) = router
            .handle(
                RequestId::Number(2),
                McpRequest::GetTaskInfo(GetTaskInfoParams {
                    task_id: task_id.clone(),
                    meta: None,
                }),
                tasks_client_extensions(),
            )
            .await
            .unwrap()
        else {
            panic!("Expected a final get-task response");
        };
        let wire = serde_json::to_value(&fetched).unwrap();
        assert_eq!(wire["resultType"], "complete");
        assert_eq!(wire["taskId"], serde_json::json!(task_id));
        assert!(wire["status"].is_string());

        // Both ack methods produce the complete acknowledgement.
        for (id, request) in [
            (
                3,
                McpRequest::UpdateTask(UpdateTaskParams {
                    task_id: task_id.clone(),
                    input_responses: HashMap::new(),
                    meta: None,
                }),
            ),
            (
                4,
                McpRequest::CancelTask(CancelTaskParams {
                    task_id: task_id.clone(),
                    reason: None,
                    meta: None,
                }),
            ),
        ] {
            let response = router
                .handle(RequestId::Number(id), request, tasks_client_extensions())
                .await
                .unwrap();
            let McpResponse::FinalTaskAck(ack) = response else {
                panic!("Expected a final ack for request {id}");
            };
            assert_eq!(
                serde_json::to_value(&ack).unwrap(),
                serde_json::json!({"resultType": "complete"})
            );
        }

        // An unknown task is invalid params, not a method error.
        let error = router
            .handle(
                RequestId::Number(5),
                McpRequest::GetTaskInfo(GetTaskInfoParams {
                    task_id: "does-not-exist".to_string(),
                    meta: None,
                }),
                tasks_client_extensions(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::JsonRpc(e) if e.code == -32602));

        // Without the client declaration the methods remain absent.
        let error = router
            .handle(
                RequestId::Number(6),
                McpRequest::GetTaskInfo(GetTaskInfoParams {
                    task_id: task_id.clone(),
                    meta: None,
                }),
                final_extensions(ClientCapabilities::default()),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::JsonRpc(e) if e.code == -32601));
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn final_required_task_tools_follow_per_request_capabilities() {
        let router = McpRouter::new()
            .tool(
                ToolBuilder::new("required_task")
                    .task_support(TaskSupportMode::Required)
                    .handler(|input: AddInput| async move {
                        Ok(CallToolResult::text(format!("{}", input.a + input.b)))
                    })
                    .build(),
            )
            .with_tasks();
        let params = || CallToolParams {
            name: "required_task".to_string(),
            arguments: serde_json::json!({"a": 1, "b": 2}),
            input_responses: None,
            request_state: None,
            meta: None,
            task: None,
        };

        let McpResponse::ListTools(without_tasks) = router
            .handle(
                RequestId::Number(1),
                McpRequest::ListTools(ListToolsParams::default()),
                final_extensions(ClientCapabilities::default()),
            )
            .await
            .unwrap()
        else {
            panic!("expected tools/list")
        };
        assert!(without_tasks.tools.is_empty());

        let McpResponse::ListTools(with_tasks) = router
            .handle(
                RequestId::Number(2),
                McpRequest::ListTools(ListToolsParams::default()),
                tasks_client_extensions(),
            )
            .await
            .unwrap()
        else {
            panic!("expected tools/list")
        };
        assert_eq!(with_tasks.tools.len(), 1);
        assert!(with_tasks.tools[0].execution.is_none());

        let error = router
            .handle(
                RequestId::Number(3),
                McpRequest::CallTool(params()),
                final_extensions(ClientCapabilities::default()),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::JsonRpc(error) if error.code == -32021));

        let response = router
            .handle(
                RequestId::Number(4),
                McpRequest::CallTool(params()),
                tasks_client_extensions(),
            )
            .await
            .unwrap();
        assert!(matches!(response, McpResponse::FinalCreateTask(_)));
    }

    #[cfg(all(feature = "oauth", feature = "stateless"))]
    #[tokio::test]
    async fn task_operations_are_bound_to_the_creating_principal() {
        fn as_principal(subject: &str) -> Extensions {
            let mut extensions = tasks_client_extensions();
            extensions.insert(crate::oauth::token::TokenClaims {
                sub: Some(subject.to_string()),
                iss: None,
                aud: None,
                exp: None,
                scope: None,
                client_id: None,
                extra: HashMap::new(),
            });
            extensions
        }

        let router = McpRouter::new()
            .tool(
                ToolBuilder::new("optional_task")
                    .task_support(TaskSupportMode::Optional)
                    .handler(|input: AddInput| async move {
                        Ok(CallToolResult::text(format!("{}", input.a + input.b)))
                    })
                    .build(),
            )
            .with_tasks();

        let McpResponse::FinalCreateTask(created) = router
            .handle(
                RequestId::Number(1),
                McpRequest::CallTool(CallToolParams {
                    name: "optional_task".to_string(),
                    arguments: serde_json::json!({"a": 1, "b": 2}),
                    input_responses: None,
                    request_state: None,
                    meta: None,
                    task: None,
                }),
                as_principal("alice"),
            )
            .await
            .unwrap()
        else {
            panic!("Expected a final create-task response");
        };
        let task_id = created.task.metadata.task_id.clone();

        // The owner is served normally.
        assert!(
            router
                .handle(
                    RequestId::Number(2),
                    McpRequest::GetTaskInfo(GetTaskInfoParams {
                        task_id: task_id.clone(),
                        meta: None,
                    }),
                    as_principal("alice"),
                )
                .await
                .is_ok()
        );

        // Knowing the ID is not authority. Every operation is refused for a
        // different principal, and for one that dropped its token.
        for (id, label, context) in [
            (3, "another principal", as_principal("bob")),
            (4, "no principal", tasks_client_extensions()),
        ] {
            for (offset, request) in [
                McpRequest::GetTaskInfo(GetTaskInfoParams {
                    task_id: task_id.clone(),
                    meta: None,
                }),
                McpRequest::UpdateTask(UpdateTaskParams {
                    task_id: task_id.clone(),
                    input_responses: HashMap::new(),
                    meta: None,
                }),
                McpRequest::CancelTask(CancelTaskParams {
                    task_id: task_id.clone(),
                    reason: None,
                    meta: None,
                }),
            ]
            .into_iter()
            .enumerate()
            {
                let error = router
                    .handle(
                        RequestId::Number(id * 10 + offset as i64),
                        request,
                        context.clone(),
                    )
                    .await
                    .unwrap_err();
                assert!(
                    matches!(error, Error::JsonRpc(ref e) if e.code == -32602),
                    "{label} was served: {error:?}"
                );
                // The refusal must be indistinguishable from an unknown task,
                // or it confirms the ID is real.
                let Error::JsonRpc(error) = error else {
                    unreachable!()
                };
                assert!(
                    error.message.contains("not found"),
                    "refusal leaked that the task exists: {}",
                    error.message
                );
            }
        }

        // The task survived every refused operation.
        assert!(
            router
                .handle(
                    RequestId::Number(9),
                    McpRequest::GetTaskInfo(GetTaskInfoParams {
                        task_id: task_id.clone(),
                        meta: None,
                    }),
                    as_principal("alice"),
                )
                .await
                .is_ok(),
            "a refused cancel must not have cancelled the task"
        );
    }

    #[cfg(all(feature = "oauth", feature = "stateless"))]
    #[tokio::test]
    async fn final_tasks_work_across_independent_routers_with_a_shared_store() {
        fn as_principal(subject: &str) -> Extensions {
            let mut extensions = tasks_client_extensions();
            extensions.insert(crate::oauth::token::TokenClaims {
                sub: Some(subject.to_string()),
                iss: None,
                aud: None,
                exp: None,
                scope: None,
                client_id: None,
                extra: HashMap::new(),
            });
            extensions
        }

        fn router_with_store(store: Arc<dyn TaskStore>) -> McpRouter {
            McpRouter::new()
                .tool(
                    ToolBuilder::new("shared_task")
                        .task_support(TaskSupportMode::Optional)
                        .handler(|_input: serde_json::Value| async move {
                            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                            Ok(CallToolResult::text("done"))
                        })
                        .build(),
                )
                .task_store(store)
                .with_tasks()
        }

        let store: Arc<dyn TaskStore> = Arc::new(MemoryTaskStore::new());
        let router_a = router_with_store(store.clone());
        let router_b = router_with_store(store);

        let McpResponse::FinalCreateTask(created) = router_a
            .handle(
                RequestId::Number(1),
                McpRequest::CallTool(CallToolParams {
                    name: "shared_task".to_string(),
                    arguments: serde_json::json!({}),
                    input_responses: None,
                    request_state: None,
                    meta: None,
                    task: None,
                }),
                as_principal("alice"),
            )
            .await
            .unwrap()
        else {
            panic!("router A did not create a final task")
        };
        let task_id = created.task.metadata.task_id;

        // A separate router instance can read the shared task for its owner.
        assert!(
            router_b
                .handle(
                    RequestId::Number(2),
                    McpRequest::GetTaskInfo(GetTaskInfoParams {
                        task_id: task_id.clone(),
                        meta: None,
                    }),
                    as_principal("alice"),
                )
                .await
                .is_ok()
        );

        // Another principal sees the same response as an unknown ID.
        let denied = router_b
            .handle(
                RequestId::Number(3),
                McpRequest::GetTaskInfo(GetTaskInfoParams {
                    task_id: task_id.clone(),
                    meta: None,
                }),
                as_principal("bob"),
            )
            .await
            .unwrap_err();
        let unknown = router_b
            .handle(
                RequestId::Number(4),
                McpRequest::GetTaskInfo(GetTaskInfoParams {
                    task_id: "unknown-task".to_string(),
                    meta: None,
                }),
                as_principal("bob"),
            )
            .await
            .unwrap_err();
        let (Error::JsonRpc(denied), Error::JsonRpc(unknown)) = (denied, unknown) else {
            panic!("expected JSON-RPC task denials")
        };
        assert_eq!(denied.code, unknown.code);
        assert_eq!(
            denied.message.replace(&task_id, "<task-id>"),
            unknown.message.replace("unknown-task", "<task-id>")
        );
        assert_eq!(denied.data, unknown.data);

        // Router B mutates the shared task, and router A immediately observes
        // the terminal state through the same backend.
        assert!(matches!(
            router_b
                .handle(
                    RequestId::Number(5),
                    McpRequest::CancelTask(CancelTaskParams {
                        task_id: task_id.clone(),
                        reason: None,
                        meta: None,
                    }),
                    as_principal("alice"),
                )
                .await
                .unwrap(),
            McpResponse::FinalTaskAck(_)
        ));
        let McpResponse::FinalGetTask(fetched) = router_a
            .handle(
                RequestId::Number(6),
                McpRequest::GetTaskInfo(GetTaskInfoParams {
                    task_id,
                    meta: None,
                }),
                as_principal("alice"),
            )
            .await
            .unwrap()
        else {
            panic!("router A did not read the shared task")
        };
        assert_eq!(fetched.task.status(), TaskStatus::Cancelled);
    }

    #[test]
    fn router_advertises_only_locally_declared_protocol_extensions() {
        let router = McpRouter::new().with_protocol_extension(
            crate::ExtensionDeclaration::new(
                "com.example/rendering",
                serde_json::json!({"formats": ["html"]}),
            )
            .unwrap(),
        );

        let stable = router.capabilities();
        let final_capabilities =
            router.capabilities_for_protocol(Some(crate::protocol::PROTOCOL_VERSION_2026_07_28));
        for capabilities in [stable, final_capabilities] {
            let extensions = capabilities.extensions.unwrap();
            assert_eq!(extensions.len(), 1);
            assert_eq!(extensions["com.example/rendering"]["formats"][0], "html");
            assert!(!extensions.contains_key("com.example/client-only"));
        }
    }

    #[tokio::test]
    async fn initialize_persists_negotiated_extensions_for_legacy_contexts() {
        let router = McpRouter::new().with_protocol_extension(
            crate::ExtensionDeclaration::new(
                "com.example/shared",
                serde_json::json!({"server": true}),
            )
            .unwrap(),
        );
        let client_capabilities = ClientCapabilities {
            extensions: Some(HashMap::from([
                (
                    "com.example/shared".to_string(),
                    serde_json::json!({"client": true}),
                ),
                ("com.example/client-only".to_string(), serde_json::json!({})),
            ])),
            ..ClientCapabilities::default()
        };

        router
            .handle(
                RequestId::Number(1),
                McpRequest::Initialize(InitializeParams {
                    protocol_version: crate::protocol::LATEST_PROTOCOL_VERSION.to_string(),
                    capabilities: client_capabilities,
                    client_info: Implementation {
                        name: "extension-test".to_string(),
                        version: "1.0.0".to_string(),
                        title: None,
                        description: None,
                        icons: None,
                        website_url: None,
                        meta: None,
                    },
                    meta: None,
                }),
                Extensions::new(),
            )
            .await
            .unwrap();

        let context = router.create_context(RequestId::Number(2), None);
        let negotiated = context.negotiated_extensions().unwrap();
        assert!(negotiated.contains("com.example/shared"));
        assert!(!negotiated.contains("com.example/client-only"));
    }

    #[cfg(feature = "stateless")]
    #[test]
    fn final_request_context_exposes_only_negotiated_extensions() {
        let router = McpRouter::new().with_protocol_extension(
            crate::ExtensionDeclaration::new(
                "com.example/shared",
                serde_json::json!({"server": true}),
            )
            .unwrap(),
        );
        let per_request = final_extensions(ClientCapabilities {
            extensions: Some(HashMap::from([
                (
                    "com.example/shared".to_string(),
                    serde_json::json!({"client": true}),
                ),
                ("com.example/client-only".to_string(), serde_json::json!({})),
            ])),
            ..ClientCapabilities::default()
        });

        let context =
            router.create_context_with_extensions(RequestId::Number(1), None, &per_request);
        let negotiated = context.negotiated_extensions().unwrap();

        assert_eq!(negotiated.len(), 1);
        assert_eq!(
            negotiated
                .get("com.example/shared")
                .unwrap()
                .client_settings()["client"],
            true
        );
        assert!(!negotiated.contains("com.example/client-only"));
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn final_protocol_withholds_incomplete_tasks_advertisement() {
        let optional = ToolBuilder::new("optional_task")
            .task_support(TaskSupportMode::Optional)
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();
        let required = ToolBuilder::new("required_task")
            .task_support(TaskSupportMode::Required)
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();
        let mut router = McpRouter::new().tool(optional).tool(required);

        // Stable clients retain the existing capability surface.
        let stable_capabilities = router.capabilities();
        assert!(stable_capabilities.tasks.is_some());
        assert!(
            stable_capabilities
                .extensions
                .as_ref()
                .is_some_and(|extensions| extensions.contains_key(TASKS_EXTENSION_ID))
        );

        // Final discovery must not claim support for the incomplete extension.
        let response = router
            .handle(
                RequestId::Number(1),
                McpRequest::Discover(DiscoverParams::default()),
                Extensions::new(),
            )
            .await
            .unwrap();
        let McpResponse::Discover(result) = response else {
            panic!("Expected Discover response");
        };
        assert!(result.capabilities.tasks.is_none());
        assert!(
            result
                .capabilities
                .extensions
                .as_ref()
                .is_none_or(|extensions| !extensions.contains_key(TASKS_EXTENSION_ID))
        );

        init_router(&mut router).await;

        // Stable discovery keeps both tools and their execution metadata.
        let response = router
            .handle(
                RequestId::Number(2),
                McpRequest::ListTools(ListToolsParams::default()),
                Extensions::new(),
            )
            .await
            .unwrap();
        let McpResponse::ListTools(result) = response else {
            panic!("Expected ListTools response");
        };
        assert_eq!(result.tools.len(), 2);
        assert!(result.tools.iter().all(|tool| tool.execution.is_some()));

        // Final discovery keeps the synchronously callable optional tool, but
        // strips Tasks metadata and hides the required-task-only tool.
        let response = router
            .handle(
                RequestId::Number(3),
                McpRequest::ListTools(ListToolsParams::default()),
                final_extensions(ClientCapabilities::default()),
            )
            .await
            .unwrap();
        let McpResponse::ListTools(result) = response else {
            panic!("Expected ListTools response");
        };
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "optional_task");
        assert!(result.tools[0].execution.is_none());
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn final_protocol_enforces_tasks_negotiation() {
        let optional = ToolBuilder::new("optional_task")
            .task_support(TaskSupportMode::Optional)
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();
        let required = ToolBuilder::new("required_task")
            .task_support(TaskSupportMode::Required)
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();
        let mut router = McpRouter::new().tool(optional).tool(required).with_tasks();
        init_router(&mut router).await;

        // The optional tool remains synchronously callable on the final path.
        let response = router
            .handle(
                RequestId::Number(1),
                McpRequest::CallTool(CallToolParams {
                    name: "optional_task".to_string(),
                    arguments: serde_json::json!({"a": 1, "b": 2}),
                    input_responses: None,
                    request_state: None,
                    meta: None,
                    task: None,
                }),
                final_extensions(ClientCapabilities::default()),
            )
            .await
            .unwrap();
        assert!(matches!(response, McpResponse::CallTool(_)));

        // The removed legacy task augmentation is invalid on the final wire.
        let error = router
            .handle(
                RequestId::Number(2),
                McpRequest::CallTool(CallToolParams {
                    name: "optional_task".to_string(),
                    arguments: serde_json::json!({"a": 1, "b": 2}),
                    input_responses: None,
                    request_state: None,
                    meta: None,
                    task: Some(TaskRequestParams { ttl: None }),
                }),
                final_extensions(ClientCapabilities::default()),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::JsonRpc(error) if error.code == -32602));

        // A required-task tool cannot run without a task, so the server names
        // the capability the client is missing rather than pretending the tool
        // does not exist.
        let error = router
            .handle(
                RequestId::Number(3),
                McpRequest::CallTool(CallToolParams {
                    name: "required_task".to_string(),
                    arguments: serde_json::json!({"a": 1, "b": 2}),
                    input_responses: None,
                    request_state: None,
                    meta: None,
                    task: None,
                }),
                final_extensions(ClientCapabilities::default()),
            )
            .await
            .unwrap_err();
        let Error::JsonRpc(error) = error else {
            panic!("expected a JSON-RPC error");
        };
        assert_eq!(error.code, -32021);
        assert_eq!(
            error.data.as_ref().unwrap()["requiredCapabilities"]["extensions"]["io.modelcontextprotocol/tasks"],
            serde_json::json!({}),
            "the error must name the extension the client needs to declare"
        );

        let task_requests = [
            McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: "task-unknown".to_string(),
                meta: None,
            }),
            McpRequest::UpdateTask(UpdateTaskParams {
                task_id: "task-unknown".to_string(),
                input_responses: HashMap::new(),
                meta: None,
            }),
            McpRequest::CancelTask(CancelTaskParams {
                task_id: "task-unknown".to_string(),
                reason: None,
                meta: None,
            }),
        ];
        for (index, request) in task_requests.into_iter().enumerate() {
            let error = router
                .handle(
                    RequestId::Number(4 + index as i64),
                    request,
                    final_extensions(ClientCapabilities::default()),
                )
                .await
                .unwrap_err();
            assert!(matches!(error, Error::JsonRpc(error) if error.code == -32601));
        }
    }

    #[cfg(feature = "stateless")]
    #[test]
    fn input_required_capability_validation_uses_capability_semantics() {
        let roots = InputRequiredResult::with_requests(
            [(
                "roots".to_string(),
                InputRequest::ListRoots(ListRootsParams::default()),
            )]
            .into_iter()
            .collect(),
        );
        let extensions = final_extensions(ClientCapabilities {
            roots: Some(RootsCapability {
                list_changed: true,
                deprecated: None,
            }),
            ..Default::default()
        });
        validate_input_required_result(&extensions, &roots).unwrap();
        assert!(client_capabilities_satisfy(
            extensions
                .get::<crate::stateless::StatelessRequestMeta>()
                .and_then(|meta| meta.client_capabilities.as_ref())
                .unwrap(),
            &ClientCapabilities {
                roots: Some(RootsCapability::default()),
                ..Default::default()
            }
        ));

        let sampling_with_tools = InputRequiredResult::with_requests(
            [(
                "sample".to_string(),
                InputRequest::CreateMessage(CreateMessageParams {
                    tools: Some(Vec::new()),
                    ..CreateMessageParams::new(vec![SamplingMessage::user("hello")], 10)
                }),
            )]
            .into_iter()
            .collect(),
        );
        let extensions = final_extensions(ClientCapabilities {
            sampling: Some(SamplingCapability::default()),
            ..Default::default()
        });
        assert!(validate_input_required_result(&extensions, &sampling_with_tools).is_err());

        let form = InputRequiredResult::with_requests(
            [(
                "form".to_string(),
                InputRequest::Elicit(ElicitRequestParams::Form(ElicitFormParams {
                    mode: Some(ElicitMode::Form),
                    message: "name".into(),
                    requested_schema: ElicitFormSchema::new(),
                    meta: None,
                })),
            )]
            .into_iter()
            .collect(),
        );
        let extensions = final_extensions(ClientCapabilities {
            elicitation: Some(ElicitationCapability::default()),
            ..Default::default()
        });
        validate_input_required_result(&extensions, &form).unwrap();
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
                    extensions: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
                meta: None,
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
                input_responses: None,
                request_state: None,
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
            _ => panic!("unexpected response variant"),
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
            _ => panic!("unexpected response variant"),
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
            _ => panic!("unexpected response variant"),
        }

        // Check third response (ping)
        match &responses[2] {
            JsonRpcResponse::Result(r) => {
                assert_eq!(r.id, RequestId::Number(3));
            }
            JsonRpcResponse::Error(_) => panic!("Expected success for ping"),
            _ => panic!("unexpected response variant"),
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
            _ => panic!("unexpected response variant"),
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
            _ => panic!("unexpected response variant"),
        }

        // Second should be an error (tool not found)
        match &responses[1] {
            JsonRpcResponse::Error(e) => {
                assert_eq!(e.id, Some(RequestId::Number(2)));
                // Error should indicate method not found
                assert!(e.error.message.contains("not found") || e.error.code == -32601);
            }
            JsonRpcResponse::Result(_) => panic!("Expected error for second request"),
            _ => panic!("unexpected response variant"),
        }

        // Third should be success
        match &responses[2] {
            JsonRpcResponse::Result(r) => {
                assert_eq!(r.id, RequestId::Number(3));
            }
            JsonRpcResponse::Error(_) => panic!("Expected success for third request"),
            _ => panic!("unexpected response variant"),
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
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
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
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            });

        let mut router = McpRouter::new().resource_template(template);

        // Initialize session
        init_router(&mut router).await;

        // Read a resource that matches the template
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ReadResource(ReadResourceParams {
                input_responses: None,
                request_state: None,
                uri: "db://users/123".to_string(),
                meta: None,
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
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
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
                input_responses: None,
                request_state: None,
                uri: "file:///README.md".to_string(),
                meta: None,
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
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            });

        let mut router = McpRouter::new().resource_template(template);

        // Initialize session
        init_router(&mut router).await;

        // Try to read a URI that doesn't match any resource or template
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ReadResource(ReadResourceParams {
                input_responses: None,
                request_state: None,
                uri: "db://posts/123".to_string(),
                meta: None,
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
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
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
                    extensions: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
                meta: None,
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
                    extensions: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
                meta: None,
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
                    extensions: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
                meta: None,
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
                input_responses: None,
                request_state: None,
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
                assert!(!result.task.task_id.is_empty());
                assert_eq!(result.task.status, TaskStatus::Working);
            }
            _ => panic!("Expected CreateTask response"),
        }
    }

    /// [`TaskStore`] wrapper that counts calls, for proving dispatch goes
    /// through an injected store.
    struct CountingTaskStore {
        inner: MemoryTaskStore,
        creates: std::sync::atomic::AtomicUsize,
        gets: std::sync::atomic::AtomicUsize,
        completes: std::sync::atomic::AtomicUsize,
    }

    impl CountingTaskStore {
        fn new() -> Self {
            Self {
                inner: MemoryTaskStore::new(),
                creates: std::sync::atomic::AtomicUsize::new(0),
                gets: std::sync::atomic::AtomicUsize::new(0),
                completes: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl TaskStore for CountingTaskStore {
        async fn create_task(
            &self,
            tool_name: &str,
            arguments: serde_json::Value,
            ttl: Option<u64>,
            owner: crate::async_task::TaskOwner,
        ) -> crate::async_task::Result<(String, crate::async_task::CancellationToken)> {
            self.creates
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner
                .create_task(tool_name, arguments, ttl, owner)
                .await
        }

        async fn get_task(&self, task_id: &str) -> crate::async_task::Result<Option<TaskObject>> {
            self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.get_task(task_id).await
        }

        async fn task_owner(
            &self,
            task_id: &str,
        ) -> crate::async_task::Result<Option<crate::async_task::TaskOwner>> {
            self.inner.task_owner(task_id).await
        }

        async fn get_task_result(
            &self,
            task_id: &str,
        ) -> crate::async_task::Result<Option<crate::async_task::TaskSnapshot>> {
            // Counted as a read: `tasks/get` dispatch fetches the snapshot so
            // it can inline the SEP-2663 DetailedTask terminal payload.
            self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.get_task_result(task_id).await
        }

        async fn wait_for_completion(
            &self,
            task_id: &str,
        ) -> crate::async_task::Result<Option<crate::async_task::TaskSnapshot>> {
            self.inner.wait_for_completion(task_id).await
        }

        async fn list_tasks(
            &self,
            status_filter: Option<TaskStatus>,
        ) -> crate::async_task::Result<Vec<TaskObject>> {
            self.inner.list_tasks(status_filter).await
        }

        async fn require_input(
            &self,
            task_id: &str,
            requests: crate::protocol::InputRequests,
            message: Option<&str>,
        ) -> crate::async_task::Result<bool> {
            self.inner.require_input(task_id, requests, message).await
        }

        async fn outstanding_input_requests(
            &self,
            task_id: &str,
        ) -> crate::async_task::Result<Option<crate::protocol::InputRequests>> {
            self.inner.outstanding_input_requests(task_id).await
        }

        async fn apply_input_responses(
            &self,
            task_id: &str,
            responses: crate::protocol::InputResponses,
        ) -> crate::async_task::Result<Option<crate::async_task::AppliedInputResponses>> {
            self.inner.apply_input_responses(task_id, responses).await
        }

        async fn set_ttl(&self, task_id: &str, ttl_ms: u64) -> crate::async_task::Result<bool> {
            self.inner.set_ttl(task_id, ttl_ms).await
        }

        async fn complete_task(
            &self,
            task_id: &str,
            result: CallToolResult,
        ) -> crate::async_task::Result<bool> {
            self.completes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.complete_task(task_id, result).await
        }

        async fn fail_task(
            &self,
            task_id: &str,
            error: JsonRpcError,
        ) -> crate::async_task::Result<bool> {
            self.inner.fail_task(task_id, error).await
        }

        async fn cancel_task(
            &self,
            task_id: &str,
            reason: Option<&str>,
        ) -> crate::async_task::Result<Option<TaskObject>> {
            self.inner.cancel_task(task_id, reason).await
        }
    }

    #[tokio::test]
    async fn test_injected_task_store_used_by_dispatch() {
        let store = Arc::new(CountingTaskStore::new());

        let add_tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .task_support(TaskSupportMode::Optional)
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();

        let mut router = McpRouter::new()
            .tool(add_tool)
            .task_store(store.clone() as Arc<dyn TaskStore>);
        init_router(&mut router).await;

        // Task-augmented tools/call must create the task in the injected store.
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                input_responses: None,
                request_state: None,
                name: "add".to_string(),
                arguments: serde_json::json!({"a": 2, "b": 3}),
                meta: None,
                task: Some(TaskRequestParams { ttl: None }),
            }),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        let task_id = match resp.inner {
            Ok(McpResponse::CreateTask(result)) => result.task.task_id,
            other => panic!("Expected CreateTask response, got {other:?}"),
        };

        assert_eq!(
            store.creates.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "create_task must go through the injected store"
        );

        // Wait for the background execution to record completion.
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        assert_eq!(
            store.completes.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "complete_task must go through the injected store"
        );

        // tasks/get must read from the injected store.
        let gets_before = store.gets.load(std::sync::atomic::Ordering::Relaxed);
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: task_id.clone(),
                meta: None,
            }),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::GetTaskInfo(info)) => {
                assert_eq!(info.task_id, task_id);
                assert_eq!(info.status, TaskStatus::Completed);
            }
            other => panic!("Expected GetTaskInfo response, got {other:?}"),
        }
        assert!(
            store.gets.load(std::sync::atomic::Ordering::Relaxed) > gets_before,
            "tasks/get must go through the injected store"
        );
    }

    #[tokio::test]
    async fn test_removed_tasks_methods_get_method_not_found() {
        // Final SEP-2663 removes tasks/list and tasks/result. They no longer
        // parse into typed requests, so the router sees Unknown and must
        // answer MethodNotFound (-32601).
        let mut router = McpRouter::new();
        init_router(&mut router).await;

        for method in ["tasks/list", "tasks/result"] {
            let req = RouterRequest {
                id: RequestId::Number(1),
                inner: McpRequest::Unknown {
                    method: method.to_string(),
                    params: None,
                },
                extensions: Extensions::new(),
            };

            let resp = router.ready().await.unwrap().call(req).await.unwrap();

            match resp.inner {
                Err(err) => {
                    assert_eq!(err.code, -32601, "{method} must be MethodNotFound");
                }
                other => panic!("Expected MethodNotFound error for {method}, got {other:?}"),
            }
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
                input_responses: None,
                request_state: None,
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

        // Poll task state via tasks/get (final SEP-2663 removed the blocking
        // tasks/result; the terminal result payload on tasks/get is the
        // phase 4 DetailedTask work, #951).
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: task_id.clone(),
                meta: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Ok(McpResponse::GetTaskInfo(info)) => {
                assert_eq!(info.task_id, task_id);
                assert_eq!(info.status, TaskStatus::Completed);
            }
            _ => panic!("Expected GetTaskInfo response"),
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
                input_responses: None,
                request_state: None,
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
                meta: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        // SEP-2663 (final): cancel acknowledges with an empty result.
        match resp.inner {
            Ok(McpResponse::CancelTask(EmptyResult {})) => {}
            other => panic!("Expected empty CancelTask ack, got {other:?}"),
        }

        // Observable status is polled via tasks/get.
        let req = RouterRequest {
            id: RequestId::Number(3),
            inner: McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: task_id.clone(),
                meta: None,
            }),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::GetTaskInfo(info)) => {
                assert_eq!(info.status, TaskStatus::Cancelled);
            }
            _ => panic!("Expected GetTaskInfo response"),
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
                input_responses: None,
                request_state: None,
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
                meta: None,
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
                input_responses: None,
                request_state: None,
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
                meta: None,
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
                meta: None,
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
                meta: None,
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
                meta: None,
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
                meta: None,
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
                    extensions: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
                meta: None,
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
                meta: None,
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
                meta: None,
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
                meta: None,
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
                meta: None,
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
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
                uri: "file:///secret.txt".to_string(),
                meta: None,
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
                input_responses: None,
                request_state: None,
                uri: "file:///public.txt".to_string(),
                meta: None,
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
                input_responses: None,
                request_state: None,
                uri: "file:///secret.txt".to_string(),
                meta: None,
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
                input_responses: None,
                request_state: None,
                name: "system_debug".to_string(),
                arguments: HashMap::new(),
                meta: None,
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
                input_responses: None,
                request_state: None,
                name: "greeting".to_string(),
                arguments: HashMap::new(),
                meta: None,
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
                input_responses: None,
                request_state: None,
                name: "system_debug".to_string(),
                arguments: HashMap::new(),
                meta: None,
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
                input_responses: None,
                request_state: None,
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
                meta: None,
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
                    extensions: None,
                },
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
                meta: None,
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
                meta: None,
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
            inner: McpRequest::ListTools(ListToolsParams {
                cursor: None,
                meta: None,
            }),
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
                meta: None,
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
            inner: McpRequest::ListTools(ListToolsParams {
                cursor: None,
                meta: None,
            }),
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
                    input_responses: None,
                    request_state: None,
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
                    input_responses: None,
                    request_state: None,
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
                    input_responses: None,
                    request_state: None,
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
                    meta: None,
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
                    input_responses: None,
                    request_state: None,
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

    #[tokio::test]
    async fn test_tool_if_true_registers() {
        let tool = ToolBuilder::new("conditional")
            .description("Conditional tool")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new().tool_if(true, tool);
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
                assert_eq!(result.tools[0].name, "conditional");
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_tool_if_false_skips() {
        let tool = ToolBuilder::new("conditional")
            .description("Conditional tool")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();

        let mut router = McpRouter::new().tool_if(false, tool);
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
    async fn test_tools_if_batch_conditional() {
        let tools = vec![
            ToolBuilder::new("a")
                .description("Tool A")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
                .build(),
            ToolBuilder::new("b")
                .description("Tool B")
                .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
                .build(),
        ];

        let mut router = McpRouter::new().tools_if(false, tools);
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

    #[test]
    fn test_resource_if_true_registers() {
        let resource = crate::resource::ResourceBuilder::new("file:///test.txt")
            .name("test")
            .text("hello");

        let router = McpRouter::new().resource_if(true, resource);
        assert_eq!(router.inner.resources.len(), 1);
    }

    #[test]
    fn test_resource_if_false_skips() {
        let resource = crate::resource::ResourceBuilder::new("file:///test.txt")
            .name("test")
            .text("hello");

        let router = McpRouter::new().resource_if(false, resource);
        assert_eq!(router.inner.resources.len(), 0);
    }

    #[test]
    fn test_prompt_if_true_registers() {
        let prompt = crate::prompt::PromptBuilder::new("greet")
            .description("Greeting")
            .user_message("Hello!");

        let router = McpRouter::new().prompt_if(true, prompt);
        assert_eq!(router.inner.prompts.len(), 1);
    }

    #[test]
    fn test_prompt_if_false_skips() {
        let prompt = crate::prompt::PromptBuilder::new("greet")
            .description("Greeting")
            .user_message("Hello!");

        let router = McpRouter::new().prompt_if(false, prompt);
        assert_eq!(router.inner.prompts.len(), 0);
    }

    #[tokio::test]
    async fn test_disable_tool_hides_from_list() {
        let safe = ToolBuilder::new("safe")
            .description("Safe tool")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();
        let dangerous = ToolBuilder::new("dangerous")
            .description("Dangerous tool")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();
        let mut router = McpRouter::new().tool(safe).tool(dangerous);
        init_router(&mut router).await;

        router.disable_tool("dangerous");
        assert!(router.is_tool_enabled("safe"));
        assert!(!router.is_tool_enabled("dangerous"));

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert_eq!(names, vec!["safe"]);
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_disable_tool_blocks_call() {
        let dangerous = ToolBuilder::new("dangerous")
            .description("Dangerous tool")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ran")) })
            .build();
        let mut router = McpRouter::new().tool(dangerous);
        init_router(&mut router).await;

        router.disable_tool("dangerous");

        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::CallTool(CallToolParams {
                input_responses: None,
                request_state: None,
                name: "dangerous".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        let err = resp.inner.expect_err("disabled tool should error");
        assert_eq!(err.code, crate::error::ErrorCode::MethodNotFound as i32);
    }

    #[tokio::test]
    async fn test_enable_tool_restores_visibility() {
        let tool = ToolBuilder::new("flippy")
            .description("Toggleable tool")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ran")) })
            .build();
        let mut router = McpRouter::new().tool(tool);
        init_router(&mut router).await;

        router.disable_tool("flippy");
        router.enable_tool("flippy");
        assert!(router.is_tool_enabled("flippy"));

        let req = RouterRequest {
            id: RequestId::Number(3),
            inner: McpRequest::CallTool(CallToolParams {
                input_responses: None,
                request_state: None,
                name: "flippy".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::CallTool(result)) => {
                assert_eq!(result.first_text(), Some("ran"));
            }
            _ => panic!("Expected CallTool response"),
        }
    }

    #[tokio::test]
    async fn test_disable_propagates_through_fresh_session() {
        let tool = ToolBuilder::new("shared")
            .description("Shared across sessions")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();
        let router = McpRouter::new().tool(tool);

        // Disable on the parent, observe via with_fresh_session clone.
        router.disable_tool("shared");
        let mut child = router.with_fresh_session();
        init_router(&mut child).await;
        assert!(!child.is_tool_enabled("shared"));

        let req = RouterRequest {
            id: RequestId::Number(4),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };
        let resp = child.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert!(result.tools.is_empty());
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_disable_resource_and_prompt() {
        let resource = crate::resource::ResourceBuilder::new("file:///hidden.txt")
            .name("hidden")
            .text("secret");
        let prompt = crate::prompt::PromptBuilder::new("hidden_prompt")
            .description("hidden")
            .user_message("hello");

        let mut router = McpRouter::new().resource(resource).prompt(prompt);
        init_router(&mut router).await;

        router.disable_resource("file:///hidden.txt");
        router.disable_prompt("hidden_prompt");
        assert!(!router.is_resource_enabled("file:///hidden.txt"));
        assert!(!router.is_prompt_enabled("hidden_prompt"));

        // resources/list excludes
        let req = RouterRequest {
            id: RequestId::Number(5),
            inner: McpRequest::ListResources(ListResourcesParams::default()),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::ListResources(result)) => {
                assert!(result.resources.is_empty());
            }
            _ => panic!("Expected ListResources response"),
        }

        // resources/read returns not found
        let req = RouterRequest {
            id: RequestId::Number(6),
            inner: McpRequest::ReadResource(ReadResourceParams {
                input_responses: None,
                request_state: None,
                uri: "file:///hidden.txt".to_string(),
                meta: None,
            }),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        let err = resp.inner.expect_err("disabled resource should error");
        assert_eq!(err.code, -32602); // SEP-2164: ResourceNotFound now uses InvalidParams

        // prompts/list excludes
        let req = RouterRequest {
            id: RequestId::Number(7),
            inner: McpRequest::ListPrompts(ListPromptsParams::default()),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::ListPrompts(result)) => {
                assert!(result.prompts.is_empty());
            }
            _ => panic!("Expected ListPrompts response"),
        }

        // prompts/get returns not found
        let req = RouterRequest {
            id: RequestId::Number(8),
            inner: McpRequest::GetPrompt(GetPromptParams {
                input_responses: None,
                request_state: None,
                name: "hidden_prompt".to_string(),
                arguments: Default::default(),
                meta: None,
            }),
            extensions: Extensions::new(),
        };
        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        let err = resp.inner.expect_err("disabled prompt should error");
        assert_eq!(err.code, crate::error::ErrorCode::MethodNotFound as i32);
    }

    #[test]
    fn test_router_request_new() {
        let req = RouterRequest::new(RequestId::Number(1), McpRequest::Ping);
        assert_eq!(req.id, RequestId::Number(1));
        assert!(req.extensions.is_empty());
    }

    #[test]
    fn test_with_inner_preserves_extensions() {
        let mut req = RouterRequest::new(RequestId::Number(1), McpRequest::Ping);
        req.extensions.insert(42u32);

        let rewritten = req.with_inner(McpRequest::ListTools(Default::default()));
        assert!(matches!(rewritten.inner, McpRequest::ListTools(_)));
        assert_eq!(rewritten.id, RequestId::Number(1));
        assert_eq!(rewritten.extensions.get::<u32>(), Some(&42));
    }

    #[test]
    fn test_with_id_and_inner_preserves_extensions() {
        let mut req = RouterRequest::new(RequestId::Number(1), McpRequest::Ping);
        req.extensions.insert(String::from("token-abc"));

        let rewritten = req.with_id_and_inner(
            RequestId::Number(99),
            McpRequest::ListResources(Default::default()),
        );
        assert_eq!(rewritten.id, RequestId::Number(99));
        assert!(matches!(rewritten.inner, McpRequest::ListResources(_)));
        assert_eq!(
            rewritten.extensions.get::<String>(),
            Some(&String::from("token-abc"))
        );
    }

    #[test]
    fn test_clone_with_inner_preserves_extensions() {
        let mut req = RouterRequest::new(RequestId::Number(1), McpRequest::Ping);
        req.extensions.insert(true);

        let cloned = req.clone_with_inner(McpRequest::ListTools(Default::default()));

        // Original still intact
        assert!(matches!(req.inner, McpRequest::Ping));
        assert_eq!(req.extensions.get::<bool>(), Some(&true));

        // Clone has new inner but same extensions
        assert!(matches!(cloned.inner, McpRequest::ListTools(_)));
        assert_eq!(cloned.extensions.get::<bool>(), Some(&true));
    }

    #[test]
    fn test_router_response_is_error() {
        let ok_resp = RouterResponse {
            id: RequestId::Number(1),
            inner: Ok(McpResponse::Pong(Default::default())),
        };
        assert!(!ok_resp.is_error());

        let err_resp = RouterResponse {
            id: RequestId::Number(2),
            inner: Err(JsonRpcError::internal_error("boom")),
        };
        assert!(err_resp.is_error());
    }

    #[test]
    fn test_extensions_len_and_is_empty() {
        let mut ext = Extensions::new();
        assert!(ext.is_empty());
        assert_eq!(ext.len(), 0);

        ext.insert(42u32);
        assert!(!ext.is_empty());
        assert_eq!(ext.len(), 1);

        ext.insert(String::from("hello"));
        assert_eq!(ext.len(), 2);
    }

    #[test]
    fn test_router_response_serde_roundtrip() {
        // Success response
        let response = RouterResponse {
            id: RequestId::Number(1),
            inner: Ok(McpResponse::Empty(EmptyResult {})),
        };
        let json = serde_json::to_string(&response).unwrap();
        let deserialized: RouterResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, RequestId::Number(1));
        assert!(!deserialized.is_error());

        // Error response
        let response = RouterResponse {
            id: RequestId::String("req-2".into()),
            inner: Err(JsonRpcError::method_not_found("unknown")),
        };
        let json = serde_json::to_string(&response).unwrap();
        let deserialized: RouterResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, RequestId::String("req-2".into()));
        assert!(deserialized.is_error());
    }

    // =========================================================================
    // Issue #872: McpRequest::Discover unit tests
    // Unit tests that exercise the router dispatch directly via JsonRpcService,
    // without going through the HTTP transport layer.
    // =========================================================================

    #[tokio::test]
    async fn test_discover_dispatch_via_jsonrpc_service() {
        // server/discover must work without any prior initialize call.
        // The router does NOT require session initialization for this RPC.
        let router = McpRouter::new().server_info("unit-test-server", "4.2.0");
        let mut service = JsonRpcService::new(router);

        let req = JsonRpcRequest::new(1, "server/discover");
        let resp = service.call_single(req).await.unwrap();

        match resp {
            JsonRpcResponse::Result(r) => {
                // supportedVersions must be a non-empty array.
                let versions = r
                    .result
                    .get("supportedVersions")
                    .and_then(|v| v.as_array())
                    .expect("result.supportedVersions must be an array");
                assert!(!versions.is_empty(), "supportedVersions must not be empty");

                // Server identity lives in _meta, not the result body (SEP-2575 final).
                assert_eq!(
                    r.result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
                    "unit-test-server",
                    "serverInfo.name must match configured value"
                );
                assert_eq!(
                    r.result["_meta"]["io.modelcontextprotocol/serverInfo"]["version"], "4.2.0",
                    "serverInfo.version must match configured value"
                );

                // server/discover must NOT include singular protocolVersion
                // (that field belongs to the initialize response shape).
                assert!(
                    r.result.get("protocolVersion").is_none(),
                    "server/discover must NOT include protocolVersion: {:?}",
                    r.result
                );
            }
            JsonRpcResponse::Error(e) => panic!("Expected success, got error: {:?}", e),
            _ => panic!("unexpected response variant"),
        }
    }

    #[tokio::test]
    async fn test_discover_does_not_require_initialization() {
        // server/discover works on a freshly created, un-initialized router.
        // No prior initialize call is made -- the session state is empty.
        let router = McpRouter::new().server_info("fresh-router", "1.0.0");
        let mut service = JsonRpcService::new(router);

        let req = JsonRpcRequest::new(2, "server/discover");
        let resp = service.call_single(req).await.unwrap();

        // Must succeed -- not return an error about missing session/initialization.
        assert!(
            !matches!(resp, JsonRpcResponse::Error(_)),
            "server/discover must not require initialization: {:?}",
            resp
        );
    }
}

#[cfg(test)]
mod cursor_property_tests {
    use super::{decode_cursor, encode_cursor};
    use proptest::prelude::*;

    fn arb_cursor_text() -> BoxedStrategy<String> {
        prop_oneof![
            8 => prop::collection::vec(any::<char>(), 0..512)
                .prop_map(|chars| chars.into_iter().collect()),
            1 => Just("\0\r\n\t\u{001b}\u{007f}".repeat(64)),
            1 => Just("A".repeat(16 * 1024)),
        ]
        .boxed()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// A cursor round-trips: decode(encode(n)) == n.
        #[test]
        fn cursor_round_trips(offset in any::<usize>()) {
            prop_assert_eq!(decode_cursor(&encode_cursor(offset)).unwrap(), offset);
        }

        /// Decoding arbitrary client input never panics; it is Ok or a clean Err.
        #[test]
        fn decode_cursor_never_panics(s in arb_cursor_text()) {
            let _ = decode_cursor(&s);
        }
    }
}
