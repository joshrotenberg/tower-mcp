//! MCP Router - routes requests to tools, resources, and prompts
//!
//! The router implements Tower's `Service` trait, making it composable with
//! standard tower middleware.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
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

fn task_store_error(e: TaskStoreError) -> Error {
    Error::JsonRpc(JsonRpcError::internal_error(format!(
        "Task store error: {}",
        e
    )))
}

async fn discard_unprepared_task(store: &Arc<dyn TaskStore>, task_id: &str) {
    if !matches!(store.discard_task(task_id).await, Ok(true)) {
        let _ = store
            .cancel_task(task_id, Some("task preparation failed"))
            .await;
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

/// Recover a readable message from a panic payload.
///
/// `panic!` with a literal yields `&str` and with a format yields `String`;
/// anything else is opaque and reported as such rather than guessed at.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "panicked with a non-string payload".to_string()
    }
}

#[derive(Clone)]
enum ClientPanicMessage {
    Detailed,
    Fixed(Arc<str>),
}

#[derive(Clone)]
enum ToolNameDisclosure {
    Omit,
    Original,
    Fixed(Arc<str>),
}

impl ToolNameDisclosure {
    fn value<'a>(&'a self, original: &'a str) -> Option<&'a str> {
        match self {
            Self::Omit => None,
            Self::Original => Some(original),
            Self::Fixed(name) => Some(name),
        }
    }

    fn mode(&self) -> &'static str {
        match self {
            Self::Omit => "omitted",
            Self::Original => "original",
            Self::Fixed(_) => "fixed",
        }
    }
}

/// Controls what Tower discloses after isolating a panicking tool handler.
///
/// Construct a redacted policy with [`PanicPolicy::redacted`], then opt in to
/// individual disclosures only when they are safe for the application. Panic
/// payloads are never included in a custom policy's client response.
///
/// Rust's process-global panic hook runs before Tower catches an unwind. This
/// policy governs only Tower's client response and Tower-generated tracing
/// event; it cannot redact output produced by an application-installed panic
/// hook or by Rust's default panic hook.
#[derive(Clone)]
pub struct PanicPolicy {
    client_message: ClientPanicMessage,
    client_tool_name: ToolNameDisclosure,
    log_tool_name: ToolNameDisclosure,
    include_payload_in_logs: bool,
}

impl std::fmt::Debug for PanicPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let client_message = match self.client_message {
            ClientPanicMessage::Detailed => "detailed",
            ClientPanicMessage::Fixed(_) => "fixed",
        };
        f.debug_struct("PanicPolicy")
            .field("client_message", &client_message)
            .field("client_tool_name", &self.client_tool_name.mode())
            .field("log_tool_name", &self.log_tool_name.mode())
            .field("include_payload_in_logs", &self.include_payload_in_logs)
            .finish()
    }
}

impl PanicPolicy {
    /// Create a policy whose client response is fixed application-supplied
    /// text and whose Tower tracing event contains neither the tool name nor
    /// the panic payload.
    pub fn redacted(client_message: impl Into<String>) -> Self {
        Self {
            client_message: ClientPanicMessage::Fixed(Arc::from(client_message.into())),
            client_tool_name: ToolNameDisclosure::Omit,
            log_tool_name: ToolNameDisclosure::Omit,
            include_payload_in_logs: false,
        }
    }

    fn detailed() -> Self {
        Self {
            client_message: ClientPanicMessage::Detailed,
            client_tool_name: ToolNameDisclosure::Original,
            log_tool_name: ToolNameDisclosure::Original,
            include_payload_in_logs: true,
        }
    }

    /// Include the registered tool name in the client-visible error.
    ///
    /// With a redacted policy this changes the response from the exact fixed
    /// message to `tool '<name>': <fixed message>`.
    #[must_use]
    pub fn include_tool_name_in_client_message(mut self, include: bool) -> Self {
        self.client_tool_name = if include {
            ToolNameDisclosure::Original
        } else {
            ToolNameDisclosure::Omit
        };
        self
    }

    /// Replace the registered tool name in the client-visible error with a
    /// fixed application-selected label.
    ///
    /// This is useful when the original catalog name is sensitive but a
    /// stable category such as `provider tool` is still useful to callers.
    #[must_use]
    pub fn client_tool_name(mut self, name: impl Into<String>) -> Self {
        self.client_tool_name = ToolNameDisclosure::Fixed(Arc::from(name.into()));
        self
    }

    /// Include the registered tool name in Tower's panic tracing event.
    #[must_use]
    pub fn include_tool_name_in_logs(mut self, include: bool) -> Self {
        self.log_tool_name = if include {
            ToolNameDisclosure::Original
        } else {
            ToolNameDisclosure::Omit
        };
        self
    }

    /// Replace the registered tool name in Tower's panic tracing event with
    /// a fixed application-selected label.
    #[must_use]
    pub fn log_tool_name(mut self, name: impl Into<String>) -> Self {
        self.log_tool_name = ToolNameDisclosure::Fixed(Arc::from(name.into()));
        self
    }

    /// Include the recovered panic payload in Tower's panic tracing event.
    ///
    /// This switch never changes the client-visible error.
    #[must_use]
    pub fn include_payload_in_logs(mut self, include: bool) -> Self {
        self.include_payload_in_logs = include;
        self
    }

    fn client_message(&self, tool_name: &str, payload: Option<&str>) -> String {
        match &self.client_message {
            ClientPanicMessage::Detailed => format!(
                "tool '{tool_name}' panicked: {}",
                payload.unwrap_or("<redacted>")
            ),
            ClientPanicMessage::Fixed(message) => match self.client_tool_name.value(tool_name) {
                Some(name) => format!("tool '{name}': {message}"),
                None => message.to_string(),
            },
        }
    }

    fn needs_payload(&self) -> bool {
        matches!(self.client_message, ClientPanicMessage::Detailed) || self.include_payload_in_logs
    }
}

/// The kind of capability a [`MergeConflict`] refers to.
///
/// Ordered so that [`McpRouter::conflicts`] reports tools before resources
/// before prompts, which reads more naturally than alphabetical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MergeConflictKind {
    /// A tool name defined by both routers.
    Tool,
    /// A resource URI defined by both routers.
    Resource,
    /// A resource template pattern defined by both routers.
    ResourceTemplate,
    /// A prompt name defined by both routers.
    Prompt,
}

impl MergeConflictKind {
    /// The name of this kind as it appears in a conflict message.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Resource => "resource",
            Self::ResourceTemplate => "resource template",
            Self::Prompt => "prompt",
        }
    }
}

impl std::fmt::Display for MergeConflictKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One capability defined by both routers in a [`McpRouter::try_merge`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MergeConflict {
    /// Which kind of capability collided.
    pub kind: MergeConflictKind,
    /// The tool or prompt name, or the resource URI or template pattern.
    pub name: String,
}

impl MergeConflict {
    fn new(kind: MergeConflictKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
        }
    }
}

impl std::fmt::Display for MergeConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} '{}'", self.kind, self.name)
    }
}

/// The error returned by [`McpRouter::try_merge`].
///
/// Carries every conflicting name rather than the first, so a startup check
/// reports all the work at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeConflicts {
    conflicts: Vec<MergeConflict>,
}

impl MergeConflicts {
    /// The conflicting capabilities, ordered by kind and then name.
    pub fn conflicts(&self) -> &[MergeConflict] {
        &self.conflicts
    }

    /// Take ownership of the conflicting capabilities.
    pub fn into_conflicts(self) -> Vec<MergeConflict> {
        self.conflicts
    }
}

impl std::fmt::Display for MergeConflicts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot merge routers: ")?;
        for (index, conflict) in self.conflicts.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{conflict}")?;
        }
        f.write_str(" defined by both")
    }
}

impl std::error::Error for MergeConflicts {}

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
/// The error an owner gets for a task the store still holds but whose TTL
/// elapsed.
///
/// Deliberately distinct from [`unknown_task_error`] in its `data`, and
/// deliberately only ever produced after the caller has been shown to own the
/// task. A caller who does not own it gets `unknown_task_error`, which is
/// also what a genuinely unknown id gets, so the two cannot be told apart
/// from outside (#1249).
fn expired_task_error(task_id: &str) -> JsonRpcError {
    let mut error = JsonRpcError::invalid_params(format!("Task expired: {task_id}"));
    error.data = Some(serde_json::json!({ "reason": "task_expired" }));
    error
}

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
    auto_instructions: Option<AutoInstructionsConfig>,
    tools: HashMap<String, Arc<Tool>>,
    resources: HashMap<String, Arc<Resource>>,
    /// Resource templates for dynamic resource matching (keyed by uri_template)
    resource_templates: Vec<Arc<ResourceTemplate>>,
    prompts: HashMap<String, Arc<Prompt>>,
    /// Whether to advertise `resources.subscribe`. Defaults to true, which
    /// is what this router has always advertised when resources exist (#1261).
    advertise_resource_subscriptions: bool,
    /// Live tasks currently running, keyed by task id (#1246).
    ///
    /// A live handler parks inside its own future rather than returning, so
    /// the router needs a handle to wake it when `tasks/update` commits and
    /// to signal it when `tasks/cancel` arrives.
    live_tasks: Arc<Mutex<HashMap<String, Arc<crate::tool::LiveTask>>>>,
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
    #[cfg(feature = "stateless")]
    subscription_observer:
        Arc<RwLock<Option<Arc<dyn crate::transport::subscriptions::SubscriptionObserver>>>>,
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
                panic_policy: None,
                auto_instructions: None,
                tools: HashMap::new(),
                resources: HashMap::new(),
                resource_templates: Vec::new(),
                prompts: HashMap::new(),
                advertise_resource_subscriptions: true,
                live_tasks: Arc::new(Mutex::new(HashMap::new())),
                in_flight: Arc::new(RwLock::new(HashMap::new())),
                notification_tx: None,
                #[cfg(all(feature = "http", feature = "stateless"))]
                modern_notification_sink: Arc::new(RwLock::new(None)),
                #[cfg(feature = "stateless")]
                subscription_observer: Arc::new(RwLock::new(None)),
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
                prompt_initializer: None,
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

    /// Whether to advertise `resources.subscribe` when resources exist.
    ///
    /// Defaults to `true`, which is what this router has always advertised as
    /// soon as any resource or template is registered. Pass `false` for a
    /// server that exposes read-only resources and no update stream, so it
    /// does not promise a subscription it will not honour (#1261).
    ///
    /// This affects advertisement only. `resources/subscribe` continues to be
    /// routed either way, so a client that ignores the capability and calls it
    /// anyway behaves as before.
    ///
    /// The 2026-07-28 revision has no `resources/subscribe` method at all, so
    /// the capability is never advertised on that lifecycle regardless of this
    /// setting.
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, ResourceBuilder};
    ///
    /// let router = McpRouter::new()
    ///     .server_info("read-only", "1.0.0")
    ///     .resource(ResourceBuilder::new("mem://one").name("one").text("hi"))
    ///     .resource_subscriptions(false);
    /// ```
    pub fn resource_subscriptions(mut self, advertise: bool) -> Self {
        Arc::make_mut(&mut self.inner).advertise_resource_subscriptions = advertise;
        self
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

    /// Convert a panicking tool handler into an error result instead of
    /// letting it unwind out of the service.
    ///
    /// Without this, a panic in one handler ends the whole server over stdio
    /// and kills the connection task over HTTP. A bug in one tool should fail
    /// that call, not disconnect every client on the process, which is what
    /// makes this worth having on a long-running shared server.
    ///
    /// Off by default, deliberately. A panic is an invariant violation, and
    /// converting one into a tidy error result hides a bug that the author
    /// probably wants to see. Opting in is a statement that availability
    /// matters more than failing fast, which is true for a shared server and
    /// often false for a local one.
    ///
    /// For ordinary and replayed handlers, the caught panic becomes a
    /// `CallToolResult` with `is_error: true` carrying the panic message. A
    /// live Task handler instead reaches `failed` with the same detailed
    /// message. Both are logged at error level with the tool name so the panic
    /// is not silently swallowed.
    ///
    /// A panic that unwinds is caught; one that aborts the process (a
    /// double panic, or `panic = "abort"`) cannot be, by construction.
    pub fn catch_panics(mut self) -> Self {
        Arc::make_mut(&mut self.inner).panic_policy = Some(PanicPolicy::detailed());
        self
    }

    /// Convert a panicking tool handler into an error result using an
    /// application-selected disclosure policy.
    ///
    /// [`PanicPolicy::redacted`] returns fixed client text and omits both the
    /// tool name and panic payload from Tower's tracing event by default.
    /// Unlike [`McpRouter::catch_panics`], a custom policy never includes the
    /// panic payload in the client response.
    ///
    /// The policy applies to ordinary calls and both replayed and live Task
    /// handlers registered on this router. Router-level configuration is not
    /// imported when another router is merged or nested, so the receiving
    /// router's policy governs the combined catalog.
    ///
    /// Rust's process-global panic hook runs before the unwind is caught, so
    /// this controls Tower's client response and tracing event only. It does
    /// not suppress application or default panic-hook output.
    ///
    /// A panic that aborts the process (a double panic, or
    /// `panic = "abort"`) cannot be caught.
    pub fn catch_panics_with(mut self, policy: PanicPolicy) -> Self {
        Arc::make_mut(&mut self.inner).panic_policy = Some(policy);
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

    /// Report the names both this router and `other` define.
    ///
    /// [`merge`](Self::merge) resolves a collision by letting the incoming
    /// router win, which is a reasonable default but leaves no trace that an
    /// implementation was dropped. A host that composes a router it does not
    /// own can call this first and fail at startup, which is the cheapest
    /// moment to catch the clash (#1232).
    ///
    /// Results are ordered by kind and then name, so they are stable enough
    /// to assert on and to print.
    ///
    /// Protocol extension declarations are deliberately excluded. Two routers
    /// both declaring the same extension is ordinary composition rather than
    /// a collision, since a declaration carries no implementation to lose.
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
    /// fn router_with(name: &str) -> McpRouter {
    ///     McpRouter::new().tool(
    ///         ToolBuilder::new(name)
    ///             .description("example")
    ///             .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///             .build(),
    ///     )
    /// }
    ///
    /// let host = router_with("get_task");
    /// let library = router_with("get_task");
    /// let clashes = host.conflicts(&library);
    /// assert_eq!(clashes.len(), 1);
    /// assert_eq!(clashes[0].name, "get_task");
    /// ```
    pub fn conflicts(&self, other: &McpRouter) -> Vec<MergeConflict> {
        let mut found = Vec::new();

        for name in other.inner.tools.keys() {
            if self.inner.tools.contains_key(name) {
                found.push(MergeConflict::new(MergeConflictKind::Tool, name));
            }
        }
        for uri in other.inner.resources.keys() {
            if self.inner.resources.contains_key(uri) {
                found.push(MergeConflict::new(MergeConflictKind::Resource, uri));
            }
        }
        // Templates are stored as a list rather than a map because matching
        // is pattern-based, so identity here is the template string itself.
        for template in &other.inner.resource_templates {
            if self
                .inner
                .resource_templates
                .iter()
                .any(|existing| existing.uri_template == template.uri_template)
            {
                found.push(MergeConflict::new(
                    MergeConflictKind::ResourceTemplate,
                    &template.uri_template,
                ));
            }
        }
        for name in other.inner.prompts.keys() {
            if self.inner.prompts.contains_key(name) {
                found.push(MergeConflict::new(MergeConflictKind::Prompt, name));
            }
        }

        // `tools`, `resources`, and `prompts` are hash maps, so without this
        // the order would vary between runs.
        found.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
        found
    }

    /// Merge another router, failing if either defines a name the other does.
    ///
    /// This is [`merge`](Self::merge) with the collision reported instead of
    /// resolved. Use it when a silently dropped tool would surface later as a
    /// capability that behaves unexpectedly rather than as an error, which is
    /// the usual case when a host merges in a router from a library that
    /// cannot know what the host already registered.
    ///
    /// Callers who want the incoming router to win keep using
    /// [`merge`](Self::merge). To inspect without consuming either router,
    /// use [`conflicts`](Self::conflicts).
    ///
    /// # Errors
    ///
    /// Returns every conflicting name, not just the first, so a startup
    /// failure names all the work to be done.
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
    /// fn router_with(name: &str) -> McpRouter {
    ///     McpRouter::new().tool(
    ///         ToolBuilder::new(name)
    ///             .description("example")
    ///             .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///             .build(),
    ///     )
    /// }
    ///
    /// // Distinct names merge.
    /// let combined = router_with("query").try_merge(router_with("fetch"));
    /// assert!(combined.is_ok());
    ///
    /// // A shared name is reported rather than dropped.
    /// let clash = router_with("get_task").try_merge(router_with("get_task"));
    /// let error = clash.unwrap_err();
    /// assert_eq!(error.conflicts().len(), 1);
    /// ```
    pub fn try_merge(self, other: McpRouter) -> std::result::Result<Self, MergeConflicts> {
        let conflicts = self.conflicts(&other);
        if conflicts.is_empty() {
            Ok(self.merge(other))
        } else {
            Err(MergeConflicts { conflicts })
        }
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
                    subscribe: self.inner.advertise_resource_subscriptions,
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

    /// Classify an operation that found nothing, after authorization passed.
    ///
    /// The task was present when it was authorized, so an operation that then
    /// found nothing means it expired in between. Resolving a second time
    /// tells the owner that, rather than reporting a task that existed moments
    /// ago as though it never had (#1249).
    ///
    /// Ownership is rechecked rather than assumed. The first resolution
    /// established it, and task ids are unguessable and not reused, so this is
    /// belt and braces; it costs one lookup and removes any argument about
    /// whether a store could return a differently-owned record here.
    async fn classify_absent_task(
        &self,
        task_id: &str,
        extensions: &crate::context::Extensions,
    ) -> Error {
        let Ok(presence) = self.inner.task_store.task_presence(task_id).await else {
            return Error::JsonRpc(unknown_task_error(task_id));
        };
        let owns = presence.owner().is_some_and(|owner| {
            crate::async_task::owner_matches(owner, request_principal(extensions).as_deref())
        });
        match presence {
            crate::async_task::TaskPresence::Expired { .. } if owns => {
                Error::JsonRpc(expired_task_error(task_id))
            }
            _ => Error::JsonRpc(unknown_task_error(task_id)),
        }
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
        if !self.final_tasks_enabled() {
            return Err(Error::JsonRpc(JsonRpcError::method_not_found(method)));
        }
        if client_declares_tasks(extensions) {
            return Ok(());
        }
        Err(Error::JsonRpc(
            JsonRpcError::missing_required_client_capability(tasks_client_capabilities()),
        ))
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
        // Resolved through presence so an expired task can be reported as
        // such to its owner. Everyone else must still see exactly what they
        // see for an id that was never issued, so the owner check happens
        // before any of that distinction escapes (#1249).
        let presence = self
            .inner
            .task_store
            .task_presence(task_id)
            .await
            .map_err(task_store_error)?;
        let Some(owner) = presence.owner() else {
            return Err(Error::JsonRpc(unknown_task_error(task_id)));
        };

        if crate::async_task::owner_matches(owner, request_principal(extensions).as_deref()) {
            match presence {
                // The owner is told the difference; nobody else reaches here.
                crate::async_task::TaskPresence::Expired { .. } => {
                    Err(Error::JsonRpc(expired_task_error(task_id)))
                }
                _ => Ok(()),
            }
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
        let (detailed, meta) = self.detailed_task(task_id).await?;
        let mut result = crate::tasks::GetTaskResult::new(detailed);
        result.meta = meta;
        Ok(McpResponse::FinalGetTask(result))
    }

    /// Build the complete status-discriminated view of a task.
    ///
    /// Both `tasks/get` and `notifications/tasks` render a task through this
    /// one path, which is what makes a pushed notification identical to the
    /// poll response a client would have received at that moment.
    async fn detailed_task(
        &self,
        task_id: &str,
    ) -> Result<(
        crate::tasks::DetailedTask,
        Option<serde_json::Map<String, serde_json::Value>>,
    )> {
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

        let meta = task.meta.and_then(|value| value.as_object().cloned());
        let detailed = match task.status {
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
        };
        Ok((detailed, meta))
    }

    /// Park a task on the input its handler asked for.
    ///
    /// The handler has returned; the task waits in `input_required` until the
    /// client answers with `tasks/update`, at which point [`Self::resume_task`]
    /// runs it again (#1208).
    async fn park_task_for_input(
        &self,
        task_id: &str,
        input_required: crate::protocol::InputRequiredResult,
    ) {
        let requests = input_required.input_requests.unwrap_or_default();
        if requests.is_empty() {
            // Parking here would strand the task: no `tasks/update` can ever
            // complete an empty request set.
            let error = JsonRpcError::internal_error(
                "handler asked for input without naming any requests, so the task has \
                 nothing to wait for",
            );
            if let Err(e) = self.inner.task_store.fail_task(task_id, error).await {
                tracing::warn!(task_id = %task_id, error = %e, "failed to record task failure");
            }
            self.notify_task_state(task_id).await;
            return;
        }

        // A park that does not take leaves the task working with nothing
        // outstanding, which no `tasks/update` can ever move. Failing it says
        // why instead of stranding it, matching the empty-request case above
        // (#1246).
        match self
            .inner
            .task_store
            .require_input(task_id, requests, input_required.request_state.as_deref())
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    task_id = %task_id,
                    "task was already terminal or gone when parking for input"
                );
            }
            Err(e) => {
                // Either way the park is lost and the task can never be
                // answered, so both end it. They are not the same fault
                // though: an invalid transition is the handler asking for
                // something the protocol forbids, which no retry fixes,
                // while a backend failure is infrastructure (#1246).
                let error = match &e {
                    crate::async_task::TaskStoreError::InvalidTransition(message) => {
                        tracing::error!(
                            task_id = %task_id,
                            error = %message,
                            "handler asked for input the protocol does not allow"
                        );
                        JsonRpcError::internal_error(format!(
                            "handler asked for input the protocol does not allow: {message}"
                        ))
                    }
                    other => {
                        tracing::warn!(
                            task_id = %task_id,
                            error = %other,
                            "task store could not park the task for input"
                        );
                        JsonRpcError::internal_error(format!(
                            "could not park the task for input: {other}"
                        ))
                    }
                };
                if let Err(e) = self.inner.task_store.fail_task(task_id, error).await {
                    tracing::warn!(task_id = %task_id, error = %e, "failed to record task failure");
                }
            }
        }
        self.notify_task_state(task_id).await;
    }

    /// Wake a live task whose input has just been committed.
    ///
    /// Returns false when the task is not live, which is how the caller knows
    /// to fall back to replay (#1246).
    fn wake_live_task(&self, task_id: &str) -> bool {
        let Ok(live) = self.inner.live_tasks.lock() else {
            return false;
        };
        match live.get(task_id) {
            Some(handle) => {
                // `notify_one`, not `notify_waiters`: a waiter only registers
                // when its future is polled, so a `notify_waiters` landing
                // between the store write and the await is lost. `notify_one`
                // leaves a permit that the next await consumes.
                handle.input_ready.notify_one();
                true
            }
            None => false,
        }
    }

    /// Signal a live task that cancellation was requested.
    ///
    /// The task stays non-terminal: its handler decides when it has finished
    /// unwinding and says so by returning `TaskOutcome::Cancelled`.
    fn signal_live_cancellation(&self, task_id: &str) -> bool {
        let Ok(live) = self.inner.live_tasks.lock() else {
            return false;
        };
        match live.get(task_id) {
            Some(handle) => {
                handle.cancelled.cancel();
                true
            }
            None => false,
        }
    }

    fn register_live_task(&self, task_id: &str, handle: Arc<crate::tool::LiveTask>) {
        if let Ok(mut live) = self.inner.live_tasks.lock() {
            live.insert(task_id.to_string(), handle);
        }
    }

    fn unregister_live_task(&self, task_id: &str) {
        if let Ok(mut live) = self.inner.live_tasks.lock() {
            live.remove(task_id);
        }
    }

    /// Re-invoke a task's handler after its input requests were answered.
    ///
    /// A task's client answers through `tasks/update` rather than by retrying
    /// `tools/call`, so the server performs the retry. The handler runs from
    /// the top with the accumulated answers readable through
    /// `RequestContext::input_responses`, exactly as a non-task MRTR handler
    /// sees them on the client's retry.
    async fn resume_task(&self, task_id: &str) {
        let resume = match self.inner.task_store.resume_context(task_id).await {
            Ok(Some(resume)) => resume,
            Ok(None) => {
                // Either the task vanished, or the store predates resumption
                // and cannot supply what a re-invocation needs. Fail loudly
                // rather than leave the task working forever.
                let error = JsonRpcError::internal_error(
                    "this task store cannot resume a task after input was provided; \
                     implement TaskStore::resume_context to support handlers that ask \
                     for input",
                );
                if let Err(e) = self.inner.task_store.fail_task(task_id, error).await {
                    tracing::warn!(task_id = %task_id, error = %e, "failed to record task failure");
                }
                self.notify_task_state(task_id).await;
                return;
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "failed to read resume context");
                return;
            }
        };

        // Static tools first, then dynamic, matching `tools/call`.
        let tool = self.inner.tools.get(&resume.tool_name).cloned();
        #[cfg(feature = "dynamic-tools")]
        let tool = tool.or_else(|| {
            self.inner
                .dynamic_tools
                .as_ref()
                .and_then(|d| d.get(&resume.tool_name))
        });
        let Some(tool) = tool else {
            let error = JsonRpcError::internal_error(format!(
                "tool '{}' is no longer registered, so the task cannot resume",
                resume.tool_name
            ));
            if let Err(e) = self.inner.task_store.fail_task(task_id, error).await {
                tracing::warn!(task_id = %task_id, error = %e, "failed to record task failure");
            }
            self.notify_task_state(task_id).await;
            return;
        };

        let mut ctx = RequestContext::new(RequestId::String(task_id.to_string()));
        // The answers reach the handler through the same MRTR extension a
        // client retry populates. Only a `stateless` build can register an
        // `mrtr_handler`, so a build without it can never park a task and
        // never reaches this.
        #[cfg(feature = "stateless")]
        ctx.extensions_mut().insert(crate::mrtr::MrtrRequest::new(
            Some(resume.input_responses),
            None,
        ));
        if let Some(tx) = &self.inner.notification_tx {
            ctx = ctx.with_notification_sender(tx.clone());
        }

        let task_id = task_id.to_string();
        let notifier = self.clone();
        let task_store = self.inner.task_store.clone();
        tokio::spawn(async move {
            let outcome = notifier
                .invoke_tool(&tool, ctx, resume.arguments, &resume.tool_name)
                .await;
            let result = match outcome {
                Ok(crate::protocol::RequestOutcome::Complete(result)) => result,
                // A handler may ask again; each round parks and resumes the
                // same way, so multi-step interactions need no special casing.
                Ok(crate::protocol::RequestOutcome::InputRequired(input_required)) => {
                    notifier.park_task_for_input(&task_id, input_required).await;
                    return;
                }
                Err(error) => CallToolResult::error(error.to_string()),
            };

            if let Err(e) = task_store.complete_task(&task_id, result).await {
                tracing::warn!(task_id = %task_id, error = %e, "failed to record task completion");
            }
            notifier.notify_task_state(&task_id).await;
        });
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

        let (detailed, meta) = match self.detailed_task(task_id).await {
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
                meta,
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
                        .map_err(task_store_error)?;

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
                                return Err(task_store_error(error));
                            }
                        };
                        if !persisted {
                            discard_unprepared_task(&task_store, &task_id).await;
                            return Err(Error::JsonRpc(JsonRpcError::internal_error(
                                "Task store could not persist preparation metadata",
                            )));
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
                                Ok(crate::tool::TaskOutcome::Completed(result)) => task_store
                                    .complete_task(&task_id_clone, result)
                                    .await
                                    .map(|_| "completed"),
                                Ok(crate::tool::TaskOutcome::Failed(error)) => task_store
                                    .fail_task(&task_id_clone, error)
                                    .await
                                    .map(|_| "failed"),
                                Ok(crate::tool::TaskOutcome::Cancelled { message }) => task_store
                                    .cancel_task(&task_id_clone, message.as_deref())
                                    .await
                                    .map(|_| "cancelled"),
                                // Propagating the cancellation error is the
                                // ordinary way a live handler unwinds, so it
                                // ends the task cancelled rather than failed.
                                Err(crate::error::Error::TaskCancelled) => task_store
                                    .cancel_task(
                                        &task_id_clone,
                                        Some("handler observed cancellation"),
                                    )
                                    .await
                                    .map(|_| "cancelled"),
                                // An unclassified error is an execution
                                // failure the handler declined to describe.
                                Err(error) => task_store
                                    .fail_task(
                                        &task_id_clone,
                                        JsonRpcError::internal_error(error.to_string()),
                                    )
                                    .await
                                    .map(|_| "failed"),
                            };
                            // The terminal write must win before unregistering
                            // (#1294), but the dead handle must not remain
                            // visible through logging or notification awaits.
                            // If the write failed, a later cancellation can
                            // now take the store path instead of signalling a
                            // handler that has already returned (#1305).
                            drop(registration);

                            match applied {
                                Ok(status) => tracing::info!(
                                    target: "mcp::tools",
                                    tool = %tool_name,
                                    task_id = %task_id_clone,
                                    duration_ms,
                                    status,
                                    "live task finished"
                                ),
                                Err(e) => tracing::warn!(
                                    task_id = %task_id_clone,
                                    error = %e,
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
                #[cfg(feature = "dynamic-tools")]
                if let Some(initializer) = &self.inner.prompt_initializer {
                    initializer()?;
                }
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
                let Some((mut task, result, error)) = self
                    .inner
                    .task_store
                    .get_task_result(&params.task_id)
                    .await
                    .map_err(task_store_error)?
                else {
                    // Present when it was authorized, absent now, so it
                    // expired in between (#1249).
                    return Err(self
                        .classify_absent_task(&params.task_id, &extensions)
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
                    self.authorize_task(&params.task_id, &extensions).await?;
                    // Partial responses are the normal case: the store
                    // consumes what matches an outstanding request and ignores
                    // unknown, already-answered, and superseded keys.
                    let Some(applied) = self
                        .inner
                        .task_store
                        .apply_input_responses(
                            &params.task_id,
                            decode_input_responses(&params.input_responses),
                        )
                        .await
                        .map_err(task_store_error)?
                    else {
                        // Nothing left to apply. A task the store still knows
                        // and has not expired is a late or duplicate update,
                        // so it gets the ordinary empty acknowledgement, which
                        // makes a client retry idempotent (#1249).
                        return match self
                            .inner
                            .task_store
                            .task_presence(&params.task_id)
                            .await
                            .map_err(task_store_error)?
                        {
                            crate::async_task::TaskPresence::Present { .. } => Ok(
                                McpResponse::FinalTaskAck(crate::tasks::TaskAcknowledgement::new()),
                            ),
                            _ => Err(self
                                .classify_absent_task(&params.task_id, &extensions)
                                .await),
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

                self.authorize_task(&params.task_id, &extensions).await?;

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
                        decode_input_responses(&params.input_responses),
                    )
                    .await
                    .map_err(task_store_error)?
                else {
                    // Nothing left to apply. A task the store still knows and
                    // has not expired is a late or duplicate update, so it
                    // gets the ordinary empty acknowledgement, which makes a
                    // client retry idempotent rather than a not-found (#1249).
                    return match self
                        .inner
                        .task_store
                        .task_presence(&params.task_id)
                        .await
                        .map_err(task_store_error)?
                    {
                        crate::async_task::TaskPresence::Present { .. } => {
                            Ok(McpResponse::UpdateTask(EmptyResult {}))
                        }
                        _ => Err(self
                            .classify_absent_task(&params.task_id, &extensions)
                            .await),
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
                    self.authorize_task(&params.task_id, &extensions).await?;
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
                        .map_err(task_store_error)?;
                    if cancelled.is_none() {
                        return Err(self
                            .classify_absent_task(&params.task_id, &extensions)
                            .await);
                    }
                    self.notify_task_state(&params.task_id).await;
                    return Ok(McpResponse::FinalTaskAck(
                        crate::tasks::TaskAcknowledgement::new(),
                    ));
                }

                self.authorize_task(&params.task_id, &extensions).await?;

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
                    .map_err(task_store_error)?
                else {
                    return Err(self
                        .classify_absent_task(&params.task_id, &extensions)
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
                    .map_err(task_store_error)?;
                if cancelled.is_none() {
                    return Err(self
                        .classify_absent_task(&params.task_id, &extensions)
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
                    let result = handler(params).await?;
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
mod tests;

#[cfg(test)]
mod cursor_property_tests;
