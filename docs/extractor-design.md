# Extractor Pattern Design for tower-mcp

RFC for issue #282 - axum-style extractors for tool/resource/prompt handlers.

## Core Traits

```rust
/// Extracts a value from the tool request context.
/// Analogous to axum's `FromRequest`.
pub trait FromToolRequest<S>: Sized {
    /// The error type if extraction fails.
    type Rejection: Into<ToolError>;

    /// Extract the value from the request parts.
    fn from_request(
        parts: &RequestParts,
        state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send;
}

/// Request parts available to extractors.
/// Does not include the input body - that's handled separately.
pub struct RequestParts {
    /// The tool/resource/prompt name being invoked
    pub name: String,
    /// Request extensions (type-map)
    pub extensions: Extensions,
    /// Session state reference
    pub session: Arc<SessionState>,
    /// Progress/cancellation context (if available)
    pub context: Option<RequestContext>,
}

/// Extracts the input body (JSON params).
/// Separate trait because body can only be consumed once.
pub trait FromToolInput<S>: Sized {
    type Rejection: Into<ToolError>;

    fn from_input(
        input: serde_json::Value,
        state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send;
}
```

## Built-in Extractors

### State Extractor

```rust
/// Shared state extractor - identical to axum.
#[derive(Debug, Clone, Copy)]
pub struct State<T>(pub T);

impl<S, T> FromToolRequest<S> for State<T>
where
    S: AsRef<T> + Send + Sync,
    T: Clone + Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request(_parts: &RequestParts, state: &S) -> Result<Self, Self::Rejection> {
        Ok(State(state.as_ref().clone()))
    }
}

// For Arc<T> as state (common case)
impl<T> AsRef<T> for Arc<T> {
    fn as_ref(&self) -> &T {
        &**self
    }
}
```

### Context Extractors

```rust
/// Full request context (progress + cancellation).
#[derive(Debug, Clone)]
pub struct Context(pub RequestContext);

impl<S> FromToolRequest<S> for Context
where
    S: Send + Sync,
{
    type Rejection = ContextNotAvailable;

    async fn from_request(parts: &RequestParts, _state: &S) -> Result<Self, Self::Rejection> {
        parts.context
            .clone()
            .map(Context)
            .ok_or(ContextNotAvailable)
    }
}

/// Just the progress sender.
#[derive(Debug, Clone)]
pub struct Progress(pub ProgressSender);

impl<S> FromToolRequest<S> for Progress
where
    S: Send + Sync,
{
    type Rejection = ContextNotAvailable;

    async fn from_request(parts: &RequestParts, _state: &S) -> Result<Self, Self::Rejection> {
        parts.context
            .as_ref()
            .map(|ctx| Progress(ctx.progress().clone()))
            .ok_or(ContextNotAvailable)
    }
}

/// Optional context - never fails, returns None if unavailable.
impl<S> FromToolRequest<S> for Option<Context>
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request(parts: &RequestParts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts.context.clone().map(Context))
    }
}
```

### Session Extractor

```rust
/// Per-session typed state.
#[derive(Debug, Clone)]
pub struct Session<T>(pub T);

impl<S, T> FromToolRequest<S> for Session<T>
where
    S: Send + Sync,
    T: Clone + Send + Sync + 'static,
{
    type Rejection = SessionDataNotFound;

    async fn from_request(parts: &RequestParts, _state: &S) -> Result<Self, Self::Rejection> {
        parts.session
            .extensions()
            .get::<T>()
            .cloned()
            .map(Session)
            .ok_or(SessionDataNotFound(std::any::type_name::<T>()))
    }
}
```

### Input Extractors

```rust
/// Deserializes JSON input - the default for any T: Deserialize.
impl<S, T> FromToolInput<S> for T
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = InputDeserializeError;

    async fn from_input(input: serde_json::Value, _state: &S) -> Result<Self, Self::Rejection> {
        serde_json::from_value(input).map_err(InputDeserializeError)
    }
}

/// Raw JSON access - escape hatch for dynamic schemas.
#[derive(Debug, Clone)]
pub struct RawArgs(pub serde_json::Value);

impl<S> FromToolInput<S> for RawArgs
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_input(input: serde_json::Value, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(RawArgs(input))
    }
}

/// No parameters expected.
impl<S> FromToolInput<S> for NoParams
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_input(_input: serde_json::Value, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(NoParams)
    }
}
```

## Handler Trait

```rust
/// A tool handler that can be called with extractors.
pub trait Handler<T, S>: Clone + Send + 'static {
    type Future: Future<Output = Result<CallToolResult, ToolError>> + Send;

    fn call(self, parts: RequestParts, input: serde_json::Value, state: S) -> Self::Future;
}

// Implementation for async fn with extractors
impl<F, Fut, S, T, E1, E2, E3> Handler<(T,), S> for F
where
    F: FnOnce(T) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<CallToolResult, ToolError>> + Send,
    S: Send + Sync + 'static,
    T: FromToolInput<S, Rejection = E1> + Send,
    E1: Into<ToolError>,
{
    type Future = Pin<Box<dyn Future<Output = Result<CallToolResult, ToolError>> + Send>>;

    fn call(self, parts: RequestParts, input: serde_json::Value, state: S) -> Self::Future {
        Box::pin(async move {
            let t = T::from_input(input, &state).await.map_err(Into::into)?;
            (self)(t).await
        })
    }
}

// Two extractors: one from request, one from input
impl<F, Fut, S, E1, E2, E3, R1, I1> Handler<(R1, I1), S> for F
where
    F: FnOnce(R1, I1) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<CallToolResult, ToolError>> + Send,
    S: Send + Sync + 'static,
    R1: FromToolRequest<S, Rejection = E1> + Send,
    I1: FromToolInput<S, Rejection = E2> + Send,
    E1: Into<ToolError>,
    E2: Into<ToolError>,
{
    type Future = Pin<Box<dyn Future<Output = Result<CallToolResult, ToolError>> + Send>>;

    fn call(self, parts: RequestParts, input: serde_json::Value, state: S) -> Self::Future {
        Box::pin(async move {
            let r1 = R1::from_request(&parts, &state).await.map_err(Into::into)?;
            let i1 = I1::from_input(input, &state).await.map_err(Into::into)?;
            (self)(r1, i1).await
        })
    }
}

// ... tuple impls up to (R1, R2, R3, ..., I1) for N extractors
```

## ToolBuilder Changes

```rust
impl<S> ToolBuilder<S>
where
    S: Clone + Send + Sync + 'static,
{
    /// Set the handler with extractor support.
    ///
    /// # Examples
    ///
    /// ```rust
    /// // Just input
    /// ToolBuilder::new("echo")
    ///     .handler(|input: EchoInput| async move {
    ///         Ok(CallToolResult::text(input.message))
    ///     });
    ///
    /// // With state
    /// ToolBuilder::new("query")
    ///     .with_state(db.clone())
    ///     .handler(|State(db): State<Database>, input: QueryInput| async move {
    ///         let result = db.query(&input.sql).await?;
    ///         Ok(CallToolResult::text(result))
    ///     });
    ///
    /// // With context for progress
    /// ToolBuilder::new("slow_task")
    ///     .handler(|Progress(p): Progress, input: TaskInput| async move {
    ///         p.send(0.5, "halfway").await;
    ///         Ok(CallToolResult::text("done"))
    ///     });
    ///
    /// // Everything
    /// ToolBuilder::new("complex")
    ///     .with_state(app_state)
    ///     .handler(|
    ///         State(db): State<Database>,
    ///         Session(user): Session<User>,
    ///         Progress(p): Progress,
    ///         input: ComplexInput,
    ///     | async move {
    ///         // ...
    ///     });
    /// ```
    pub fn handler<H, T>(self, handler: H) -> Self
    where
        H: Handler<T, S>,
    {
        // Store the handler
    }

    /// Set state for the handler.
    /// Analogous to axum's `Router::with_state()`.
    pub fn with_state<S2>(self, state: S2) -> ToolBuilder<S2> {
        // Convert to new state type
    }
}
```

## Resource and Prompt Extractors

```rust
/// For resource handlers - the URI being accessed.
#[derive(Debug, Clone)]
pub struct ResourceUri(pub String);

impl<S> FromResourceRequest<S> for ResourceUri { ... }

/// For prompt handlers - the arguments map.
#[derive(Debug, Clone)]
pub struct PromptArgs(pub HashMap<String, serde_json::Value>);

impl<S> FromPromptRequest<S> for PromptArgs { ... }

// Similar trait hierarchies for resources and prompts:
// - FromResourceRequest<S>
// - FromPromptRequest<S>
// - ResourceHandler<T, S>
// - PromptHandler<T, S>
```

## Error Types

```rust
#[derive(Debug, thiserror::Error)]
pub enum ExtractorError {
    #[error("context not available (no progress token in request)")]
    ContextNotAvailable,

    #[error("session data not found: {0}")]
    SessionDataNotFound(&'static str),

    #[error("failed to deserialize input: {0}")]
    InputDeserialize(#[from] serde_json::Error),

    #[error("missing required extension: {0}")]
    MissingExtension(&'static str),
}

impl From<ExtractorError> for ToolError {
    fn from(err: ExtractorError) -> Self {
        ToolError::new(ErrorCode::InvalidParams, err.to_string())
    }
}
```

## Migration Path

### Phase 1: Add new API alongside old

```rust
// Old API still works
ToolBuilder::new("foo")
    .handler_with_state(state, |state, input| async { ... })

// New API available
ToolBuilder::new("foo")
    .with_state(state)
    .handler(|State(s): State<S>, input: Input| async { ... })
```

### Phase 2: Soft deprecation (0.x)

```rust
#[deprecated(since = "0.4.0", note = "use .with_state().handler() instead")]
pub fn handler_with_state(...) { ... }
```

### Phase 3: Remove in 1.0

Only the extractor-based `.handler()` remains.

## Open Questions

1. **Extractor order**: Should `FromToolRequest` extractors run before or in parallel?
   - Recommendation: Sequential in declaration order (like axum)

2. **Input position**: Must input extractor be last, or anywhere?
   - Recommendation: Anywhere, detected by trait bound

3. **Multiple inputs**: Allow `(QueryInput, RawArgs)` together?
   - Recommendation: No, only one input extractor per handler

4. **Macro support**: Add `#[tower_mcp::handler]` proc macro?
   - Recommendation: Later, pure trait approach works first
