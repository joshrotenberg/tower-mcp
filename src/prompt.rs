//! Prompt definition and builder API
//!
//! Provides ergonomic ways to define MCP prompts:
//!
//! 1. **Builder pattern** - Fluent API for defining prompts
//! 2. **Trait-based** - Implement `McpPrompt` for full control
//! 3. **Per-prompt middleware** - Apply tower middleware layers to individual prompts
//!
//! # Per-Prompt Middleware
//!
//! The `.layer()` method on `PromptBuilder` (after `.handler()`) allows applying
//! tower middleware to a single prompt. This is useful for prompt-specific concerns
//! like timeouts, rate limiting, or caching.
//!
//! ```rust
//! use std::collections::HashMap;
//! use std::time::Duration;
//! use tower::timeout::TimeoutLayer;
//! use tower_mcp::prompt::PromptBuilder;
//! use tower_mcp::protocol::{GetPromptResult, PromptMessage, PromptRole, Content};
//!
//! let prompt = PromptBuilder::new("slow_prompt")
//!     .description("A prompt that might take a while")
//!     .handler(|args: HashMap<String, String>| async move {
//!         // Slow prompt generation logic...
//!         Ok(GetPromptResult {
//!             description: Some("Generated prompt".to_string()),
//!             messages: vec![PromptMessage {
//!                 role: PromptRole::User,
//!                 content: Content::Text {
//!                     text: "Hello!".to_string(),
//!                     annotations: None,
//!                     meta: None,
//!                 },
//!                 meta: None,
//!             }],
//!             meta: None,
//!         })
//!     })
//!     .layer(TimeoutLayer::new(Duration::from_secs(5)));
//!
//! assert_eq!(prompt.name, "slow_prompt");
//! ```

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::Mutex;
use tower::util::BoxCloneService;
use tower::{Layer, ServiceExt};
use tower_service::Service;

use crate::context::RequestContext;
use crate::error::{Error, Result};
use crate::protocol::{
    Content, GetPromptResult, PromptArgument, PromptDefinition, PromptMessage, PromptRole,
    RequestId, ToolIcon,
};

/// A boxed future for prompt handlers
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// =============================================================================
// Per-Prompt Middleware Types
// =============================================================================

/// Request type for prompt middleware.
///
/// Contains the request context and prompt arguments, allowing middleware
/// to access and modify the request before it reaches the prompt handler.
#[derive(Debug, Clone)]
pub struct PromptRequest {
    /// The request context with progress reporting, cancellation, etc.
    pub context: RequestContext,
    /// The prompt arguments (name -> value)
    pub arguments: HashMap<String, String>,
}

impl PromptRequest {
    /// Create a new prompt request with the given context and arguments.
    pub fn new(context: RequestContext, arguments: HashMap<String, String>) -> Self {
        Self { context, arguments }
    }

    /// Create a prompt request with a default context (for testing or simple use cases).
    pub fn with_arguments(arguments: HashMap<String, String>) -> Self {
        Self {
            context: RequestContext::new(RequestId::Number(0)),
            arguments,
        }
    }
}

/// A boxed, cloneable prompt service with `Error = Infallible`.
///
/// This is the service type used internally after applying middleware layers.
/// It wraps any `Service<PromptRequest>` implementation so that the prompt
/// handler can consume it without knowing the concrete middleware stack.
pub type BoxPromptService = BoxCloneService<PromptRequest, GetPromptResult, Infallible>;

/// A service wrapper that catches errors from middleware and converts them
/// into prompt errors, maintaining the `Error = Infallible` contract.
///
/// When a middleware layer (e.g., `TimeoutLayer`) produces an error, this
/// wrapper converts it into a prompt error. This allows error information to
/// flow through the normal response path rather than requiring special
/// error handling.
pub struct PromptCatchError<S> {
    inner: S,
}

impl<S> PromptCatchError<S> {
    /// Create a new `PromptCatchError` wrapping the given service.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S: Clone> Clone for PromptCatchError<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: fmt::Debug> fmt::Debug for PromptCatchError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptCatchError")
            .field("inner", &self.inner)
            .finish()
    }
}

impl<S> Service<PromptRequest> for PromptCatchError<S>
where
    S: Service<PromptRequest, Response = GetPromptResult> + Clone + Send + 'static,
    S::Error: fmt::Display + Send,
    S::Future: Send,
{
    type Response = GetPromptResult;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<GetPromptResult, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(|_| unreachable!())
    }

    fn call(&mut self, req: PromptRequest) -> Self::Future {
        let fut = self.inner.call(req);

        Box::pin(async move {
            match fut.await {
                Ok(response) => Ok(response),
                Err(err) => {
                    // Convert middleware error to an error prompt result
                    // We return a single error message as the prompt content
                    Ok(GetPromptResult {
                        description: Some(format!("Prompt error: {}", err)),
                        messages: vec![PromptMessage {
                            role: PromptRole::Assistant,
                            content: Content::Text {
                                text: format!("Error generating prompt: {}", err),
                                annotations: None,
                                meta: None,
                            },
                            meta: None,
                        }],
                        meta: None,
                    })
                }
            }
        })
    }
}

/// Adapts a prompt handler function into a `Service<PromptRequest>`.
///
/// This allows the handler to be wrapped with tower middleware layers.
/// Used by `.layer()` on `PromptBuilderWithHandler`.
pub struct PromptHandlerService<F> {
    handler: F,
}

impl<F> Clone for PromptHandlerService<F>
where
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
        }
    }
}

impl<F, Fut> Service<PromptRequest> for PromptHandlerService<F>
where
    F: Fn(HashMap<String, String>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<GetPromptResult>> + Send + 'static,
{
    type Response = GetPromptResult;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = std::result::Result<GetPromptResult, Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: PromptRequest) -> Self::Future {
        let handler = self.handler.clone();
        Box::pin(async move { handler(req.arguments).await })
    }
}

/// Adapts a context-aware prompt handler function into a `Service<PromptRequest>`.
///
/// Used by `.layer()` on `PromptBuilderWithContextHandler`.
pub struct PromptContextHandlerService<F> {
    handler: F,
}

impl<F> Clone for PromptContextHandlerService<F>
where
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
        }
    }
}

impl<F, Fut> Service<PromptRequest> for PromptContextHandlerService<F>
where
    F: Fn(RequestContext, HashMap<String, String>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<GetPromptResult>> + Send + 'static,
{
    type Response = GetPromptResult;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = std::result::Result<GetPromptResult, Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: PromptRequest) -> Self::Future {
        let handler = self.handler.clone();
        Box::pin(async move { handler(req.context, req.arguments).await })
    }
}

/// Prompt handler trait - the core abstraction for prompt generation
pub trait PromptHandler: Send + Sync {
    /// Get the prompt with the given arguments
    fn get(&self, arguments: HashMap<String, String>) -> BoxFuture<'_, Result<GetPromptResult>>;

    /// Get the prompt with request context
    ///
    /// The default implementation ignores the context and calls `get`.
    /// Override this to receive context for progress reporting, cancellation, etc.
    fn get_with_context(
        &self,
        _ctx: RequestContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GetPromptResult>> {
        self.get(arguments)
    }

    /// Returns true if this handler uses context (for optimization)
    fn uses_context(&self) -> bool {
        false
    }
}

/// A complete prompt definition with handler
pub struct Prompt {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub icons: Option<Vec<ToolIcon>>,
    pub arguments: Vec<PromptArgument>,
    handler: Arc<dyn PromptHandler>,
}

impl Clone for Prompt {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            icons: self.icons.clone(),
            arguments: self.arguments.clone(),
            handler: self.handler.clone(),
        }
    }
}

impl std::fmt::Debug for Prompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prompt")
            .field("name", &self.name)
            .field("title", &self.title)
            .field("description", &self.description)
            .field("icons", &self.icons)
            .field("arguments", &self.arguments)
            .finish_non_exhaustive()
    }
}

impl Prompt {
    /// Create a new prompt builder
    pub fn builder(name: impl Into<String>) -> PromptBuilder {
        PromptBuilder::new(name)
    }

    /// Get the prompt definition for prompts/list
    pub fn definition(&self) -> PromptDefinition {
        PromptDefinition {
            name: self.name.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            icons: self.icons.clone(),
            arguments: self.arguments.clone(),
            meta: None,
        }
    }

    /// Get the prompt with arguments
    pub fn get(
        &self,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GetPromptResult>> {
        self.handler.get(arguments)
    }

    /// Get the prompt with request context
    ///
    /// Use this when you have a RequestContext available for progress/cancellation.
    pub fn get_with_context(
        &self,
        ctx: RequestContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GetPromptResult>> {
        self.handler.get_with_context(ctx, arguments)
    }

    /// Returns true if this prompt uses context
    pub fn uses_context(&self) -> bool {
        self.handler.uses_context()
    }
}

// =============================================================================
// Builder API
// =============================================================================

/// Builder for creating prompts with a fluent API
///
/// # Example
///
/// ```rust
/// use tower_mcp::prompt::PromptBuilder;
/// use tower_mcp::protocol::{GetPromptResult, PromptMessage, PromptRole, Content};
///
/// let prompt = PromptBuilder::new("greet")
///     .description("Generate a greeting")
///     .required_arg("name", "The name to greet")
///     .handler(|args| async move {
///         let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
///         Ok(GetPromptResult {
///             description: Some("A greeting prompt".to_string()),
///             messages: vec![PromptMessage {
///                 role: PromptRole::User,
///                 content: Content::Text {
///                     text: format!("Please greet {}", name),
///                     annotations: None,
///                     meta: None,
///                 },
///                 meta: None,
///             }],
///             meta: None,
///         })
///     })
///     .build();
///
/// assert_eq!(prompt.name, "greet");
/// ```
pub struct PromptBuilder {
    name: String,
    title: Option<String>,
    description: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    arguments: Vec<PromptArgument>,
}

impl PromptBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            title: None,
            description: None,
            icons: None,
            arguments: Vec::new(),
        }
    }

    /// Set a human-readable title for the prompt
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the prompt description
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add an icon for the prompt
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

    /// Add a required argument
    pub fn required_arg(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.arguments.push(PromptArgument {
            name: name.into(),
            description: Some(description.into()),
            required: true,
        });
        self
    }

    /// Add an optional argument
    pub fn optional_arg(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.arguments.push(PromptArgument {
            name: name.into(),
            description: Some(description.into()),
            required: false,
        });
        self
    }

    /// Add an argument with full control
    pub fn argument(mut self, arg: PromptArgument) -> Self {
        self.arguments.push(arg);
        self
    }

    /// Set the handler function for getting the prompt.
    ///
    /// Returns a `PromptBuilderWithHandler` which can be finalized with `.build()`
    /// or have middleware applied with `.layer()`.
    ///
    /// # Sharing State
    ///
    /// Capture an [`Arc`] in the closure to share state across handler
    /// invocations or with other parts of your application:
    ///
    /// ```rust
    /// use std::collections::HashMap;
    /// use std::sync::Arc;
    /// use tokio::sync::RwLock;
    /// use tower_mcp::prompt::PromptBuilder;
    /// use tower_mcp::protocol::{GetPromptResult, PromptMessage, PromptRole, Content};
    ///
    /// let templates = Arc::new(RwLock::new(HashMap::from([
    ///     ("greeting".to_string(), "Hello, {name}!".to_string()),
    /// ])));
    ///
    /// let tpl = Arc::clone(&templates);
    /// let prompt = PromptBuilder::new("greet")
    ///     .description("Greet a user by name")
    ///     .required_arg("name", "The user's name")
    ///     .handler(move |args: HashMap<String, String>| {
    ///         let tpl = Arc::clone(&tpl);
    ///         async move {
    ///             let templates = tpl.read().await;
    ///             let greeting = templates.get("greeting").unwrap();
    ///             let name = args.get("name").unwrap();
    ///             let text = greeting.replace("{name}", name);
    ///             Ok(GetPromptResult {
    ///                 description: Some("A greeting".to_string()),
    ///                 messages: vec![PromptMessage {
    ///                     role: PromptRole::User,
    ///                     content: Content::text(text),
    ///                     meta: None,
    ///                 }],
    ///                 meta: None,
    ///             })
    ///         }
    ///     })
    ///     .build();
    /// ```
    ///
    /// [`Arc`]: std::sync::Arc
    pub fn handler<F, Fut>(self, handler: F) -> PromptBuilderWithHandler<F>
    where
        F: Fn(HashMap<String, String>) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<GetPromptResult>> + Send + 'static,
    {
        PromptBuilderWithHandler {
            name: self.name,
            title: self.title,
            description: self.description,
            icons: self.icons,
            arguments: self.arguments,
            handler,
        }
    }

    /// Set a context-aware handler function for getting the prompt
    ///
    /// The handler receives a `RequestContext` for progress reporting and
    /// cancellation checking, along with the prompt arguments.
    pub fn handler_with_context<F, Fut>(self, handler: F) -> PromptBuilderWithContextHandler<F>
    where
        F: Fn(RequestContext, HashMap<String, String>) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<GetPromptResult>> + Send + 'static,
    {
        PromptBuilderWithContextHandler {
            name: self.name,
            title: self.title,
            description: self.description,
            icons: self.icons,
            arguments: self.arguments,
            handler,
        }
    }

    /// Create a static prompt (no arguments needed)
    pub fn static_prompt(self, messages: Vec<PromptMessage>) -> Prompt {
        let description = self.description.clone();
        self.handler(move |_| {
            let messages = messages.clone();
            let description = description.clone();
            async move {
                Ok(GetPromptResult {
                    description,
                    messages,
                    meta: None,
                })
            }
        })
        .build()
    }

    /// Create a simple text prompt with a user message
    pub fn user_message(self, text: impl Into<String>) -> Prompt {
        let text = text.into();
        self.static_prompt(vec![PromptMessage {
            role: PromptRole::User,
            content: Content::Text {
                text,
                annotations: None,
                meta: None,
            },
            meta: None,
        }])
    }

    /// Finalize the builder into a Prompt
    ///
    /// This is an alias for `handler(...).build()` for when you want to
    /// explicitly mark the build step.
    pub fn build<F, Fut>(self, handler: F) -> Prompt
    where
        F: Fn(HashMap<String, String>) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<GetPromptResult>> + Send + 'static,
    {
        self.handler(handler).build()
    }
}

/// Builder state after handler is specified
///
/// This allows either calling `.build()` to create the prompt directly,
/// or `.layer()` to apply middleware before building.
pub struct PromptBuilderWithHandler<F> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    arguments: Vec<PromptArgument>,
    handler: F,
}

impl<F, Fut> PromptBuilderWithHandler<F>
where
    F: Fn(HashMap<String, String>) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = Result<GetPromptResult>> + Send + 'static,
{
    /// Build the prompt without any middleware
    pub fn build(self) -> Prompt {
        Prompt {
            name: self.name,
            title: self.title,
            description: self.description,
            icons: self.icons,
            arguments: self.arguments,
            handler: Arc::new(FnHandler {
                handler: self.handler,
            }),
        }
    }

    /// Apply a tower middleware layer to this prompt
    ///
    /// The layer wraps the prompt handler, allowing middleware like timeouts,
    /// rate limiting, or retries to be applied to this specific prompt.
    ///
    /// # Example
    ///
    /// ```rust
    /// use std::collections::HashMap;
    /// use std::time::Duration;
    /// use tower::timeout::TimeoutLayer;
    /// use tower_mcp::prompt::PromptBuilder;
    /// use tower_mcp::protocol::{GetPromptResult, PromptMessage, PromptRole, Content};
    ///
    /// let prompt = PromptBuilder::new("slow_prompt")
    ///     .description("A prompt that might take a while")
    ///     .handler(|_args: HashMap<String, String>| async move {
    ///         Ok(GetPromptResult {
    ///             description: Some("Generated prompt".to_string()),
    ///             messages: vec![PromptMessage {
    ///                 role: PromptRole::User,
    ///                 content: Content::Text {
    ///                     text: "Hello!".to_string(),
    ///                     annotations: None,
    ///                     meta: None,
    ///                 },
    ///                 meta: None,
    ///             }],
    ///             meta: None,
    ///         })
    ///     })
    ///     .layer(TimeoutLayer::new(Duration::from_secs(5)));
    /// ```
    pub fn layer<L>(self, layer: L) -> Prompt
    where
        L: Layer<PromptHandlerService<F>> + Send + Sync + 'static,
        L::Service: Service<PromptRequest, Response = GetPromptResult> + Clone + Send + 'static,
        <L::Service as Service<PromptRequest>>::Error: fmt::Display + Send,
        <L::Service as Service<PromptRequest>>::Future: Send,
    {
        let service = PromptHandlerService {
            handler: self.handler,
        };
        let wrapped = layer.layer(service);
        let boxed = BoxCloneService::new(PromptCatchError::new(wrapped));

        Prompt {
            name: self.name,
            title: self.title,
            description: self.description,
            icons: self.icons,
            arguments: self.arguments,
            handler: Arc::new(ServiceHandler {
                service: Mutex::new(boxed),
            }),
        }
    }
}

/// Builder state after context-aware handler is specified
pub struct PromptBuilderWithContextHandler<F> {
    name: String,
    title: Option<String>,
    description: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    arguments: Vec<PromptArgument>,
    handler: F,
}

impl<F, Fut> PromptBuilderWithContextHandler<F>
where
    F: Fn(RequestContext, HashMap<String, String>) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = Result<GetPromptResult>> + Send + 'static,
{
    /// Build the prompt without any middleware
    pub fn build(self) -> Prompt {
        Prompt {
            name: self.name,
            title: self.title,
            description: self.description,
            icons: self.icons,
            arguments: self.arguments,
            handler: Arc::new(ContextAwareHandler {
                handler: self.handler,
            }),
        }
    }

    /// Apply a tower middleware layer to this prompt
    pub fn layer<L>(self, layer: L) -> Prompt
    where
        L: Layer<PromptContextHandlerService<F>> + Send + Sync + 'static,
        L::Service: Service<PromptRequest, Response = GetPromptResult> + Clone + Send + 'static,
        <L::Service as Service<PromptRequest>>::Error: fmt::Display + Send,
        <L::Service as Service<PromptRequest>>::Future: Send,
    {
        let service = PromptContextHandlerService {
            handler: self.handler,
        };
        let wrapped = layer.layer(service);
        let boxed = BoxCloneService::new(PromptCatchError::new(wrapped));

        Prompt {
            name: self.name,
            title: self.title,
            description: self.description,
            icons: self.icons,
            arguments: self.arguments,
            handler: Arc::new(ServiceContextHandler {
                service: Mutex::new(boxed),
            }),
        }
    }
}

// =============================================================================
// Handler implementations
// =============================================================================

/// Handler wrapping a function
struct FnHandler<F> {
    handler: F,
}

impl<F, Fut> PromptHandler for FnHandler<F>
where
    F: Fn(HashMap<String, String>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<GetPromptResult>> + Send + 'static,
{
    fn get(&self, arguments: HashMap<String, String>) -> BoxFuture<'_, Result<GetPromptResult>> {
        Box::pin((self.handler)(arguments))
    }
}

/// Handler that receives request context
struct ContextAwareHandler<F> {
    handler: F,
}

impl<F, Fut> PromptHandler for ContextAwareHandler<F>
where
    F: Fn(RequestContext, HashMap<String, String>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<GetPromptResult>> + Send + 'static,
{
    fn get(&self, arguments: HashMap<String, String>) -> BoxFuture<'_, Result<GetPromptResult>> {
        // When called without context, create a dummy context
        let ctx = RequestContext::new(RequestId::Number(0));
        self.get_with_context(ctx, arguments)
    }

    fn get_with_context(
        &self,
        ctx: RequestContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GetPromptResult>> {
        Box::pin((self.handler)(ctx, arguments))
    }

    fn uses_context(&self) -> bool {
        true
    }
}

/// Handler wrapping a boxed service (used when middleware is applied)
///
/// Uses a Mutex to make the BoxCloneService (which is Send but not Sync) safe
/// for use in a Sync context. Since we clone the service before each call,
/// the lock is only held briefly during the clone.
struct ServiceHandler {
    service: Mutex<BoxPromptService>,
}

impl PromptHandler for ServiceHandler {
    fn get(&self, arguments: HashMap<String, String>) -> BoxFuture<'_, Result<GetPromptResult>> {
        Box::pin(async move {
            let req = PromptRequest::with_arguments(arguments);
            let mut service = self.service.lock().await.clone();
            match service.ready().await {
                Ok(svc) => svc.call(req).await.map_err(|e| match e {}),
                Err(e) => match e {},
            }
        })
    }

    fn get_with_context(
        &self,
        ctx: RequestContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GetPromptResult>> {
        Box::pin(async move {
            let req = PromptRequest::new(ctx, arguments);
            let mut service = self.service.lock().await.clone();
            match service.ready().await {
                Ok(svc) => svc.call(req).await.map_err(|e| match e {}),
                Err(e) => match e {},
            }
        })
    }
}

/// Handler wrapping a boxed service for context-aware prompts
struct ServiceContextHandler {
    service: Mutex<BoxPromptService>,
}

impl PromptHandler for ServiceContextHandler {
    fn get(&self, arguments: HashMap<String, String>) -> BoxFuture<'_, Result<GetPromptResult>> {
        let ctx = RequestContext::new(RequestId::Number(0));
        self.get_with_context(ctx, arguments)
    }

    fn get_with_context(
        &self,
        ctx: RequestContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GetPromptResult>> {
        Box::pin(async move {
            let req = PromptRequest::new(ctx, arguments);
            let mut service = self.service.lock().await.clone();
            match service.ready().await {
                Ok(svc) => svc.call(req).await.map_err(|e| match e {}),
                Err(e) => match e {},
            }
        })
    }

    fn uses_context(&self) -> bool {
        true
    }
}

// =============================================================================
// Trait-based prompt definition
// =============================================================================

/// Trait for defining prompts with full control
///
/// Implement this trait when you need more control than the builder provides,
/// or when you want to define prompts as standalone types.
///
/// # Example
///
/// ```rust
/// use std::collections::HashMap;
/// use tower_mcp::prompt::McpPrompt;
/// use tower_mcp::protocol::{GetPromptResult, PromptArgument, PromptMessage, PromptRole, Content};
/// use tower_mcp::error::Result;
///
/// struct CodeReviewPrompt;
///
/// impl McpPrompt for CodeReviewPrompt {
///     const NAME: &'static str = "code_review";
///     const DESCRIPTION: &'static str = "Review code for issues";
///
///     fn arguments(&self) -> Vec<PromptArgument> {
///         vec![
///             PromptArgument {
///                 name: "code".to_string(),
///                 description: Some("The code to review".to_string()),
///                 required: true,
///             },
///             PromptArgument {
///                 name: "language".to_string(),
///                 description: Some("Programming language".to_string()),
///                 required: false,
///             },
///         ]
///     }
///
///     async fn get(&self, args: HashMap<String, String>) -> Result<GetPromptResult> {
///         let code = args.get("code").map(|s| s.as_str()).unwrap_or("");
///         let lang = args.get("language").map(|s| s.as_str()).unwrap_or("unknown");
///
///         Ok(GetPromptResult {
///             description: Some("Code review prompt".to_string()),
///             messages: vec![PromptMessage {
///                 role: PromptRole::User,
///                 content: Content::Text {
///                     text: format!("Please review this {} code:\n\n```{}\n{}\n```", lang, lang, code),
///                     annotations: None,
///                     meta: None,
///                 },
///                 meta: None,
///             }],
///             meta: None,
///         })
///     }
/// }
///
/// let prompt = CodeReviewPrompt.into_prompt();
/// assert_eq!(prompt.name, "code_review");
/// ```
pub trait McpPrompt: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;

    /// Define the arguments for this prompt
    fn arguments(&self) -> Vec<PromptArgument> {
        Vec::new()
    }

    fn get(
        &self,
        arguments: HashMap<String, String>,
    ) -> impl Future<Output = Result<GetPromptResult>> + Send;

    /// Convert to a Prompt instance
    fn into_prompt(self) -> Prompt
    where
        Self: Sized,
    {
        let arguments = self.arguments();
        let prompt = Arc::new(self);
        Prompt {
            name: Self::NAME.to_string(),
            title: None,
            description: Some(Self::DESCRIPTION.to_string()),
            icons: None,
            arguments,
            handler: Arc::new(McpPromptHandler { prompt }),
        }
    }
}

/// Wrapper to make McpPrompt implement PromptHandler
struct McpPromptHandler<T: McpPrompt> {
    prompt: Arc<T>,
}

impl<T: McpPrompt> PromptHandler for McpPromptHandler<T> {
    fn get(&self, arguments: HashMap<String, String>) -> BoxFuture<'_, Result<GetPromptResult>> {
        let prompt = self.prompt.clone();
        Box::pin(async move { prompt.get(arguments).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_builder_prompt() {
        let prompt = PromptBuilder::new("greet")
            .description("A greeting prompt")
            .required_arg("name", "Name to greet")
            .handler(|args| async move {
                let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
                Ok(GetPromptResult {
                    description: Some("Greeting".to_string()),
                    messages: vec![PromptMessage {
                        role: PromptRole::User,
                        content: Content::Text {
                            text: format!("Hello, {}!", name),
                            annotations: None,
                            meta: None,
                        },
                        meta: None,
                    }],
                    meta: None,
                })
            })
            .build();

        assert_eq!(prompt.name, "greet");
        assert_eq!(prompt.description.as_deref(), Some("A greeting prompt"));
        assert_eq!(prompt.arguments.len(), 1);
        assert!(prompt.arguments[0].required);

        let mut args = HashMap::new();
        args.insert("name".to_string(), "Alice".to_string());
        let result = prompt.get(args).await.unwrap();

        assert_eq!(result.messages.len(), 1);
        match &result.messages[0].content {
            Content::Text { text, .. } => assert_eq!(text, "Hello, Alice!"),
            _ => panic!("Expected text content"),
        }
    }

    #[tokio::test]
    async fn test_static_prompt() {
        let prompt = PromptBuilder::new("help")
            .description("Help prompt")
            .user_message("How can I help you today?");

        let result = prompt.get(HashMap::new()).await.unwrap();
        assert_eq!(result.messages.len(), 1);
        match &result.messages[0].content {
            Content::Text { text, .. } => assert_eq!(text, "How can I help you today?"),
            _ => panic!("Expected text content"),
        }
    }

    #[tokio::test]
    async fn test_trait_prompt() {
        struct TestPrompt;

        impl McpPrompt for TestPrompt {
            const NAME: &'static str = "test";
            const DESCRIPTION: &'static str = "A test prompt";

            fn arguments(&self) -> Vec<PromptArgument> {
                vec![PromptArgument {
                    name: "input".to_string(),
                    description: Some("Test input".to_string()),
                    required: true,
                }]
            }

            async fn get(&self, args: HashMap<String, String>) -> Result<GetPromptResult> {
                let input = args.get("input").map(|s| s.as_str()).unwrap_or("default");
                Ok(GetPromptResult {
                    description: Some("Test".to_string()),
                    messages: vec![PromptMessage {
                        role: PromptRole::User,
                        content: Content::Text {
                            text: format!("Input: {}", input),
                            annotations: None,
                            meta: None,
                        },
                        meta: None,
                    }],
                    meta: None,
                })
            }
        }

        let prompt = TestPrompt.into_prompt();
        assert_eq!(prompt.name, "test");
        assert_eq!(prompt.arguments.len(), 1);

        let mut args = HashMap::new();
        args.insert("input".to_string(), "hello".to_string());
        let result = prompt.get(args).await.unwrap();

        match &result.messages[0].content {
            Content::Text { text, .. } => assert_eq!(text, "Input: hello"),
            _ => panic!("Expected text content"),
        }
    }

    #[test]
    fn test_prompt_definition() {
        let prompt = PromptBuilder::new("test")
            .description("Test description")
            .required_arg("arg1", "First arg")
            .optional_arg("arg2", "Second arg")
            .user_message("Test");

        let def = prompt.definition();
        assert_eq!(def.name, "test");
        assert_eq!(def.description.as_deref(), Some("Test description"));
        assert_eq!(def.arguments.len(), 2);
        assert!(def.arguments[0].required);
        assert!(!def.arguments[1].required);
    }

    #[tokio::test]
    async fn test_handler_with_context() {
        let prompt = PromptBuilder::new("context_prompt")
            .description("A prompt with context")
            .handler_with_context(|ctx: RequestContext, args| async move {
                // Verify we have access to the context
                let _ = ctx.is_cancelled();
                let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
                Ok(GetPromptResult {
                    description: Some("Context prompt".to_string()),
                    messages: vec![PromptMessage {
                        role: PromptRole::User,
                        content: Content::Text {
                            text: format!("Hello, {}!", name),
                            annotations: None,
                            meta: None,
                        },
                        meta: None,
                    }],
                    meta: None,
                })
            })
            .build();

        assert_eq!(prompt.name, "context_prompt");
        assert!(prompt.uses_context());

        let ctx = RequestContext::new(RequestId::Number(1));
        let mut args = HashMap::new();
        args.insert("name".to_string(), "Alice".to_string());
        let result = prompt.get_with_context(ctx, args).await.unwrap();

        match &result.messages[0].content {
            Content::Text { text, .. } => assert_eq!(text, "Hello, Alice!"),
            _ => panic!("Expected text content"),
        }
    }

    #[tokio::test]
    async fn test_prompt_with_timeout_layer() {
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let prompt = PromptBuilder::new("timeout_prompt")
            .description("A prompt with timeout")
            .handler(|args: HashMap<String, String>| async move {
                let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
                Ok(GetPromptResult {
                    description: Some("Timeout prompt".to_string()),
                    messages: vec![PromptMessage {
                        role: PromptRole::User,
                        content: Content::Text {
                            text: format!("Hello, {}!", name),
                            annotations: None,
                            meta: None,
                        },
                        meta: None,
                    }],
                    meta: None,
                })
            })
            .layer(TimeoutLayer::new(Duration::from_secs(5)));

        assert_eq!(prompt.name, "timeout_prompt");

        let mut args = HashMap::new();
        args.insert("name".to_string(), "Alice".to_string());
        let result = prompt.get(args).await.unwrap();

        match &result.messages[0].content {
            Content::Text { text, .. } => assert_eq!(text, "Hello, Alice!"),
            _ => panic!("Expected text content"),
        }
    }

    #[tokio::test]
    async fn test_prompt_timeout_expires() {
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let prompt = PromptBuilder::new("slow_prompt")
            .description("A slow prompt")
            .handler(|_args: HashMap<String, String>| async move {
                // Sleep much longer than timeout to ensure timeout fires reliably in CI
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(GetPromptResult {
                    description: Some("Slow prompt".to_string()),
                    messages: vec![PromptMessage {
                        role: PromptRole::User,
                        content: Content::Text {
                            text: "This should not appear".to_string(),
                            annotations: None,
                            meta: None,
                        },
                        meta: None,
                    }],
                    meta: None,
                })
            })
            .layer(TimeoutLayer::new(Duration::from_millis(50)));

        let result = prompt.get(HashMap::new()).await.unwrap();

        // Should get an error message due to timeout
        assert!(result.description.as_ref().unwrap().contains("error"));
        match &result.messages[0].content {
            Content::Text { text, .. } => {
                assert!(text.contains("Error generating prompt"));
            }
            _ => panic!("Expected text content"),
        }
    }

    #[tokio::test]
    async fn test_context_handler_with_layer() {
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let prompt = PromptBuilder::new("context_timeout")
            .description("Context prompt with timeout")
            .handler_with_context(
                |_ctx: RequestContext, args: HashMap<String, String>| async move {
                    let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
                    Ok(GetPromptResult {
                        description: Some("Context timeout".to_string()),
                        messages: vec![PromptMessage {
                            role: PromptRole::User,
                            content: Content::Text {
                                text: format!("Hello, {}!", name),
                                annotations: None,
                                meta: None,
                            },
                            meta: None,
                        }],
                        meta: None,
                    })
                },
            )
            .layer(TimeoutLayer::new(Duration::from_secs(5)));

        assert_eq!(prompt.name, "context_timeout");
        assert!(prompt.uses_context());

        let ctx = RequestContext::new(RequestId::Number(1));
        let mut args = HashMap::new();
        args.insert("name".to_string(), "Bob".to_string());
        let result = prompt.get_with_context(ctx, args).await.unwrap();

        match &result.messages[0].content {
            Content::Text { text, .. } => assert_eq!(text, "Hello, Bob!"),
            _ => panic!("Expected text content"),
        }
    }

    #[test]
    fn test_prompt_request_construction() {
        let args: HashMap<String, String> = [("key".to_string(), "value".to_string())]
            .into_iter()
            .collect();

        let req = PromptRequest::with_arguments(args.clone());
        assert_eq!(req.arguments.get("key"), Some(&"value".to_string()));

        let ctx = RequestContext::new(RequestId::Number(42));
        let req2 = PromptRequest::new(ctx, args);
        assert_eq!(req2.arguments.get("key"), Some(&"value".to_string()));
    }

    #[test]
    fn test_prompt_catch_error_clone() {
        // Just verify the type can be constructed and cloned
        let handler = PromptHandlerService {
            handler: |_args: HashMap<String, String>| async {
                Ok::<GetPromptResult, Error>(GetPromptResult {
                    description: None,
                    messages: vec![],
                    meta: None,
                })
            },
        };
        let catch_error = PromptCatchError::new(handler);
        let _clone = catch_error.clone();
        // PromptCatchError with PromptHandlerService doesn't implement Debug
        // because the handler function doesn't implement Debug
    }
}
