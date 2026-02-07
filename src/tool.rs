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

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tower::util::BoxCloneService;
use tower_service::Service;

use crate::context::RequestContext;
use crate::error::{Error, Result, ResultExt};
use crate::protocol::{CallToolResult, ToolAnnotations, ToolDefinition, ToolIcon};

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

/// Catches errors from the inner service and converts them to `CallToolResult::error()`.
///
/// This wrapper ensures that middleware errors (e.g., timeouts, rate limits)
/// and handler errors are converted to tool-level error responses with
/// `is_error: true`, rather than propagating as Tower service errors.
pub struct ToolCatchError<S> {
    inner: S,
}

impl<S> ToolCatchError<S> {
    /// Create a new `ToolCatchError` wrapping the given service.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S: Clone> Clone for ToolCatchError<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: fmt::Debug> fmt::Debug for ToolCatchError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCatchError")
            .field("inner", &self.inner)
            .finish()
    }
}

impl<S> Service<ToolRequest> for ToolCatchError<S>
where
    S: Service<ToolRequest, Response = CallToolResult> + Clone + Send + 'static,
    S::Error: fmt::Display + Send,
    S::Future: Send,
{
    type Response = CallToolResult;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<CallToolResult, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        // Map any readiness error to Infallible (we catch it on call)
        match self.inner.poll_ready(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(_)) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, req: ToolRequest) -> Self::Future {
        let fut = self.inner.call(req);

        Box::pin(async move {
            match fut.await {
                Ok(result) => Ok(result),
                Err(err) => Ok(CallToolResult::error(err.to_string())),
            }
        })
    }
}

/// A tower [`Layer`](tower::Layer) that applies a guard function before the inner service.
///
/// Guards run before the tool handler and can short-circuit with an error message.
/// Use via [`ToolBuilderWithHandler::guard`] or [`Tool::with_guard`] rather than
/// constructing directly.
///
/// # Example
///
/// ```rust
/// use tower_mcp::{ToolBuilder, ToolRequest, CallToolResult};
/// use schemars::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(Debug, Deserialize, JsonSchema)]
/// struct DeleteInput { id: String, confirm: bool }
///
/// let tool = ToolBuilder::new("delete")
///     .description("Delete a record")
///     .handler(|input: DeleteInput| async move {
///         Ok(CallToolResult::text(format!("deleted {}", input.id)))
///     })
///     .guard(|req: &ToolRequest| {
///         let confirm = req.args.get("confirm").and_then(|v| v.as_bool()).unwrap_or(false);
///         if !confirm {
///             return Err("Must set confirm=true to delete".to_string());
///         }
///         Ok(())
///     })
///     .build();
/// ```
#[derive(Clone)]
pub struct GuardLayer<G> {
    guard: G,
}

impl<G> GuardLayer<G> {
    /// Create a new guard layer from a closure.
    ///
    /// The closure receives a `&ToolRequest` and returns `Ok(())` to proceed
    /// or `Err(String)` to reject with an error message.
    pub fn new(guard: G) -> Self {
        Self { guard }
    }
}

impl<G, S> tower::Layer<S> for GuardLayer<G>
where
    G: Clone,
{
    type Service = GuardService<G, S>;

    fn layer(&self, inner: S) -> Self::Service {
        GuardService {
            guard: self.guard.clone(),
            inner,
        }
    }
}

/// Service wrapper that runs a guard check before calling the inner service.
///
/// Created by [`GuardLayer`]. See its documentation for usage.
#[derive(Clone)]
pub struct GuardService<G, S> {
    guard: G,
    inner: S,
}

impl<G, S> Service<ToolRequest> for GuardService<G, S>
where
    G: Fn(&ToolRequest) -> std::result::Result<(), String> + Clone + Send + Sync + 'static,
    S: Service<ToolRequest, Response = CallToolResult> + Clone + Send + 'static,
    S::Error: Into<Error> + Send,
    S::Future: Send,
{
    type Response = CallToolResult;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = std::result::Result<CallToolResult, Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: ToolRequest) -> Self::Future {
        match (self.guard)(&req) {
            Ok(()) => {
                let fut = self.inner.call(req);
                Box::pin(async move { fut.await.map_err(Into::into) })
            }
            Err(msg) => Box::pin(async move { Err(Error::tool(msg)) }),
        }
    }
}

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

/// Validate a tool name according to MCP spec.
///
/// Tool names must be:
/// - 1-128 characters long
/// - Contain only alphanumeric characters, underscores, hyphens, and dots
///
/// Returns `Ok(())` if valid, `Err` with description if invalid.
pub fn validate_tool_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::tool("Tool name cannot be empty"));
    }
    if name.len() > 128 {
        return Err(Error::tool(format!(
            "Tool name '{}' exceeds maximum length of 128 characters (got {})",
            name,
            name.len()
        )));
    }
    if let Some(invalid_char) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-' && *c != '.')
    {
        return Err(Error::tool(format!(
            "Tool name '{}' contains invalid character '{}'. Only alphanumeric, underscore, hyphen, and dot are allowed.",
            name, invalid_char
        )));
    }
    Ok(())
}

/// A boxed future for tool handlers
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

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
    /// Tool name (must be 1-128 chars, alphanumeric/underscore/hyphen/dot only)
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
    /// The boxed service that executes the tool
    pub(crate) service: BoxToolService,
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
            name: self.name.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            output_schema: self.output_schema.clone(),
            icons: self.icons.clone(),
            annotations: self.annotations.clone(),
            service: self.service.clone(),
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
        ToolDefinition {
            name: self.name.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
            icons: self.icons.clone(),
            annotations: self.annotations.clone(),
        }
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
        use tower::ServiceExt;
        let service = self.service.clone();
        Box::pin(async move {
            // ServiceExt::oneshot properly handles poll_ready before call
            // Service is Infallible, so unwrap is safe
            service.oneshot(ToolRequest::new(ctx, args)).await.unwrap()
        })
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
        let guarded = GuardService {
            guard,
            inner: self.service,
        };
        let caught = ToolCatchError::new(guarded);
        Tool {
            service: BoxCloneService::new(caught),
            ..self
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
            name: format!("{}.{}", prefix, self.name),
            title: self.title.clone(),
            description: self.description.clone(),
            output_schema: self.output_schema.clone(),
            icons: self.icons.clone(),
            annotations: self.annotations.clone(),
            service: self.service.clone(),
            input_schema: self.input_schema.clone(),
        }
    }

    /// Create a tool from a handler (internal helper)
    fn from_handler<H: ToolHandler + 'static>(
        name: String,
        title: Option<String>,
        description: Option<String>,
        output_schema: Option<Value>,
        icons: Option<Vec<ToolIcon>>,
        annotations: Option<ToolAnnotations>,
        handler: H,
    ) -> Self {
        let input_schema = handler.input_schema();
        let handler_service = ToolHandlerService::new(handler);
        let catch_error = ToolCatchError::new(handler_service);
        let service = BoxCloneService::new(catch_error);

        Self {
            name,
            title,
            description,
            output_schema,
            icons,
            annotations,
            service,
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
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
}

impl ToolBuilder {
    /// Create a new tool builder with the given name.
    ///
    /// Tool names must be 1-128 characters and contain only alphanumeric
    /// characters, underscores, hyphens, and dots.
    ///
    /// Use [`try_new`](Self::try_new) if the name comes from runtime input.
    ///
    /// # Panics
    ///
    /// Panics if `name` is empty, exceeds 128 characters, or contains
    /// characters other than ASCII alphanumerics, `_`, `-`, and `.`.
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
            icons: None,
            annotations: None,
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
            icons: None,
            annotations: None,
        })
    }

    /// Set a human-readable title for the tool
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the output schema (JSON Schema for structured output)
    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Add an icon for the tool
    pub fn icon(mut self, src: impl Into<String>) -> Self {
        self.icons.get_or_insert_with(Vec::new).push(ToolIcon {
            src: src.into(),
            mime_type: None,
            sizes: None,
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

    /// Mark the tool as idempotent (same args = same effect)
    pub fn idempotent(mut self) -> Self {
        self.annotations
            .get_or_insert_with(ToolAnnotations::default)
            .idempotent_hint = true;
        self
    }

    /// Set tool annotations directly
    pub fn annotations(mut self, annotations: ToolAnnotations) -> Self {
        self.annotations = Some(annotations);
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
            icons: self.icons,
            annotations: self.annotations,
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
            icons: self.icons,
            annotations: self.annotations,
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
    /// This means `extractor_handler` produces the same schema as
    /// `extractor_handler_typed` for the common case, without requiring a turbofish.
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
        crate::extract::ToolBuilderWithExtractor {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            state,
            handler,
            input_schema: F::input_schema(),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Create a tool using the extractor pattern with typed JSON input.
    ///
    /// This is similar to [`extractor_handler`](Self::extractor_handler) but requires
    /// an explicit type parameter for the JSON input type via turbofish syntax.
    ///
    /// Since `extractor_handler` now auto-detects the JSON schema from `Json<T>`
    /// extractors, this method is typically unnecessary. It remains available for
    /// cases where you need explicit control over the schema type parameter.
    ///
    /// # Example
    ///
    /// ```rust
    /// use std::sync::Arc;
    /// use tower_mcp::{ToolBuilder, CallToolResult};
    /// use tower_mcp::extract::{Json, State};
    /// use schemars::JsonSchema;
    /// use serde::Deserialize;
    ///
    /// #[derive(Clone)]
    /// struct AppState { prefix: String }
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct GreetInput { name: String }
    ///
    /// let state = Arc::new(AppState { prefix: "Hello".to_string() });
    ///
    /// let tool = ToolBuilder::new("greet")
    ///     .description("Greet someone")
    ///     .extractor_handler_typed::<_, _, _, GreetInput>(state, |
    ///         State(app): State<Arc<AppState>>,
    ///         Json(input): Json<GreetInput>,
    ///     | async move {
    ///         Ok(CallToolResult::text(format!("{}, {}!", app.prefix, input.name)))
    ///     })
    ///     .build();
    /// ```
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
            icons: self.icons,
            annotations: self.annotations,
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
pub struct ToolBuilderWithHandler<I, F> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
    handler: F,
    _phantom: std::marker::PhantomData<I>,
}

/// Builder state for tools with no parameters.
///
/// Created by [`ToolBuilder::no_params_handler`].
pub struct ToolBuilderWithNoParamsHandler<F> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
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
            icons: self.icons,
            annotations: self.annotations,
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
pub struct ToolBuilderWithNoParamsHandlerLayer<F, L> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
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
        let input_schema = serde_json::json!({ "type": "object" });

        let handler_service = ToolHandlerService::new(NoParamsTypedHandler {
            handler: self.handler,
        });
        let layered = self.layer.layer(handler_service);
        let catch_error = ToolCatchError::new(layered);
        let service = BoxCloneService::new(catch_error);

        Tool {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            service,
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
            icons: self.icons,
            annotations: self.annotations,
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
        Tool::from_handler(
            self.name,
            self.title,
            self.description,
            self.output_schema,
            self.icons,
            self.annotations,
            TypedHandler {
                handler: self.handler,
                _phantom: std::marker::PhantomData,
            },
        )
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
            icons: self.icons,
            annotations: self.annotations,
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

/// Builder state after a layer has been applied to the handler.
///
/// This builder allows chaining additional layers and building the final tool.
pub struct ToolBuilderWithLayer<I, F, L> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ToolAnnotations>,
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
        let input_schema = schemars::schema_for!(I);
        let input_schema = serde_json::to_value(input_schema)
            .unwrap_or_else(|_| serde_json::json!({ "type": "object" }));

        let handler_service = ToolHandlerService::new(TypedHandler {
            handler: self.handler,
            _phantom: std::marker::PhantomData,
        });
        let layered = self.layer.layer(handler_service);
        let catch_error = ToolCatchError::new(layered);
        let service = BoxCloneService::new(catch_error);

        Tool {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            service,
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
            icons: self.icons,
            annotations: self.annotations,
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

impl<I, F, Fut> ToolHandler for TypedHandler<I, F>
where
    I: JsonSchema + DeserializeOwned + Send + Sync + 'static,
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send + 'static,
{
    fn call(&self, args: Value) -> BoxFuture<'_, Result<CallToolResult>> {
        Box::pin(async move {
            let input: I = serde_json::from_value(args).tool_context("Invalid input")?;
            (self.handler)(input).await
        })
    }

    fn input_schema(&self) -> Value {
        let schema = schemars::schema_for!(I);
        serde_json::to_value(schema).unwrap_or_else(|_| {
            serde_json::json!({
                "type": "object"
            })
        })
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
/// use tower_mcp::tool::McpTool;
/// use tower_mcp::error::Result;
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
    const NAME: &'static str;
    const DESCRIPTION: &'static str;

    type Input: JsonSchema + DeserializeOwned + Send;
    type Output: Serialize + Send;

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
            let input: T::Input = serde_json::from_value(args).tool_context("Invalid input")?;
            let output = tool.call(input).await?;
            let value = serde_json::to_value(output).tool_context("Failed to serialize output")?;
            Ok(CallToolResult::json(value))
        })
    }

    fn input_schema(&self) -> Value {
        let schema = schemars::schema_for!(T::Input);
        serde_json::to_value(schema).unwrap_or_else(|_| {
            serde_json::json!({
                "type": "object"
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Context, Json, RawArgs, State};
    use crate::protocol::Content;
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct GreetInput {
        name: String,
    }

    #[tokio::test]
    async fn test_builder_tool() {
        let tool = ToolBuilder::new("greet")
            .description("Greet someone")
            .handler(|input: GreetInput| async move {
                Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
            })
            .build();

        assert_eq!(tool.name, "greet");
        assert_eq!(tool.description.as_deref(), Some("Greet someone"));

        let result = tool.call(serde_json::json!({"name": "World"})).await;

        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_raw_handler() {
        let tool = ToolBuilder::new("echo")
            .description("Echo input")
            .extractor_handler((), |RawArgs(args): RawArgs| async move {
                Ok(CallToolResult::json(args))
            })
            .build();

        let result = tool.call(serde_json::json!({"foo": "bar"})).await;

        assert!(!result.is_error);
    }

    #[test]
    fn test_invalid_tool_name_empty() {
        let err = ToolBuilder::try_new("").err().expect("should fail");
        assert!(err.to_string().contains("cannot be empty"));
    }

    #[test]
    fn test_invalid_tool_name_too_long() {
        let long_name = "a".repeat(129);
        let err = ToolBuilder::try_new(long_name).err().expect("should fail");
        assert!(err.to_string().contains("exceeds maximum"));
    }

    #[test]
    fn test_invalid_tool_name_bad_chars() {
        let err = ToolBuilder::try_new("my tool!").err().expect("should fail");
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    #[should_panic(expected = "cannot be empty")]
    fn test_new_panics_on_empty_name() {
        ToolBuilder::new("");
    }

    #[test]
    #[should_panic(expected = "exceeds maximum")]
    fn test_new_panics_on_too_long_name() {
        ToolBuilder::new("a".repeat(129));
    }

    #[test]
    #[should_panic(expected = "invalid character")]
    fn test_new_panics_on_invalid_chars() {
        ToolBuilder::new("my tool!");
    }

    #[test]
    fn test_valid_tool_names() {
        // All valid characters
        let names = [
            "my_tool",
            "my-tool",
            "my.tool",
            "MyTool123",
            "a",
            &"a".repeat(128),
        ];
        for name in names {
            assert!(
                ToolBuilder::try_new(name).is_ok(),
                "Expected '{}' to be valid",
                name
            );
        }
    }

    #[tokio::test]
    async fn test_context_aware_handler() {
        use crate::context::notification_channel;
        use crate::protocol::{ProgressToken, RequestId};

        #[derive(Debug, Deserialize, JsonSchema)]
        struct ProcessInput {
            count: i32,
        }

        let tool = ToolBuilder::new("process")
            .description("Process with context")
            .extractor_handler(
                (),
                |ctx: Context, Json(input): Json<ProcessInput>| async move {
                    // Simulate progress reporting
                    for i in 0..input.count {
                        if ctx.is_cancelled() {
                            return Ok(CallToolResult::error("Cancelled"));
                        }
                        ctx.report_progress(i as f64, Some(input.count as f64), None)
                            .await;
                    }
                    Ok(CallToolResult::text(format!(
                        "Processed {} items",
                        input.count
                    )))
                },
            )
            .build();

        assert_eq!(tool.name, "process");

        // Test with a context that has progress token and notification sender
        let (tx, mut rx) = notification_channel(10);
        let ctx = RequestContext::new(RequestId::Number(1))
            .with_progress_token(ProgressToken::Number(42))
            .with_notification_sender(tx);

        let result = tool
            .call_with_context(ctx, serde_json::json!({"count": 3}))
            .await;

        assert!(!result.is_error);

        // Check that progress notifications were sent
        let mut progress_count = 0;
        while rx.try_recv().is_ok() {
            progress_count += 1;
        }
        assert_eq!(progress_count, 3);
    }

    #[tokio::test]
    async fn test_context_aware_handler_cancellation() {
        use crate::protocol::RequestId;
        use std::sync::atomic::{AtomicI32, Ordering};

        #[derive(Debug, Deserialize, JsonSchema)]
        struct LongRunningInput {
            iterations: i32,
        }

        let iterations_completed = Arc::new(AtomicI32::new(0));
        let iterations_ref = iterations_completed.clone();

        let tool = ToolBuilder::new("long_running")
            .description("Long running task")
            .extractor_handler(
                (),
                move |ctx: Context, Json(input): Json<LongRunningInput>| {
                    let completed = iterations_ref.clone();
                    async move {
                        for i in 0..input.iterations {
                            if ctx.is_cancelled() {
                                return Ok(CallToolResult::error("Cancelled"));
                            }
                            completed.fetch_add(1, Ordering::SeqCst);
                            // Simulate work
                            tokio::task::yield_now().await;
                            // Cancel after iteration 2
                            if i == 2 {
                                ctx.cancellation_token().cancel();
                            }
                        }
                        Ok(CallToolResult::text("Done"))
                    }
                },
            )
            .build();

        let ctx = RequestContext::new(RequestId::Number(1));

        let result = tool
            .call_with_context(ctx, serde_json::json!({"iterations": 10}))
            .await;

        // Should have been cancelled after 3 iterations (0, 1, 2)
        // The next iteration (3) checks cancellation and returns
        assert!(result.is_error);
        assert_eq!(iterations_completed.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_tool_builder_with_enhanced_fields() {
        let output_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "greeting": {"type": "string"}
            }
        });

        let tool = ToolBuilder::new("greet")
            .title("Greeting Tool")
            .description("Greet someone")
            .output_schema(output_schema.clone())
            .icon("https://example.com/icon.png")
            .icon_with_meta(
                "https://example.com/icon-large.png",
                Some("image/png".to_string()),
                Some(vec!["96x96".to_string()]),
            )
            .handler(|input: GreetInput| async move {
                Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
            })
            .build();

        assert_eq!(tool.name, "greet");
        assert_eq!(tool.title.as_deref(), Some("Greeting Tool"));
        assert_eq!(tool.description.as_deref(), Some("Greet someone"));
        assert_eq!(tool.output_schema, Some(output_schema));
        assert!(tool.icons.is_some());
        assert_eq!(tool.icons.as_ref().unwrap().len(), 2);

        // Test definition includes new fields
        let def = tool.definition();
        assert_eq!(def.title.as_deref(), Some("Greeting Tool"));
        assert!(def.output_schema.is_some());
        assert!(def.icons.is_some());
    }

    #[tokio::test]
    async fn test_handler_with_state() {
        let shared = Arc::new("shared-state".to_string());

        let tool = ToolBuilder::new("stateful")
            .description("Uses shared state")
            .extractor_handler(
                shared,
                |State(state): State<Arc<String>>, Json(input): Json<GreetInput>| async move {
                    Ok(CallToolResult::text(format!(
                        "{}: Hello, {}!",
                        state, input.name
                    )))
                },
            )
            .build();

        let result = tool.call(serde_json::json!({"name": "World"})).await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_handler_with_state_and_context() {
        use crate::protocol::RequestId;

        let shared = Arc::new(42_i32);

        let tool =
            ToolBuilder::new("stateful_ctx")
                .description("Uses state and context")
                .extractor_handler(
                    shared,
                    |State(state): State<Arc<i32>>,
                     _ctx: Context,
                     Json(input): Json<GreetInput>| async move {
                        Ok(CallToolResult::text(format!(
                            "{}: Hello, {}!",
                            state, input.name
                        )))
                    },
                )
                .build();

        let ctx = RequestContext::new(RequestId::Number(1));
        let result = tool
            .call_with_context(ctx, serde_json::json!({"name": "World"}))
            .await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_handler_no_params() {
        let tool = ToolBuilder::new("no_params")
            .description("Takes no parameters")
            .extractor_handler((), |Json(_): Json<NoParams>| async {
                Ok(CallToolResult::text("no params result"))
            })
            .build();

        assert_eq!(tool.name, "no_params");

        // Should work with empty args
        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);

        // Should also work with unexpected args (ignored)
        let result = tool.call(serde_json::json!({"unexpected": "value"})).await;
        assert!(!result.is_error);

        // Check input schema includes type: object
        let schema = tool.definition().input_schema;
        assert_eq!(schema.get("type").unwrap().as_str().unwrap(), "object");
    }

    #[tokio::test]
    async fn test_handler_with_state_no_params() {
        let shared = Arc::new("shared_value".to_string());

        let tool = ToolBuilder::new("with_state_no_params")
            .description("Takes no parameters but has state")
            .extractor_handler(
                shared,
                |State(state): State<Arc<String>>, Json(_): Json<NoParams>| async move {
                    Ok(CallToolResult::text(format!("state: {}", state)))
                },
            )
            .build();

        assert_eq!(tool.name, "with_state_no_params");

        // Should work with empty args
        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "state: shared_value");

        // Check input schema includes type: object
        let schema = tool.definition().input_schema;
        assert_eq!(schema.get("type").unwrap().as_str().unwrap(), "object");
    }

    #[tokio::test]
    async fn test_handler_no_params_with_context() {
        let tool = ToolBuilder::new("no_params_with_context")
            .description("Takes no parameters but has context")
            .extractor_handler((), |_ctx: Context, Json(_): Json<NoParams>| async move {
                Ok(CallToolResult::text("context available"))
            })
            .build();

        assert_eq!(tool.name, "no_params_with_context");

        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "context available");
    }

    #[tokio::test]
    async fn test_handler_with_state_and_context_no_params() {
        let shared = Arc::new("shared".to_string());

        let tool = ToolBuilder::new("state_context_no_params")
            .description("Has state and context, no params")
            .extractor_handler(
                shared,
                |State(state): State<Arc<String>>,
                 _ctx: Context,
                 Json(_): Json<NoParams>| async move {
                    Ok(CallToolResult::text(format!("state: {}", state)))
                },
            )
            .build();

        assert_eq!(tool.name, "state_context_no_params");

        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "state: shared");
    }

    #[tokio::test]
    async fn test_raw_handler_with_state() {
        let prefix = Arc::new("prefix:".to_string());

        let tool = ToolBuilder::new("raw_with_state")
            .description("Raw handler with state")
            .extractor_handler(
                prefix,
                |State(state): State<Arc<String>>, RawArgs(args): RawArgs| async move {
                    Ok(CallToolResult::text(format!("{} {}", state, args)))
                },
            )
            .build();

        assert_eq!(tool.name, "raw_with_state");

        let result = tool.call(serde_json::json!({"key": "value"})).await;
        assert!(!result.is_error);
        assert!(result.first_text().unwrap().starts_with("prefix:"));
    }

    #[tokio::test]
    async fn test_raw_handler_with_state_and_context() {
        let prefix = Arc::new("prefix:".to_string());

        let tool = ToolBuilder::new("raw_state_context")
            .description("Raw handler with state and context")
            .extractor_handler(
                prefix,
                |State(state): State<Arc<String>>,
                 _ctx: Context,
                 RawArgs(args): RawArgs| async move {
                    Ok(CallToolResult::text(format!("{} {}", state, args)))
                },
            )
            .build();

        assert_eq!(tool.name, "raw_state_context");

        let result = tool.call(serde_json::json!({"key": "value"})).await;
        assert!(!result.is_error);
        assert!(result.first_text().unwrap().starts_with("prefix:"));
    }

    #[tokio::test]
    async fn test_tool_with_timeout_layer() {
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        #[derive(Debug, Deserialize, JsonSchema)]
        struct SlowInput {
            delay_ms: u64,
        }

        // Create a tool with a short timeout
        let tool = ToolBuilder::new("slow_tool")
            .description("A slow tool")
            .handler(|input: SlowInput| async move {
                tokio::time::sleep(Duration::from_millis(input.delay_ms)).await;
                Ok(CallToolResult::text("completed"))
            })
            .layer(TimeoutLayer::new(Duration::from_millis(50)))
            .build();

        // Fast call should succeed
        let result = tool.call(serde_json::json!({"delay_ms": 10})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "completed");

        // Slow call should timeout and return an error result
        let result = tool.call(serde_json::json!({"delay_ms": 200})).await;
        assert!(result.is_error);
        // Tower's timeout error message is "request timed out"
        let msg = result.first_text().unwrap().to_lowercase();
        assert!(
            msg.contains("timed out") || msg.contains("timeout") || msg.contains("elapsed"),
            "Expected timeout error, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_tool_with_concurrency_limit_layer() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::Duration;
        use tower::limit::ConcurrencyLimitLayer;

        #[derive(Debug, Deserialize, JsonSchema)]
        struct WorkInput {
            id: u32,
        }

        let max_concurrent = Arc::new(AtomicU32::new(0));
        let current_concurrent = Arc::new(AtomicU32::new(0));
        let max_ref = max_concurrent.clone();
        let current_ref = current_concurrent.clone();

        // Create a tool with concurrency limit of 2
        let tool = ToolBuilder::new("concurrent_tool")
            .description("A concurrent tool")
            .handler(move |input: WorkInput| {
                let max = max_ref.clone();
                let current = current_ref.clone();
                async move {
                    // Track concurrency
                    let prev = current.fetch_add(1, Ordering::SeqCst);
                    max.fetch_max(prev + 1, Ordering::SeqCst);

                    // Simulate work
                    tokio::time::sleep(Duration::from_millis(50)).await;

                    current.fetch_sub(1, Ordering::SeqCst);
                    Ok(CallToolResult::text(format!("completed {}", input.id)))
                }
            })
            .layer(ConcurrencyLimitLayer::new(2))
            .build();

        // Launch 4 concurrent calls
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let t = tool.call(serde_json::json!({"id": i}));
                tokio::spawn(t)
            })
            .collect();

        for handle in handles {
            let result = handle.await.unwrap();
            assert!(!result.is_error);
        }

        // Max concurrent should not exceed 2
        assert!(max_concurrent.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn test_tool_with_multiple_layers() {
        use std::time::Duration;
        use tower::limit::ConcurrencyLimitLayer;
        use tower::timeout::TimeoutLayer;

        #[derive(Debug, Deserialize, JsonSchema)]
        struct Input {
            value: String,
        }

        // Create a tool with multiple layers stacked
        let tool = ToolBuilder::new("multi_layer_tool")
            .description("Tool with multiple layers")
            .handler(|input: Input| async move {
                Ok(CallToolResult::text(format!("processed: {}", input.value)))
            })
            .layer(TimeoutLayer::new(Duration::from_secs(5)))
            .layer(ConcurrencyLimitLayer::new(10))
            .build();

        let result = tool.call(serde_json::json!({"value": "test"})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "processed: test");
    }

    #[test]
    fn test_tool_catch_error_clone() {
        // ToolCatchError should be Clone when inner is Clone
        // Use a simple tool that we can clone
        let tool = ToolBuilder::new("test")
            .description("test")
            .extractor_handler((), |RawArgs(_args): RawArgs| async {
                Ok(CallToolResult::text("ok"))
            })
            .build();
        // The tool contains a BoxToolService which is cloneable
        let _clone = tool.call(serde_json::json!({}));
    }

    #[test]
    fn test_tool_catch_error_debug() {
        // ToolCatchError implements Debug when inner implements Debug
        // Since our internal services don't require Debug, just verify
        // that ToolCatchError has a Debug impl for appropriate types
        #[derive(Debug, Clone)]
        struct DebugService;

        impl Service<ToolRequest> for DebugService {
            type Response = CallToolResult;
            type Error = crate::error::Error;
            type Future = Pin<
                Box<
                    dyn Future<Output = std::result::Result<CallToolResult, crate::error::Error>>
                        + Send,
                >,
            >;

            fn poll_ready(
                &mut self,
                _cx: &mut std::task::Context<'_>,
            ) -> Poll<std::result::Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, _req: ToolRequest) -> Self::Future {
                Box::pin(async { Ok(CallToolResult::text("ok")) })
            }
        }

        let catch_error = ToolCatchError::new(DebugService);
        let debug = format!("{:?}", catch_error);
        assert!(debug.contains("ToolCatchError"));
    }

    #[test]
    fn test_tool_request_new() {
        use crate::protocol::RequestId;

        let ctx = RequestContext::new(RequestId::Number(42));
        let args = serde_json::json!({"key": "value"});
        let req = ToolRequest::new(ctx.clone(), args.clone());

        assert_eq!(req.args, args);
    }

    #[test]
    fn test_no_params_schema() {
        // NoParams should produce a schema with type: "object"
        let schema = schemars::schema_for!(NoParams);
        let schema_value = serde_json::to_value(&schema).unwrap();
        assert_eq!(
            schema_value.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "NoParams should generate type: object schema"
        );
    }

    #[test]
    fn test_no_params_deserialize() {
        // NoParams should deserialize from various inputs
        let from_empty_object: NoParams = serde_json::from_str("{}").unwrap();
        assert_eq!(from_empty_object, NoParams);

        let from_null: NoParams = serde_json::from_str("null").unwrap();
        assert_eq!(from_null, NoParams);

        // Should also accept objects with unexpected fields (ignored)
        let from_object_with_fields: NoParams =
            serde_json::from_str(r#"{"unexpected": "value"}"#).unwrap();
        assert_eq!(from_object_with_fields, NoParams);
    }

    #[tokio::test]
    async fn test_no_params_type_in_handler() {
        // NoParams can be used as a handler input type
        let tool = ToolBuilder::new("status")
            .description("Get status")
            .handler(|_input: NoParams| async move { Ok(CallToolResult::text("OK")) })
            .build();

        // Check schema has type: object (not type: null like () would produce)
        let schema = tool.definition().input_schema;
        assert_eq!(
            schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "NoParams handler should produce type: object schema"
        );

        // Should work with empty input
        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_tool_with_name_prefix() {
        #[derive(Debug, Deserialize, JsonSchema)]
        struct Input {
            value: String,
        }

        let tool = ToolBuilder::new("query")
            .description("Query something")
            .title("Query Tool")
            .handler(|input: Input| async move { Ok(CallToolResult::text(&input.value)) })
            .build();

        // Create prefixed version
        let prefixed = tool.with_name_prefix("db");

        // Check name is prefixed
        assert_eq!(prefixed.name, "db.query");

        // Check other fields are preserved
        assert_eq!(prefixed.description.as_deref(), Some("Query something"));
        assert_eq!(prefixed.title.as_deref(), Some("Query Tool"));

        // Check the tool still works
        let result = prefixed
            .call(serde_json::json!({"value": "test input"}))
            .await;
        assert!(!result.is_error);
        match &result.content[0] {
            Content::Text { text, .. } => assert_eq!(text, "test input"),
            _ => panic!("Expected text content"),
        }
    }

    #[tokio::test]
    async fn test_tool_with_name_prefix_multiple_levels() {
        let tool = ToolBuilder::new("action")
            .description("Do something")
            .handler(|_: NoParams| async move { Ok(CallToolResult::text("done")) })
            .build();

        // Apply multiple prefixes
        let prefixed = tool.with_name_prefix("level1");
        assert_eq!(prefixed.name, "level1.action");

        let double_prefixed = prefixed.with_name_prefix("level0");
        assert_eq!(double_prefixed.name, "level0.level1.action");
    }

    // =============================================================================
    // no_params_handler tests
    // =============================================================================

    #[tokio::test]
    async fn test_no_params_handler_basic() {
        let tool = ToolBuilder::new("get_status")
            .description("Get current status")
            .no_params_handler(|| async { Ok(CallToolResult::text("OK")) })
            .build();

        assert_eq!(tool.name, "get_status");
        assert_eq!(tool.description.as_deref(), Some("Get current status"));

        // Should work with empty args
        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "OK");

        // Should also work with null args
        let result = tool.call(serde_json::json!(null)).await;
        assert!(!result.is_error);

        // Check input schema has type: object
        let schema = tool.definition().input_schema;
        assert_eq!(schema.get("type").and_then(|v| v.as_str()), Some("object"));
    }

    #[tokio::test]
    async fn test_no_params_handler_with_captured_state() {
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_ref = counter.clone();

        let tool = ToolBuilder::new("increment")
            .description("Increment counter")
            .no_params_handler(move || {
                let c = counter_ref.clone();
                async move {
                    let prev = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(CallToolResult::text(format!("Incremented from {}", prev)))
                }
            })
            .build();

        // Call multiple times
        let _ = tool.call(serde_json::json!({})).await;
        let _ = tool.call(serde_json::json!({})).await;
        let result = tool.call(serde_json::json!({})).await;

        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "Incremented from 2");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_no_params_handler_with_layer() {
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let tool = ToolBuilder::new("slow_status")
            .description("Slow status check")
            .no_params_handler(|| async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(CallToolResult::text("done"))
            })
            .layer(TimeoutLayer::new(Duration::from_secs(1)))
            .build();

        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "done");
    }

    #[tokio::test]
    async fn test_no_params_handler_timeout() {
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let tool = ToolBuilder::new("very_slow_status")
            .description("Very slow status check")
            .no_params_handler(|| async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(CallToolResult::text("done"))
            })
            .layer(TimeoutLayer::new(Duration::from_millis(50)))
            .build();

        let result = tool.call(serde_json::json!({})).await;
        assert!(result.is_error);
        let msg = result.first_text().unwrap().to_lowercase();
        assert!(
            msg.contains("timed out") || msg.contains("timeout") || msg.contains("elapsed"),
            "Expected timeout error, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_no_params_handler_with_multiple_layers() {
        use std::time::Duration;
        use tower::limit::ConcurrencyLimitLayer;
        use tower::timeout::TimeoutLayer;

        let tool = ToolBuilder::new("multi_layer_status")
            .description("Status with multiple layers")
            .no_params_handler(|| async { Ok(CallToolResult::text("status ok")) })
            .layer(TimeoutLayer::new(Duration::from_secs(5)))
            .layer(ConcurrencyLimitLayer::new(10))
            .build();

        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "status ok");
    }

    // =========================================================================
    // Guard tests
    // =========================================================================

    #[tokio::test]
    async fn test_guard_allows_request() {
        #[derive(Debug, Deserialize, JsonSchema)]
        #[allow(dead_code)]
        struct DeleteInput {
            id: String,
            confirm: bool,
        }

        let tool = ToolBuilder::new("delete")
            .description("Delete a record")
            .handler(|input: DeleteInput| async move {
                Ok(CallToolResult::text(format!("deleted {}", input.id)))
            })
            .guard(|req: &ToolRequest| {
                let confirm = req
                    .args
                    .get("confirm")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !confirm {
                    return Err("Must set confirm=true to delete".to_string());
                }
                Ok(())
            })
            .build();

        let result = tool
            .call(serde_json::json!({"id": "abc", "confirm": true}))
            .await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "deleted abc");
    }

    #[tokio::test]
    async fn test_guard_rejects_request() {
        #[derive(Debug, Deserialize, JsonSchema)]
        #[allow(dead_code)]
        struct DeleteInput2 {
            id: String,
            confirm: bool,
        }

        let tool = ToolBuilder::new("delete2")
            .description("Delete a record")
            .handler(|input: DeleteInput2| async move {
                Ok(CallToolResult::text(format!("deleted {}", input.id)))
            })
            .guard(|req: &ToolRequest| {
                let confirm = req
                    .args
                    .get("confirm")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !confirm {
                    return Err("Must set confirm=true to delete".to_string());
                }
                Ok(())
            })
            .build();

        let result = tool
            .call(serde_json::json!({"id": "abc", "confirm": false}))
            .await;
        assert!(result.is_error);
        assert!(
            result
                .first_text()
                .unwrap()
                .contains("Must set confirm=true")
        );
    }

    #[tokio::test]
    async fn test_guard_with_layer() {
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let tool = ToolBuilder::new("guarded_timeout")
            .description("Guarded with timeout")
            .handler(|input: GreetInput| async move {
                Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
            })
            .layer(TimeoutLayer::new(Duration::from_secs(5)))
            .guard(|_req: &ToolRequest| Ok(()))
            .build();

        let result = tool.call(serde_json::json!({"name": "World"})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "Hello, World!");
    }

    #[tokio::test]
    async fn test_guard_on_no_params_handler() {
        let allowed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let allowed_clone = allowed.clone();

        let tool = ToolBuilder::new("status")
            .description("Get status")
            .no_params_handler(|| async { Ok(CallToolResult::text("ok")) })
            .guard(move |_req: &ToolRequest| {
                if allowed_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    Ok(())
                } else {
                    Err("Access denied".to_string())
                }
            })
            .build();

        // Allowed
        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "ok");

        // Denied
        allowed.store(false, std::sync::atomic::Ordering::Relaxed);
        let result = tool.call(serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.first_text().unwrap().contains("Access denied"));
    }

    #[tokio::test]
    async fn test_guard_on_no_params_handler_with_layer() {
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let tool = ToolBuilder::new("status_layered")
            .description("Get status with layers")
            .no_params_handler(|| async { Ok(CallToolResult::text("ok")) })
            .layer(TimeoutLayer::new(Duration::from_secs(5)))
            .guard(|_req: &ToolRequest| Ok(()))
            .build();

        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "ok");
    }

    #[tokio::test]
    async fn test_guard_on_extractor_handler() {
        use std::sync::Arc;

        #[derive(Clone)]
        struct AppState {
            prefix: String,
        }

        #[derive(Debug, Deserialize, JsonSchema)]
        struct QueryInput {
            query: String,
        }

        let state = Arc::new(AppState {
            prefix: "db".to_string(),
        });

        let tool = ToolBuilder::new("search")
            .description("Search")
            .extractor_handler(
                state,
                |State(app): State<Arc<AppState>>, Json(input): Json<QueryInput>| async move {
                    Ok(CallToolResult::text(format!(
                        "{}: {}",
                        app.prefix, input.query
                    )))
                },
            )
            .guard(|req: &ToolRequest| {
                let query = req.args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                if query.is_empty() {
                    return Err("Query cannot be empty".to_string());
                }
                Ok(())
            })
            .build();

        // Valid query
        let result = tool.call(serde_json::json!({"query": "hello"})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "db: hello");

        // Empty query rejected by guard
        let result = tool.call(serde_json::json!({"query": ""})).await;
        assert!(result.is_error);
        assert!(
            result
                .first_text()
                .unwrap()
                .contains("Query cannot be empty")
        );
    }

    #[tokio::test]
    async fn test_guard_on_extractor_handler_with_layer() {
        use std::sync::Arc;
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        #[derive(Clone)]
        struct AppState2 {
            prefix: String,
        }

        #[derive(Debug, Deserialize, JsonSchema)]
        struct QueryInput2 {
            query: String,
        }

        let state = Arc::new(AppState2 {
            prefix: "db".to_string(),
        });

        let tool = ToolBuilder::new("search2")
            .description("Search with layer and guard")
            .extractor_handler(
                state,
                |State(app): State<Arc<AppState2>>, Json(input): Json<QueryInput2>| async move {
                    Ok(CallToolResult::text(format!(
                        "{}: {}",
                        app.prefix, input.query
                    )))
                },
            )
            .layer(TimeoutLayer::new(Duration::from_secs(5)))
            .guard(|_req: &ToolRequest| Ok(()))
            .build();

        let result = tool.call(serde_json::json!({"query": "hello"})).await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "db: hello");
    }

    #[tokio::test]
    async fn test_tool_with_guard_post_build() {
        let tool = ToolBuilder::new("admin_action")
            .description("Admin action")
            .handler(|_input: GreetInput| async move { Ok(CallToolResult::text("done")) })
            .build();

        // Apply guard after building
        let guarded = tool.with_guard(|req: &ToolRequest| {
            let name = req.args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name == "admin" {
                Ok(())
            } else {
                Err("Only admin allowed".to_string())
            }
        });

        // Admin passes
        let result = guarded.call(serde_json::json!({"name": "admin"})).await;
        assert!(!result.is_error);

        // Non-admin blocked
        let result = guarded.call(serde_json::json!({"name": "user"})).await;
        assert!(result.is_error);
        assert!(result.first_text().unwrap().contains("Only admin allowed"));
    }

    #[tokio::test]
    async fn test_with_guard_preserves_tool_metadata() {
        let tool = ToolBuilder::new("my_tool")
            .description("A tool")
            .title("My Tool")
            .read_only()
            .handler(|_input: GreetInput| async move { Ok(CallToolResult::text("done")) })
            .build();

        let guarded = tool.with_guard(|_req: &ToolRequest| Ok(()));

        assert_eq!(guarded.name, "my_tool");
        assert_eq!(guarded.description.as_deref(), Some("A tool"));
        assert_eq!(guarded.title.as_deref(), Some("My Tool"));
        assert!(guarded.annotations.is_some());
    }

    #[tokio::test]
    async fn test_guard_group_pattern() {
        // Demonstrate applying the same guard to multiple tools (per-group pattern)
        let require_auth = |req: &ToolRequest| {
            let token = req
                .args
                .get("_token")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if token == "valid" {
                Ok(())
            } else {
                Err("Authentication required".to_string())
            }
        };

        let tool1 = ToolBuilder::new("action1")
            .description("Action 1")
            .handler(|_input: GreetInput| async move { Ok(CallToolResult::text("action1")) })
            .build();
        let tool2 = ToolBuilder::new("action2")
            .description("Action 2")
            .handler(|_input: GreetInput| async move { Ok(CallToolResult::text("action2")) })
            .build();

        // Apply same guard to both
        let guarded1 = tool1.with_guard(require_auth);
        let guarded2 = tool2.with_guard(require_auth);

        // Without auth
        let r1 = guarded1
            .call(serde_json::json!({"name": "test", "_token": "invalid"}))
            .await;
        let r2 = guarded2
            .call(serde_json::json!({"name": "test", "_token": "invalid"}))
            .await;
        assert!(r1.is_error);
        assert!(r2.is_error);

        // With auth
        let r1 = guarded1
            .call(serde_json::json!({"name": "test", "_token": "valid"}))
            .await;
        let r2 = guarded2
            .call(serde_json::json!({"name": "test", "_token": "valid"}))
            .await;
        assert!(!r1.is_error);
        assert!(!r2.is_error);
    }
}
