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

// =============================================================================
// Rejection Types
// =============================================================================

/// A simple rejection with a message string.
///
/// This is a general-purpose rejection type for custom extractors.
/// For more specific error information, use the typed rejection types
/// like [`JsonRejection`] or [`ExtensionRejection`].
#[derive(Debug, Clone)]
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

    /// Get the rejection message.
    pub fn message(&self) -> &str {
        &self.message
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

/// Rejection returned when JSON deserialization fails.
///
/// This rejection provides structured information about the deserialization
/// error, including the path to the failing field when available.
///
/// # Example
///
/// ```rust
/// use tower_mcp::extract::JsonRejection;
///
/// let rejection = JsonRejection::new("missing field `name`");
/// assert!(rejection.message().contains("name"));
/// ```
#[derive(Debug, Clone)]
pub struct JsonRejection {
    message: String,
    /// The serde error path, if available (e.g., "users[0].name")
    path: Option<String>,
}

impl JsonRejection {
    /// Create a new JSON rejection from a serde error.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path: None,
        }
    }

    /// Create a JSON rejection with a path to the failing field.
    pub fn with_path(message: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path: Some(path.into()),
        }
    }

    /// Get the error message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Get the path to the failing field, if available.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }
}

impl std::fmt::Display for JsonRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(path) = &self.path {
            write!(f, "Invalid input at `{}`: {}", path, self.message)
        } else {
            write!(f, "Invalid input: {}", self.message)
        }
    }
}

impl std::error::Error for JsonRejection {}

impl From<JsonRejection> for Error {
    fn from(rejection: JsonRejection) -> Self {
        Error::tool(rejection.to_string())
    }
}

impl From<serde_json::Error> for JsonRejection {
    fn from(err: serde_json::Error) -> Self {
        // Try to extract path information from serde error
        let path = if err.is_data() {
            // serde_json provides line/column but not field path in the error itself
            // The path is embedded in the message for some error types
            None
        } else {
            None
        };

        Self {
            message: err.to_string(),
            path,
        }
    }
}

/// Rejection returned when an extension is not found.
///
/// This rejection is returned by the [`Extension`] extractor when the
/// requested type is not present in the router's extensions.
///
/// # Example
///
/// ```rust
/// use tower_mcp::extract::ExtensionRejection;
///
/// let rejection = ExtensionRejection::not_found::<String>();
/// assert!(rejection.type_name().contains("String"));
/// ```
#[derive(Debug, Clone)]
pub struct ExtensionRejection {
    type_name: &'static str,
}

impl ExtensionRejection {
    /// Create a rejection for a missing extension type.
    pub fn not_found<T>() -> Self {
        Self {
            type_name: std::any::type_name::<T>(),
        }
    }

    /// Get the type name of the missing extension.
    pub fn type_name(&self) -> &'static str {
        self.type_name
    }
}

impl std::fmt::Display for ExtensionRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Extension of type `{}` not found. Did you call `router.with_state()` or `router.with_extension()`?",
            self.type_name
        )
    }
}

impl std::error::Error for ExtensionRejection {}

impl From<ExtensionRejection> for Error {
    fn from(rejection: ExtensionRejection) -> Self {
        Error::tool(rejection.to_string())
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
/// Returns a [`JsonRejection`] if deserialization fails. The rejection contains
/// the error message and potentially the path to the failing field.
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
    type Rejection = JsonRejection;

    fn from_tool_request(
        _ctx: &RequestContext,
        _state: &S,
        args: &Value,
    ) -> std::result::Result<Self, Self::Rejection> {
        serde_json::from_value(args.clone())
            .map(Json)
            .map_err(JsonRejection::from)
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
/// [`crate::McpRouter::with_state()`] or [`crate::McpRouter::with_extension()`], or
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
/// Returns an [`ExtensionRejection`] if the requested type is not found in the extensions.
/// The rejection contains the type name of the missing extension.
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
    type Rejection = ExtensionRejection;

    fn from_tool_request(
        ctx: &RequestContext,
        _state: &S,
        _args: &Value,
    ) -> std::result::Result<Self, Self::Rejection> {
        ctx.extension::<T>()
            .cloned()
            .map(Extension)
            .ok_or_else(ExtensionRejection::not_found::<T>)
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
    T1: FromToolRequest<S> + HasSchema + Send,
{
    type Future = Pin<Box<dyn Future<Output = Result<CallToolResult>> + Send>>;

    fn call(self, ctx: RequestContext, state: S, args: Value) -> Self::Future {
        Box::pin(async move {
            let t1 = T1::from_tool_request(&ctx, &state, &args).map_err(Into::into)?;
            self(t1).await
        })
    }

    fn input_schema() -> Value {
        if let Some(schema) = T1::schema() {
            return schema;
        }
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
    T1: FromToolRequest<S> + HasSchema + Send,
    T2: FromToolRequest<S> + HasSchema + Send,
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
        if let Some(schema) = T2::schema() {
            return schema;
        }
        if let Some(schema) = T1::schema() {
            return schema;
        }
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
    T1: FromToolRequest<S> + HasSchema + Send,
    T2: FromToolRequest<S> + HasSchema + Send,
    T3: FromToolRequest<S> + HasSchema + Send,
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
        if let Some(schema) = T3::schema() {
            return schema;
        }
        if let Some(schema) = T2::schema() {
            return schema;
        }
        if let Some(schema) = T1::schema() {
            return schema;
        }
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
    T1: FromToolRequest<S> + HasSchema + Send,
    T2: FromToolRequest<S> + HasSchema + Send,
    T3: FromToolRequest<S> + HasSchema + Send,
    T4: FromToolRequest<S> + HasSchema + Send,
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
        if let Some(schema) = T4::schema() {
            return schema;
        }
        if let Some(schema) = T3::schema() {
            return schema;
        }
        if let Some(schema) = T2::schema() {
            return schema;
        }
        if let Some(schema) = T1::schema() {
            return schema;
        }
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
    T1: FromToolRequest<S> + HasSchema + Send,
    T2: FromToolRequest<S> + HasSchema + Send,
    T3: FromToolRequest<S> + HasSchema + Send,
    T4: FromToolRequest<S> + HasSchema + Send,
    T5: FromToolRequest<S> + HasSchema + Send,
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
        if let Some(schema) = T5::schema() {
            return schema;
        }
        if let Some(schema) = T4::schema() {
            return schema;
        }
        if let Some(schema) = T3::schema() {
            return schema;
        }
        if let Some(schema) = T2::schema() {
            return schema;
        }
        if let Some(schema) = T1::schema() {
            return schema;
        }
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

impl<T> HasSchema for Extension<T> {
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

use crate::tool::{
    BoxFuture, Tool, ToolCatchError, ToolHandler, ToolHandlerService, ToolRequest,
    validate_tool_name,
};
use tower::util::BoxCloneService;
use tower_service::Service;

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

        let handler_service = ToolHandlerService::new(handler);
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

    /// Apply a Tower layer (middleware) to this tool.
    ///
    /// The layer wraps the tool's handler service, enabling functionality like
    /// timeouts, rate limiting, and metrics collection at the per-tool level.
    ///
    /// # Example
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
    /// struct AppState { prefix: String }
    ///
    /// #[derive(Debug, Deserialize, JsonSchema)]
    /// struct QueryInput { query: String }
    ///
    /// let state = Arc::new(AppState { prefix: "db".to_string() });
    ///
    /// let tool = ToolBuilder::new("search")
    ///     .description("Search with timeout")
    ///     .extractor_handler(state, |
    ///         State(app): State<Arc<AppState>>,
    ///         Json(input): Json<QueryInput>,
    ///     | async move {
    ///         Ok(CallToolResult::text(format!("{}: {}", app.prefix, input.query)))
    ///     })
    ///     .layer(TimeoutLayer::new(Duration::from_secs(30)))
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn layer<L>(self, layer: L) -> ToolBuilderWithExtractorLayer<S, F, T, L> {
        ToolBuilderWithExtractorLayer {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            state: self.state,
            handler: self.handler,
            input_schema: self.input_schema,
            layer,
            _phantom: PhantomData,
        }
    }
}

/// Builder state after a layer has been applied to an extractor handler.
///
/// This builder allows chaining additional layers and building the final tool.
pub struct ToolBuilderWithExtractorLayer<S, F, T, L> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    output_schema: Option<Value>,
    icons: Option<Vec<crate::protocol::ToolIcon>>,
    annotations: Option<crate::protocol::ToolAnnotations>,
    state: S,
    handler: F,
    input_schema: Value,
    layer: L,
    _phantom: PhantomData<T>,
}

#[allow(private_bounds)]
impl<S, F, T, L> ToolBuilderWithExtractorLayer<S, F, T, L>
where
    S: Clone + Send + Sync + 'static,
    F: ExtractorHandler<S, T> + Clone,
    T: Send + Sync + 'static,
    L: tower::Layer<ToolHandlerService<ExtractorToolHandler<S, F, T>>>
        + Clone
        + Send
        + Sync
        + 'static,
    L::Service: Service<ToolRequest, Response = CallToolResult> + Clone + Send + 'static,
    <L::Service as Service<ToolRequest>>::Error: std::fmt::Display + Send,
    <L::Service as Service<ToolRequest>>::Future: Send,
{
    /// Build the tool with the applied layer(s).
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

        let handler_service = ToolHandlerService::new(handler);
        let layered = self.layer.layer(handler_service);
        let catch_error = ToolCatchError::new(layered);
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

    /// Apply an additional Tower layer (middleware).
    ///
    /// Layers are applied in order, with earlier layers wrapping later ones.
    /// This means the first layer added is the outermost middleware.
    pub fn layer<L2>(
        self,
        layer: L2,
    ) -> ToolBuilderWithExtractorLayer<S, F, T, tower::layer::util::Stack<L2, L>> {
        ToolBuilderWithExtractorLayer {
            name: self.name,
            title: self.title,
            description: self.description,
            output_schema: self.output_schema,
            icons: self.icons,
            annotations: self.annotations,
            state: self.state,
            handler: self.handler,
            input_schema: self.input_schema,
            layer: tower::layer::util::Stack::new(layer, self.layer),
            _phantom: PhantomData,
        }
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
        let rejection = result.unwrap_err();
        // JsonRejection contains the serde error message
        assert!(rejection.message().contains("count"));
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
        let rejection = result.unwrap_err();
        // ExtensionRejection contains the type name
        assert!(rejection.type_name().contains("NotPresent"));
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

    #[test]
    fn test_json_rejection() {
        // Test basic JsonRejection
        let rejection = JsonRejection::new("missing field `name`");
        assert_eq!(rejection.message(), "missing field `name`");
        assert!(rejection.path().is_none());
        assert!(rejection.to_string().contains("Invalid input"));

        // Test JsonRejection with path
        let rejection = JsonRejection::with_path("expected string", "users[0].name");
        assert_eq!(rejection.message(), "expected string");
        assert_eq!(rejection.path(), Some("users[0].name"));
        assert!(rejection.to_string().contains("users[0].name"));

        // Test conversion to Error
        let error: Error = rejection.into();
        assert!(error.to_string().contains("users[0].name"));
    }

    #[test]
    fn test_json_rejection_from_serde_error() {
        // Create a real serde error by deserializing invalid JSON
        #[derive(Debug, serde::Deserialize)]
        struct TestStruct {
            #[allow(dead_code)]
            name: String,
        }

        let result: std::result::Result<TestStruct, _> =
            serde_json::from_value(serde_json::json!({"count": 42}));
        assert!(result.is_err());

        let rejection: JsonRejection = result.unwrap_err().into();
        assert!(rejection.message().contains("name"));
    }

    #[test]
    fn test_extension_rejection() {
        // Test ExtensionRejection
        let rejection = ExtensionRejection::not_found::<String>();
        assert!(rejection.type_name().contains("String"));
        assert!(rejection.to_string().contains("not found"));
        assert!(rejection.to_string().contains("with_state"));

        // Test conversion to Error
        let error: Error = rejection.into();
        assert!(error.to_string().contains("not found"));
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

    #[tokio::test]
    async fn test_extractor_handler_auto_schema() {
        use crate::ToolBuilder;

        let state = Arc::new("auto-schema".to_string());

        // extractor_handler (not _typed) should auto-detect Json<TestInput> schema
        let tool = ToolBuilder::new("test_auto_schema")
            .description("Test auto schema detection")
            .extractor_handler(
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

        // Verify schema is properly generated from TestInput (not generic object)
        let def = tool.definition();
        let schema = def.input_schema;
        assert!(
            schema.get("properties").is_some(),
            "Schema should have properties from TestInput, got: {}",
            schema
        );
        let props = schema.get("properties").unwrap();
        assert!(
            props.get("name").is_some(),
            "Schema should have 'name' property"
        );
        assert!(
            props.get("count").is_some(),
            "Schema should have 'count' property"
        );

        // Test calling the tool
        let result = tool
            .call(serde_json::json!({"name": "world", "count": 99}))
            .await;
        assert!(!result.is_error);
    }

    #[test]
    fn test_extractor_handler_no_json_fallback() {
        use crate::ToolBuilder;

        // extractor_handler without Json<T> should fall back to generic schema
        let tool = ToolBuilder::new("test_no_json")
            .description("Test no json fallback")
            .extractor_handler((), |RawArgs(args): RawArgs| async move {
                Ok(CallToolResult::json(args))
            })
            .build()
            .expect("valid tool name");

        let def = tool.definition();
        let schema = def.input_schema;
        assert_eq!(
            schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "Schema should be generic object"
        );
        assert_eq!(
            schema.get("additionalProperties").and_then(|v| v.as_bool()),
            Some(true),
            "Schema should allow additional properties"
        );
        // Should NOT have specific properties
        assert!(
            schema.get("properties").is_none(),
            "Generic schema should not have specific properties"
        );
    }

    #[tokio::test]
    async fn test_extractor_handler_with_layer() {
        use crate::ToolBuilder;
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let state = Arc::new("layered".to_string());

        let tool = ToolBuilder::new("test_extractor_layer")
            .description("Test extractor handler with layer")
            .extractor_handler(
                state,
                |State(s): State<Arc<String>>, Json(input): Json<TestInput>| async move {
                    Ok(CallToolResult::text(format!("{}: {}", s, input.name)))
                },
            )
            .layer(TimeoutLayer::new(Duration::from_secs(5)))
            .build()
            .expect("valid tool name");

        // Verify the tool works
        let result = tool
            .call(serde_json::json!({"name": "test", "count": 1}))
            .await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "layered: test");

        // Verify schema is still properly generated
        let def = tool.definition();
        let schema = def.input_schema;
        assert!(
            schema.get("properties").is_some(),
            "Schema should have properties even with layer"
        );
    }

    #[tokio::test]
    async fn test_extractor_handler_with_timeout_layer() {
        use crate::ToolBuilder;
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let tool = ToolBuilder::new("test_extractor_timeout")
            .description("Test extractor handler timeout")
            .extractor_handler((), |Json(input): Json<TestInput>| async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(CallToolResult::text(input.name.to_string()))
            })
            .layer(TimeoutLayer::new(Duration::from_millis(50)))
            .build()
            .expect("valid tool name");

        // Should timeout
        let result = tool
            .call(serde_json::json!({"name": "slow", "count": 1}))
            .await;
        assert!(result.is_error);
        let msg = result.first_text().unwrap().to_lowercase();
        assert!(
            msg.contains("timed out") || msg.contains("timeout") || msg.contains("elapsed"),
            "Expected timeout error, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_extractor_handler_with_multiple_layers() {
        use crate::ToolBuilder;
        use std::time::Duration;
        use tower::limit::ConcurrencyLimitLayer;
        use tower::timeout::TimeoutLayer;

        let state = Arc::new("multi".to_string());

        let tool = ToolBuilder::new("test_multi_layer")
            .description("Test multiple layers")
            .extractor_handler(
                state,
                |State(s): State<Arc<String>>, Json(input): Json<TestInput>| async move {
                    Ok(CallToolResult::text(format!("{}: {}", s, input.name)))
                },
            )
            .layer(TimeoutLayer::new(Duration::from_secs(5)))
            .layer(ConcurrencyLimitLayer::new(10))
            .build()
            .expect("valid tool name");

        let result = tool
            .call(serde_json::json!({"name": "test", "count": 1}))
            .await;
        assert!(!result.is_error);
        assert_eq!(result.first_text().unwrap(), "multi: test");
    }
}
