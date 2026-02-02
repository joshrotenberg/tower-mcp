//! Extractor pattern for tool handlers
//!
//! This module provides an axum-inspired extractor pattern that makes state and context
//! injection more declarative, reducing the combinatorial explosion of handler variants.
//!
//! # Overview
//!
//! Extractors implement [`FromToolRequest`], which extracts data from the tool request
//! (context, state, and arguments). Multiple extractors can be combined in handler
//! function parameters.
//!
//! # Built-in Extractors
//!
//! - [`Json<T>`] - Extract typed input from args (deserializes JSON)
//! - [`State<T>`] - Extract shared state from per-tool state (cloned for each request)
//! - [`Extension<T>`] - Extract data from router extensions (via `router.with_state()`)
//! - [`Context`] - Extract the [`RequestContext`] for progress, cancellation, etc.
//! - [`RawArgs`] - Extract raw `serde_json::Value` arguments
//!
//! ## State vs Extension
//!
//! - Use **`State<T>`** when state is passed directly to `extractor_handler()` (per-tool state)
//! - Use **`Extension<T>`** when state is set via `McpRouter::with_state()` (router-level state)
//!
//! # Example
//!
//! ```rust
//! use std::sync::Arc;
//! use tower_mcp::{ToolBuilder, CallToolResult};
//! use tower_mcp::extract::{Json, State, Context};
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Clone)]
//! struct AppState {
//!     db_url: String,
//! }
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct QueryInput {
//!     query: String,
//! }
//!
//! let state = Arc::new(AppState { db_url: "postgres://...".to_string() });
//!
//! let tool = ToolBuilder::new("search")
//!     .description("Search the database")
//!     .extractor_handler(state, |
//!         State(db): State<Arc<AppState>>,
//!         ctx: Context,
//!         Json(input): Json<QueryInput>,
//!     | async move {
//!         // Check cancellation
//!         if ctx.is_cancelled() {
//!             return Ok(CallToolResult::error("Cancelled"));
//!         }
//!         // Report progress
//!         ctx.report_progress(0.5, Some(1.0), Some("Searching...")).await;
//!         // Use state
//!         Ok(CallToolResult::text(format!("Searched {} with query: {}", db.db_url, input.query)))
//!     })
//!     .build()
//!     .unwrap();
//! ```
//!
//! # Extractor Order
//!
//! The order of extractors in the function signature doesn't matter. Each extractor
//! independently extracts its data from the request.
//!
//! # Error Handling
//!
//! If an extractor fails (e.g., JSON deserialization fails), the handler returns
//! a `CallToolResult::error()` with the rejection message.

use std::future::Future;
use std::marker::PhantomData;
use std::ops::Deref;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::context::RequestContext;
use crate::error::{Error, Result};
use crate::protocol::CallToolResult;

/// A rejection returned by an extractor.
///
/// Rejections are converted to tool errors via the `Into<Error>` implementation.
#[derive(Debug)]
pub struct Rejection {
    message: String,
}

impl Rejection {
    /// Create a new rejection with the given message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Rejection {}

impl From<Rejection> for Error {
    fn from(rejection: Rejection) -> Self {
        Error::tool(rejection.message)
    }
}

/// Trait for extracting data from a tool request.
///
/// Implement this trait to create custom extractors that can be used
/// in `extractor_handler` functions.
///
/// # Type Parameters
///
/// - `S` - The state type. Defaults to `()` for extractors that don't need state.
///
/// # Example
///
/// ```rust
/// use tower_mcp::extract::{FromToolRequest, Rejection};
/// use tower_mcp::RequestContext;
/// use serde_json::Value;
///
/// struct RequestId(String);
///
/// impl<S> FromToolRequest<S> for RequestId {
///     type Rejection = Rejection;
///
///     fn from_tool_request(
///         ctx: &RequestContext,
///         _state: &S,
///         _args: &Value,
///     ) -> Result<Self, Self::Rejection> {
///         Ok(RequestId(format!("{:?}", ctx.request_id())))
///     }
/// }
/// ```
pub trait FromToolRequest<S = ()>: Sized {
    /// The rejection type returned when extraction fails.
    type Rejection: Into<Error>;

    /// Extract this type from the tool request.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The request context with progress, cancellation, etc.
    /// * `state` - The shared state passed to the handler
    /// * `args` - The raw JSON arguments to the tool
    fn from_tool_request(
        ctx: &RequestContext,
        state: &S,
        args: &Value,
    ) -> std::result::Result<Self, Self::Rejection>;
}

// =============================================================================
// Built-in Extractors
// =============================================================================

/// Extract and deserialize JSON arguments into a typed struct.
///
/// This extractor deserializes the tool's JSON arguments into type `T`.
/// The type must implement [`serde::de::DeserializeOwned`] and [`schemars::JsonSchema`].
///
/// # Example
///
/// ```rust
/// use tower_mcp::extract::Json;
/// use schemars::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(Debug, Deserialize, JsonSchema)]
/// struct MyInput {
///     name: String,
///     count: i32,
/// }
///
/// // In an extractor handler:
/// // |Json(input): Json<MyInput>| async move { ... }
/// ```
///
/// # Rejection
///
/// Returns a [`Rejection`] if deserialization fails, with a message describing
/// the JSON error.
#[derive(Debug, Clone, Copy)]
pub struct Json<T>(pub T);

impl<T> Deref for Json<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S, T> FromToolRequest<S> for Json<T>
where
    T: DeserializeOwned,
{
    type Rejection = Rejection;

    fn from_tool_request(
        _ctx: &RequestContext,
        _state: &S,
        args: &Value,
    ) -> std::result::Result<Self, Self::Rejection> {
        serde_json::from_value(args.clone())
            .map(Json)
            .map_err(|e| Rejection::new(format!("Invalid input: {}", e)))
    }
}

/// Extract shared state.
///
/// This extractor clones the state passed to `extractor_handler` and provides
/// it to the handler. The state type must match the type passed to the builder.
///
/// # Example
///
/// ```rust
/// use std::sync::Arc;
/// use tower_mcp::extract::State;
///
/// #[derive(Clone)]
/// struct AppState {
///     db_url: String,
/// }
///
/// // In an extractor handler:
/// // |State(state): State<Arc<AppState>>| async move { ... }
/// ```
///
/// # Note
///
/// For expensive-to-clone types, wrap them in `Arc` before passing to
/// `extractor_handler`.
#[derive(Debug, Clone, Copy)]
pub struct State<T>(pub T);

impl<T> Deref for State<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S: Clone> FromToolRequest<S> for State<S> {
    type Rejection = Rejection;

    fn from_tool_request(
        _ctx: &RequestContext,
        state: &S,
        _args: &Value,
    ) -> std::result::Result<Self, Self::Rejection> {
        Ok(State(state.clone()))
    }
}

/// Extract the request context.
///
/// This extractor provides access to the [`RequestContext`], which contains:
/// - Progress reporting via `report_progress()`
/// - Cancellation checking via `is_cancelled()`
/// - Sampling capabilities via `sample()`
/// - Elicitation capabilities via `elicit_form()` and `elicit_url()`
/// - Log sending via `send_log()`
///
/// # Example
///
/// ```rust
/// use tower_mcp::extract::Context;
///
/// // In an extractor handler:
/// // |ctx: Context| async move {
/// //     ctx.report_progress(0.5, Some(1.0), Some("Working...")).await;
/// //     // ...
/// // }
/// ```
#[derive(Debug, Clone)]
pub struct Context(RequestContext);

impl Context {
    /// Get the inner RequestContext
    pub fn into_inner(self) -> RequestContext {
        self.0
    }
}

impl Deref for Context {
    type Target = RequestContext;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S> FromToolRequest<S> for Context {
    type Rejection = Rejection;

    fn from_tool_request(
        ctx: &RequestContext,
        _state: &S,
        _args: &Value,
    ) -> std::result::Result<Self, Self::Rejection> {
        Ok(Context(ctx.clone()))
    }
}

/// Extract raw JSON arguments.
///
/// This extractor provides the raw `serde_json::Value` arguments without
/// any deserialization. Useful when you need full control over argument
/// parsing or when the schema is dynamic.
///
/// # Example
///
/// ```rust
/// use tower_mcp::extract::RawArgs;
///
/// // In an extractor handler:
/// // |RawArgs(args): RawArgs| async move {
/// //     // args is serde_json::Value
/// //     if let Some(name) = args.get("name") { ... }
/// // }
/// ```
#[derive(Debug, Clone)]
pub struct RawArgs(pub Value);

impl Deref for RawArgs {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S> FromToolRequest<S> for RawArgs {
    type Rejection = Rejection;

    fn from_tool_request(
        _ctx: &RequestContext,
        _state: &S,
        args: &Value,
    ) -> std::result::Result<Self, Self::Rejection> {
        Ok(RawArgs(args.clone()))
    }
}

/// Extract typed data from router extensions.
///
/// This extractor retrieves data that was added to the router via
/// [`McpRouter::with_state()`] or [`McpRouter::with_extension()`], or
/// inserted by middleware into the request context's extensions.
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
/// struct DatabasePool {
///     url: String,
/// }
///
/// #[derive(Deserialize, JsonSchema)]
/// struct QueryInput {
///     sql: String,
/// }
///
/// let pool = Arc::new(DatabasePool { url: "postgres://...".into() });
///
/// let tool = ToolBuilder::new("query")
///     .description("Run a query")
///     .extractor_handler_typed::<_, _, _, QueryInput>(
///         (),
///         |Extension(db): Extension<Arc<DatabasePool>>, Json(input): Json<QueryInput>| async move {
///             Ok(CallToolResult::text(format!("Query on {}: {}", db.url, input.sql)))
///         },
///     )
///     .build()
///     .unwrap();
///
/// let router = McpRouter::new()
///     .with_state(pool)
///     .tool(tool);
/// ```
///
/// # Rejection
///
/// Returns a [`Rejection`] if the requested type is not found in the extensions.
#[derive(Debug, Clone)]
pub struct Extension<T>(pub T);

impl<T> Deref for Extension<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S, T> FromToolRequest<S> for Extension<T>
where
    T: Clone + Send + Sync + 'static,
{
    type Rejection = Rejection;

    fn from_tool_request(
        ctx: &RequestContext,
        _state: &S,
        _args: &Value,
    ) -> std::result::Result<Self, Self::Rejection> {
        ctx.extension::<T>()
            .cloned()
            .map(Extension)
            .ok_or_else(|| {
                Rejection::new(format!(
                    "Extension of type `{}` not found. Did you call `router.with_state()` or `router.with_extension()`?",
                    std::any::type_name::<T>()
                ))
            })
    }
}

// =============================================================================
// Handler Trait
// =============================================================================

/// A handler that uses extractors.
///
/// This trait is implemented for functions that take extractors as arguments.
/// You don't need to implement this trait directly; it's automatically
/// implemented for compatible async functions.
pub trait ExtractorHandler<S, T>: Clone + Send + Sync + 'static {
    /// The future returned by the handler.
    type Future: Future<Output = Result<CallToolResult>> + Send;

    /// Call the handler with extracted values.
    fn call(self, ctx: RequestContext, state: S, args: Value) -> Self::Future;

    /// Get the input schema for this handler.
    ///
    /// Returns `None` if no `Json<T>` extractor is used.
    fn input_schema() -> Value;
}

// Implementation for single extractor
impl<S, F, Fut, T1> ExtractorHandler<S, (T1,)> for F
where
    S: Clone + Send + Sync + 'static,
    F: Fn(T1) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send,
    T1: FromToolRequest<S> + Send,
{
    type Future = Pin<Box<dyn Future<Output = Result<CallToolResult>> + Send>>;

    fn call(self, ctx: RequestContext, state: S, args: Value) -> Self::Future {
        Box::pin(async move {
            let t1 = T1::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            self(t1).await
        })
    }

    fn input_schema() -> Value {
        // For single extractors, check if it's Json<T>
        serde_json::json!({
            "type": "object",
            "additionalProperties": true
        })
    }
}

// Implementation for two extractors
impl<S, F, Fut, T1, T2> ExtractorHandler<S, (T1, T2)> for F
where
    S: Clone + Send + Sync + 'static,
    F: Fn(T1, T2) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send,
    T1: FromToolRequest<S> + Send,
    T2: FromToolRequest<S> + Send,
{
    type Future = Pin<Box<dyn Future<Output = Result<CallToolResult>> + Send>>;

    fn call(self, ctx: RequestContext, state: S, args: Value) -> Self::Future {
        Box::pin(async move {
            let t1 = T1::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            let t2 = T2::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            self(t1, t2).await
        })
    }

    fn input_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": true
        })
    }
}

// Implementation for three extractors
impl<S, F, Fut, T1, T2, T3> ExtractorHandler<S, (T1, T2, T3)> for F
where
    S: Clone + Send + Sync + 'static,
    F: Fn(T1, T2, T3) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send,
    T1: FromToolRequest<S> + Send,
    T2: FromToolRequest<S> + Send,
    T3: FromToolRequest<S> + Send,
{
    type Future = Pin<Box<dyn Future<Output = Result<CallToolResult>> + Send>>;

    fn call(self, ctx: RequestContext, state: S, args: Value) -> Self::Future {
        Box::pin(async move {
            let t1 = T1::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            let t2 = T2::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            let t3 = T3::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            self(t1, t2, t3).await
        })
    }

    fn input_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": true
        })
    }
}

// Implementation for four extractors
impl<S, F, Fut, T1, T2, T3, T4> ExtractorHandler<S, (T1, T2, T3, T4)> for F
where
    S: Clone + Send + Sync + 'static,
    F: Fn(T1, T2, T3, T4) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send,
    T1: FromToolRequest<S> + Send,
    T2: FromToolRequest<S> + Send,
    T3: FromToolRequest<S> + Send,
    T4: FromToolRequest<S> + Send,
{
    type Future = Pin<Box<dyn Future<Output = Result<CallToolResult>> + Send>>;

    fn call(self, ctx: RequestContext, state: S, args: Value) -> Self::Future {
        Box::pin(async move {
            let t1 = T1::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            let t2 = T2::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            let t3 = T3::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            let t4 = T4::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            self(t1, t2, t3, t4).await
        })
    }

    fn input_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": true
        })
    }
}

// Implementation for five extractors
impl<S, F, Fut, T1, T2, T3, T4, T5> ExtractorHandler<S, (T1, T2, T3, T4, T5)> for F
where
    S: Clone + Send + Sync + 'static,
    F: Fn(T1, T2, T3, T4, T5) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send,
    T1: FromToolRequest<S> + Send,
    T2: FromToolRequest<S> + Send,
    T3: FromToolRequest<S> + Send,
    T4: FromToolRequest<S> + Send,
    T5: FromToolRequest<S> + Send,
{
    type Future = Pin<Box<dyn Future<Output = Result<CallToolResult>> + Send>>;

    fn call(self, ctx: RequestContext, state: S, args: Value) -> Self::Future {
        Box::pin(async move {
            let t1 = T1::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            let t2 = T2::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            let t3 = T3::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            let t4 = T4::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            let t5 = T5::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            self(t1, t2, t3, t4, t5).await
        })
    }

    fn input_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": true
        })
    }
}

// =============================================================================
// Schema Extraction Helper
// =============================================================================

/// Helper trait to get schema from `Json<T>` extractor
pub trait HasSchema {
    fn schema() -> Option<Value>;
}

impl<T: JsonSchema> HasSchema for Json<T> {
    fn schema() -> Option<Value> {
        let schema = schemars::schema_for!(T);
        serde_json::to_value(schema).ok()
    }
}

// Default impl for non-Json extractors
impl HasSchema for Context {
    fn schema() -> Option<Value> {
        None
    }
}

impl HasSchema for RawArgs {
    fn schema() -> Option<Value> {
        None
    }
}

impl<T> HasSchema for State<T> {
    fn schema() -> Option<Value> {
        None
    }
}

// =============================================================================
// Typed Extractor Handler
// =============================================================================

/// A handler that uses extractors with typed JSON input.
///
/// This trait is similar to [`ExtractorHandler`] but provides proper JSON
/// schema generation for the input type when `Json<T>` is used.
pub trait TypedExtractorHandler<S, T, I>: Clone + Send + Sync + 'static
where
    I: JsonSchema,
{
    /// The future returned by the handler.
    type Future: Future<Output = Result<CallToolResult>> + Send;

    /// Call the handler with extracted values.
    fn call(self, ctx: RequestContext, state: S, args: Value) -> Self::Future;
}

// Single extractor with Json<T>
impl<S, F, Fut, T> TypedExtractorHandler<S, (Json<T>,), T> for F
where
    S: Clone + Send + Sync + 'static,
    F: Fn(Json<T>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send,
    T: DeserializeOwned + JsonSchema + Send,
{
    type Future = Pin<Box<dyn Future<Output = Result<CallToolResult>> + Send>>;

    fn call(self, ctx: RequestContext, state: S, args: Value) -> Self::Future {
        Box::pin(async move {
            let t1 =
                Json::<T>::from_tool_request(&ctx, &state, &args).map_err(Into::<Error>::into)?;
            self(t1).await
        })
    }
}

// Two extractors ending with Json<T>
impl<S, F, Fut, T1, T> TypedExtractorHandler<S, (T1, Json<T>), T> for F
where
    S: Clone + Send + Sync + 'static,
    F: Fn(T1, Json<T>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send,
    T1: FromToolRequest<S> + Send,
    T: DeserializeOwned + JsonSchema + Send,
{
    type Future = Pin<Box<dyn Future<Output = Result<CallToolResult>> + Send>>;

    fn call(self, ctx: RequestContext, state: S, args: Value) -> Self::Future {
        Box::pin(async move {
            let t1 = T1::from_tool_request(&ctx, &state, &args).map_err(Into::<Error>::into)?;
            let t2 =
                Json::<T>::from_tool_request(&ctx, &state, &args).map_err(Into::<Error>::into)?;
            self(t1, t2).await
        })
    }
}

// Three extractors ending with Json<T>
impl<S, F, Fut, T1, T2, T> TypedExtractorHandler<S, (T1, T2, Json<T>), T> for F
where
    S: Clone + Send + Sync + 'static,
    F: Fn(T1, T2, Json<T>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send,
    T1: FromToolRequest<S> + Send,
    T2: FromToolRequest<S> + Send,
    T: DeserializeOwned + JsonSchema + Send,
{
    type Future = Pin<Box<dyn Future<Output = Result<CallToolResult>> + Send>>;

    fn call(self, ctx: RequestContext, state: S, args: Value) -> Self::Future {
        Box::pin(async move {
            let t1 = T1::from_tool_request(&ctx, &state, &args).map_err(Into::<Error>::into)?;
            let t2 = T2::from_tool_request(&ctx, &state, &args).map_err(Into::<Error>::into)?;
            let t3 =
                Json::<T>::from_tool_request(&ctx, &state, &args).map_err(Into::<Error>::into)?;
            self(t1, t2, t3).await
        })
    }
}

// Four extractors ending with Json<T>
impl<S, F, Fut, T1, T2, T3, T> TypedExtractorHandler<S, (T1, T2, T3, Json<T>), T> for F
where
    S: Clone + Send + Sync + 'static,
    F: Fn(T1, T2, T3, Json<T>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<CallToolResult>> + Send,
    T1: FromToolRequest<S> + Send,
    T2: FromToolRequest<S> + Send,
    T3: FromToolRequest<S> + Send,
    T: DeserializeOwned + JsonSchema + Send,
{
    type Future = Pin<Box<dyn Future<Output = Result<CallToolResult>> + Send>>;

    fn call(self, ctx: RequestContext, state: S, args: Value) -> Self::Future {
        Box::pin(async move {
            let t1 = T1::from_tool_request(&ctx, &state, &args).map_err(Into::<Error>::into)?;
            let t2 = T2::from_tool_request(&ctx, &state, &args).map_err(Into::<Error>::into)?;
            let t3 = T3::from_tool_request(&ctx, &state, &args).map_err(Into::<Error>::into)?;
            let t4 =
                Json::<T>::from_tool_request(&ctx, &state, &args).map_err(Into::<Error>::into)?;
            self(t1, t2, t3, t4).await
        })
    }
}

// =============================================================================
// ToolBuilder Extensions
// =============================================================================

use crate::tool::{BoxFuture, Tool, ToolCatchError, ToolHandler, validate_tool_name};
use tower::util::BoxCloneService;

/// Internal handler wrapper for extractor-based handlers
pub(crate) struct ExtractorToolHandler<S, F, T> {
    state: S,
    handler: F,
    input_schema: Value,
    _phantom: PhantomData<T>,
}

impl<S, F, T> ToolHandler for ExtractorToolHandler<S, F, T>
where
    S: Clone + Send + Sync + 'static,
    F: ExtractorHandler<S, T> + Clone,
    T: Send + Sync + 'static,
{
    fn call(&self, args: Value) -> BoxFuture<'_, Result<CallToolResult>> {
        let ctx = RequestContext::new(crate::protocol::RequestId::Number(0));
        self.call_with_context(ctx, args)
    }

    fn call_with_context(
        &self,
        ctx: RequestContext,
        args: Value,
    ) -> BoxFuture<'_, Result<CallToolResult>> {
        let state = self.state.clone();
        let handler = self.handler.clone();
        Box::pin(async move { handler.call(ctx, state, args).await })
    }

    fn uses_context(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }
}

/// Builder state for extractor-based handlers
pub struct ToolBuilderWithExtractor<S, F, T> {
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) output_schema: Option<Value>,
    pub(crate) icons: Option<Vec<crate::protocol::ToolIcon>>,
    pub(crate) annotations: Option<crate::protocol::ToolAnnotations>,
    pub(crate) state: S,
    pub(crate) handler: F,
    pub(crate) input_schema: Value,
    pub(crate) _phantom: PhantomData<T>,
}

impl<S, F, T> ToolBuilderWithExtractor<S, F, T>
where
    S: Clone + Send + Sync + 'static,
    F: ExtractorHandler<S, T> + Clone,
    T: Send + Sync + 'static,
{
    /// Build the tool.
    ///
    /// Returns an error if the tool name is invalid.
    pub fn build(self) -> Result<Tool> {
        validate_tool_name(&self.name)?;

        let handler = ExtractorToolHandler {
            state: self.state,
            handler: self.handler,
            input_schema: self.input_schema.clone(),
            _phantom: PhantomData,
        };

        let handler_service = crate::tool::ToolHandlerService::new(handler);
        let catch_error = ToolCatchError::new(handler_service);
        let service = BoxCloneService::new(catch_error);

        Ok(Tool {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            service,
            input_schema: self.input_schema,
        })
    }
}

/// Builder state for extractor-based handlers with typed JSON input
pub struct ToolBuilderWithTypedExtractor<S, F, T, I> {
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) output_schema: Option<Value>,
    pub(crate) icons: Option<Vec<crate::protocol::ToolIcon>>,
    pub(crate) annotations: Option<crate::protocol::ToolAnnotations>,
    pub(crate) state: S,
    pub(crate) handler: F,
    pub(crate) _phantom: PhantomData<(T, I)>,
}

impl<S, F, T, I> ToolBuilderWithTypedExtractor<S, F, T, I>
where
    S: Clone + Send + Sync + 'static,
    F: TypedExtractorHandler<S, T, I> + Clone,
    T: Send + Sync + 'static,
    I: JsonSchema + Send + Sync + 'static,
{
    /// Build the tool.
    ///
    /// Returns an error if the tool name is invalid.
    pub fn build(self) -> Result<Tool> {
        validate_tool_name(&self.name)?;

        let input_schema = {
            let schema = schemars::schema_for!(I);
            serde_json::to_value(schema).unwrap_or_else(|_| {
                serde_json::json!({
                    "type": "object"
                })
            })
        };

        let handler = TypedExtractorToolHandler {
            state: self.state,
            handler: self.handler,
            input_schema: input_schema.clone(),
            _phantom: PhantomData,
        };

        let handler_service = crate::tool::ToolHandlerService::new(handler);
        let catch_error = ToolCatchError::new(handler_service);
        let service = BoxCloneService::new(catch_error);

        Ok(Tool {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            service,
            input_schema,
        })
    }
}

/// Internal handler wrapper for typed extractor-based handlers
struct TypedExtractorToolHandler<S, F, T, I> {
    state: S,
    handler: F,
    input_schema: Value,
    _phantom: PhantomData<(T, I)>,
}

impl<S, F, T, I> ToolHandler for TypedExtractorToolHandler<S, F, T, I>
where
    S: Clone + Send + Sync + 'static,
    F: TypedExtractorHandler<S, T, I> + Clone,
    T: Send + Sync + 'static,
    I: JsonSchema + Send + Sync + 'static,
{
    fn call(&self, args: Value) -> BoxFuture<'_, Result<CallToolResult>> {
        let ctx = RequestContext::new(crate::protocol::RequestId::Number(0));
        self.call_with_context(ctx, args)
    }

    fn call_with_context(
        &self,
        ctx: RequestContext,
        args: Value,
    ) -> BoxFuture<'_, Result<CallToolResult>> {
        let state = self.state.clone();
        let handler = self.handler.clone();
        Box::pin(async move { handler.call(ctx, state, args).await })
    }

    fn uses_context(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RequestId;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use std::sync::Arc;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct TestInput {
        name: String,
        count: i32,
    }

    #[test]
    fn test_json_extraction() {
        let args = serde_json::json!({"name": "test", "count": 42});
        let ctx = RequestContext::new(RequestId::Number(1));

        let result = Json::<TestInput>::from_tool_request(&ctx, &(), &args);
        assert!(result.is_ok());
        let Json(input) = result.unwrap();
        assert_eq!(input.name, "test");
        assert_eq!(input.count, 42);
    }

    #[test]
    fn test_json_extraction_error() {
        let args = serde_json::json!({"name": "test"}); // missing count
        let ctx = RequestContext::new(RequestId::Number(1));

        let result = Json::<TestInput>::from_tool_request(&ctx, &(), &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("Invalid input"));
    }

    #[test]
    fn test_state_extraction() {
        let args = serde_json::json!({});
        let ctx = RequestContext::new(RequestId::Number(1));
        let state = Arc::new("my-state".to_string());

        let result = State::<Arc<String>>::from_tool_request(&ctx, &state, &args);
        assert!(result.is_ok());
        let State(extracted) = result.unwrap();
        assert_eq!(*extracted, "my-state");
    }

    #[test]
    fn test_context_extraction() {
        let args = serde_json::json!({});
        let ctx = RequestContext::new(RequestId::Number(42));

        let result = Context::from_tool_request(&ctx, &(), &args);
        assert!(result.is_ok());
        let extracted = result.unwrap();
        assert_eq!(*extracted.request_id(), RequestId::Number(42));
    }

    #[test]
    fn test_raw_args_extraction() {
        let args = serde_json::json!({"foo": "bar", "baz": 123});
        let ctx = RequestContext::new(RequestId::Number(1));

        let result = RawArgs::from_tool_request(&ctx, &(), &args);
        assert!(result.is_ok());
        let RawArgs(extracted) = result.unwrap();
        assert_eq!(extracted["foo"], "bar");
        assert_eq!(extracted["baz"], 123);
    }

    #[test]
    fn test_extension_extraction() {
        use crate::context::Extensions;

        #[derive(Clone, Debug, PartialEq)]
        struct DatabasePool {
            url: String,
        }

        let args = serde_json::json!({});

        // Create extensions with a value
        let mut extensions = Extensions::new();
        extensions.insert(Arc::new(DatabasePool {
            url: "postgres://localhost".to_string(),
        }));

        // Create context with extensions
        let ctx = RequestContext::new(RequestId::Number(1)).with_extensions(Arc::new(extensions));

        // Extract the extension
        let result = Extension::<Arc<DatabasePool>>::from_tool_request(&ctx, &(), &args);
        assert!(result.is_ok());
        let Extension(pool) = result.unwrap();
        assert_eq!(pool.url, "postgres://localhost");
    }

    #[test]
    fn test_extension_extraction_missing() {
        #[derive(Clone, Debug)]
        struct NotPresent;

        let args = serde_json::json!({});
        let ctx = RequestContext::new(RequestId::Number(1));

        // Try to extract something that's not in extensions
        let result = Extension::<NotPresent>::from_tool_request(&ctx, &(), &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("not found"));
    }

    #[tokio::test]
    async fn test_single_extractor_handler() {
        let handler = |Json(input): Json<TestInput>| async move {
            Ok(CallToolResult::text(format!(
                "{}: {}",
                input.name, input.count
            )))
        };

        let ctx = RequestContext::new(RequestId::Number(1));
        let args = serde_json::json!({"name": "test", "count": 5});

        // Use explicit trait to avoid ambiguity
        let result: Result<CallToolResult> =
            ExtractorHandler::<(), (Json<TestInput>,)>::call(handler, ctx, (), args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_two_extractor_handler() {
        let handler = |State(state): State<Arc<String>>, Json(input): Json<TestInput>| async move {
            Ok(CallToolResult::text(format!(
                "{}: {} - {}",
                state, input.name, input.count
            )))
        };

        let ctx = RequestContext::new(RequestId::Number(1));
        let state = Arc::new("prefix".to_string());
        let args = serde_json::json!({"name": "test", "count": 5});

        // Use explicit trait to avoid ambiguity
        let result: Result<CallToolResult> = ExtractorHandler::<
            Arc<String>,
            (State<Arc<String>>, Json<TestInput>),
        >::call(handler, ctx, state, args)
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_three_extractor_handler() {
        let handler = |State(state): State<Arc<String>>,
                       ctx: Context,
                       Json(input): Json<TestInput>| async move {
            // Verify we can access all extractors
            assert!(!ctx.is_cancelled());
            Ok(CallToolResult::text(format!(
                "{}: {} - {}",
                state, input.name, input.count
            )))
        };

        let ctx = RequestContext::new(RequestId::Number(1));
        let state = Arc::new("prefix".to_string());
        let args = serde_json::json!({"name": "test", "count": 5});

        // Use explicit trait to avoid ambiguity
        let result: Result<CallToolResult> = ExtractorHandler::<
            Arc<String>,
            (State<Arc<String>>, Context, Json<TestInput>),
        >::call(handler, ctx, state, args)
        .await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_json_schema_generation() {
        let schema = Json::<TestInput>::schema();
        assert!(schema.is_some());
        let schema = schema.unwrap();
        assert!(schema.get("properties").is_some());
    }

    #[test]
    fn test_rejection_into_error() {
        let rejection = Rejection::new("test error");
        let error: Error = rejection.into();
        assert!(error.to_string().contains("test error"));
    }

    #[tokio::test]
    async fn test_tool_builder_extractor_handler() {
        use crate::ToolBuilder;

        let state = Arc::new("shared-state".to_string());

        let tool =
            ToolBuilder::new("test_extractor")
                .description("Test extractor handler")
                .extractor_handler(
                    state,
                    |State(state): State<Arc<String>>,
                     ctx: Context,
                     Json(input): Json<TestInput>| async move {
                        assert!(!ctx.is_cancelled());
                        Ok(CallToolResult::text(format!(
                            "{}: {} - {}",
                            state, input.name, input.count
                        )))
                    },
                )
                .build()
                .expect("valid tool name");

        assert_eq!(tool.name, "test_extractor");
        assert_eq!(tool.description.as_deref(), Some("Test extractor handler"));

        // Test calling the tool
        let result = tool
            .call(serde_json::json!({"name": "test", "count": 42}))
            .await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_tool_builder_extractor_handler_typed() {
        use crate::ToolBuilder;

        let state = Arc::new("typed-state".to_string());

        let tool = ToolBuilder::new("test_typed")
            .description("Test typed extractor handler")
            .extractor_handler_typed::<_, _, _, TestInput>(
                state,
                |State(state): State<Arc<String>>, Json(input): Json<TestInput>| async move {
                    Ok(CallToolResult::text(format!(
                        "{}: {} - {}",
                        state, input.name, input.count
                    )))
                },
            )
            .build()
            .expect("valid tool name");

        assert_eq!(tool.name, "test_typed");

        // Verify schema is properly generated from TestInput
        let def = tool.definition();
        let schema = def.input_schema;
        assert!(schema.get("properties").is_some());

        // Test calling the tool
        let result = tool
            .call(serde_json::json!({"name": "world", "count": 99}))
            .await;
        assert!(!result.is_error);
    }
}
