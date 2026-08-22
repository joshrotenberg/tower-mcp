//! MCP Router - routes requests to tools, resources, and prompts
//!
//! The router implements Tower's `Service` trait, making it composable with
//! standard tower middleware.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};

use tower_service::Service;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

use crate::async_task::{MemoryTaskStore, TaskStore, TaskStoreError};
use crate::context::{
    CancellationToken, ClientRequesterHandle, NotificationSender, RequestContext,
    ServerNotification,
};
use crate::error::{Error, JsonRpcError, Result};
use crate::filter::{
    CapabilityFilterContext, CapabilityOperation, PromptFilter, ResourceFilter,
    ResourceTemplateFilter, ToolFilter,
};
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
    dyn Fn(
            RequestContext,
            CompleteParams,
        ) -> Pin<Box<dyn Future<Output = Result<CompleteResult>> + Send>>
        + Send
        + Sync,
>;

fn prompt_not_found(name: &str) -> Error {
    Error::JsonRpc(JsonRpcError::method_not_found(&format!(
        "Prompt not found: {name}"
    )))
}

/// Releases a live task's registry entry however its handler leaves.
///
/// The handler can return, panic, or be dropped. Unregistering only on the
/// return path left a panicking handler's entry installed, so a later
/// `tasks/cancel` found a handle nobody was reading and took the live path
/// instead of the store one (#1305).
struct LiveTaskRegistration {
    router: McpRouter,
    task_id: String,
}

impl Drop for LiveTaskRegistration {
    fn drop(&mut self) {
        self.router.unregister_live_task(&self.task_id);
    }
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
    /// Legacy `resources/subscribe` membership for this logical session.
    ///
    /// Ordinary router clones share this state because transports clone a
    /// session router for each request. [`Self::with_fresh_session`] replaces
    /// it so one client's membership cannot affect another client.
    subscriptions: Arc<RwLock<HashSet<String>>>,
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

#[cfg(feature = "dynamic-tools")]
type PromptInitializer = Arc<dyn Fn() -> Result<()> + Send + Sync + 'static>;

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
    /// How to convert a panicking tool handler into an error result rather
    /// than letting it unwind out of the service (#1230, #1306).
    panic_policy: Option<PanicPolicy>,
    /// Root-owned mapping for client-visible Task lifecycle failures.
    task_error_policy: TaskErrorPolicy,
    auto_instructions: Option<AutoInstructionsConfig>,
    tools: HashMap<String, Arc<Tool>>,
    resources: HashMap<String, Arc<Resource>>,
    /// Resource templates for dynamic resource matching (keyed by uri_template)
    resource_templates: Vec<Arc<ResourceTemplate>>,
    prompts: HashMap<String, Arc<Prompt>>,
    /// Whether to advertise `resources.subscribe`. Defaults to true, which
    /// is what this router has always advertised when resources exist (#1261).
    advertise_resource_subscriptions: bool,
    /// Explicit override for whether to advertise `tools.listChanged`.
    /// `None` derives from whether a notification channel is attached, which
    /// is what this router has always advertised (#1338).
    advertise_tools_list_changed: Option<bool>,
    /// Explicit override for whether to advertise `prompts.listChanged`.
    /// `None` derives from whether a notification channel is attached, which
    /// is what this router has always advertised (#1338).
    advertise_prompts_list_changed: Option<bool>,
    /// Explicit override for whether to advertise `resources.listChanged`.
    /// `None` derives from whether a notification channel is attached, which
    /// is what this router has always advertised (#1338).
    advertise_resources_list_changed: Option<bool>,
    /// Explicit override for whether to advertise the `logging` capability.
    /// `None` derives from whether a notification channel is attached, which
    /// is what this router has always advertised (#1338).
    advertise_mcp_logging: Option<bool>,
    /// Live tasks currently running, keyed by task id (#1246).
    ///
    /// A live handler parks inside its own future rather than returning, so
    /// the router needs a handle to wake it when `tasks/update` commits and
    /// to signal it when `tasks/cancel` arrives.
    live_tasks: Arc<Mutex<HashMap<String, Arc<crate::tool::LiveTask>>>>,
    /// In-flight requests for cancellation tracking (shared across clones).
    ///
    /// Keyed by request id for lookup, but each id holds one entry per
    /// *dispatch*. A client should not reuse an id that is still in flight,
    /// but when one does, the twins have to coexist: keyed by id alone the
    /// second registration evicted the first and the first became
    /// uncancellable (#1270).
    in_flight: Arc<RwLock<HashMap<RequestId, Vec<InFlightDispatch>>>>,
    /// Source of the per-dispatch ids in `in_flight`, shared across clones.
    next_dispatch: Arc<AtomicU64>,
    /// Channel for sending notifications to connected clients
    notification_tx: Option<NotificationSender>,
    /// Transport-lifetime sink for final HTTP subscription notifications.
    ///
    /// The lock is shared across router clones so an application-owned clone
    /// can publish after the transport attaches its subscription registry.
    #[cfg(all(feature = "http", feature = "stateless"))]
    modern_notification_sink: Arc<RwLock<Option<ModernNotificationSink>>>,
    #[cfg(feature = "stateless")]
    subscription_observer:
        Arc<RwLock<Option<Arc<dyn crate::transport::subscriptions::SubscriptionObserver>>>>,
    /// Handle for sending requests to the client (for sampling, etc.)
    client_requester: Option<ClientRequesterHandle>,
    /// Task store for async operations
    task_store: Arc<dyn TaskStore>,
    /// Handler for completion requests
    completion_handler: Option<CompletionHandler>,
    /// Filter for tools based on session state
    tool_filter: Option<ToolFilter>,
    /// Filter for resources based on session state
    resource_filter: Option<ResourceFilter>,
    /// Filter for resource templates and their concrete resolved URIs
    resource_template_filter: Option<ResourceTemplateFilter>,
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
    /// Lazily populates the dynamic prompt registry before list/get access.
    #[cfg(feature = "dynamic-tools")]
    prompt_initializer: Option<PromptInitializer>,
    /// Dynamic resources registry for runtime resource (de)registration
    #[cfg(feature = "dynamic-tools")]
    dynamic_resources: Option<Arc<DynamicResourcesInner>>,
    /// Dynamic resource templates registry for runtime template (de)registration
    #[cfg(feature = "dynamic-tools")]
    dynamic_resource_templates: Option<Arc<DynamicResourceTemplatesInner>>,
}

impl McpRouter {
    fn capability_filter_context<'a>(
        &'a self,
        request_extensions: &Extensions,
        operation: CapabilityOperation<'a>,
    ) -> CapabilityFilterContext<'a> {
        CapabilityFilterContext::new(
            &self.session,
            &self.inner.extensions,
            request_extensions,
            operation,
        )
    }

    fn resource_template_is_visible(
        &self,
        context: &CapabilityFilterContext<'_>,
        template: &ResourceTemplate,
    ) -> bool {
        match &self.inner.resource_template_filter {
            Some(filter) => filter.is_visible_with_context(context, template),
            None => self.inner.resource_filter.is_none(),
        }
    }

    fn authorize_resource_template(
        &self,
        context: &CapabilityFilterContext<'_>,
        template: &ResourceTemplate,
        concrete_uri: &str,
    ) -> Result<()> {
        if let Some(filter) = &self.inner.resource_template_filter {
            if !filter.is_visible_with_context(context, template) {
                return Err(filter.denial_error(concrete_uri));
            }
        } else if let Some(filter) = &self.inner.resource_filter {
            // A ResourceFilter cannot safely evaluate a template. Existing
            // filtered deployments therefore fail closed until they opt into
            // an explicit ResourceTemplateFilter (#1399).
            return Err(filter.denial_error(concrete_uri));
        }
        Ok(())
    }

    /// Authorize a resource-template completion without disclosing whether
    /// the referenced template exists under the default denial policy.
    fn authorize_completion_resource_template(
        &self,
        context: &CapabilityFilterContext<'_>,
        template: &ResourceTemplate,
        target: &str,
    ) -> Result<()> {
        if let Some(filter) = &self.inner.resource_template_filter {
            if !filter.is_visible_with_context(context, template) {
                return Err(filter.denial_error_or_not_found(target, || {
                    Error::JsonRpc(JsonRpcError::resource_not_found(target))
                }));
            }
        } else if let Some(filter) = &self.inner.resource_filter {
            // An exact-resource filter cannot authorize a template. Match
            // resources/read's fail-closed policy, but use the completion
            // reference's canonical unknown response for default NotFound.
            return Err(filter.denial_error_or_not_found(target, || {
                Error::JsonRpc(JsonRpcError::resource_not_found(target))
            }));
        }
        Ok(())
    }

    /// Authorize access to an exact statically registered resource.
    ///
    /// Legacy resource subscriptions deliberately retain their existing
    /// exact-static scope. This helper gives subscribe and unsubscribe the
    /// same disabled/filter policy and concrete-target denial behavior as a
    /// static resource read without expanding the accepted URI universe to
    /// dynamic resources or templates.
    fn authorize_static_resource_subscription(
        &self,
        request_extensions: &Extensions,
        uri: &str,
    ) -> Result<()> {
        let disabled = self.inner.disabled_resources.read().unwrap().contains(uri);
        if disabled {
            return Err(Error::JsonRpc(JsonRpcError::resource_not_found(uri)));
        }

        let resource = self
            .inner
            .resources
            .get(uri)
            .ok_or_else(|| Error::JsonRpc(JsonRpcError::resource_not_found(uri)))?;
        let context = self.capability_filter_context(
            request_extensions,
            CapabilityOperation::Access { target: uri },
        );
        if let Some(filter) = &self.inner.resource_filter
            && !filter.is_visible_with_context(&context, resource)
        {
            return Err(filter.denial_error(uri));
        }

        Ok(())
    }

    /// Authorize the capability named by a completion request.
    ///
    /// Completion is a second access path to prompts and resources, so it
    /// must resolve and authorize the same winning registration as the
    /// ordinary get/read paths before the application handler sees the
    /// request. In particular, a denied static registration must not fall
    /// through to a dynamic registration with the same name or URI.
    fn authorize_completion_reference(
        &self,
        request_extensions: &Extensions,
        reference: &CompletionReference,
    ) -> Result<()> {
        match reference {
            CompletionReference::Prompt { name } => {
                #[cfg(feature = "dynamic-tools")]
                if let Some(initializer) = &self.inner.prompt_initializer {
                    initializer()?;
                }

                if self.inner.disabled_prompts.read().unwrap().contains(name) {
                    return Err(prompt_not_found(name));
                }

                let prompt = self.inner.prompts.get(name).cloned();
                #[cfg(feature = "dynamic-tools")]
                let prompt = prompt.or_else(|| {
                    self.inner
                        .dynamic_prompts
                        .as_ref()
                        .and_then(|dynamic| dynamic.get(name))
                });
                let prompt = prompt.ok_or_else(|| prompt_not_found(name))?;

                let context = self.capability_filter_context(
                    request_extensions,
                    CapabilityOperation::Access { target: name },
                );
                if let Some(filter) = &self.inner.prompt_filter
                    && !filter.is_visible_with_context(&context, &prompt)
                {
                    return Err(filter.denial_error_or_not_found(name, || prompt_not_found(name)));
                }
                Ok(())
            }
            CompletionReference::Resource { uri } => {
                if self.inner.disabled_resources.read().unwrap().contains(uri) {
                    return Err(Error::JsonRpc(JsonRpcError::resource_not_found(uri)));
                }

                let context = self.capability_filter_context(
                    request_extensions,
                    CapabilityOperation::Access { target: uri },
                );

                // Exact resources take precedence over templates, matching
                // resources/read. Static resources likewise shadow dynamic
                // resources with the same URI.
                if let Some(resource) = self.inner.resources.get(uri) {
                    if let Some(filter) = &self.inner.resource_filter
                        && !filter.is_visible_with_context(&context, resource)
                    {
                        return Err(filter.denial_error_or_not_found(uri, || {
                            Error::JsonRpc(JsonRpcError::resource_not_found(uri))
                        }));
                    }
                    return Ok(());
                }
                #[cfg(feature = "dynamic-tools")]
                if let Some(resource) = self
                    .inner
                    .dynamic_resources
                    .as_ref()
                    .and_then(|dynamic| dynamic.get(uri))
                {
                    if let Some(filter) = &self.inner.resource_filter
                        && !filter.is_visible_with_context(&context, &resource)
                    {
                        return Err(filter.denial_error_or_not_found(uri, || {
                            Error::JsonRpc(JsonRpcError::resource_not_found(uri))
                        }));
                    }
                    return Ok(());
                }

                // A completion reference may contain either a resource URI
                // or the registered URI-template pattern. Check exact
                // patterns before treating the value as a concrete URI so a
                // template definition authorizes its own completion request.
                if let Some(template) = self
                    .inner
                    .resource_templates
                    .iter()
                    .find(|template| template.uri_template == *uri)
                {
                    return self.authorize_completion_resource_template(&context, template, uri);
                }
                #[cfg(feature = "dynamic-tools")]
                if let Some(template) =
                    self.inner
                        .dynamic_resource_templates
                        .as_ref()
                        .and_then(|dynamic| {
                            dynamic
                                .list()
                                .into_iter()
                                .find(|template| template.uri_template == *uri)
                        })
                {
                    return self.authorize_completion_resource_template(&context, &template, uri);
                }

                // Concrete URIs produced by a template are resource
                // references too. Preserve resources/read's first-match and
                // static-before-dynamic routing semantics.
                if let Some(template) = self
                    .inner
                    .resource_templates
                    .iter()
                    .find(|template| template.match_uri(uri).is_some())
                {
                    return self.authorize_completion_resource_template(&context, template, uri);
                }
                #[cfg(feature = "dynamic-tools")]
                if let Some((template, _variables)) = self
                    .inner
                    .dynamic_resource_templates
                    .as_ref()
                    .and_then(|dynamic| dynamic.match_uri(uri))
                {
                    return self.authorize_completion_resource_template(&context, &template, uri);
                }

                Err(Error::JsonRpc(JsonRpcError::resource_not_found(uri)))
            }
            _ => Err(Error::JsonRpc(JsonRpcError::invalid_params(
                "Unsupported completion reference",
            ))),
        }
    }

    /// Generate request-filtered instructions from registered capabilities.
    fn generate_instructions(
        &self,
        config: &AutoInstructionsConfig,
        request_extensions: &Extensions,
    ) -> String {
        let mut parts = Vec::new();
        let context = self.capability_filter_context(request_extensions, CapabilityOperation::List);

        if let Some(prefix) = &config.prefix {
            parts.push(prefix.clone());
        }

        // Tools section
        let disabled_tools = self.inner.disabled_tools.read().unwrap().clone();
        let mut tools: Vec<_> = self
            .inner
            .tools
            .values()
            .filter(|tool| {
                !disabled_tools.contains(&tool.name)
                    && self
                        .inner
                        .tool_filter
                        .as_ref()
                        .is_none_or(|filter| filter.is_visible_with_context(&context, tool))
            })
            .collect();
        if !tools.is_empty() {
            let mut lines = vec!["## Tools".to_string(), String::new()];
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
        let disabled_resources = self.inner.disabled_resources.read().unwrap().clone();
        let mut resources: Vec<_> = self
            .inner
            .resources
            .values()
            .filter(|resource| {
                !disabled_resources.contains(&resource.uri)
                    && self
                        .inner
                        .resource_filter
                        .as_ref()
                        .is_none_or(|filter| filter.is_visible_with_context(&context, resource))
            })
            .collect();
        let mut templates: Vec<_> = self
            .inner
            .resource_templates
            .iter()
            .filter(|template| self.resource_template_is_visible(&context, template))
            .collect();
        if !resources.is_empty() || !templates.is_empty() {
            let mut lines = vec!["## Resources".to_string(), String::new()];
            resources.sort_by(|a, b| a.uri.cmp(&b.uri));
            for resource in resources {
                let desc = resource.description.as_deref().unwrap_or("No description");
                lines.push(format!("- **{}**: {}", resource.uri, desc));
            }
            templates.sort_by(|a, b| a.uri_template.cmp(&b.uri_template));
            for template in templates {
                let desc = template.description.as_deref().unwrap_or("No description");
                lines.push(format!("- **{}**: {}", template.uri_template, desc));
            }
            parts.push(lines.join("\n"));
        }

        // Prompts section
        let disabled_prompts = self.inner.disabled_prompts.read().unwrap().clone();
        let mut prompts: Vec<_> = self
            .inner
            .prompts
            .values()
            .filter(|prompt| {
                !disabled_prompts.contains(&prompt.name)
                    && self
                        .inner
                        .prompt_filter
                        .as_ref()
                        .is_none_or(|filter| filter.is_visible_with_context(&context, prompt))
            })
            .collect();
        if !prompts.is_empty() {
            let mut lines = vec!["## Prompts".to_string(), String::new()];
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
                panic_policy: None,
                task_error_policy: TaskErrorPolicy::default(),
                auto_instructions: None,
                tools: HashMap::new(),
                resources: HashMap::new(),
                resource_templates: Vec::new(),
                prompts: HashMap::new(),
                advertise_resource_subscriptions: true,
                advertise_tools_list_changed: None,
                advertise_prompts_list_changed: None,
                advertise_resources_list_changed: None,
                advertise_mcp_logging: None,
                live_tasks: Arc::new(Mutex::new(HashMap::new())),
                in_flight: Arc::new(RwLock::new(HashMap::new())),
                next_dispatch: Arc::new(AtomicU64::new(0)),
                notification_tx: None,
                #[cfg(all(feature = "http", feature = "stateless"))]
                modern_notification_sink: Arc::new(RwLock::new(None)),
                #[cfg(feature = "stateless")]
                subscription_observer: Arc::new(RwLock::new(None)),
                client_requester: None,
                task_store: Arc::new(MemoryTaskStore::new()),
                extensions: Arc::new(crate::context::Extensions::new()),
                protocol_extensions: HashMap::new(),
                completion_handler: None,
                tool_filter: None,
                resource_filter: None,
                resource_template_filter: None,
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
                prompt_initializer: None,
                #[cfg(feature = "dynamic-tools")]
                dynamic_resources: None,
                #[cfg(feature = "dynamic-tools")]
                dynamic_resource_templates: None,
            }),
            session: SessionState::new(),
            subscriptions: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Create a clone with fresh session state.
    ///
    /// Use this when creating a new logical session (e.g., per HTTP connection).
    /// The router configuration (tools, resources, prompts) is shared, but the
    /// session state (phase, extensions) and legacy resource subscriptions are
    /// independent.
    ///
    /// This is typically called by transports when establishing a new client session.
    pub fn with_fresh_session(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            session: SessionState::new(),
            subscriptions: Arc::new(RwLock::new(HashSet::new())),
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

    /// Set the root router's client-visible Task error policy.
    ///
    /// The policy applies to task creation, `tasks/get`, `tasks/update`,
    /// `tasks/cancel`, and failures while parking, executing, resuming, or
    /// finalizing a handler. Like [`McpRouter::catch_panics_with`], it is root
    /// configuration: merging or nesting another router imports that router's
    /// capabilities but not its policy, so the receiving router governs the
    /// combined catalog.
    ///
    /// Tower's default preserves the established missing/expired response
    /// shapes and redacts every [`TaskStoreError`] to a fixed internal error.
    #[must_use]
    pub fn task_error_policy(mut self, policy: TaskErrorPolicy) -> Self {
        Arc::make_mut(&mut self.inner).task_error_policy = policy;
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

    /// Run an initializer before each `prompts/list` or `prompts/get` access.
    ///
    /// This supports prompt definitions backed by an application-owned lazy
    /// catalog. The initializer should populate the registry returned by
    /// [`Self::with_dynamic_prompts`] and implement its own caching.
    #[cfg(feature = "dynamic-tools")]
    pub fn dynamic_prompt_initializer<F>(mut self, initializer: F) -> Self
    where
        F: Fn() -> Result<()> + Send + Sync + 'static,
    {
        Arc::make_mut(&mut self.inner).prompt_initializer = Some(Arc::new(initializer));
        self
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

    /// Observe the terminal half of `subscriptions/listen` streams.
    ///
    /// Every transport built from this router reports stream closes (reason
    /// and duration) through the observer. The request half of the boundary
    /// is ordinary `Service<RouterRequest>` middleware; see
    /// [`SubscriptionObserver`](crate::transport::subscriptions::SubscriptionObserver) for how the two compose.
    #[cfg(feature = "stateless")]
    pub fn with_subscription_observer(
        self,
        observer: Arc<dyn crate::transport::subscriptions::SubscriptionObserver>,
    ) -> Self {
        if let Ok(mut slot) = self.inner.subscription_observer.write() {
            *slot = Some(observer);
        }
        self
    }

    /// The attached close observer, if any.
    #[cfg(feature = "stateless")]
    pub(crate) fn subscription_observer(
        &self,
    ) -> Option<Arc<dyn crate::transport::subscriptions::SubscriptionObserver>> {
        self.inner
            .subscription_observer
            .read()
            .ok()
            .and_then(|slot| slot.clone())
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
        let final_lifecycle = is_final_protocol_request(per_request);
        let ctx = ctx.with_final_lifecycle(final_lifecycle);
        let ctx = if final_lifecycle {
            ctx
        } else {
            ctx.with_resource_subscriptions(self.subscriptions.clone())
        };
        let ctx = if !final_lifecycle
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

        let ctx = ctx
            .with_extensions(Arc::new(merged))
            .with_session(self.session.clone());

        // Set up log level filtering
        let ctx = ctx.with_min_log_level(self.inner.min_log_level.clone());

        // Register for cancellation tracking. `Service::call` mints the
        // dispatch id and threads it through the extensions so the guard it
        // holds and this registration name the same entry; a caller driving
        // the router directly gets a fresh one.
        let dispatch = per_request
            .get::<DispatchId>()
            .copied()
            .unwrap_or_else(|| self.next_dispatch());
        self.register_in_flight(request_id, dispatch, ctx.cancellation_token());

        ctx
    }

    /// Allocate a dispatch id, unique for the lifetime of this router.
    fn next_dispatch(&self) -> DispatchId {
        DispatchId(
            self.inner
                .next_dispatch
                .fetch_add(1, AtomicOrdering::Relaxed),
        )
    }

    /// Track one dispatch for cancellation.
    ///
    /// Appends rather than overwrites: a client that reuses an id which is
    /// still in flight gets both requests tracked, so cancelling the id can
    /// still reach both (#1270).
    fn register_in_flight(
        &self,
        request_id: RequestId,
        dispatch: DispatchId,
        token: CancellationToken,
    ) {
        if let Ok(mut in_flight) = self.inner.in_flight.write() {
            in_flight
                .entry(request_id)
                .or_default()
                .push(InFlightDispatch { dispatch, token });
        }
    }

    /// Stop tracking one dispatch, leaving any twin under the same id alone.
    fn complete_dispatch(&self, request_id: &RequestId, dispatch: DispatchId) {
        if let Ok(mut in_flight) = self.inner.in_flight.write()
            && let Some(entries) = in_flight.get_mut(request_id)
        {
            entries.retain(|entry| entry.dispatch != dispatch);
            if entries.is_empty() {
                in_flight.remove(request_id);
            }
        }
    }

    /// Remove a request from tracking (called when request completes).
    ///
    /// Untracks *every* dispatch under `request_id`, which is the only
    /// granularity this signature offers. Requests dispatched through
    /// [`Service::call`] do not need it: each holds a guard that untracks its
    /// own dispatch when the future completes, is dropped, or unwinds. It
    /// remains for callers driving [`McpRouter::create_context`] and request
    /// handling themselves.
    pub fn complete_request(&self, request_id: &RequestId) {
        if let Ok(mut in_flight) = self.inner.in_flight.write() {
            in_flight.remove(request_id);
        }
    }

    /// Cancel a tracked request.
    ///
    /// Cancels every dispatch still running under `request_id`. The id is the
    /// only handle a client has, so a client that reused one in flight gets
    /// both stopped rather than an arbitrary one.
    fn cancel_request(&self, request_id: &RequestId) -> bool {
        let Ok(in_flight) = self.inner.in_flight.read() else {
            return false;
        };
        let Some(entries) = in_flight.get(request_id) else {
            return false;
        };
        for entry in entries {
            entry.token.cancel();
        }
        !entries.is_empty()
    }

    /// Server capabilities, derived from what is registered.
    fn capabilities(&self) -> ServerCapabilities {
        let has_resources =
            !self.inner.resources.is_empty() || !self.inner.resource_templates.is_empty();
        let has_notifications = self.inner.notification_tx.is_some();

        // Each of these defaults to `has_notifications`, which is what this
        // router has always advertised as soon as a transport attached a
        // notification channel. An explicit builder call
        // (`tools_list_changed`, `prompts_list_changed`,
        // `resources_list_changed`, `mcp_logging`) overrides that default in
        // either direction, independently of the channel (#1338).
        let tools_list_changed = self
            .inner
            .advertise_tools_list_changed
            .unwrap_or(has_notifications);
        let prompts_list_changed = self
            .inner
            .advertise_prompts_list_changed
            .unwrap_or(has_notifications);
        let resources_list_changed = self
            .inner
            .advertise_resources_list_changed
            .unwrap_or(has_notifications);
        let mcp_logging = self
            .inner
            .advertise_mcp_logging
            .unwrap_or(has_notifications);

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
                    list_changed: tools_list_changed,
                })
            },
            resources: if has_resources || has_dynamic_resources {
                Some(ResourcesCapability {
                    subscribe: self.inner.advertise_resource_subscriptions,
                    list_changed: resources_list_changed,
                })
            } else {
                None
            },
            prompts: if self.inner.prompts.is_empty() && !has_dynamic_prompts {
                None
            } else {
                Some(PromptsCapability {
                    list_changed: prompts_list_changed,
                })
            },
            // Advertised when a notification channel is configured, unless
            // overridden by `mcp_logging` (#1338).
            logging: if mcp_logging {
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
            // `resources/subscribe` and `resources/unsubscribe` are not part
            // of this revision, and the inspector already classifies them as
            // unavailable here. Advertising the capability would promise a
            // method the same build refuses to route (#1261).
            if let Some(resources) = capabilities.resources.as_mut() {
                resources.subscribe = false;
            }
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

    /// Invoke a tool, optionally converting a panic into an error result.
    ///
    /// Enabled by [`McpRouter::catch_panics`] or
    /// [`McpRouter::catch_panics_with`]. Without either this is a direct call
    /// and a panic unwinds as before, which is the default because a panic is
    /// an invariant violation and hiding one is not always a favour.
    async fn invoke_tool(
        &self,
        tool: &crate::tool::Tool,
        ctx: RequestContext,
        arguments: serde_json::Value,
        tool_name: &str,
    ) -> Result<crate::protocol::RequestOutcome<CallToolResult>> {
        let Some(policy) = &self.inner.panic_policy else {
            return tool.call_outcome_with_context(ctx, arguments).await;
        };

        use futures::FutureExt;
        // AssertUnwindSafe: the future may hold &mut across the await, which
        // Rust cannot prove safe to observe post-unwind. Any state a panicking
        // handler leaves behind belongs to that handler; the router's own
        // state is not mutated by this call.
        let called = std::panic::AssertUnwindSafe(async move {
            tool.call_outcome_with_context(ctx, arguments).await
        })
        .catch_unwind()
        .await;

        match called {
            Ok(outcome) => outcome,
            Err(payload) => {
                let message = self.handle_caught_panic(policy, tool_name, None, &*payload);
                Ok(crate::protocol::RequestOutcome::Complete(
                    CallToolResult::error(message),
                ))
            }
        }
    }

    /// Apply the selected disclosure policy to a caught handler panic.
    ///
    /// Payload recovery is intentionally conditional: the fully redacted
    /// path never downcasts, clones, or formats the panic payload.
    fn handle_caught_panic(
        &self,
        policy: &PanicPolicy,
        tool_name: &str,
        task_id: Option<&str>,
        payload: &(dyn std::any::Any + Send),
    ) -> String {
        let payload = policy.needs_payload().then(|| panic_message(payload));
        let logged_tool = policy.log_tool_name.value(tool_name);
        let logged_payload = policy
            .include_payload_in_logs
            .then(|| payload.as_deref().unwrap_or("<redacted>"));

        Self::log_caught_panic(logged_tool, logged_payload, task_id);
        policy.client_message(tool_name, payload.as_deref())
    }

    /// Build the JSON-RPC error a transport sends for an internal failure of
    /// its own, honouring the configured disclosure policy.
    ///
    /// A transport that hand-builds an error response is outside every path
    /// that consults [`PanicPolicy`], so before this existed it sent the
    /// error's `Display` text whatever the operator had configured (#1354).
    /// Routing the two websocket sites through one helper rather than
    /// widening the panic path is what keeps the next transport from
    /// reintroducing the gap: the previous round of this, #1335, fixed one of
    /// a pair of near-identical sites and left the other to drift.
    ///
    /// With no policy installed the error's text is returned unchanged, which
    /// is both the behaviour these paths already had and the stance the crate
    /// takes elsewhere: a panic is not caught at all until `catch_panics` asks
    /// for it.
    ///
    /// Gated on `websocket` because that is where the two sites are. Widen the
    /// gate rather than duplicating the decision when another transport needs
    /// it, which is the whole point of it being one helper.
    #[cfg(feature = "websocket")]
    pub(crate) fn transport_internal_error(&self, error: &dyn std::fmt::Display) -> JsonRpcError {
        match &self.inner.panic_policy {
            Some(policy) => JsonRpcError::internal_error(policy.internal_error_message(error)),
            None => JsonRpcError::internal_error(error.to_string()),
        }
    }

    fn log_caught_panic(tool_name: Option<&str>, payload: Option<&str>, task_id: Option<&str>) {
        match (tool_name, payload, task_id) {
            (Some(tool_name), Some(payload), Some(task_id)) => tracing::error!(
                target: "mcp::tools",
                tool = %tool_name,
                panic = %payload,
                task_id = %task_id,
                "tool handler panicked; returning an error result"
            ),
            (Some(tool_name), Some(payload), None) => tracing::error!(
                target: "mcp::tools",
                tool = %tool_name,
                panic = %payload,
                "tool handler panicked; returning an error result"
            ),
            (Some(tool_name), None, Some(task_id)) => tracing::error!(
                target: "mcp::tools",
                tool = %tool_name,
                task_id = %task_id,
                "tool handler panicked; returning an error result"
            ),
            (Some(tool_name), None, None) => tracing::error!(
                target: "mcp::tools",
                tool = %tool_name,
                "tool handler panicked; returning an error result"
            ),
            (None, Some(payload), Some(task_id)) => tracing::error!(
                target: "mcp::tools",
                panic = %payload,
                task_id = %task_id,
                "tool handler panicked; returning an error result"
            ),
            (None, Some(payload), None) => tracing::error!(
                target: "mcp::tools",
                panic = %payload,
                "tool handler panicked; returning an error result"
            ),
            (None, None, Some(task_id)) => tracing::error!(
                target: "mcp::tools",
                task_id = %task_id,
                "tool handler panicked; returning an error result"
            ),
            (None, None, None) => tracing::error!(
                target: "mcp::tools",
                "tool handler panicked; returning an error result"
            ),
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
                        Some(self.generate_instructions(config, &extensions))
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
                        Some(self.generate_instructions(config, &extensions))
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
                let filter_context =
                    self.capability_filter_context(&extensions, CapabilityOperation::List);
                let filter = self.inner.tool_filter.as_ref();
                let disabled = self.inner.disabled_tools.read().unwrap().clone();
                let is_visible = |t: &Tool| {
                    !disabled.contains(&t.name)
                        && !(final_protocol
                            && matches!(t.task_support, TaskSupportMode::Required)
                            && !final_tasks_negotiated)
                        && filter
                            .map(|f| f.is_visible_with_context(&filter_context, t))
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
                let filter_context = self.capability_filter_context(
                    &extensions,
                    CapabilityOperation::Access {
                        target: &params.name,
                    },
                );
                if let Some(filter) = &self.inner.tool_filter
                    && !filter.is_visible_with_context(&filter_context, &tool)
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
                            // A live task is never replayed, so its arguments
                            // are not needed and are deliberately not
                            // persisted. That is how a server keeps prompts or
                            // credentials out of durable task storage (#1246).
                            if tool.live_handler.is_some() {
                                serde_json::Value::Null
                            } else {
                                params.arguments.clone()
                            },
                            task_ttl,
                            request_principal(&extensions),
                        )
                        .await
                        .map_err(|error| {
                            self.task_store_error(TaskOperation::Create, None, error)
                        })?;

                    tracing::info!(task_id = %task_id, tool = %params.name, "Created async task");

                    // Create a context for the async task execution
                    let progress_token = params.meta.and_then(|m| m.progress_token);
                    let ctx = self.create_context_with_extensions(
                        request_id,
                        progress_token,
                        &extensions,
                    );

                    let task_store = self.inner.task_store.clone();
                    let task_context = crate::tool::TaskContext::new(task_id.clone());
                    let mut ctx = ctx;
                    ctx.extensions_mut().insert(task_context.clone());
                    let preparation = match tool
                        .prepare_task(task_context, params.arguments.clone())
                        .await
                    {
                        Ok(preparation) => preparation,
                        Err(error) => {
                            discard_unprepared_task(&task_store, &task_id).await;
                            return Err(error);
                        }
                    };
                    if let Some(meta) = preparation.meta {
                        let value = serde_json::Value::Object(meta);
                        if let Err(error) = crate::protocol::validate_meta_object(&value) {
                            discard_unprepared_task(&task_store, &task_id).await;
                            return Err(Error::invalid_params(format!(
                                "Invalid task metadata: {error}"
                            )));
                        }
                        let persisted = match task_store.set_task_meta(&task_id, value).await {
                            Ok(persisted) => persisted,
                            Err(error) => {
                                discard_unprepared_task(&task_store, &task_id).await;
                                return Err(self.task_store_error(
                                    TaskOperation::Create,
                                    Some(&task_id),
                                    error,
                                ));
                            }
                        };
                        if !persisted {
                            discard_unprepared_task(&task_store, &task_id).await;
                            return Err(self.task_error(
                                TaskOperation::Create,
                                Some(&task_id),
                                TaskFailure::Internal(
                                    "Task store could not persist preparation metadata",
                                ),
                            ));
                        }
                    }
                    ctx.extensions_mut().merge(&preparation.extensions);

                    // Spawn the task execution in the background
                    let tool = tool.clone();
                    let arguments = params.arguments;
                    let task_id_clone = task_id.clone();

                    let tool_name = params.name.clone();
                    let notifier = self.clone();
                    tokio::spawn(async move {
                        // A live handler owns its execution: it parks inside
                        // its own future rather than returning, so it is never
                        // replayed and nothing else writes its terminal state
                        // (#1246).
                        if let Some(live_handler) = tool.live_handler.clone() {
                            let handle = std::sync::Arc::new(crate::tool::LiveTask {
                                store: task_store.clone(),
                                error_policy: notifier.inner.task_error_policy.clone(),
                                input_ready: tokio::sync::Notify::new(),
                                cancelled: crate::context::CancellationToken::new(),
                            });
                            // Register before inspecting the store token, not
                            // after. A `tasks/cancel` landing between the two
                            // used to find no live handle, take the store path,
                            // terminalize, and acknowledge, after which the
                            // handle was registered uncancelled and the handler
                            // ran on against an already-cancelled task (#1294).
                            //
                            // In this order a cancel before registration is
                            // caught by the check below, and one after it
                            // signals the handle directly. There is no ordering
                            // left where cancellation selects the store path
                            // while live execution is running and cannot see it.
                            notifier.register_live_task(&task_id_clone, handle.clone());
                            // Released on drop, so the entry goes whether the
                            // handler returns, panics, or is dropped (#1305).
                            let registration = LiveTaskRegistration {
                                router: notifier.clone(),
                                task_id: task_id_clone.clone(),
                            };
                            if cancellation_token.is_cancelled() {
                                handle.cancelled.cancel();
                            }
                            let live_ctx =
                                crate::tool::TaskContext::with_live(task_id_clone.clone(), handle);

                            let start = std::time::Instant::now();
                            // The replay paths get their panic boundary from
                            // `invoke_tool`; the live branch calls the handler
                            // directly and had none, so a panic unwound before
                            // any terminal state was written and left the task
                            // at `working` forever (#1305).
                            let outcome = if let Some(policy) = &notifier.inner.panic_policy {
                                use futures::FutureExt;
                                let called = std::panic::AssertUnwindSafe(async move {
                                    live_handler.call(ctx, live_ctx, arguments).await
                                })
                                .catch_unwind()
                                .await;
                                match called {
                                    Ok(outcome) => outcome,
                                    Err(payload) => {
                                        let message = notifier.handle_caught_panic(
                                            policy,
                                            &tool_name,
                                            Some(&task_id_clone),
                                            &*payload,
                                        );
                                        // A panic is an execution failure, not
                                        // a tool reporting a domain error, so
                                        // it fails the task rather than
                                        // completing it with `isError`.
                                        Ok(crate::tool::TaskOutcome::Failed(
                                            JsonRpcError::internal_error(message),
                                        ))
                                    }
                                }
                            } else {
                                live_handler.call(ctx, live_ctx, arguments).await
                            };
                            let duration_ms = start.elapsed().as_secs_f64() * 1000.0;

                            let applied = match outcome {
                                Ok(crate::tool::TaskOutcome::Completed(result)) => notifier
                                    .complete_task_or_fail(&task_id_clone, result)
                                    .await
                                    .then_some("completed"),
                                Ok(crate::tool::TaskOutcome::Failed(error)) => notifier
                                    .record_task_failure(&task_id_clone, error)
                                    .await
                                    .then_some("failed"),
                                Ok(crate::tool::TaskOutcome::Cancelled { message }) => notifier
                                    .record_task_cancellation(&task_id_clone, message.as_deref())
                                    .await
                                    .then_some("cancelled"),
                                // Propagating the cancellation error is the
                                // ordinary way a live handler unwinds, so it
                                // ends the task cancelled rather than failed.
                                Err(crate::error::Error::TaskCancelled) => notifier
                                    .record_task_cancellation(
                                        &task_id_clone,
                                        Some("handler observed cancellation"),
                                    )
                                    .await
                                    .then_some("cancelled"),
                                // An unclassified error is an execution
                                // failure the handler declined to describe.
                                Err(Error::JsonRpc(error)) => notifier
                                    .record_task_failure(&task_id_clone, error)
                                    .await
                                    .then_some("failed"),
                                Err(_error) => {
                                    tracing::warn!(
                                        task_id = %task_id_clone,
                                        "live task handler returned an unclassified error"
                                    );
                                    let error = notifier.task_json_rpc_error(
                                        TaskOperation::Execute,
                                        Some(&task_id_clone),
                                        TaskFailure::Handler,
                                    );
                                    notifier
                                        .record_task_failure(&task_id_clone, error)
                                        .await
                                        .then_some("failed")
                                }
                            };
                            // The terminal write must win before unregistering
                            // (#1294), but the dead handle must not remain
                            // visible through logging or notification awaits.
                            // If the write failed, a later cancellation can
                            // now take the store path instead of signalling a
                            // handler that has already returned (#1305).
                            drop(registration);

                            match applied {
                                Some(status) => tracing::info!(
                                    target: "mcp::tools",
                                    tool = %tool_name,
                                    task_id = %task_id_clone,
                                    duration_ms,
                                    status,
                                    "live task finished"
                                ),
                                None => tracing::warn!(
                                    task_id = %task_id_clone,
                                    "failed to record live task outcome"
                                ),
                            }
                            notifier.notify_task_state(&task_id_clone).await;
                            return;
                        }

                        // Check for cancellation before starting
                        if cancellation_token.is_cancelled() {
                            tracing::debug!(task_id = %task_id_clone, "Task cancelled before execution");
                            notifier.notify_task_state(&task_id_clone).await;
                            return;
                        }

                        // Execute the tool.
                        //
                        // The outcome-aware call preserves an input-required
                        // return, which parks the task until the client
                        // answers with `tasks/update` and the router resumes
                        // it (#1208).
                        let start = std::time::Instant::now();
                        let outcome = notifier
                            .invoke_tool(&tool, ctx, arguments, &tool_name)
                            .await;
                        let duration_ms = start.elapsed().as_secs_f64() * 1000.0;

                        let result = match outcome {
                            Ok(crate::protocol::RequestOutcome::Complete(result)) => result,
                            Ok(crate::protocol::RequestOutcome::InputRequired(input_required)) => {
                                notifier
                                    .park_task_for_input(&task_id_clone, input_required)
                                    .await;
                                return;
                            }
                            // Preserved from the previous call path: a handler
                            // error becomes an `isError` result, which
                            // completes the task rather than failing it.
                            Err(error) => CallToolResult::error(error.to_string()),
                        };

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
                            if notifier.complete_task_or_fail(&task_id_clone, result).await {
                                tracing::info!(
                                    target: "mcp::tools",
                                    tool = %tool_name,
                                    task_id = %task_id_clone,
                                    duration_ms,
                                    status,
                                    error = error_msg.as_deref().unwrap_or_default(),
                                    "tool call completed"
                                );
                            }
                            notifier.notify_task_state(&task_id_clone).await;
                        }
                    });

                    let task = self
                        .inner
                        .task_store
                        .get_task(&task_id)
                        .await
                        .map_err(|error| {
                            self.task_store_error(TaskOperation::Create, Some(&task_id), error)
                        })?
                        .ok_or_else(|| {
                            self.task_error(
                                TaskOperation::Create,
                                Some(&task_id),
                                TaskFailure::Internal("Failed to retrieve created task"),
                            )
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
                        let mut result = crate::tasks::CreateTaskResult::new(
                            crate::tasks::Task::new(metadata, task.status),
                        );
                        result.meta = task.meta.and_then(|value| value.as_object().cloned());
                        return Ok(McpResponse::FinalCreateTask(result));
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
                    let outcome = self
                        .invoke_tool(&tool, ctx, params.arguments, &params.name)
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
                let filter_context =
                    self.capability_filter_context(&extensions, CapabilityOperation::List);
                let disabled = self.inner.disabled_resources.read().unwrap().clone();
                let is_visible = |r: &Resource| -> bool {
                    !disabled.contains(&r.uri)
                        && self
                            .inner
                            .resource_filter
                            .as_ref()
                            .map(|f| f.is_visible_with_context(&filter_context, r))
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
                let filter_context =
                    self.capability_filter_context(&extensions, CapabilityOperation::List);
                #[cfg(feature = "dynamic-tools")]
                let static_patterns: HashSet<String> = self
                    .inner
                    .resource_templates
                    .iter()
                    .map(|template| template.uri_template.clone())
                    .collect();
                let mut resource_templates: Vec<ResourceTemplateDefinition> = self
                    .inner
                    .resource_templates
                    .iter()
                    .filter(|template| self.resource_template_is_visible(&filter_context, template))
                    .map(|t| t.definition())
                    .collect();

                // Resolve static/dynamic precedence before filtering. A hidden
                // static template still shadows a dynamic template with the
                // same pattern, so policy denial cannot reveal a fallback.
                #[cfg(feature = "dynamic-tools")]
                if let Some(ref dynamic) = self.inner.dynamic_resource_templates {
                    for t in dynamic.list() {
                        if !static_patterns.contains(&t.uri_template)
                            && self.resource_template_is_visible(&filter_context, &t)
                        {
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
                let filter_context = self.capability_filter_context(
                    &extensions,
                    CapabilityOperation::Access {
                        target: &params.uri,
                    },
                );

                // First, try to find a static resource
                if let Some(resource) = self.inner.resources.get(&params.uri) {
                    // Check resource filter if configured
                    if let Some(filter) = &self.inner.resource_filter
                        && !filter.is_visible_with_context(&filter_context, resource)
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
                            && !filter.is_visible_with_context(&filter_context, &resource)
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
                        self.authorize_resource_template(&filter_context, template, &params.uri)?;
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
                        self.authorize_resource_template(&filter_context, &template, &params.uri)?;
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
                self.authorize_static_resource_subscription(&extensions, &params.uri)?;

                tracing::debug!(uri = %params.uri, "Subscribing to resource");
                self.subscribe(&params.uri);

                Ok(McpResponse::SubscribeResource(EmptyResult {}))
            }

            McpRequest::UnsubscribeResource(params) => {
                // Authorize before consulting or mutating membership. Otherwise
                // success for an existing subscription and denial for an
                // unowned URI would disclose session state for hidden resources.
                self.authorize_static_resource_subscription(&extensions, &params.uri)?;
                self.unsubscribe(&params.uri);

                tracing::debug!(uri = %params.uri, "Unsubscribing from resource");

                Ok(McpResponse::UnsubscribeResource(EmptyResult {}))
            }

            McpRequest::ListPrompts(params) => {
                #[cfg(feature = "dynamic-tools")]
                if let Some(initializer) = &self.inner.prompt_initializer {
                    initializer()?;
                }
                let filter_context =
                    self.capability_filter_context(&extensions, CapabilityOperation::List);
                let disabled = self.inner.disabled_prompts.read().unwrap().clone();
                let is_visible = |p: &Prompt| -> bool {
                    !disabled.contains(&p.name)
                        && self
                            .inner
                            .prompt_filter
                            .as_ref()
                            .map(|f| f.is_visible_with_context(&filter_context, p))
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
                #[cfg(feature = "dynamic-tools")]
                if let Some(initializer) = &self.inner.prompt_initializer {
                    initializer()?;
                }
                // Disabled prompts are reported as if they don't exist.
                if self
                    .inner
                    .disabled_prompts
                    .read()
                    .unwrap()
                    .contains(&params.name)
                {
                    return Err(prompt_not_found(&params.name));
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
                let prompt = prompt.ok_or_else(|| prompt_not_found(&params.name))?;

                // Check prompt filter if configured
                let filter_context = self.capability_filter_context(
                    &extensions,
                    CapabilityOperation::Access {
                        target: &params.name,
                    },
                );
                if let Some(filter) = &self.inner.prompt_filter
                    && !filter.is_visible_with_context(&filter_context, &prompt)
                {
                    return Err(filter.denial_error(&params.name));
                }

                // Before dispatch, so every path shares one check: layered and
                // unlayered, ordinary and MRTR. A handler never sees a request
                // missing an argument it declared required (#1281).
                let missing =
                    crate::prompt::missing_required_arguments(&prompt.arguments, &params.arguments);
                if !missing.is_empty() {
                    return Err(Error::JsonRpc(crate::prompt::missing_arguments_error(
                        &params.name,
                        &missing,
                    )));
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
                    self.authorize_task(TaskOperation::Get, &params.task_id, &extensions)
                        .await?;
                    return self.final_get_task(&params.task_id, &extensions).await;
                }
                self.authorize_task(TaskOperation::Get, &params.task_id, &extensions)
                    .await?;

                // SEP-2663 DetailedTask: `tasks/get` carries the
                // status-discriminated payload inline. `completed` includes
                // the result the synchronous request would have returned;
                // `failed` includes the JSON-RPC error. This replaced the
                // removed blocking `tasks/result` method as the way clients
                // retrieve a task's outcome.
                let Some((mut task, result, error)) = self
                    .inner
                    .task_store
                    .get_task_result(&params.task_id)
                    .await
                    .map_err(|error| {
                        self.task_store_error(TaskOperation::Get, Some(&params.task_id), error)
                    })?
                else {
                    // Present when it was authorized, absent now, so it
                    // expired in between (#1249).
                    return Err(self
                        .classify_absent_task(TaskOperation::Get, &params.task_id, &extensions)
                        .await);
                };

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
                    self.authorize_task(TaskOperation::Update, &params.task_id, &extensions)
                        .await?;
                    // Partial responses are the normal case: the store
                    // consumes what matches an outstanding request and ignores
                    // unknown, already-answered, and superseded keys.
                    let Some(applied) = self
                        .inner
                        .task_store
                        .apply_input_responses(
                            &params.task_id,
                            decode_input_responses(self, &params.task_id, &params.input_responses)?,
                        )
                        .await
                        .map_err(|error| {
                            self.task_store_error(
                                TaskOperation::Update,
                                Some(&params.task_id),
                                error,
                            )
                        })?
                    else {
                        // Nothing left to apply. A task the store still knows
                        // and has not expired is a late or duplicate update,
                        // so it gets the ordinary empty acknowledgement, which
                        // makes a client retry idempotent (#1249).
                        let presence = self
                            .task_presence(TaskOperation::Update, &params.task_id)
                            .await?;
                        return match presence {
                            crate::async_task::TaskPresence::Present { .. } => Ok(
                                McpResponse::FinalTaskAck(crate::tasks::TaskAcknowledgement::new()),
                            ),
                            absent => Err(self.classify_absent_presence(
                                TaskOperation::Update,
                                &params.task_id,
                                &extensions,
                                absent,
                            )),
                        };
                    };
                    // Answering the last outstanding request resumes the task,
                    // so the status a subscriber sees changes here even though
                    // the ack itself is empty.
                    self.notify_task_state(&params.task_id).await;
                    // The client answered everything outstanding, so re-invoke
                    // the handler with the accumulated responses (#1208). A
                    // partial answer leaves the task parked for the rest.
                    //
                    // `is_complete` is also true when nothing was outstanding
                    // in the first place, so on its own it would resume a task
                    // that never parked. Requiring this update to have answered
                    // something is what distinguishes a real
                    // `input_required -> working` transition from a stray,
                    // duplicate, or already-satisfied update, either of which
                    // would otherwise start a second handler alongside the one
                    // still running (#1246).
                    if !applied.accepted.is_empty() && applied.is_complete() {
                        // A live handler is parked inside its own future and
                        // must be woken, not replayed. Waking only after the
                        // store has committed is what guarantees it cannot
                        // observe an answer that was not recorded (#1246).
                        if !self.wake_live_task(&params.task_id) {
                            self.resume_task(&params.task_id).await;
                        }
                    }
                    return Ok(McpResponse::FinalTaskAck(
                        crate::tasks::TaskAcknowledgement::new(),
                    ));
                }

                self.authorize_task(TaskOperation::Update, &params.task_id, &extensions)
                    .await?;

                // Input responses reach the store on this path exactly as they
                // do on the final path above. The spec allowance for ignoring
                // `inputResponses` covers keys that are not outstanding, not
                // every key, so dropping them wholesale left a server whose
                // store models input requests with a working flow on
                // 2026-07-28 and a silent stall on 2025-11-25 (#1188).
                let Some(applied) = self
                    .inner
                    .task_store
                    .apply_input_responses(
                        &params.task_id,
                        decode_input_responses(self, &params.task_id, &params.input_responses)?,
                    )
                    .await
                    .map_err(|error| {
                        self.task_store_error(TaskOperation::Update, Some(&params.task_id), error)
                    })?
                else {
                    // Nothing left to apply. A task the store still knows and
                    // has not expired is a late or duplicate update, so it
                    // gets the ordinary empty acknowledgement, which makes a
                    // client retry idempotent rather than a not-found (#1249).
                    let presence = self
                        .task_presence(TaskOperation::Update, &params.task_id)
                        .await?;
                    return match presence {
                        crate::async_task::TaskPresence::Present { .. } => {
                            Ok(McpResponse::UpdateTask(EmptyResult {}))
                        }
                        absent => Err(self.classify_absent_presence(
                            TaskOperation::Update,
                            &params.task_id,
                            &extensions,
                            absent,
                        )),
                    };
                };
                // A final-protocol subscriber watching this task should see it
                // resume regardless of which lifecycle the updating client
                // used. Self-guards when the extension is not enabled.
                self.notify_task_state(&params.task_id).await;
                // A live task parks inside its own future, so answering its
                // input on this lifecycle has to wake it just as the final
                // path does, or it waits forever (#1246). Waking only after
                // the store has committed is what guarantees the handler
                // cannot observe an unrecorded answer.
                if !applied.accepted.is_empty() && applied.is_complete() {
                    self.wake_live_task(&params.task_id);
                }
                Ok(McpResponse::UpdateTask(EmptyResult {}))
            }

            McpRequest::CancelTask(params) => {
                if is_final_protocol_request(&extensions) {
                    self.require_negotiated_tasks(&extensions, "tasks/cancel")?;
                    self.authorize_task(TaskOperation::Cancel, &params.task_id, &extensions)
                        .await?;
                    // A live task is signalled and left non-terminal: its
                    // handler owns the teardown and reports when it actually
                    // stopped, so completion can still legitimately win the
                    // race. SEP-2663 describes cancellation as eventually
                    // consistent, which is exactly this (#1246).
                    if self.signal_live_cancellation(&params.task_id) {
                        self.notify_task_state(&params.task_id).await;
                        return Ok(McpResponse::FinalTaskAck(
                            crate::tasks::TaskAcknowledgement::new(),
                        ));
                    }
                    // The final ack does not require a terminal transition:
                    // cancelling an already-terminal task is acknowledged, and
                    // the observable status is polled via `tasks/get`.
                    let cancelled = self
                        .inner
                        .task_store
                        .cancel_task(&params.task_id, params.reason.as_deref())
                        .await
                        .map_err(|error| {
                            self.task_store_error(
                                TaskOperation::Cancel,
                                Some(&params.task_id),
                                error,
                            )
                        })?;
                    if cancelled.is_none() {
                        return Err(self
                            .classify_absent_task(
                                TaskOperation::Cancel,
                                &params.task_id,
                                &extensions,
                            )
                            .await);
                    }
                    self.notify_task_state(&params.task_id).await;
                    return Ok(McpResponse::FinalTaskAck(
                        crate::tasks::TaskAcknowledgement::new(),
                    ));
                }

                self.authorize_task(TaskOperation::Cancel, &params.task_id, &extensions)
                    .await?;

                // Same reasoning as the final path: a live task owns its own
                // teardown, so it is signalled and left non-terminal (#1246).
                if self.signal_live_cancellation(&params.task_id) {
                    self.notify_task_state(&params.task_id).await;
                    return Ok(McpResponse::CancelTask(EmptyResult {}));
                }

                // First check if the task exists and is not already terminal
                let Some(current) = self
                    .inner
                    .task_store
                    .get_task(&params.task_id)
                    .await
                    .map_err(|error| {
                        self.task_store_error(TaskOperation::Cancel, Some(&params.task_id), error)
                    })?
                else {
                    return Err(self
                        .classify_absent_task(TaskOperation::Cancel, &params.task_id, &extensions)
                        .await);
                };

                if current.status.is_terminal() {
                    return Err(Error::JsonRpc(JsonRpcError::invalid_params(format!(
                        "Task {} is already in terminal state: {}",
                        params.task_id, current.status
                    ))));
                }

                let cancelled = self
                    .inner
                    .task_store
                    .cancel_task(&params.task_id, params.reason.as_deref())
                    .await
                    .map_err(|error| {
                        self.task_store_error(TaskOperation::Cancel, Some(&params.task_id), error)
                    })?;
                if cancelled.is_none() {
                    return Err(self
                        .classify_absent_task(TaskOperation::Cancel, &params.task_id, &extensions)
                        .await);
                }

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
                    self.authorize_completion_reference(&extensions, &params.reference)?;
                    let progress_token = params
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.get("progressToken"))
                        .map(|token| serde_json::from_value(token.clone()))
                        .transpose()
                        .map_err(|error| {
                            Error::invalid_params(format!(
                                "Invalid completion progress token: {error}"
                            ))
                        })?;
                    let ctx = self.create_context_with_extensions(
                        request_id,
                        progress_token,
                        &extensions,
                    );
                    let result = handler(ctx, params).await?;
                    Ok(McpResponse::Complete(result))
                } else {
                    // No completion handler registered, return empty completions
                    Ok(McpResponse::Complete(CompleteResult::new(vec![])))
                }
            }

            #[cfg(feature = "stateless")]
            McpRequest::SubscriptionsListen(params) => {
                // The stream itself is transport-owned: transports dispatch
                // the request here before upgrading the connection, so
                // `Service<RouterRequest>` middleware observes accepted and
                // rejected listens and the validation lives in one place
                // (#1182). The response is consumed by the transport, never
                // written to the wire.
                if !is_final_protocol_request(&extensions) {
                    // A legacy peer gets exactly what the old catch-all
                    // produced for this method.
                    return Err(Error::JsonRpc(JsonRpcError::method_not_found(
                        "subscriptions/listen",
                    )));
                }
                let Some(requested) = params.notifications else {
                    return Err(Error::JsonRpc(JsonRpcError::invalid_params(
                        "subscriptions/listen requires a notifications filter",
                    )));
                };
                // SEP-2663: task status notifications require the declared
                // extension, the same answer the three task methods give.
                if requested.task_ids.is_some() && !client_declares_tasks(&extensions) {
                    return Err(Error::JsonRpc(
                        JsonRpcError::missing_required_client_capability(
                            tasks_client_capabilities(),
                        ),
                    ));
                }
                if self.final_tasks_enabled()
                    && let Some(task_ids) = requested.task_ids.as_deref()
                {
                    for task_id in task_ids {
                        self.authorize_task_subscription(task_id, &extensions)
                            .await?;
                    }
                }
                let notifications = crate::transport::subscriptions::accepted_subscription_filter(
                    requested,
                    self.final_tasks_enabled(),
                );
                Ok(McpResponse::SubscriptionsAccepted(
                    crate::protocol::SubscriptionsAcceptedResult { notifications },
                ))
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
                } else if phase_before == crate::session::SessionPhase::Uninitialized {
                    tracing::warn!(
                        "Ignoring initialized notification: no initialize request has been \
                         received for this session"
                    );
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

/// Identifies one dispatch of one request, unique for a router's lifetime.
///
/// The request id cannot play this role: a client may reuse one that is still
/// in flight, and two requests sharing an id must still be tracked separately
/// (#1270). Minted by [`Service::call`] and passed to the handler through the
/// request extensions so the registration and the guard name the same entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DispatchId(u64);

/// One tracked dispatch in the router's in-flight registry.
struct InFlightDispatch {
    dispatch: DispatchId,
    token: CancellationToken,
}

/// Untracks a single dispatch when the request future ends, however it ends.
///
/// Removal used to sit on the success path in [`Service::call`], so a future
/// dropped before that point (a timeout layer firing, an HTTP client
/// disconnecting, a handler unwinding) left its entry in the registry for the
/// process lifetime. `Drop` runs on every one of those paths.
struct InFlightGuard {
    router: McpRouter,
    request_id: RequestId,
    dispatch: DispatchId,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.router
            .complete_dispatch(&self.request_id, self.dispatch);
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

    fn call(&mut self, mut req: RouterRequest) -> Self::Future {
        let router = self.clone();
        let request_id = req.id.clone();

        // Name the dispatch before `handle` builds its context, so the
        // registration inside and the guard out here refer to the same entry.
        let dispatch = router.next_dispatch();
        req.extensions.insert(dispatch);

        Box::pin(async move {
            let _tracked = InFlightGuard {
                router: router.clone(),
                request_id: request_id.clone(),
                dispatch,
            };
            let result = router.handle(req.id, req.inner, req.extensions).await;
            Ok(RouterResponse {
                id: request_id,
                // Map tower-mcp errors to JSON-RPC errors: a structured
                // Error::JsonRpc is forwarded as-is (preserves the original
                // code and message); everything else is sanitized to
                // -32603 (Internal Error). See Error::into_json_rpc_error.
                inner: result.map_err(Error::into_json_rpc_error),
            })
        })
    }
}

mod builder;
mod capabilities;
mod merge;
mod notify;
mod pagination;
mod policy;
mod task_ops;

use capabilities::{final_client_capabilities, request_principal};
use pagination::paginate;
use policy::panic_message;

// Gated in `capabilities` too, for the same reason `task_ops` gates below: an
// unconditional import here breaks every build that is not `--all-features`.
#[cfg(feature = "stateless")]
use capabilities::client_capabilities_satisfy;

// The cursor tests are siblings and reach these through `super`, but nothing in
// this module calls them directly now that `paginate` owns the encoding.
#[cfg(test)]
use pagination::{decode_cursor, encode_cursor};

// These are named in `lib.rs`'s re-export list, so they keep the `router::`
// path they have always had rather than gaining a submodule in it.
pub use merge::{MergeConflict, MergeConflictKind, MergeConflicts};
pub use policy::{PanicPolicy, TaskErrorContext, TaskErrorPolicy, TaskFailure, TaskOperation};

use task_ops::{
    client_declares_tasks, decode_input_responses, discard_unprepared_task,
    tasks_client_capabilities,
};
// Gated in `task_ops` too, so importing it unconditionally breaks the default
// build that `--all-features` never exercises.
#[cfg(feature = "stateless")]
use task_ops::validate_input_required_result;

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "stateless"))]
mod task_error_tests;

#[cfg(test)]
mod cursor_property_tests;
