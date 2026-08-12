//! Tool definition and builder API
//!
//! Provides ergonomic ways to define MCP tools:
//!
//! 1. **Builder pattern** - Fluent API for defining tools
//! 2. **Trait-based** - Implement `McpTool` for full control
//! 3. **Function-based** - Quick tools from async functions
//!
//! ## Per-Tool Middleware
//!
//! Tools are implemented as Tower services internally, enabling middleware
//! composition via the `.layer()` method:
//!
//! ```rust
//! use std::time::Duration;
//! use tower::timeout::TimeoutLayer;
//! use tower_mcp::{ToolBuilder, CallToolResult};
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct SearchInput { query: String }
//!
//! let tool = ToolBuilder::new("slow_search")
//!     .description("Search with extended timeout")
//!     .handler(|input: SearchInput| async move {
//!         Ok(CallToolResult::text("result"))
//!     })
//!     .layer(TimeoutLayer::new(Duration::from_secs(30)))
//!     .build();
//! ```

use std::borrow::Cow;
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
#[cfg(feature = "stateless")]
use tower::ServiceExt;
use tower::util::BoxCloneService;
use tower_service::Service;

#[cfg(feature = "stateless")]
use tokio::sync::Mutex;

use crate::context::{Extensions, RequestContext};
use crate::error::{Error, Result, ResultExt};
use crate::protocol::{
    CallToolResult, ClientCapabilities, InputRequests, InputResponses, RequestOutcome, TaskStatus,
    TaskSupportMode, ToolAnnotations, ToolDefinition, ToolExecution, ToolIcon,
};

// =============================================================================
// Service Types for Per-Tool Middleware
// =============================================================================

/// Request type for tool services.
///
/// Contains the request context (for progress reporting, cancellation, etc.)
/// and the tool arguments as raw JSON.
#[derive(Debug, Clone)]
pub struct ToolRequest {
    /// Request context for progress reporting, cancellation, and client requests
    pub ctx: RequestContext,
    /// Tool arguments as raw JSON
    pub args: Value,
}

impl ToolRequest {
    /// Create a new tool request
    pub fn new(ctx: RequestContext, args: Value) -> Self {
        Self { ctx, args }
    }
}

/// A boxed, cloneable tool service with `Error = Infallible`.
///
/// This is the internal service type that tools use. Middleware errors are
/// caught and converted to `CallToolResult::error()` responses, so the
/// service never fails at the Tower level.
pub type BoxToolService = BoxCloneService<ToolRequest, CallToolResult, Infallible>;

/// A boxed MRTR-capable tool service.
#[cfg(feature = "stateless")]
type BoxMrtrToolService = BoxCloneService<ToolRequest, RequestOutcome<CallToolResult>, Infallible>;

/// A marker type for tools that take no parameters.
///
/// Use this instead of `()` when defining tools with no input parameters.
/// The unit type `()` generates `"type": "null"` in JSON Schema, which many
/// MCP clients reject. `NoParams` generates `"type": "object"` with no
/// required properties, which is the correct schema for parameterless tools.
///
/// # Example
///
/// ```rust
/// use tower_mcp::{ToolBuilder, CallToolResult, NoParams};
///
/// let tool = ToolBuilder::new("get_status")
///     .description("Get current status")
///     .handler(|_input: NoParams| async move {
///         Ok(CallToolResult::text("OK"))
///     })
///     .build();
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoParams;

impl<'de> serde::Deserialize<'de> for NoParams {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Accept null, empty object, or any object (ignoring all fields)
        struct NoParamsVisitor;

        impl<'de> serde::de::Visitor<'de> for NoParamsVisitor {
            type Value = NoParams;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("null or an object")
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(NoParams)
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(NoParams)
            }

            fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                serde::Deserialize::deserialize(deserializer)
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                // Drain the map, ignoring all entries
                while map
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(NoParams)
            }
        }

        deserializer.deserialize_any(NoParamsVisitor)
    }
}

impl JsonSchema for NoParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("NoParams")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        serde_json::json!({
            "type": "object"
        })
        .try_into()
        .expect("valid schema")
    }
}

/// Validate a tool name according to MCP spec (SEP-986).
///
/// Tool names must be:
/// - 1-64 characters long
/// - Contain only ASCII alphanumeric characters, underscores, hyphens, dots,
///   and forward slashes
///
/// Returns `Ok(())` if valid, `Err` with description if invalid.
pub(crate) fn validate_tool_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::tool("Tool name cannot be empty"));
    }
    if name.len() > 64 {
        return Err(Error::tool(format!(
            "Tool name '{}' exceeds maximum length of 64 characters (got {})",
            name,
            name.len()
        )));
    }
    if let Some(invalid_char) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-' && *c != '.' && *c != '/')
    {
        return Err(Error::tool(format!(
            "Tool name '{}' contains invalid character '{}'. Only alphanumeric, underscore, hyphen, dot, and forward slash are allowed.",
            name, invalid_char
        )));
    }
    Ok(())
}

/// Ensure a JSON Schema value has `"type": "object"`.
///
/// The MCP spec requires tool input schemas to be JSON objects with a `"type"` field.
/// Some types (e.g., `serde_json::Value`) generate schemas via schemars that lack
/// the `"type"` field, which causes MCP clients to reject the tool.
pub(crate) fn ensure_object_schema(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut()
        && !obj.contains_key("type")
    {
        obj.insert("type".to_string(), serde_json::json!("object"));
    }
    schema
}

/// A boxed future for tool handlers
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A tool handler that owns its execution rather than being replayed.
///
/// Implemented for closures by [`ToolBuilder::live_task_handler`]; the trait
/// exists so the router can hold one type-erased (#1246).
#[async_trait::async_trait]
pub(crate) trait LiveToolHandler: Send + Sync {
    async fn call(
        &self,
        ctx: RequestContext,
        task: TaskContext,
        arguments: Value,
    ) -> Result<TaskOutcome>;
}

/// The input schema a live registration derives from its input type.
fn live_input_schema<I: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(I))
        .unwrap_or_else(|_| serde_json::json!({"type": "object"}))
}

struct FnLiveToolHandlerWithContext<I, F> {
    handler: F,
    _input: std::marker::PhantomData<fn() -> I>,
}

#[async_trait::async_trait]
impl<I, F, Fut> LiveToolHandler for FnLiveToolHandlerWithContext<I, F>
where
    I: DeserializeOwned + Send + Sync + 'static,
    F: Fn(RequestContext, TaskContext, I) -> Fut + Send + Sync,
    Fut: Future<Output = Result<TaskOutcome>> + Send,
{
    async fn call(
        &self,
        ctx: RequestContext,
        task: TaskContext,
        arguments: Value,
    ) -> Result<TaskOutcome> {
        let input: I = serde_json::from_value(arguments).map_err(|e| {
            crate::error::Error::Tool(crate::error::ToolError::new(format!(
                "invalid arguments: {e}"
            )))
        })?;
        (self.handler)(ctx, task, input).await
    }
}

/// Applies a guard to a live handler.
///
/// Mirrors `GuardedMrtrToolHandler`. Without this, `Tool::with_guard` on a
/// live tool reached an `expect` on the absent service and panicked (#1295).
struct GuardedLiveToolHandler<G> {
    guard: G,
    inner: Arc<dyn LiveToolHandler>,
}

#[async_trait::async_trait]
impl<G> LiveToolHandler for GuardedLiveToolHandler<G>
where
    G: Fn(&ToolRequest) -> std::result::Result<(), String> + Clone + Send + Sync + 'static,
{
    async fn call(
        &self,
        ctx: RequestContext,
        task: TaskContext,
        arguments: Value,
    ) -> Result<TaskOutcome> {
        let request = ToolRequest::new(ctx, arguments);
        match (self.guard)(&request) {
            Ok(()) => self.inner.call(request.ctx, task, request.args).await,
            // A rejected call completes with a domain error, matching what a
            // guarded ordinary or MRTR tool does.
            Err(message) => Ok(TaskOutcome::Completed(CallToolResult::error(message))),
        }
    }
}

struct FnLiveToolHandler<I, F> {
    handler: F,
    _input: std::marker::PhantomData<fn() -> I>,
}

#[async_trait::async_trait]
impl<I, F, Fut> LiveToolHandler for FnLiveToolHandler<I, F>
where
    I: DeserializeOwned + Send + Sync + 'static,
    F: Fn(TaskContext, I) -> Fut + Send + Sync,
    Fut: Future<Output = Result<TaskOutcome>> + Send,
{
    async fn call(
        &self,
        _ctx: RequestContext,
        task: TaskContext,
        arguments: Value,
    ) -> Result<TaskOutcome> {
        let input: I = serde_json::from_value(arguments).map_err(|e| {
            crate::error::Error::Tool(crate::error::ToolError::new(format!(
                "invalid arguments: {e}"
            )))
        })?;
        (self.handler)(task, input).await
    }
}

/// Tool handler trait - the core abstraction for tool execution
pub trait ToolHandler: Send + Sync {
    /// Execute the tool with the given arguments
    fn call(&self, args: Value) -> BoxFuture<'_, Result<CallToolResult>>;

    /// Execute the tool with request context for progress/cancellation support
    ///
    /// The default implementation ignores the context and calls `call`.
    /// Override this to receive progress/cancellation context.
    fn call_with_context(
        &self,
        _ctx: RequestContext,
        args: Value,
    ) -> BoxFuture<'_, Result<CallToolResult>> {
        self.call(args)
    }

    /// Returns true if this handler uses context (for optimization)
    fn uses_context(&self) -> bool {
        false
    }

    /// Get the tool's input schema
    fn input_schema(&self) -> Value;
}

/// Handler for a tool that can complete or return an SEP-2322
/// [`RequestOutcome::InputRequired`] continuation.
#[cfg(feature = "stateless")]
pub trait MrtrToolHandler: Send + Sync {
    /// Execute an MRTR-capable tool with request context and raw arguments.
    fn call(
        &self,
        ctx: RequestContext,
        args: Value,
    ) -> BoxFuture<'_, Result<RequestOutcome<CallToolResult>>>;

    /// Get the tool's input schema.
    fn input_schema(&self) -> Value;
}

/// Adapts an MRTR handler to Tower's service abstraction.
#[cfg(feature = "stateless")]
struct MrtrToolHandlerService<H> {
    handler: Arc<H>,
}

#[cfg(feature = "stateless")]
impl<H> MrtrToolHandlerService<H> {
    fn new(handler: H) -> Self {
        Self {
            handler: Arc::new(handler),
        }
    }
}

#[cfg(feature = "stateless")]
impl<H> Clone for MrtrToolHandlerService<H> {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
        }
    }
}

#[cfg(feature = "stateless")]
impl<H> Service<ToolRequest> for MrtrToolHandlerService<H>
where
    H: MrtrToolHandler + 'static,
{
    type Response = RequestOutcome<CallToolResult>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: ToolRequest) -> Self::Future {
        let handler = self.handler.clone();
        Box::pin(async move { handler.call(req.ctx, req.args).await })
    }
}

/// Runs an erased MRTR Tower service as an MRTR handler.
#[cfg(feature = "stateless")]
struct ServiceMrtrToolHandler {
    service: Mutex<BoxMrtrToolService>,
    input_schema: Value,
}

#[cfg(feature = "stateless")]
struct GuardedMrtrToolHandler<G> {
    guard: G,
    inner: Arc<dyn MrtrToolHandler>,
}

#[cfg(feature = "stateless")]
impl<G> MrtrToolHandler for GuardedMrtrToolHandler<G>
where
    G: Fn(&ToolRequest) -> std::result::Result<(), String> + Clone + Send + Sync + 'static,
{
    fn call(
        &self,
        ctx: RequestContext,
        args: Value,
    ) -> BoxFuture<'_, Result<RequestOutcome<CallToolResult>>> {
        let request = ToolRequest::new(ctx, args);
        match (self.guard)(&request) {
            Ok(()) => self.inner.call(request.ctx, request.args),
            Err(message) => {
                Box::pin(
                    async move { Ok(RequestOutcome::Complete(CallToolResult::error(message))) },
                )
            }
        }
    }

    fn input_schema(&self) -> Value {
        self.inner.input_schema()
    }
}

#[cfg(feature = "stateless")]
impl MrtrToolHandler for ServiceMrtrToolHandler {
    fn call(
        &self,
        ctx: RequestContext,
        args: Value,
    ) -> BoxFuture<'_, Result<RequestOutcome<CallToolResult>>> {
        Box::pin(async move {
            let mut service = self.service.lock().await.clone();
            let outcome = service
                .ready()
                .await
                .expect("MRTR tool service is infallible")
                .call(ToolRequest::new(ctx, args))
                .await
                .expect("MRTR tool service is infallible");
            Ok(outcome)
        })
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }
}

/// Adapts a `ToolHandler` to a Tower `Service<ToolRequest>`.
///
/// This is an internal adapter that bridges the handler abstraction to the
/// service abstraction, enabling middleware composition.
pub(crate) struct ToolHandlerService<H> {
    handler: Arc<H>,
}

impl<H> ToolHandlerService<H> {
    pub(crate) fn new(handler: H) -> Self {
        Self {
            handler: Arc::new(handler),
        }
    }
}

impl<H> Clone for ToolHandlerService<H> {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
        }
    }
}

impl<H> Service<ToolRequest> for ToolHandlerService<H>
where
    H: ToolHandler + 'static,
{
    type Response = CallToolResult;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = std::result::Result<CallToolResult, Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: ToolRequest) -> Self::Future {
        let handler = self.handler.clone();
        Box::pin(async move { handler.call_with_context(req.ctx, req.args).await })
    }
}

/// A complete tool definition with service-based execution.
///
/// Tools are implemented as Tower services internally, enabling middleware
/// composition via the builder's `.layer()` method. The service is wrapped
/// in [`ToolCatchError`] to convert any errors (from handlers or middleware)
/// into `CallToolResult::error()` responses.
pub struct Tool {
    /// Tool name (must be 1-64 chars, alphanumeric/underscore/hyphen/dot only)
    pub name: String,
    /// Human-readable title for the tool
    pub title: Option<String>,
    /// Description of what the tool does
    pub description: Option<String>,
    /// JSON Schema for the tool's output (optional)
    pub output_schema: Option<Value>,
    /// Icons for the tool
    pub icons: Option<Vec<ToolIcon>>,
    /// Tool annotations (hints about behavior)
    pub annotations: Option<ToolAnnotations>,
    /// Validated protocol metadata included in `tools/list`.
    pub meta: Option<Value>,
    /// Task support mode for this tool
    pub task_support: TaskSupportMode,
    /// Client capabilities required to invoke this tool in the modern
    /// per-request protocol.
    pub(crate) required_client_capabilities: Option<ClientCapabilities>,
    /// Optional callback run after task allocation and before task execution.
    pub(crate) task_preparer: Option<Arc<dyn TaskPreparer>>,
    /// The boxed service that executes the tool
    pub(crate) service: Option<BoxToolService>,
    #[cfg(feature = "stateless")]
    pub(crate) mrtr_handler: Option<Arc<dyn MrtrToolHandler>>,
    /// Live handler, which owns its execution instead of being replayed (#1246).
    pub(crate) live_handler: Option<Arc<dyn LiveToolHandler>>,
    /// JSON Schema for the tool's input
    pub(crate) input_schema: Value,
}

impl std::fmt::Debug for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tool")
            .field("name", &self.name)
            .field("title", &self.title)
            .field("description", &self.description)
            .field("output_schema", &self.output_schema)
            .field("icons", &self.icons)
            .field("annotations", &self.annotations)
            .field("meta", &self.meta)
            .field("task_support", &self.task_support)
            .field(
                "required_client_capabilities",
                &self.required_client_capabilities,
            )
            .finish_non_exhaustive()
    }
}

// SAFETY: BoxCloneService is Send + Sync (tower provides unsafe impl Sync),
// and all other fields in Tool are Send + Sync.
unsafe impl Send for Tool {}
unsafe impl Sync for Tool {}

impl Clone for Tool {
    fn clone(&self) -> Self {
        Self {
            live_handler: self.live_handler.clone(),
            name: self.name.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            output_schema: self.output_schema.clone(),
            icons: self.icons.clone(),
            annotations: self.annotations.clone(),
            meta: self.meta.clone(),
            task_support: self.task_support,
            required_client_capabilities: self.required_client_capabilities.clone(),
            task_preparer: self.task_preparer.clone(),
            service: self.service.clone(),
            #[cfg(feature = "stateless")]
            mrtr_handler: self.mrtr_handler.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

impl Tool {
    /// Create a new tool builder
    pub fn builder(name: impl Into<String>) -> ToolBuilder {
        ToolBuilder::new(name)
    }

    /// Get the tool definition for tools/list
    pub fn definition(&self) -> ToolDefinition {
        let execution = match self.task_support {
            TaskSupportMode::Forbidden => None,
            mode => Some(ToolExecution {
                task_support: Some(mode),
            }),
        };
        ToolDefinition {
            name: self.name.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
            icons: self.icons.clone(),
            annotations: self.annotations.clone(),
            execution,
            meta: self.meta.clone(),
        }
    }

    /// Attach validated protocol metadata to this tool definition.
    pub fn with_meta(
        mut self,
        meta: Value,
    ) -> std::result::Result<Self, crate::protocol::MetaValidationError> {
        crate::protocol::validate_meta_object(&meta)?;
        self.meta = Some(meta);
        Ok(self)
    }

    /// Call the tool without context
    ///
    /// Creates a dummy request context. For full context support, use
    /// [`call_with_context`](Self::call_with_context).
    pub fn call(&self, args: Value) -> BoxFuture<'static, CallToolResult> {
        let ctx = RequestContext::new(crate::protocol::RequestId::Number(0));
        self.call_with_context(ctx, args)
    }

    /// Call the tool with request context
    ///
    /// The context provides progress reporting, cancellation support, and
    /// access to client requests (for sampling, etc.).
    ///
    /// # Note
    ///
    /// This method returns `CallToolResult` directly (not `Result<CallToolResult>`).
    /// Any errors from the handler or middleware are converted to
    /// `CallToolResult::error()` with `is_error: true`.
    pub fn call_with_context(
        &self,
        ctx: RequestContext,
        args: Value,
    ) -> BoxFuture<'static, CallToolResult> {
        let tool = self.clone();
        Box::pin(async move {
            match tool.call_outcome_with_context(ctx, args).await {
                Ok(RequestOutcome::Complete(result)) => result,
                Ok(RequestOutcome::InputRequired(_)) => CallToolResult::error(
                    "tool requires additional client input; use call_outcome_with_context",
                ),
                Err(error) => CallToolResult::error(error.to_string()),
            }
        })
    }

    /// Call the tool and preserve an SEP-2322 input-required outcome.
    pub fn call_outcome(
        &self,
        args: Value,
    ) -> BoxFuture<'static, Result<RequestOutcome<CallToolResult>>> {
        let ctx = RequestContext::new(crate::protocol::RequestId::Number(0));
        self.call_outcome_with_context(ctx, args)
    }

    /// Call the tool with context and preserve an SEP-2322 input-required
    /// outcome or protocol-level handler error.
    pub fn call_outcome_with_context(
        &self,
        ctx: RequestContext,
        args: Value,
    ) -> BoxFuture<'static, Result<RequestOutcome<CallToolResult>>> {
        use tower::ServiceExt;
        #[cfg(feature = "stateless")]
        if let Some(handler) = self.mrtr_handler.clone() {
            return Box::pin(async move { handler.call(ctx, args).await });
        }
        let Some(service) = self.service.clone() else {
            let error = Error::tool(
                "tool has no synchronous or MRTR handler; it can only be invoked as a task",
            );
            return Box::pin(async move { Err(error) });
        };
        Box::pin(async move {
            let result = service.oneshot(ToolRequest::new(ctx, args)).await.unwrap();
            Ok(RequestOutcome::Complete(result))
        })
    }

    /// Require the given client capability shape before this tool may be
    /// invoked using the modern per-request protocol.
    ///
    /// Required objects are matched recursively. For example, requiring
    /// `ClientCapabilities { sampling: Some(Default::default()), .. }`
    /// accepts any advertised `sampling` capability, including one with
    /// additional optional fields.
    pub fn require_client_capabilities(mut self, required: ClientCapabilities) -> Self {
        self.required_client_capabilities = Some(required);
        self
    }

    /// Return the client capability shape required by this tool, if any.
    pub fn required_client_capabilities(&self) -> Option<&ClientCapabilities> {
        self.required_client_capabilities.as_ref()
    }

    /// Add a preparation callback for task-backed invocations.
    ///
    /// The callback runs exactly once after task ID allocation and before the
    /// initial task response. It is skipped for synchronous calls.
    pub fn with_task_preparation<F, Fut>(mut self, prepare: F) -> Self
    where
        F: Fn(TaskContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TaskPreparation>> + Send + 'static,
    {
        self.task_preparer = Some(Arc::new(prepare));
        self
    }

    /// Add a typed preparation callback to an already-built tool.
    pub fn with_typed_task_preparation<I, F, Fut>(mut self, prepare: F) -> Self
    where
        I: DeserializeOwned + Send + Sync + 'static,
        F: Fn(TaskContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TaskPreparation>> + Send + 'static,
    {
        self.task_preparer = Some(Arc::new(TypedTaskPreparer {
            prepare,
            _phantom: std::marker::PhantomData,
        }));
        self
    }

    pub(crate) async fn prepare_task(
        &self,
        context: TaskContext,
        arguments: Value,
    ) -> Result<TaskPreparation> {
        match self.task_preparer.as_ref() {
            Some(prepare) => prepare.prepare(context, arguments).await,
            None => Ok(TaskPreparation::default()),
        }
    }

    /// Apply a guard to this built tool.
    ///
    /// The guard runs before the handler and can short-circuit with an error.
    /// This is useful for applying the same guard to multiple tools (per-group
    /// pattern):
    ///
    /// ```rust
    /// use tower_mcp::{ToolBuilder, CallToolResult};
    /// use tower_mcp::tool::ToolRequest;
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct Input { value: String }
    ///
    /// fn build_tool(name: &str) -> tower_mcp::tool::Tool {
    ///     ToolBuilder::new(name)
    ///         .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///         .build()
    /// }
    ///
    /// let guard = |_req: &ToolRequest| -> Result<(), String> { Ok(()) };
    ///
    /// let tools: Vec<_> = vec![build_tool("a"), build_tool("b")]
    ///     .into_iter()
    ///     .map(|t| t.with_guard(guard.clone()))
    ///     .collect();
    /// ```
    pub fn with_guard<G>(self, guard: G) -> Self
    where
        G: Fn(&ToolRequest) -> std::result::Result<(), String> + Clone + Send + Sync + 'static,
    {
        // A tool can now have a live handler and a fallback at the same time
        // (#1246), so every path that is actually present gets wrapped,
        // rather than returning after whichever one this found first and
        // leaving a coexisting fallback unguarded.
        let live_handler = self.live_handler.clone().map(|inner| {
            Arc::new(GuardedLiveToolHandler {
                guard: guard.clone(),
                inner,
            }) as Arc<dyn LiveToolHandler>
        });

        #[cfg(feature = "stateless")]
        if let Some(inner) = self.mrtr_handler.clone() {
            return Tool {
                live_handler,
                mrtr_handler: Some(Arc::new(GuardedMrtrToolHandler { guard, inner })),
                ..self
            };
        }

        match self.service.clone() {
            Some(service) => {
                let guarded = GuardService {
                    guard,
                    inner: service,
                };
                let caught = ToolCatchError::new(guarded);
                Tool {
                    live_handler,
                    service: Some(BoxCloneService::new(caught)),
                    ..self
                }
            }
            // Live-only: there is no synchronous or MRTR path to guard.
            None if live_handler.is_some() => Tool {
                live_handler,
                ..self
            },
            None => panic!("tool must have a complete, MRTR, or live handler"),
        }
    }

    /// Create a new tool with a prefixed name.
    ///
    /// This creates a copy of the tool with its name prefixed by the given
    /// string and a dot separator. For example, if the tool is named "query"
    /// and the prefix is "db", the new tool will be named "db.query".
    ///
    /// This is used internally by `McpRouter::nest()` to namespace tools.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{ToolBuilder, CallToolResult};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct Input { value: String }
    ///
    /// let tool = ToolBuilder::new("query")
    ///     .description("Query the database")
    ///     .handler(|i: Input| async move { Ok(CallToolResult::text(&i.value)) })
    ///     .build();
    ///
    /// let prefixed = tool.with_name_prefix("db");
    /// assert_eq!(prefixed.name, "db.query");
    /// ```
    pub fn with_name_prefix(&self, prefix: &str) -> Self {
        Self {
            live_handler: self.live_handler.clone(),
            name: format!("{}.{}", prefix, self.name),
            title: self.title.clone(),
            description: self.description.clone(),
            output_schema: self.output_schema.clone(),
            icons: self.icons.clone(),
            annotations: self.annotations.clone(),
            meta: self.meta.clone(),
            task_support: self.task_support,
            required_client_capabilities: self.required_client_capabilities.clone(),
            task_preparer: self.task_preparer.clone(),
            service: self.service.clone(),
            #[cfg(feature = "stateless")]
            mrtr_handler: self.mrtr_handler.clone(),
            input_schema: self.input_schema.clone(),
        }
    }

    /// Create a tool from a handler (internal helper)
    #[allow(clippy::too_many_arguments)]
    fn from_handler<H: ToolHandler + 'static>(
        name: String,
        title: Option<String>,
        description: Option<String>,
        output_schema: Option<Value>,
        icons: Option<Vec<ToolIcon>>,
        annotations: Option<ToolAnnotations>,
        task_support: TaskSupportMode,
        input_schema_override: Option<Value>,
        handler: H,
    ) -> Self {
        let input_schema =
            ensure_object_schema(input_schema_override.unwrap_or_else(|| handler.input_schema()));
        let handler_service = ToolHandlerService::new(handler);
        let catch_error = ToolCatchError::new(handler_service);
        let service = BoxCloneService::new(catch_error);

        Self {
            live_handler: None,
            name,
            title,
            description,
            output_schema,
            icons,
            annotations,
            meta: None,
            task_support,
            required_client_capabilities: None,
            task_preparer: None,
            service: Some(service),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
            input_schema,
        }
    }

    #[cfg(feature = "stateless")]
    #[allow(clippy::too_many_arguments)]
    fn from_mrtr_handler<H: MrtrToolHandler + 'static>(
        name: String,
        title: Option<String>,
        description: Option<String>,
        output_schema: Option<Value>,
        icons: Option<Vec<ToolIcon>>,
        annotations: Option<ToolAnnotations>,
        task_support: TaskSupportMode,
        input_schema_override: Option<Value>,
        handler: H,
    ) -> Self {
        let input_schema =
            ensure_object_schema(input_schema_override.unwrap_or_else(|| handler.input_schema()));
        Self {
            live_handler: None,
            name,
            title,
            description,
            output_schema,
            icons,
            annotations,
            meta: None,
            task_support,
            required_client_capabilities: None,
            task_preparer: None,
            service: None,
            mrtr_handler: Some(Arc::new(handler)),
            input_schema,
        }
    }
}

// =============================================================================
// Builder API
// =============================================================================

/// Builder for creating tools with a fluent API
///
/// # Example
///
/// ```rust
/// use tower_mcp::{ToolBuilder, CallToolResult};
/// use schemars::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(Debug, Deserialize, JsonSchema)]
/// struct GreetInput {
///     name: String,
/// }
///
/// let tool = ToolBuilder::new("greet")
///     .description("Greet someone by name")
///     .handler(|input: GreetInput| async move {
///         Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
///     })
///     .build();
///
/// assert_eq!(tool.name, "greet");
/// ```
pub struct ToolBuilder {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    input_schema_override: Option<Value>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
    task_support: TaskSupportMode,
}

impl ToolBuilder {
    /// Create a new tool builder with the given name.
    ///
    /// Tool names must be 1-64 characters and contain only ASCII alphanumeric
    /// characters, underscores, hyphens, dots, and forward slashes (per
    /// [SEP-986](https://github.com/modelcontextprotocol/specification/issues/986)).
    ///
    /// Use [`try_new`](Self::try_new) if the name comes from runtime input.
    ///
    /// # Panics
    ///
    /// Panics if `name` is empty, exceeds 64 characters, or contains
    /// characters other than ASCII alphanumerics, `_`, `-`, `.`, and `/`.
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        if let Err(e) = validate_tool_name(&name) {
            panic!("{e}");
        }
        Self {
            name,
            title: None,
            description: None,
            output_schema: None,
            input_schema_override: None,
            icons: None,
            annotations: None,
            task_support: TaskSupportMode::default(),
        }
    }

    /// Create a new tool builder, returning an error if the name is invalid.
    ///
    /// This is the fallible alternative to [`new`](Self::new) for cases where
    /// the tool name comes from runtime input (e.g., user configuration or
    /// database).
    pub fn try_new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_tool_name(&name)?;
        Ok(Self {
            name,
            title: None,
            description: None,
            output_schema: None,
            input_schema_override: None,
            icons: None,
            annotations: None,
            task_support: TaskSupportMode::default(),
        })
    }

    /// Set a human-readable title for the tool.
    ///
    /// The title is displayed by MCP clients (e.g., Claude Code's `/mcp` tool list)
    /// as a friendly label instead of the raw tool name. For example, a tool named
    /// `search_crates` with title `"Search Crates"` will display the title in UIs
    /// that support it.
    ///
    /// ```
    /// # use tower_mcp::ToolBuilder;
    /// let tool = ToolBuilder::new("search_crates")
    ///     .title("Search Crates")
    ///     .description("Search for Rust crates on crates.io")
    ///     .handler(|()| async { Ok(tower_mcp::CallToolResult::text("results")) })
    ///     .build();
    /// ```
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the output schema (JSON Schema for structured output)
    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Override the input schema (JSON Schema for tool arguments).
    ///
    /// By default, the input schema is auto-generated from the handler's input
    /// type via [`schemars::JsonSchema`]. Calling this method overrides that
    /// auto-generation with an explicit schema. This is particularly useful for
    /// handlers that use [`RawArgs`](crate::extract::RawArgs) (which has no typed
    /// input struct) but still need to declare a non-trivial schema, or to
    /// supply richer JSON Schema 2020-12 constructs (`oneOf`, `anyOf`,
    /// `if`/`then`, `$ref`, etc.) that schemars cannot express.
    ///
    /// The supplied schema is normalized via the same `type: "object"` check
    /// the auto-generated schemas go through, so MCP-spec compliance is
    /// preserved.
    ///
    /// When called alongside a typed handler (`.handler(|x: Foo| ...)` or a
    /// [`Json<T>`](crate::extract::Json) extractor), the explicit schema wins
    /// over the schemars-generated one.
    ///
    /// # Example
    ///
    /// ```rust
    /// use serde_json::json;
    /// use tower_mcp::{CallToolResult, ToolBuilder};
    /// use tower_mcp::extract::RawArgs;
    ///
    /// let tool = ToolBuilder::new("query")
    ///     .description("Query with a conditional schema")
    ///     .input_schema(json!({
    ///         "type": "object",
    ///         "properties": {
    ///             "filter": {
    ///                 "oneOf": [
    ///                     { "type": "string" },
    ///                     {
    ///                         "type": "object",
    ///                         "properties": { "field": { "type": "string" } },
    ///                         "required": ["field"]
    ///                     }
    ///                 ]
    ///             }
    ///         },
    ///         "required": ["filter"]
    ///     }))
    ///     .extractor_handler((), |RawArgs(args): RawArgs| async move {
    ///         Ok(CallToolResult::json(args))
    ///     })
    ///     .build();
    ///
    /// let schema = tool.definition().input_schema;
    /// assert_eq!(schema["type"], "object");
    /// assert!(schema["properties"]["filter"]["oneOf"].is_array());
    /// ```
    pub fn input_schema(mut self, schema: Value) -> Self {
        self.input_schema_override = Some(schema);
        self
    }

    /// Add an icon for the tool
    pub fn icon(mut self, src: impl Into<String>) -> Self {
        self.icons.get_or_insert_with(Vec::new).push(ToolIcon {
            src: src.into(),
            mime_type: None,
            sizes: None,
            theme: None,
        });
        self
    }

    /// Add an icon with metadata
    pub fn icon_with_meta(
        mut self,
        src: impl Into<String>,
        mime_type: Option<String>,
        sizes: Option<Vec<String>>,
    ) -> Self {
        self.icons.get_or_insert_with(Vec::new).push(ToolIcon {
            src: src.into(),
            mime_type,
            sizes,
            theme: None,
        });
        self
    }

    /// Set the tool description
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Mark the tool as read-only (does not modify state)
    pub fn read_only(mut self) -> Self {
        self.annotations
            .get_or_insert_with(ToolAnnotations::default)
            .read_only_hint = true;
        self
    }

    /// Mark the tool as non-destructive
    pub fn non_destructive(mut self) -> Self {
        self.annotations
            .get_or_insert_with(ToolAnnotations::default)
            .destructive_hint = false;
        self
    }

    /// Mark the tool as destructive (may perform irreversible operations)
    pub fn destructive(mut self) -> Self {
        self.annotations
            .get_or_insert_with(ToolAnnotations::default)
            .destructive_hint = true;
        self
    }

    /// Mark the tool as idempotent (same args = same effect)
    pub fn idempotent(mut self) -> Self {
        self.annotations
            .get_or_insert_with(ToolAnnotations::default)
            .idempotent_hint = true;
        self
    }

    /// Mark the tool as read-only, idempotent, and non-destructive.
    ///
    /// This is a convenience method for safe, side-effect-free tools.
    /// For finer control, use `.read_only()`, `.idempotent()`, and
    /// `.non_destructive()` individually.
    pub fn read_only_safe(mut self) -> Self {
        let ann = self
            .annotations
            .get_or_insert_with(ToolAnnotations::default);
        ann.read_only_hint = true;
        ann.idempotent_hint = true;
        ann.destructive_hint = false;
        self
    }

    /// Set tool annotations directly
    pub fn annotations(mut self, annotations: ToolAnnotations) -> Self {
        self.annotations = Some(annotations);
        self
    }

    /// Set the task support mode for this tool
    pub fn task_support(mut self, mode: TaskSupportMode) -> Self {
        self.task_support = mode;
        self
    }

    /// Create a tool that takes no parameters.
    ///
    /// This is a convenience method for tools that don't require any input.
    /// It generates the correct `{"type": "object"}` schema that MCP clients expect.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::{ToolBuilder, CallToolResult};
    ///
    /// let tool = ToolBuilder::new("get_status")
    ///     .description("Get current status")
    ///     .no_params_handler(|| async {
    ///         Ok(CallToolResult::text("OK"))
    ///     })
    ///     .build();
    /// ```
    pub fn no_params_handler<F, Fut>(self, handler: F) -> ToolBuilderWithNoParamsHandler<F>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
    {
        ToolBuilderWithNoParamsHandler {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema_override: self.input_schema_override,
            icons: self.icons,
            annotations: self.annotations,
            task_support: self.task_support,
            handler,
        }
    }

    /// Specify input type and handler.
    ///
    /// The input type must implement `JsonSchema` and `DeserializeOwned`.
    /// The handler receives the deserialized input and returns a `CallToolResult`.
    ///
    /// # State Sharing
    ///
    /// To share state across tool calls (e.g., database connections, API clients),
    /// wrap your state in an `Arc` and clone it into the async block:
    ///
    /// ```rust
    /// use std::sync::Arc;
    /// use tower_mcp::{ToolBuilder, CallToolResult};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// struct AppState {
    ///     api_key: String,
    /// }
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct MyInput {
    ///     query: String,
    /// }
    ///
    /// let state = Arc::new(AppState { api_key: "secret".to_string() });
    ///
    /// let tool = ToolBuilder::new("my_tool")
    ///     .description("A tool that uses shared state")
    ///     .handler(move |input: MyInput| {
    ///         let state = state.clone(); // Clone Arc for the async block
    ///         async move {
    ///             // Use state.api_key here...
    ///             Ok(CallToolResult::text(format!("Query: {}", input.query)))
    ///         }
    ///     })
    ///     .build();
    /// ```
    ///
    /// The `move` keyword on the closure captures the `Arc<AppState>`, and
    /// cloning it inside the closure body allows each async invocation to
    /// have its own reference to the shared state.
    pub fn handler<I, F, Fut>(self, handler: F) -> ToolBuilderWithHandler<I, F>
    where
        I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
        F: Fn(I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
    {
        ToolBuilderWithHandler {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema_override: self.input_schema_override,
            icons: self.icons,
            annotations: self.annotations,
            task_support: self.task_support,
            task_preparer: None,
            handler,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Set a handler that owns its execution instead of being replayed.
    ///
    /// A task handler registered with [`mrtr_handler`](Self::mrtr_handler)
    /// ends when it asks for input, and the router re-invokes it from the top
    /// once the answers arrive. That is right for a handler which is a
    /// function of its arguments, and wrong for one that owns live state: a
    /// subprocess, an open stream, a registered responder. Running it again
    /// does not continue the operation, it starts a second one.
    ///
    /// A live handler stays alive instead. It awaits its answers:
    ///
    /// ```rust,no_run
    /// use tower_mcp::{CallToolResult, TaskContext, TaskOutcome, ToolBuilder};
    /// use tower_mcp::protocol::InputRequests;
    /// use serde::Deserialize;
    /// use tower_mcp::schemars::JsonSchema;
    ///
    /// #[derive(Deserialize, JsonSchema)]
    /// struct Run { prompt: String }
    ///
    /// # fn approval() -> InputRequests { Default::default() }
    /// let tool = ToolBuilder::new("run")
    ///     .description("Runs something long")
    ///     .live_task_handler(|task: TaskContext, input: Run| async move {
    ///         // Whatever this owns is still owned after the await.
    ///         let answers = task.require_input(approval()).await?;
    ///         let _ = (answers, input);
    ///         Ok(TaskOutcome::Completed(CallToolResult::text("done")))
    ///     })
    ///     .build();
    /// ```
    ///
    /// The handler returns a [`TaskOutcome`] and the router applies it, so
    /// nothing else writes terminal state and completion cannot race the
    /// handler.
    ///
    /// # Consequences
    ///
    /// The tool is registered as [`TaskSupportMode::Required`], because a
    /// live handler has no task to park against otherwise.
    ///
    /// No invocation arguments are persisted, since nothing replays them. A
    /// server whose arguments carry prompts or credentials keeps them out of
    /// durable storage by using a live handler.
    ///
    /// A live future does not survive its process. On restart, live tasks left
    /// non-terminal have nothing behind them, and the application reconciles
    /// them rather than the router guessing.
    pub fn live_task_handler<I, F, Fut>(self, handler: F) -> ToolBuilderWithLiveHandler
    where
        I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
        F: Fn(TaskContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TaskOutcome>> + Send + 'static,
    {
        self.into_live(
            Arc::new(FnLiveToolHandler {
                handler,
                _input: std::marker::PhantomData::<fn() -> I>,
            }),
            live_input_schema::<I>(),
        )
    }

    /// A live task handler that also receives the request's context.
    ///
    /// Same as [`live_task_handler`](Self::live_task_handler), plus the
    /// [`RequestContext`] of the `tools/call` that created the task. That is
    /// how a live handler reads an authenticated principal, a trace id, or an
    /// extension added by [`TaskPreparation`], without keeping its own
    /// registry keyed by task id (#1301).
    ///
    /// # The context is the request's, not the task's
    ///
    /// The originating `tools/call` has already returned a task handle to the
    /// client, so its cancellation token tracks *that request*. Reaching for
    /// `ctx.cancelled()` when you mean task cancellation is wrong in both
    /// directions: it can fire when a client disconnects from a call that was
    /// already answered, and it does not fire when the task itself is
    /// cancelled.
    ///
    /// Task cancellation is [`TaskContext`], which is what
    /// [`TaskContext::require_input`] already returns
    /// [`crate::Error::TaskCancelled`] from.
    ///
    /// ```rust,no_run
    /// use tower_mcp::{CallToolResult, RequestContext, TaskContext, TaskOutcome, ToolBuilder};
    /// use serde::Deserialize;
    /// use tower_mcp::schemars::JsonSchema;
    ///
    /// #[derive(Clone)]
    /// struct Principal(String);
    ///
    /// #[derive(Deserialize, JsonSchema)]
    /// struct Run { prompt: String }
    ///
    /// let tool = ToolBuilder::new("run")
    ///     .description("Runs as the calling principal")
    ///     .live_task_handler_with_context(
    ///         |ctx: RequestContext, task: TaskContext, input: Run| async move {
    ///             let who = ctx
    ///                 .extension::<Principal>()
    ///                 .map(|p| p.0.clone())
    ///                 .unwrap_or_else(|| "anonymous".into());
    ///             let _ = (task, input);
    ///             Ok(TaskOutcome::Completed(CallToolResult::text(who)))
    ///         },
    ///     )
    ///     .build();
    /// ```
    pub fn live_task_handler_with_context<I, F, Fut>(self, handler: F) -> ToolBuilderWithLiveHandler
    where
        I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
        F: Fn(RequestContext, TaskContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TaskOutcome>> + Send + 'static,
    {
        self.into_live(
            Arc::new(FnLiveToolHandlerWithContext {
                handler,
                _input: std::marker::PhantomData::<fn() -> I>,
            }),
            live_input_schema::<I>(),
        )
    }

    /// Shared tail of both live registration forms.
    fn into_live(
        self,
        handler: Arc<dyn LiveToolHandler>,
        derived_schema: Value,
    ) -> ToolBuilderWithLiveHandler {
        ToolBuilderWithLiveHandler {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema: self.input_schema_override.unwrap_or(derived_schema),
            icons: self.icons,
            annotations: self.annotations,
            handler,
        }
    }

    /// Set an SEP-2322 handler that may return either a complete tool result
    /// or an input-required continuation.
    ///
    /// The handler receives [`RequestContext`], where
    /// [`RequestContext::input_responses`] and
    /// [`RequestContext::request_state`] expose values from a retry.
    #[cfg(feature = "stateless")]
    pub fn mrtr_handler<I, F, Fut>(self, handler: F) -> ToolBuilderWithMrtrHandler<I, F>
    where
        I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
        F: Fn(RequestContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<RequestOutcome<CallToolResult>>> + Send + 'static,
    {
        ToolBuilderWithMrtrHandler {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema_override: self.input_schema_override,
            icons: self.icons,
            annotations: self.annotations,
            task_support: self.task_support,
            handler,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Create a tool using the extractor pattern.
    ///
    /// This method provides an axum-inspired way to define handlers where state,
    /// context, and input are extracted declaratively from function parameters.
    /// This reduces the combinatorial explosion of handler variants like
    /// `handler_with_state`, `handler_with_context`, etc.
    ///
    /// # Schema Auto-Detection
    ///
    /// When a [`Json<T>`](crate::extract::Json) extractor is used, the proper JSON
    /// schema is automatically generated from `T`'s `JsonSchema` implementation.
    /// No turbofish is needed -- the schema type is inferred from the closure
    /// parameters.
    ///
    /// # Extractors
    ///
    /// Built-in extractors available in [`crate::extract`]:
    /// - [`Json<T>`](crate::extract::Json) - Deserialize JSON arguments to type `T`
    /// - [`State<T>`](crate::extract::State) - Extract cloned state
    /// - [`Extension<T>`](crate::extract::Extension) - Extract router-level state
    /// - [`Context`](crate::extract::Context) - Extract request context
    /// - [`RawArgs`](crate::extract::RawArgs) - Extract raw JSON arguments
    ///
    /// # Per-Tool Middleware
    ///
    /// The returned builder supports `.layer()` to apply Tower middleware:
    ///
    /// ```rust
    /// use std::sync::Arc;
    /// use std::time::Duration;
    /// use tower::timeout::TimeoutLayer;
    /// use tower_mcp::{ToolBuilder, CallToolResult};
    /// use tower_mcp::extract::{Json, State};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Clone)]
    /// struct Database { url: String }
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct QueryInput { query: String }
    ///
    /// let db = Arc::new(Database { url: "postgres://...".to_string() });
    ///
    /// let tool = ToolBuilder::new("search")
    ///     .description("Search the database")
    ///     .extractor_handler(db, |
    ///         State(db): State<Arc<Database>>,
    ///         Json(input): Json<QueryInput>,
    ///     | async move {
    ///         Ok(CallToolResult::text(format!("Searched {} with: {}", db.url, input.query)))
    ///     })
    ///     .layer(TimeoutLayer::new(Duration::from_secs(30)))
    ///     .build();
    /// ```
    ///
    /// # Example
    ///
    /// ```rust
    /// use std::sync::Arc;
    /// use tower_mcp::{ToolBuilder, CallToolResult};
    /// use tower_mcp::extract::{Json, State, Context};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Clone)]
    /// struct Database { url: String }
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct QueryInput { query: String }
    ///
    /// let db = Arc::new(Database { url: "postgres://...".to_string() });
    ///
    /// let tool = ToolBuilder::new("search")
    ///     .description("Search the database")
    ///     .extractor_handler(db, |
    ///         State(db): State<Arc<Database>>,
    ///         ctx: Context,
    ///         Json(input): Json<QueryInput>,
    ///     | async move {
    ///         if ctx.is_cancelled() {
    ///             return Ok(CallToolResult::error("Cancelled"));
    ///         }
    ///         ctx.report_progress(0.5, Some(1.0), Some("Searching...")).await;
    ///         Ok(CallToolResult::text(format!("Searched {} with: {}", db.url, input.query)))
    ///     })
    ///     .build();
    /// ```
    ///
    /// # Type Inference
    ///
    /// The compiler infers extractor types from the function signature. Make sure
    /// to annotate the extractor types explicitly in the closure parameters.
    pub fn extractor_handler<S, F, T>(
        self,
        state: S,
        handler: F,
    ) -> crate::extract::ToolBuilderWithExtractor<S, F, T>
    where
        S: Clone + Send + Sync + 'static,
        F: crate::extract::ExtractorHandler<S, T> + Clone,
        T: Send + Sync + 'static,
    {
        let input_schema = ensure_object_schema(
            self.input_schema_override
                .unwrap_or_else(|| F::input_schema()),
        );
        crate::extract::ToolBuilderWithExtractor {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            task_support: self.task_support,
            state,
            handler,
            input_schema,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Create a tool using the extractor pattern with typed JSON input.
    ///
    /// # Deprecated
    ///
    /// Use [`extractor_handler`](Self::extractor_handler) instead. It auto-detects
    /// the JSON schema from `Json<T>` extractors, producing identical results
    /// without requiring a turbofish.
    ///
    /// ```rust
    /// # use std::sync::Arc;
    /// # use tower_mcp::{ToolBuilder, CallToolResult};
    /// # use tower_mcp::extract::{Json, State};
    /// # use schemars::JsonSchema;
    /// # use serde::Deserialize;
    /// # #[derive(Clone)]
    /// # struct AppState { prefix: String }
    /// # #[derive(Debug, Deserialize, JsonSchema)]
    /// # struct GreetInput { name: String }
    /// # let state = Arc::new(AppState { prefix: "Hello".to_string() });
    /// // Before (deprecated):
    /// // .extractor_handler_typed::<_, _, _, GreetInput>(state, handler)
    ///
    /// // After:
    /// let tool = ToolBuilder::new("greet")
    ///     .description("Greet someone")
    ///     .extractor_handler(state, |
    ///         State(app): State<Arc<AppState>>,
    ///         Json(input): Json<GreetInput>,
    ///     | async move {
    ///         Ok(CallToolResult::text(format!("{}, {}!", app.prefix, input.name)))
    ///     })
    ///     .build();
    /// ```
    #[deprecated(
        since = "0.8.0",
        note = "Use `extractor_handler` instead -- it auto-detects JSON schema from `Json<T>` extractors without requiring a turbofish"
    )]
    #[allow(deprecated)]
    pub fn extractor_handler_typed<S, F, T, I>(
        self,
        state: S,
        handler: F,
    ) -> crate::extract::ToolBuilderWithTypedExtractor<S, F, T, I>
    where
        S: Clone + Send + Sync + 'static,
        F: crate::extract::TypedExtractorHandler<S, T, I> + Clone,
        T: Send + Sync + 'static,
        I: schemars::JsonSchema + Send + Sync + 'static,
    {
        crate::extract::ToolBuilderWithTypedExtractor {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema_override: self.input_schema_override,
            icons: self.icons,
            annotations: self.annotations,
            task_support: self.task_support,
            state,
            handler,
            _phantom: std::marker::PhantomData,
        }
    }
}

/// Handler for tools with no parameters.
///
/// Used internally by [`ToolBuilder::no_params_handler`].
struct NoParamsTypedHandler<F> {
    handler: F,
}

impl<F, Fut> ToolHandler for NoParamsTypedHandler<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
{
    fn call(&self, _args: Value) -> BoxFuture<'_, Result<CallToolResult>> {
        Box::pin(async move { (self.handler)().await })
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({ "type": "object" })
    }
}

/// Builder state after handler is specified
#[doc(hidden)]
pub struct ToolBuilderWithHandler<I, F> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    input_schema_override: Option<Value>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
    task_support: TaskSupportMode,
    task_preparer: Option<Arc<dyn TaskPreparer>>,
    handler: F,
    _phantom: std::marker::PhantomData<I>,
}

/// Builder state for an SEP-2322-capable tool handler.
#[cfg(feature = "stateless")]
#[doc(hidden)]
pub struct ToolBuilderWithMrtrHandler<I, F> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    input_schema_override: Option<Value>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
    task_support: TaskSupportMode,
    handler: F,
    _phantom: std::marker::PhantomData<I>,
}

/// Builder state after a live task handler is set.
///
/// A live handler owns its execution, so it is only meaningful as a task and
/// the tool is registered with [`TaskSupportMode::Required`] (#1246).
/// Builder state after a live task handler is set.
///
/// Both registration forms build their adapter here and store one boxed
/// handler, so they cannot drift and `build` has a single path.
pub struct ToolBuilderWithLiveHandler {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    input_schema: Value,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
    handler: Arc<dyn LiveToolHandler>,
}

impl ToolBuilderWithLiveHandler {
    /// Finish the tool.
    pub fn build(self) -> Tool {
        Tool {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            meta: None,
            // A live handler cannot run without a task to park against, so
            // requiring one is the honest contract rather than silently
            // degrading when a client does not ask for a task.
            task_support: TaskSupportMode::Required,
            required_client_capabilities: None,
            task_preparer: None,
            service: None,
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
            live_handler: Some(self.handler),
            input_schema: ensure_object_schema(self.input_schema),
        }
    }

    /// Add a synchronous fallback for calls that do not negotiate Tasks.
    ///
    /// [`live_task_handler`](ToolBuilder::live_task_handler) alone forces
    /// [`TaskSupportMode::Required`]: a live handler has nothing to run for a
    /// call that never becomes a task, so the tool is invisible to a client
    /// that has not negotiated Tasks. Adding a fallback here changes that:
    /// the tool is registered as [`TaskSupportMode::Optional`] instead
    /// (still overridable with
    /// [`task_support`](ToolBuilderWithLiveAndFallback::task_support)), the
    /// live handler runs for a task-backed call, and this fallback runs for
    /// an ordinary one -- same schema, same name, whichever path the caller
    /// actually used (#1246).
    ///
    /// ```rust,no_run
    /// use tower_mcp::{CallToolResult, TaskContext, TaskOutcome, ToolBuilder};
    /// use serde::Deserialize;
    /// use tower_mcp::schemars::JsonSchema;
    ///
    /// #[derive(Deserialize, JsonSchema)]
    /// struct Run { prompt: String }
    ///
    /// let tool = ToolBuilder::new("run")
    ///     .description("Live when Tasks are negotiated, synchronous otherwise")
    ///     .live_task_handler(|task: TaskContext, input: Run| async move {
    ///         let _ = (task, input);
    ///         Ok(TaskOutcome::Completed(CallToolResult::text("done")))
    ///     })
    ///     .fallback_handler(|input: Run| async move {
    ///         Ok(CallToolResult::text(format!("synchronous: {}", input.prompt)))
    ///     })
    ///     .build();
    /// ```
    ///
    /// # No shared handler logic
    ///
    /// The two closures are independent rather than one shape reused twice.
    /// A live handler owns a future that stays alive across input rounds; a
    /// fallback returns once. Unifying them would mean either allocating
    /// task machinery a synchronous call never uses, or running a task to
    /// completion inside a single request -- so this asks for both instead of
    /// forcing a false unification.
    pub fn fallback_handler<I, F, Fut>(self, handler: F) -> ToolBuilderWithLiveAndFallback
    where
        I: DeserializeOwned + Send + Sync + 'static,
        F: Fn(I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
    {
        let service = ToolHandlerService::new(LiveFallbackHandler {
            handler,
            _phantom: std::marker::PhantomData::<fn() -> I>,
        });
        let caught = ToolCatchError::new(service);
        ToolBuilderWithLiveAndFallback {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema: self.input_schema,
            icons: self.icons,
            annotations: self.annotations,
            task_support: TaskSupportMode::Optional,
            live_handler: self.handler,
            fallback: BoxCloneService::new(caught),
        }
    }

    /// Add an SEP-2322 MRTR fallback for calls that do not negotiate Tasks.
    ///
    /// Same routing as [`fallback_handler`](Self::fallback_handler): this
    /// runs when a call is not task-backed, the live handler runs when it is.
    /// Unlike the plain fallback, this one can ask for input itself and
    /// resume from [`RequestContext::input_responses`] on the client's
    /// retry, which matters when the non-task caller also needs a
    /// multi-round interaction rather than a single synchronous result.
    ///
    /// A [`RequestOutcome::InputRequired`] result from this fallback is only
    /// accepted by the router from a 2026-07-28 caller (a pre-existing rule,
    /// not new here -- see `validate_input_required_result` in `router.rs`).
    /// A legacy 2025-11-25 caller can still reach this fallback, but only the
    /// [`RequestOutcome::Complete`] branch is usable for it.
    ///
    /// ```rust,no_run
    /// use tower_mcp::{
    ///     CallToolResult, RequestContext, RequestOutcome, TaskContext, TaskOutcome, ToolBuilder,
    /// };
    /// use serde::Deserialize;
    /// use tower_mcp::schemars::JsonSchema;
    ///
    /// #[derive(Deserialize, JsonSchema)]
    /// struct Run { prompt: String }
    ///
    /// let tool = ToolBuilder::new("run")
    ///     .description("Live when Tasks are negotiated, MRTR otherwise")
    ///     .live_task_handler(|task: TaskContext, input: Run| async move {
    ///         let _ = (task, input);
    ///         Ok(TaskOutcome::Completed(CallToolResult::text("done")))
    ///     })
    ///     .fallback_mrtr_handler(|ctx: RequestContext, input: Run| async move {
    ///         let _ = input;
    ///         if ctx.input_responses().is_some() {
    ///             return Ok(RequestOutcome::Complete(CallToolResult::text("resumed")));
    ///         }
    ///         Ok(RequestOutcome::Complete(CallToolResult::text("done")))
    ///     })
    ///     .build();
    /// ```
    #[cfg(feature = "stateless")]
    pub fn fallback_mrtr_handler<I, F, Fut>(self, handler: F) -> ToolBuilderWithLiveAndMrtrFallback
    where
        I: DeserializeOwned + Send + Sync + 'static,
        F: Fn(RequestContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<RequestOutcome<CallToolResult>>> + Send + 'static,
    {
        ToolBuilderWithLiveAndMrtrFallback {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema: self.input_schema,
            icons: self.icons,
            annotations: self.annotations,
            task_support: TaskSupportMode::Optional,
            live_handler: self.handler,
            fallback: Arc::new(LiveFallbackMrtrHandler {
                handler,
                _phantom: std::marker::PhantomData::<fn() -> I>,
            }),
        }
    }
}

/// A synchronous fallback for a tool that also has a live handler.
///
/// Unlike [`TypedHandler`], the input type does not need [`JsonSchema`]: the
/// schema is already fixed by the live handler this fallback is attached to,
/// so there is nothing left for this adapter to derive it from (#1246).
struct LiveFallbackHandler<I, F> {
    handler: F,
    _phantom: std::marker::PhantomData<fn() -> I>,
}

impl<I, F, Fut> ToolHandler for LiveFallbackHandler<I, F>
where
    I: DeserializeOwned + Send + Sync + 'static,
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
{
    fn call(&self, args: Value) -> BoxFuture<'_, Result<CallToolResult>> {
        Box::pin(async move {
            let input: I = match serde_json::from_value(args) {
                Ok(input) => input,
                Err(e) => return Ok(CallToolResult::error(format!("Invalid input: {e}"))),
            };
            (self.handler)(input).await
        })
    }

    fn input_schema(&self) -> Value {
        // Never consulted: `ToolBuilderWithLiveAndFallback::build` uses the
        // schema already fixed by the live handler rather than asking this
        // adapter to derive one.
        serde_json::json!({ "type": "object" })
    }
}

/// An MRTR fallback for a tool that also has a live handler. Same relaxed
/// bound as [`LiveFallbackHandler`], for the same reason.
#[cfg(feature = "stateless")]
struct LiveFallbackMrtrHandler<I, F> {
    handler: F,
    _phantom: std::marker::PhantomData<fn() -> I>,
}

#[cfg(feature = "stateless")]
impl<I, F, Fut> MrtrToolHandler for LiveFallbackMrtrHandler<I, F>
where
    I: DeserializeOwned + Send + Sync + 'static,
    F: Fn(RequestContext, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RequestOutcome<CallToolResult>>> + Send + 'static,
{
    fn call(
        &self,
        ctx: RequestContext,
        args: Value,
    ) -> BoxFuture<'_, Result<RequestOutcome<CallToolResult>>> {
        Box::pin(async move {
            let input: I = serde_json::from_value(args)
                .map_err(|error| Error::invalid_params(format!("Invalid input: {error}")))?;
            (self.handler)(ctx, input).await
        })
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({ "type": "object" })
    }
}

/// Builder state after a live handler and a synchronous fallback have both
/// been set.
///
/// Created by [`ToolBuilderWithLiveHandler::fallback_handler`]. `task_support`
/// defaults to [`TaskSupportMode::Optional`] because a fallback now exists;
/// override with [`task_support`](Self::task_support) if the tool should
/// stay [`TaskSupportMode::Required`] anyway (#1246).
pub struct ToolBuilderWithLiveAndFallback {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    input_schema: Value,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
    task_support: TaskSupportMode,
    live_handler: Arc<dyn LiveToolHandler>,
    fallback: BoxToolService,
}

impl ToolBuilderWithLiveAndFallback {
    /// Override the default [`TaskSupportMode::Optional`].
    pub fn task_support(mut self, mode: TaskSupportMode) -> Self {
        self.task_support = mode;
        self
    }

    /// Finish the tool.
    pub fn build(self) -> Tool {
        Tool {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            meta: None,
            task_support: self.task_support,
            required_client_capabilities: None,
            task_preparer: None,
            service: Some(self.fallback),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
            live_handler: Some(self.live_handler),
            input_schema: ensure_object_schema(self.input_schema),
        }
    }
}

/// Builder state after a live handler and an MRTR fallback have both been
/// set.
///
/// Created by [`ToolBuilderWithLiveHandler::fallback_mrtr_handler`]. Same
/// `task_support` default and override as
/// [`ToolBuilderWithLiveAndFallback`].
#[cfg(feature = "stateless")]
pub struct ToolBuilderWithLiveAndMrtrFallback {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    input_schema: Value,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
    task_support: TaskSupportMode,
    live_handler: Arc<dyn LiveToolHandler>,
    fallback: Arc<dyn MrtrToolHandler>,
}

#[cfg(feature = "stateless")]
impl ToolBuilderWithLiveAndMrtrFallback {
    /// Override the default [`TaskSupportMode::Optional`].
    pub fn task_support(mut self, mode: TaskSupportMode) -> Self {
        self.task_support = mode;
        self
    }

    /// Finish the tool.
    pub fn build(self) -> Tool {
        Tool {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            meta: None,
            task_support: self.task_support,
            required_client_capabilities: None,
            task_preparer: None,
            service: None,
            mrtr_handler: Some(self.fallback),
            live_handler: Some(self.live_handler),
            input_schema: ensure_object_schema(self.input_schema),
        }
    }
}

/// Builder state after a layer has been applied to an MRTR handler.
#[cfg(feature = "stateless")]
#[doc(hidden)]
pub struct ToolBuilderWithMrtrLayer<I, F, L> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    input_schema_override: Option<Value>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
    task_support: TaskSupportMode,
    handler: F,
    layer: L,
    _phantom: std::marker::PhantomData<I>,
}

/// Builder state for tools with no parameters.
///
/// Created by [`ToolBuilder::no_params_handler`].
#[doc(hidden)]
pub struct ToolBuilderWithNoParamsHandler<F> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    input_schema_override: Option<Value>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
    task_support: TaskSupportMode,
    handler: F,
}

impl<F, Fut> ToolBuilderWithNoParamsHandler<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
{
    /// Build the tool.
    pub fn build(self) -> Tool {
        Tool::from_handler(
            self.name,
            self.title,
            self.description,
            self.output_schema,
            self.icons,
            self.annotations,
            self.task_support,
            self.input_schema_override,
            NoParamsTypedHandler {
                handler: self.handler,
            },
        )
    }

    /// Apply a Tower layer (middleware) to this tool.
    ///
    /// See [`ToolBuilderWithHandler::layer`] for details.
    pub fn layer<L>(self, layer: L) -> ToolBuilderWithNoParamsHandlerLayer<F, L> {
        ToolBuilderWithNoParamsHandlerLayer {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema_override: self.input_schema_override,
            icons: self.icons,
            annotations: self.annotations,
            task_support: self.task_support,
            handler: self.handler,
            layer,
        }
    }

    /// Apply a guard to this tool.
    ///
    /// See [`ToolBuilderWithHandler::guard`] for details.
    pub fn guard<G>(self, guard: G) -> ToolBuilderWithNoParamsHandlerLayer<F, GuardLayer<G>>
    where
        G: Fn(&ToolRequest) -> std::result::Result<(), String> + Clone + Send + Sync + 'static,
    {
        self.layer(GuardLayer::new(guard))
    }
}

/// Builder state after a layer has been applied to a no-params handler.
#[doc(hidden)]
pub struct ToolBuilderWithNoParamsHandlerLayer<F, L> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    input_schema_override: Option<Value>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
    task_support: TaskSupportMode,
    handler: F,
    layer: L,
}

#[allow(private_bounds)]
impl<F, Fut, L> ToolBuilderWithNoParamsHandlerLayer<F, L>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
    L: tower::Layer<ToolHandlerService<NoParamsTypedHandler<F>>> + Clone + Send + Sync + 'static,
    L::Service: Service<ToolRequest, Response = CallToolResult> + Clone + Send + 'static,
    <L::Service as Service<ToolRequest>>::Error: fmt::Display + Send,
    <L::Service as Service<ToolRequest>>::Future: Send,
{
    /// Build the tool with the applied layer(s).
    pub fn build(self) -> Tool {
        let input_schema = ensure_object_schema(
            self.input_schema_override
                .unwrap_or_else(|| serde_json::json!({ "type": "object" })),
        );

        let handler_service = ToolHandlerService::new(NoParamsTypedHandler {
            handler: self.handler,
        });
        let layered = self.layer.layer(handler_service);
        let catch_error = ToolCatchError::new(layered);
        let service = BoxCloneService::new(catch_error);

        Tool {
            live_handler: None,
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            meta: None,
            task_support: self.task_support,
            required_client_capabilities: None,
            task_preparer: None,
            service: Some(service),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
            input_schema,
        }
    }

    /// Apply an additional Tower layer (middleware).
    pub fn layer<L2>(
        self,
        layer: L2,
    ) -> ToolBuilderWithNoParamsHandlerLayer<F, tower::layer::util::Stack<L2, L>> {
        ToolBuilderWithNoParamsHandlerLayer {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema_override: self.input_schema_override,
            icons: self.icons,
            annotations: self.annotations,
            task_support: self.task_support,
            handler: self.handler,
            layer: tower::layer::util::Stack::new(layer, self.layer),
        }
    }

    /// Apply a guard to this tool.
    ///
    /// See [`ToolBuilderWithHandler::guard`] for details.
    pub fn guard<G>(
        self,
        guard: G,
    ) -> ToolBuilderWithNoParamsHandlerLayer<F, tower::layer::util::Stack<GuardLayer<G>, L>>
    where
        G: Fn(&ToolRequest) -> std::result::Result<(), String> + Clone + Send + Sync + 'static,
    {
        self.layer(GuardLayer::new(guard))
    }
}

impl<I, F, Fut> ToolBuilderWithHandler<I, F>
where
    I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
{
    /// Build the tool.
    pub fn build(self) -> Tool {
        let mut tool = Tool::from_handler(
            self.name,
            self.title,
            self.description,
            self.output_schema,
            self.icons,
            self.annotations,
            self.task_support,
            self.input_schema_override,
            TypedHandler {
                handler: self.handler,
                _phantom: std::marker::PhantomData,
            },
        );
        tool.task_preparer = self.task_preparer;
        tool
    }

    /// Add a typed preparation step for task-backed invocations.
    pub fn task_preparation<P, PrepareFuture>(mut self, prepare: P) -> Self
    where
        P: Fn(TaskContext, I) -> PrepareFuture + Send + Sync + 'static,
        PrepareFuture: Future<Output = Result<TaskPreparation>> + Send + 'static,
    {
        self.task_preparer = Some(Arc::new(TypedTaskPreparer {
            prepare,
            _phantom: std::marker::PhantomData,
        }));
        self
    }

    /// Apply a Tower layer (middleware) to this tool.
    ///
    /// The layer wraps the tool's handler service, enabling functionality like
    /// timeouts, rate limiting, and metrics collection at the per-tool level.
    ///
    /// # Example
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use tower::timeout::TimeoutLayer;
    /// use tower_mcp::{ToolBuilder, CallToolResult};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct Input { query: String }
    ///
    /// let tool = ToolBuilder::new("search")
    ///     .description("Search with timeout")
    ///     .handler(|input: Input| async move {
    ///         Ok(CallToolResult::text("result"))
    ///     })
    ///     .layer(TimeoutLayer::new(Duration::from_secs(30)))
    ///     .build();
    /// ```
    pub fn layer<L>(self, layer: L) -> ToolBuilderWithLayer<I, F, L> {
        ToolBuilderWithLayer {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema_override: self.input_schema_override,
            icons: self.icons,
            annotations: self.annotations,
            task_support: self.task_support,
            task_preparer: self.task_preparer,
            handler: self.handler,
            layer,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Apply a guard to this tool.
    ///
    /// The guard runs before the handler and can short-circuit with an error
    /// message. This is syntactic sugar for `.layer(GuardLayer::new(f))`.
    ///
    /// See [`GuardLayer`] for a full example.
    pub fn guard<G>(self, guard: G) -> ToolBuilderWithLayer<I, F, GuardLayer<G>>
    where
        G: Fn(&ToolRequest) -> std::result::Result<(), String> + Clone + Send + Sync + 'static,
    {
        self.layer(GuardLayer::new(guard))
    }
}

#[cfg(feature = "stateless")]
impl<I, F, Fut> ToolBuilderWithMrtrHandler<I, F>
where
    I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    F: Fn(RequestContext, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RequestOutcome<CallToolResult>>> + Send + 'static,
{
    /// Build the MRTR-capable tool.
    pub fn build(self) -> Tool {
        Tool::from_mrtr_handler(
            self.name,
            self.title,
            self.description,
            self.output_schema,
            self.icons,
            self.annotations,
            self.task_support,
            self.input_schema_override,
            TypedMrtrHandler {
                handler: self.handler,
                _phantom: std::marker::PhantomData,
            },
        )
    }

    /// Apply a Tower layer to every attempt at this MRTR-capable tool.
    ///
    /// Each MRTR retry is an independent request, so the layer runs once per
    /// round. Middleware failures become complete tool error results, matching
    /// the behavior of layers on non-MRTR tools.
    pub fn layer<L>(self, layer: L) -> ToolBuilderWithMrtrLayer<I, F, L> {
        ToolBuilderWithMrtrLayer {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema_override: self.input_schema_override,
            icons: self.icons,
            annotations: self.annotations,
            task_support: self.task_support,
            handler: self.handler,
            layer,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Apply a guard to every attempt at this MRTR-capable tool.
    pub fn guard<G>(self, guard: G) -> ToolBuilderWithMrtrLayer<I, F, GuardLayer<G>>
    where
        G: Fn(&ToolRequest) -> std::result::Result<(), String> + Clone + Send + Sync + 'static,
    {
        self.layer(GuardLayer::new(guard))
    }
}

#[cfg(feature = "stateless")]
#[allow(private_bounds)]
impl<I, F, Fut, L> ToolBuilderWithMrtrLayer<I, F, L>
where
    I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    F: Fn(RequestContext, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RequestOutcome<CallToolResult>>> + Send + 'static,
    L: tower::Layer<MrtrToolHandlerService<TypedMrtrHandler<I, F>>> + Clone + Send + Sync + 'static,
    L::Service:
        Service<ToolRequest, Response = RequestOutcome<CallToolResult>> + Clone + Send + 'static,
    <L::Service as Service<ToolRequest>>::Error: fmt::Display + Send + 'static,
    <L::Service as Service<ToolRequest>>::Future: Send + 'static,
{
    /// Build the MRTR-capable tool with the applied layer(s).
    pub fn build(self) -> Tool {
        let input_schema = self.input_schema_override.unwrap_or_else(|| {
            let schema = schemars::schema_for!(I);
            serde_json::to_value(schema).unwrap_or_else(|_| serde_json::json!({ "type": "object" }))
        });
        let input_schema = ensure_object_schema(input_schema);
        let service = MrtrToolHandlerService::new(TypedMrtrHandler {
            handler: self.handler,
            _phantom: std::marker::PhantomData,
        });
        let service = self.layer.layer(service);
        let service = BoxCloneService::new(MrtrToolCatchError::new(service));

        Tool {
            live_handler: None,
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            meta: None,
            task_support: self.task_support,
            required_client_capabilities: None,
            task_preparer: None,
            service: None,
            mrtr_handler: Some(Arc::new(ServiceMrtrToolHandler {
                service: Mutex::new(service),
                input_schema: input_schema.clone(),
            })),
            input_schema,
        }
    }

    /// Apply an additional Tower layer.
    pub fn layer<L2>(
        self,
        layer: L2,
    ) -> ToolBuilderWithMrtrLayer<I, F, tower::layer::util::Stack<L2, L>> {
        ToolBuilderWithMrtrLayer {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema_override: self.input_schema_override,
            icons: self.icons,
            annotations: self.annotations,
            task_support: self.task_support,
            handler: self.handler,
            layer: tower::layer::util::Stack::new(layer, self.layer),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Apply an additional guard.
    pub fn guard<G>(
        self,
        guard: G,
    ) -> ToolBuilderWithMrtrLayer<I, F, tower::layer::util::Stack<GuardLayer<G>, L>>
    where
        G: Fn(&ToolRequest) -> std::result::Result<(), String> + Clone + Send + Sync + 'static,
    {
        self.layer(GuardLayer::new(guard))
    }
}

/// Builder state after a layer has been applied to the handler.
///
/// This builder allows chaining additional layers and building the final tool.
#[doc(hidden)]
pub struct ToolBuilderWithLayer<I, F, L> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    input_schema_override: Option<Value>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
    task_support: TaskSupportMode,
    task_preparer: Option<Arc<dyn TaskPreparer>>,
    handler: F,
    layer: L,
    _phantom: std::marker::PhantomData<I>,
}

// Allow private_bounds because these internal types (ToolHandlerService, TypedHandler, etc.)
// are implementation details that users don't interact with directly.
#[allow(private_bounds)]
impl<I, F, Fut, L> ToolBuilderWithLayer<I, F, L>
where
    I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
    L: tower::Layer<ToolHandlerService<TypedHandler<I, F>>> + Clone + Send + Sync + 'static,
    L::Service: Service<ToolRequest, Response = CallToolResult> + Clone + Send + 'static,
    <L::Service as Service<ToolRequest>>::Error: fmt::Display + Send,
    <L::Service as Service<ToolRequest>>::Future: Send,
{
    /// Build the tool with the applied layer(s).
    pub fn build(self) -> Tool {
        let input_schema = self.input_schema_override.unwrap_or_else(|| {
            let input_schema = schemars::schema_for!(I);
            serde_json::to_value(input_schema)
                .unwrap_or_else(|_| serde_json::json!({ "type": "object" }))
        });
        let input_schema = ensure_object_schema(input_schema);

        let handler_service = ToolHandlerService::new(TypedHandler {
            handler: self.handler,
            _phantom: std::marker::PhantomData,
        });
        let layered = self.layer.layer(handler_service);
        let catch_error = ToolCatchError::new(layered);
        let service = BoxCloneService::new(catch_error);

        Tool {
            live_handler: None,
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            meta: None,
            task_support: self.task_support,
            required_client_capabilities: None,
            task_preparer: self.task_preparer,
            service: Some(service),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
            input_schema,
        }
    }

    /// Apply an additional Tower layer (middleware).
    ///
    /// Layers are applied in order, with earlier layers wrapping later ones.
    /// This means the first layer added is the outermost middleware.
    pub fn layer<L2>(
        self,
        layer: L2,
    ) -> ToolBuilderWithLayer<I, F, tower::layer::util::Stack<L2, L>> {
        ToolBuilderWithLayer {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            input_schema_override: self.input_schema_override,
            icons: self.icons,
            annotations: self.annotations,
            task_support: self.task_support,
            task_preparer: self.task_preparer,
            handler: self.handler,
            layer: tower::layer::util::Stack::new(layer, self.layer),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Apply a guard to this tool.
    ///
    /// See [`ToolBuilderWithHandler::guard`] for details.
    pub fn guard<G>(
        self,
        guard: G,
    ) -> ToolBuilderWithLayer<I, F, tower::layer::util::Stack<GuardLayer<G>, L>>
    where
        G: Fn(&ToolRequest) -> std::result::Result<(), String> + Clone + Send + Sync + 'static,
    {
        self.layer(GuardLayer::new(guard))
    }
}

// =============================================================================
// Handler implementations
// =============================================================================

/// Handler that deserializes input to a specific type
struct TypedHandler<I, F> {
    handler: F,
    _phantom: std::marker::PhantomData<I>,
}

#[cfg(feature = "stateless")]
struct TypedMrtrHandler<I, F> {
    handler: F,
    _phantom: std::marker::PhantomData<I>,
}

#[cfg(feature = "stateless")]
impl<I, F, Fut> MrtrToolHandler for TypedMrtrHandler<I, F>
where
    I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    F: Fn(RequestContext, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RequestOutcome<CallToolResult>>> + Send + 'static,
{
    fn call(
        &self,
        ctx: RequestContext,
        args: Value,
    ) -> BoxFuture<'_, Result<RequestOutcome<CallToolResult>>> {
        Box::pin(async move {
            let input: I = serde_json::from_value(args)
                .map_err(|error| Error::invalid_params(format!("Invalid input: {error}")))?;
            (self.handler)(ctx, input).await
        })
    }

    fn input_schema(&self) -> Value {
        let schema = schemars::schema_for!(I);
        ensure_object_schema(
            serde_json::to_value(schema)
                .unwrap_or_else(|_| serde_json::json!({ "type": "object" })),
        )
    }
}

impl<I, F, Fut> ToolHandler for TypedHandler<I, F>
where
    I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
{
    fn call(&self, args: Value) -> BoxFuture<'_, Result<CallToolResult>> {
        Box::pin(async move {
            let input: I = match serde_json::from_value(args) {
                Ok(input) => input,
                Err(e) => return Ok(CallToolResult::error(format!("Invalid input: {e}"))),
            };
            (self.handler)(input).await
        })
    }

    fn input_schema(&self) -> Value {
        let schema = schemars::schema_for!(I);
        let schema = serde_json::to_value(schema).unwrap_or_else(|_| {
            serde_json::json!({
                "type": "object"
            })
        });
        ensure_object_schema(schema)
    }
}

// =============================================================================
// Trait-based tool definition
// =============================================================================

/// Trait for defining tools with full control
///
/// Implement this trait when you need more control than the builder provides,
/// or when you want to define tools as standalone types.
///
/// # Example
///
/// ```rust
/// use tower_mcp::{McpTool, Result};
/// use schemars::JsonSchema;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Debug, Deserialize, JsonSchema)]
/// struct AddInput {
///     a: i64,
///     b: i64,
/// }
///
/// struct AddTool;
///
/// impl McpTool for AddTool {
///     const NAME: &'static str = "add";
///     const DESCRIPTION: &'static str = "Add two numbers";
///
///     type Input = AddInput;
///     type Output = i64;
///
///     async fn call(&self, input: Self::Input) -> Result<Self::Output> {
///         Ok(input.a + input.b)
///     }
/// }
///
/// let tool = AddTool.into_tool();
/// assert_eq!(tool.name, "add");
/// ```
pub trait McpTool: Send + Sync + 'static {
    /// The tool name (must be unique within the router).
    const NAME: &'static str;
    /// A human-readable description of the tool.
    const DESCRIPTION: &'static str;

    /// The input type, deserialized from tool call arguments.
    type Input: JsonSchema + DeserializeOwned + Send;
    /// The output type, serialized into the tool call result.
    type Output: Serialize + Send;

    /// Execute the tool with the given input.
    fn call(&self, input: Self::Input) -> impl Future<Output = Result<Self::Output>> + Send;

    /// Optional annotations for the tool
    fn annotations(&self) -> Option<ToolAnnotations> {
        None
    }

    /// Convert to a [`Tool`] instance.
    ///
    /// # Panics
    ///
    /// Panics if [`NAME`](Self::NAME) is not a valid tool name. Since `NAME`
    /// is a `&'static str`, invalid names are caught immediately during
    /// development.
    fn into_tool(self) -> Tool
    where
        Self: Sized,
    {
        if let Err(e) = validate_tool_name(Self::NAME) {
            panic!("{e}");
        }
        let annotations = self.annotations();
        let tool = Arc::new(self);
        Tool::from_handler(
            Self::NAME.to_string(),
            None,
            Some(Self::DESCRIPTION.to_string()),
            None,
            None,
            annotations,
            TaskSupportMode::default(),
            None,
            McpToolHandler { tool },
        )
    }
}

/// Wrapper to make McpTool implement ToolHandler
struct McpToolHandler<T: McpTool> {
    tool: Arc<T>,
}

impl<T: McpTool> ToolHandler for McpToolHandler<T> {
    fn call(&self, args: Value) -> BoxFuture<'_, Result<CallToolResult>> {
        let tool = self.tool.clone();
        Box::pin(async move {
            let input: T::Input = match serde_json::from_value(args) {
                Ok(input) => input,
                Err(e) => return Ok(CallToolResult::error(format!("Invalid input: {e}"))),
            };
            let output = tool.call(input).await?;
            let value = serde_json::to_value(output).tool_context("Failed to serialize output")?;
            Ok(CallToolResult::json(value))
        })
    }

    fn input_schema(&self) -> Value {
        let schema = schemars::schema_for!(T::Input);
        let schema = serde_json::to_value(schema).unwrap_or_else(|_| {
            serde_json::json!({
                "type": "object"
            })
        });
        ensure_object_schema(schema)
    }
}

mod service;
mod task;

// Re-exported at the visibility each item already had, so every path that
// resolved before still resolves and nothing widens on the way out.
pub use service::{GuardLayer, GuardService, ToolCatchError};
use task::TypedTaskPreparer;
pub(crate) use task::{LiveTask, TaskPreparer};
pub use task::{PendingInput, TaskContext, TaskOutcome, TaskPreparation};

// Gated where they are defined, so importing them unconditionally would break
// every build that is not `--all-features`.
#[cfg(feature = "stateless")]
use service::MrtrToolCatchError;

#[cfg(test)]
mod tests;
