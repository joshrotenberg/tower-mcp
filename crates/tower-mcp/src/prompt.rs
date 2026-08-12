//! Prompts: named message templates a user invokes
//!
//! A prompt is a named generator of chat messages that the user picks
//! deliberately, unlike a tool, which the model calls. `prompts/list`
//! advertises the name, description, and arguments; `prompts/get` passes the
//! supplied arguments to the handler and returns the messages it built.
//!
//! Build one with [`PromptBuilder`], or write it as a type by implementing
//! [`McpPrompt`], then register it with
//! [`McpRouter::prompt`](crate::McpRouter::prompt).
//!
//! # Arguments are strings, and nothing checks them
//!
//! Arguments arrive as a `HashMap<String, String>` because the protocol
//! carries them that way, so a numeric argument is a string the handler
//! parses. [`PromptBuilder::required_arg`] and [`PromptBuilder::optional_arg`]
//! describe arguments to clients and nothing more: the router does not reject
//! a `prompts/get` that omits a required argument, so a handler that indexes
//! the map can be panicked by a client. Read arguments with `get` and supply
//! a default.
//!
//! # Handler forms
//!
//! | builder call | what the handler receives |
//! |---|---|
//! | [`PromptBuilder::handler`] | the arguments |
//! | [`PromptBuilder::handler_with_context`] | a [`RequestContext`] and the arguments |
//! | [`PromptBuilder::static_prompt`], [`PromptBuilder::user_message`] | no handler at all |
//!
//! With the `stateless` feature, `mrtr_handler` accepts a handler that can
//! suspend and ask the client for more input (SEP-2322).
//!
//! # A layer does not change what a handler error means
//!
//! An `Err` from the handler propagates and the router reports a JSON-RPC
//! error whether or not `.layer()` was applied. `.layer()` turns the handler
//! into a Tower service so a middleware stack can compose around it, but the
//! handler's own error is converted to a structured JSON-RPC error before it
//! ever reaches a layer, so it rides through untouched. Only a genuine
//! failure at the Tower level -- a timeout, a rate limit, anything from a
//! layer rather than from the handler -- is sanitized to a generic `-32603`
//! Internal Error:
//!
//! ```rust
//! use std::collections::HashMap;
//! use std::time::Duration;
//! use tower::timeout::TimeoutLayer;
//! use tower_mcp::error::Error;
//! use tower_mcp::prompt::PromptBuilder;
//!
//! # tokio_test::block_on(async {
//! let plain = PromptBuilder::new("plain")
//!     .handler(|_: HashMap<String, String>| async {
//!         Err(Error::internal("template store is offline"))
//!     })
//!     .build();
//! let plain_err = plain.get(HashMap::new()).await.unwrap_err();
//! assert!(plain_err.to_string().contains("template store is offline"));
//!
//! let layered = PromptBuilder::new("layered")
//!     .handler(|_: HashMap<String, String>| async {
//!         Err(Error::internal("template store is offline"))
//!     })
//!     .layer(TimeoutLayer::new(Duration::from_secs(5)));
//!
//! let layered_err = layered.get(HashMap::new()).await.unwrap_err();
//! assert!(layered_err.to_string().contains("template store is offline"));
//! # });
//! ```
//!
//! Per-prompt middleware is still the place for a bound that only one prompt
//! needs, such as a timeout on a prompt that reads from a network template
//! store. Layering the whole router covers everything else.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;

use tokio::sync::Mutex;
use tower::util::BoxCloneService;
use tower::{Layer, ServiceExt};
use tower_service::Service;

use crate::context::RequestContext;
use crate::error::{Error, JsonRpcError, Result};
use crate::protocol::{
    Content, GetPromptResult, PromptArgument, PromptDefinition, PromptMessage, PromptRole,
    RequestId, RequestOutcome, ToolIcon,
};

/// A boxed future for prompt handlers
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// =============================================================================
// Per-Prompt Middleware Types
// =============================================================================

/// What a layer applied with `.layer()` sees for one `prompts/get`.
///
/// Middleware runs on this rather than on the handler's arguments, so a layer
/// can read or rewrite the arguments, and reach the request context, without
/// knowing anything about the handler behind it.
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

    /// Create a request carrying a placeholder context.
    ///
    /// The context reports request id `0`, has no notification channel, and is
    /// never cancelled, which is what a test or an isolated layer needs and
    /// not what a real request carries.
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
/// handler can consume it without knowing the concrete middleware stack. The
/// service itself never fails at the Tower level; whether the prompt
/// succeeded is carried instead in the success value, `Ok(GetPromptResult)`
/// or `Err(JsonRpcError)`. A structured `JsonRpcError` here is the handler's
/// own error, converted before it reached any layer; [`PromptCatchError`]
/// only ever produces one itself for a genuine middleware failure.
pub type BoxPromptService =
    BoxCloneService<PromptRequest, std::result::Result<GetPromptResult, JsonRpcError>, Infallible>;

#[cfg(feature = "stateless")]
type BoxMrtrPromptService = BoxCloneService<
    PromptRequest,
    std::result::Result<RequestOutcome<GetPromptResult>, JsonRpcError>,
    Infallible,
>;

/// A service wrapper that catches errors from middleware and converts them
/// into prompt errors, maintaining the `Error = Infallible` contract.
///
/// When a middleware layer (e.g., `TimeoutLayer`) produces an error, this
/// wrapper converts it into a prompt error. This allows error information to
/// flow through the normal response path rather than requiring special
/// error handling.
#[doc(hidden)]
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

pin_project! {
    /// Future for [`PromptCatchError`].
    #[doc(hidden)]
    pub struct PromptCatchErrorFuture<F> {
        #[pin]
        inner: F,
    }
}

impl<F, E> Future for PromptCatchErrorFuture<F>
where
    F: Future<Output = std::result::Result<std::result::Result<GetPromptResult, JsonRpcError>, E>>,
    E: fmt::Display,
{
    type Output =
        std::result::Result<std::result::Result<GetPromptResult, JsonRpcError>, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().inner.poll(cx) {
            Poll::Pending => Poll::Pending,
            // The inner service already decided: either the prompt
            // succeeded, or the handler's own error was already converted
            // to a structured JsonRpcError. Either way, pass it through
            // untouched.
            Poll::Ready(Ok(inner)) => Poll::Ready(Ok(inner)),
            // The inner service itself failed at the Tower level: a genuine
            // middleware failure (timeout, rate limit, ...), not the
            // handler's own error. Sanitize it to a generic Internal Error.
            Poll::Ready(Err(err)) => {
                Poll::Ready(Ok(Err(JsonRpcError::internal_error(err.to_string()))))
            }
        }
    }
}

impl<S> Service<PromptRequest> for PromptCatchError<S>
where
    S: Service<PromptRequest, Response = std::result::Result<GetPromptResult, JsonRpcError>>
        + Clone
        + Send
        + 'static,
    S::Error: fmt::Display + Send,
    S::Future: Send,
{
    type Response = std::result::Result<GetPromptResult, JsonRpcError>;
    type Error = Infallible;
    type Future = PromptCatchErrorFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(|_| unreachable!())
    }

    fn call(&mut self, req: PromptRequest) -> Self::Future {
        PromptCatchErrorFuture {
            inner: self.inner.call(req),
        }
    }
}

#[cfg(feature = "stateless")]
#[derive(Clone)]
struct MrtrPromptCatchError<S> {
    inner: S,
}

#[cfg(feature = "stateless")]
impl<S> MrtrPromptCatchError<S> {
    fn new(inner: S) -> Self {
        Self { inner }
    }
}

#[cfg(feature = "stateless")]
impl<S> Service<PromptRequest> for MrtrPromptCatchError<S>
where
    S: Service<
            PromptRequest,
            Response = std::result::Result<RequestOutcome<GetPromptResult>, JsonRpcError>,
        > + Clone
        + Send
        + 'static,
    S::Error: fmt::Display + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = std::result::Result<RequestOutcome<GetPromptResult>, JsonRpcError>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        match self.inner.poll_ready(cx) {
            Poll::Ready(Ok(())) | Poll::Ready(Err(_)) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, req: PromptRequest) -> Self::Future {
        let future = self.inner.call(req);
        Box::pin(async move {
            Ok(match future.await {
                Ok(inner) => inner,
                Err(error) => Err(JsonRpcError::internal_error(error.to_string())),
            })
        })
    }
}

/// Adapts a prompt handler function into a `Service<PromptRequest>`.
///
/// This allows the handler to be wrapped with tower middleware layers.
/// Used by `.layer()` on `PromptBuilderWithHandler`.
#[doc(hidden)]
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
    type Response = std::result::Result<GetPromptResult, JsonRpcError>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: PromptRequest) -> Self::Future {
        let handler = self.handler.clone();
        Box::pin(async move {
            Ok(handler(req.arguments)
                .await
                .map_err(Error::into_json_rpc_error))
        })
    }
}

/// Adapts a context-aware prompt handler function into a `Service<PromptRequest>`.
///
/// Used by `.layer()` on `PromptBuilderWithContextHandler`.
#[doc(hidden)]
pub struct PromptContextHandlerService<F> {
    handler: F,
}

#[cfg(feature = "stateless")]
#[doc(hidden)]
pub struct MrtrPromptHandlerService<F> {
    handler: F,
}

#[cfg(feature = "stateless")]
impl<F: Clone> Clone for MrtrPromptHandlerService<F> {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
        }
    }
}

#[cfg(feature = "stateless")]
impl<F, Fut> Service<PromptRequest> for MrtrPromptHandlerService<F>
where
    F: Fn(RequestContext, HashMap<String, String>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<RequestOutcome<GetPromptResult>>> + Send + 'static,
{
    type Response = std::result::Result<RequestOutcome<GetPromptResult>, JsonRpcError>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: PromptRequest) -> Self::Future {
        let handler = self.handler.clone();
        Box::pin(async move {
            Ok(handler(req.context, req.arguments)
                .await
                .map_err(Error::into_json_rpc_error))
        })
    }
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
    type Response = std::result::Result<GetPromptResult, JsonRpcError>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: PromptRequest) -> Self::Future {
        let handler = self.handler.clone();
        Box::pin(async move {
            Ok(handler(req.context, req.arguments)
                .await
                .map_err(Error::into_json_rpc_error))
        })
    }
}

/// The shape a prompt handler is adapted to internally.
///
/// [`PromptBuilder`] wraps closures, and layered services, in an
/// implementation of this trait, and the router drives every prompt through
/// it. No public constructor accepts an implementation, so a prompt written as
/// a type rather than a closure implements [`McpPrompt`] instead.
pub(crate) trait PromptHandler: Send + Sync {
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

/// Prompt handler that may return an SEP-2322 input-required continuation.
#[cfg(feature = "stateless")]
pub(crate) trait MrtrPromptHandler: Send + Sync {
    /// Resolve a prompt attempt with continuation values available through
    /// the request context.
    fn get(
        &self,
        ctx: RequestContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<RequestOutcome<GetPromptResult>>>;
}

/// A prompt that is ready to register.
///
/// Produced by [`PromptBuilder`] or [`McpPrompt::into_prompt`] and consumed by
/// [`McpRouter::prompt`](crate::McpRouter::prompt). The public fields are
/// exactly what `prompts/list` advertises; the handler behind them is private,
/// so a prompt is cheap to clone and to share between routers.
pub struct Prompt {
    /// The prompt name (must be unique within the router).
    pub name: String,
    /// Optional human-readable title.
    pub title: Option<String>,
    /// Optional description of the prompt.
    pub description: Option<String>,
    /// Optional icons for the prompt.
    pub icons: Option<Vec<ToolIcon>>,
    /// The arguments this prompt accepts.
    pub arguments: Vec<PromptArgument>,
    handler: Option<Arc<dyn PromptHandler>>,
    #[cfg(feature = "stateless")]
    mrtr_handler: Option<Arc<dyn MrtrPromptHandler>>,
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
            #[cfg(feature = "stateless")]
            mrtr_handler: self.mrtr_handler.clone(),
        }
    }
}

/// Reject a `prompts/get` that omits an argument the prompt declares required.
///
/// The spec requires missing required arguments to produce `-32602`, and
/// `prompts/list` already tells clients which ones are required, so the router
/// was advertising a guarantee nothing provided and leaving every handler to
/// re-implement the same check (#1281).
///
/// Deliberately narrow. Arguments are unvalidated strings by design, so this
/// checks presence and nothing else:
///
/// - an empty string is present, since emptiness is a value the handler may
///   legitimately want;
/// - an omitted optional argument is fine;
/// - unknown extra keys are accepted, because the protocol does not say prompt
///   arguments are a closed set.
///
/// Missing names come back sorted, so the error is the same whichever way the
/// map iterated.
pub(crate) fn missing_required_arguments(
    declared: &[PromptArgument],
    supplied: &HashMap<String, String>,
) -> Vec<String> {
    let mut missing: Vec<String> = declared
        .iter()
        .filter(|argument| argument.required && !supplied.contains_key(&argument.name))
        .map(|argument| argument.name.clone())
        .collect();
    missing.sort();
    missing
}

/// The `-32602` for a `prompts/get` missing required arguments.
///
/// The names travel in `data.missingArguments` as well as the message, so a
/// client can act on them without parsing prose.
pub(crate) fn missing_arguments_error(
    name: &str,
    missing: &[String],
) -> crate::error::JsonRpcError {
    crate::error::JsonRpcError::invalid_params(format!(
        "prompt '{name}' is missing required argument{}: {}",
        if missing.len() == 1 { "" } else { "s" },
        missing.join(", ")
    ))
    .with_data(serde_json::json!({ "missingArguments": missing }))
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

    /// The entry this prompt contributes to `prompts/list`.
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

    /// Generate the messages for these arguments.
    ///
    /// A handler registered through [`PromptBuilder::handler_with_context`]
    /// still runs, but against a placeholder context, so progress reporting
    /// and cancellation are inert. The router calls
    /// [`get_with_context`](Self::get_with_context) instead.
    ///
    /// A handler error arrives as `Err` whether or not the prompt carries
    /// middleware; see the [module documentation](crate::prompt).
    pub fn get(
        &self,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GetPromptResult>> {
        match &self.handler {
            Some(handler) => handler.get(arguments),
            None => Box::pin(async {
                Err(Error::invalid_params(
                    "MRTR prompt requires get_outcome_with_context",
                ))
            }),
        }
    }

    /// Generate the messages with the context the transport built.
    ///
    /// This is the path the router takes. The context carries the request id,
    /// progress reporting, the cancellation token, and the client requester
    /// used for sampling and elicitation.
    ///
    /// A prompt registered with `mrtr_handler` has no plain handler to call
    /// and reports that as an error here; use
    /// [`get_outcome_with_context`](Self::get_outcome_with_context).
    pub fn get_with_context(
        &self,
        ctx: RequestContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GetPromptResult>> {
        match &self.handler {
            Some(handler) => handler.get_with_context(ctx, arguments),
            None => Box::pin(async {
                Err(Error::invalid_params(
                    "MRTR prompt requires get_outcome_with_context",
                ))
            }),
        }
    }

    /// Generate the messages while preserving an SEP-2322 continuation.
    ///
    /// The router calls this so a handler that needs more input from the
    /// client can say so, rather than having the request fail or return
    /// half-built messages. A prompt without an MRTR handler answers here
    /// too, always with a completed result.
    pub fn get_outcome_with_context(
        &self,
        ctx: RequestContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<RequestOutcome<GetPromptResult>>> {
        #[cfg(feature = "stateless")]
        if let Some(handler) = &self.mrtr_handler {
            return handler.get(ctx, arguments);
        }
        match &self.handler {
            Some(handler) => Box::pin(async move {
                handler
                    .get_with_context(ctx, arguments)
                    .await
                    .map(RequestOutcome::Complete)
            }),
            None => Box::pin(async {
                Err(Error::invalid_params(
                    "prompt has neither a complete nor MRTR handler",
                ))
            }),
        }
    }

    /// Whether the handler was registered in a context-aware form.
    ///
    /// An MRTR prompt reports `true` as well, since a continuation is only
    /// reachable through the context. The router does not branch on this; it
    /// is here for code that drives prompts directly.
    pub fn uses_context(&self) -> bool {
        self.handler
            .as_ref()
            .is_none_or(|handler| handler.uses_context())
    }
}

// =============================================================================
// Builder API
// =============================================================================

/// Builder for a prompt.
///
/// The name is the identity clients call by and must be unique in the router.
/// Declare the arguments so a client knows what to collect, attach a handler,
/// and finish with `.build()`. For a prompt with no logic behind it, end the
/// chain at [`user_message`](Self::user_message) or
/// [`static_prompt`](Self::static_prompt) instead.
///
/// # Example
///
/// ```rust
/// use std::collections::HashMap;
/// use tower_mcp::prompt::PromptBuilder;
/// use tower_mcp::protocol::GetPromptResult;
///
/// # tokio_test::block_on(async {
/// let prompt = PromptBuilder::new("greet")
///     .description("Generate a greeting")
///     .required_arg("name", "The name to greet")
///     .handler(|args: HashMap<String, String>| async move {
///         // Required arguments are advertised, not enforced, so read
///         // defensively rather than indexing.
///         let name = args.get("name").map(String::as_str).unwrap_or("World");
///         Ok(GetPromptResult::user_message_with_description(
///             format!("Please greet {name}"),
///             "A greeting prompt",
///         ))
///     })
///     .build();
///
/// assert_eq!(prompt.name, "greet");
///
/// let result = prompt.get(HashMap::new()).await.unwrap();
/// assert_eq!(result.first_message_text(), Some("Please greet World"));
/// # });
/// ```
pub struct PromptBuilder {
    name: String,
    title: Option<String>,
    description: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    arguments: Vec<PromptArgument>,
}

impl PromptBuilder {
    /// Start a prompt with this name.
    ///
    /// The name is the key the router registers under, so registering a second
    /// prompt with the same name replaces the first rather than failing.
    /// [`McpRouter::try_merge`](crate::McpRouter::try_merge) reports that
    /// collision when composing routers.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            title: None,
            description: None,
            icons: None,
            arguments: Vec::new(),
        }
    }

    /// Set the display title, which a client prefers over the name.
    ///
    /// Worth setting when the name has to stay stable for other reasons and
    /// is not what a person should read.
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

    /// Declare an argument clients are expected to supply.
    ///
    /// This is advertisement, not validation. The argument appears in
    /// `prompts/list` so a client can collect it, and a `prompts/get` that
    /// omits it still reaches the handler, so the handler needs a fallback.
    pub fn required_arg(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.arguments.push(PromptArgument {
            name: name.into(),
            description: Some(description.into()),
            required: true,
        });
        self
    }

    /// Declare an argument clients may supply.
    ///
    /// The only difference from [`required_arg`](Self::required_arg) is the
    /// flag clients see, since neither is enforced server-side.
    pub fn optional_arg(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.arguments.push(PromptArgument {
            name: name.into(),
            description: Some(description.into()),
            required: false,
        });
        self
    }

    /// Add an argument that was built elsewhere.
    ///
    /// For argument metadata that comes from a schema, a manifest, or another
    /// prompt, rather than from a literal in the builder chain.
    pub fn argument(mut self, arg: PromptArgument) -> Self {
        self.arguments.push(arg);
        self
    }

    /// Set the handler that builds the messages.
    ///
    /// The handler is called with whatever arguments the client sent, as
    /// strings, with no entry at all for the ones it left out. Finish with
    /// `.build()`, or apply middleware with `.layer()` first; a handler's
    /// `Err` propagates the same way either way.
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

    /// Set a handler that receives the [`RequestContext`] with the arguments.
    ///
    /// Worth the extra parameter when assembling the messages is expensive
    /// enough to report progress against or to abandon on cancellation, or
    /// when the prompt needs something from the client. Otherwise use
    /// [`handler`](Self::handler).
    ///
    /// ```rust
    /// use std::collections::HashMap;
    /// use tower_mcp::context::RequestContext;
    /// use tower_mcp::error::Error;
    /// use tower_mcp::prompt::PromptBuilder;
    /// use tower_mcp::protocol::GetPromptResult;
    ///
    /// let prompt = PromptBuilder::new("summarize_changes")
    ///     .description("Summarize the changed files")
    ///     .handler_with_context(
    ///         |ctx: RequestContext, _args: HashMap<String, String>| async move {
    ///             let files = ["src/lib.rs", "src/router.rs"];
    ///             let mut body = String::new();
    ///             for (index, file) in files.iter().enumerate() {
    ///                 if ctx.is_cancelled() {
    ///                     return Err(Error::internal("cancelled while reading files"));
    ///                 }
    ///                 ctx.report_progress(index as f64, Some(files.len() as f64), Some(file))
    ///                     .await;
    ///                 body.push_str(file);
    ///                 body.push('\n');
    ///             }
    ///             Ok(GetPromptResult::user_message(format!("Summarize:\n{body}")))
    ///         },
    ///     )
    ///     .build();
    ///
    /// # tokio_test::block_on(async {
    /// let result = prompt.get(HashMap::new()).await.unwrap();
    /// let text = result.first_message_text().unwrap();
    /// assert!(text.contains("src/router.rs"), "{text}");
    /// # });
    /// ```
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

    /// Set an SEP-2322 prompt handler that may return input-required.
    #[cfg(feature = "stateless")]
    pub fn mrtr_handler<F, Fut>(self, handler: F) -> PromptBuilderWithMrtrHandler<F>
    where
        F: Fn(RequestContext, HashMap<String, String>) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<RequestOutcome<GetPromptResult>>> + Send + 'static,
    {
        PromptBuilderWithMrtrHandler {
            name: self.name,
            title: self.title,
            description: self.description,
            icons: self.icons,
            arguments: self.arguments,
            handler,
        }
    }

    /// Finish the builder with a fixed list of messages.
    ///
    /// Arguments are ignored, so this is for a prompt that is the same every
    /// time: a checklist, a house style, a standing instruction. The
    /// description set on the builder is carried into every result.
    ///
    /// ```rust
    /// use std::collections::HashMap;
    /// use tower_mcp::prompt::PromptBuilder;
    /// use tower_mcp::protocol::{Content, PromptMessage, PromptRole};
    ///
    /// # tokio_test::block_on(async {
    /// let prompt = PromptBuilder::new("review_checklist")
    ///     .description("The standing review checklist")
    ///     .static_prompt(vec![
    ///         PromptMessage {
    ///             role: PromptRole::Assistant,
    ///             content: Content::text("I check correctness before style."),
    ///             meta: None,
    ///         },
    ///         PromptMessage {
    ///             role: PromptRole::User,
    ///             content: Content::text("Review the diff."),
    ///             meta: None,
    ///         },
    ///     ]);
    ///
    /// let result = prompt.get(HashMap::new()).await.unwrap();
    /// assert_eq!(result.messages.len(), 2);
    /// assert_eq!(
    ///     result.description.as_deref(),
    ///     Some("The standing review checklist")
    /// );
    /// # });
    /// ```
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

    /// Finish the builder with one user message.
    ///
    /// The shortest complete prompt there is, and the shape most prompts that
    /// only seed a conversation want.
    ///
    /// ```rust
    /// use std::collections::HashMap;
    /// use tower_mcp::prompt::PromptBuilder;
    ///
    /// # tokio_test::block_on(async {
    /// let prompt = PromptBuilder::new("help")
    ///     .description("Ask the user what they need")
    ///     .user_message("How can I help you today?");
    ///
    /// let result = prompt.get(HashMap::new()).await.unwrap();
    /// assert_eq!(result.first_message_text(), Some("How can I help you today?"));
    /// # });
    /// ```
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

    /// Attach a handler and finish, in one call.
    ///
    /// `PromptBuilder::build(handler)` and `handler(handler).build()` produce
    /// the same prompt. Note that the argument-less `.build()` seen in most
    /// examples belongs to the builder [`handler`](Self::handler) returns, not
    /// to this type, so only this form takes the handler as a parameter, and
    /// only the other form can be preceded by `.layer()`.
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

/// Builder state after an MRTR handler is specified.
///
/// The multi-round-trip form of [`PromptBuilderWithHandler`], reached by
/// [`PromptBuilder::mrtr_handler`]. Same choices from here: apply middleware
/// with `.layer()`, or finish with `.build()`.
#[cfg(feature = "stateless")]
pub struct PromptBuilderWithMrtrHandler<F> {
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
            handler: Some(Arc::new(FnHandler {
                handler: self.handler,
            })),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
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
        L::Service: Service<PromptRequest, Response = std::result::Result<GetPromptResult, JsonRpcError>>
            + Clone
            + Send
            + 'static,
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
            handler: Some(Arc::new(ServiceHandler {
                service: Mutex::new(boxed),
            })),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
        }
    }
}

#[cfg(feature = "stateless")]
impl<F, Fut> PromptBuilderWithMrtrHandler<F>
where
    F: Fn(RequestContext, HashMap<String, String>) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = Result<RequestOutcome<GetPromptResult>>> + Send + 'static,
{
    /// Build the MRTR-capable prompt.
    pub fn build(self) -> Prompt {
        Prompt {
            name: self.name,
            title: self.title,
            description: self.description,
            icons: self.icons,
            arguments: self.arguments,
            handler: None,
            mrtr_handler: Some(Arc::new(MrtrContextHandler {
                handler: self.handler,
            })),
        }
    }

    /// Apply a Tower layer to every attempt at this MRTR-capable prompt.
    ///
    /// Each retry is an independent request, so the layer runs once per
    /// round. A genuine middleware failure is sanitized to a `-32603`
    /// JSON-RPC error, matching non-MRTR prompt middleware; the handler's
    /// own error, if any, rides through untouched.
    #[allow(private_bounds)]
    pub fn layer<L>(self, layer: L) -> Prompt
    where
        L: Layer<MrtrPromptHandlerService<F>> + Send + Sync + 'static,
        L::Service: Service<
                PromptRequest,
                Response = std::result::Result<RequestOutcome<GetPromptResult>, JsonRpcError>,
            > + Clone
            + Send
            + 'static,
        <L::Service as Service<PromptRequest>>::Error: fmt::Display + Send + 'static,
        <L::Service as Service<PromptRequest>>::Future: Send + 'static,
    {
        let service = MrtrPromptHandlerService {
            handler: self.handler,
        };
        let service = layer.layer(service);
        let service = BoxCloneService::new(MrtrPromptCatchError::new(service));

        Prompt {
            name: self.name,
            title: self.title,
            description: self.description,
            icons: self.icons,
            arguments: self.arguments,
            handler: None,
            mrtr_handler: Some(Arc::new(ServiceMrtrPromptHandler {
                service: Mutex::new(service),
            })),
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
            handler: Some(Arc::new(ContextAwareHandler {
                handler: self.handler,
            })),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
        }
    }

    /// Apply a tower middleware layer to this prompt
    pub fn layer<L>(self, layer: L) -> Prompt
    where
        L: Layer<PromptContextHandlerService<F>> + Send + Sync + 'static,
        L::Service: Service<PromptRequest, Response = std::result::Result<GetPromptResult, JsonRpcError>>
            + Clone
            + Send
            + 'static,
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
            handler: Some(Arc::new(ServiceContextHandler {
                service: Mutex::new(boxed),
            })),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
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

#[cfg(feature = "stateless")]
struct MrtrContextHandler<F> {
    handler: F,
}

#[cfg(feature = "stateless")]
struct ServiceMrtrPromptHandler {
    service: Mutex<BoxMrtrPromptService>,
}

#[cfg(feature = "stateless")]
impl MrtrPromptHandler for ServiceMrtrPromptHandler {
    fn get(
        &self,
        ctx: RequestContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<RequestOutcome<GetPromptResult>>> {
        Box::pin(async move {
            let request = PromptRequest::new(ctx, arguments);
            let mut service = self.service.lock().await.clone();
            let outcome = service
                .ready()
                .await
                .expect("MRTR prompt service is infallible")
                .call(request)
                .await
                .expect("MRTR prompt service is infallible");
            outcome.map_err(Into::into)
        })
    }
}

#[cfg(feature = "stateless")]
impl<F, Fut> MrtrPromptHandler for MrtrContextHandler<F>
where
    F: Fn(RequestContext, HashMap<String, String>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RequestOutcome<GetPromptResult>>> + Send + 'static,
{
    fn get(
        &self,
        ctx: RequestContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<RequestOutcome<GetPromptResult>>> {
        Box::pin((self.handler)(ctx, arguments))
    }
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
            let outcome = match service.ready().await {
                Ok(svc) => svc.call(req).await,
                Err(e) => match e {},
            };
            match outcome {
                Ok(inner) => inner.map_err(Into::into),
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
            let outcome = match service.ready().await {
                Ok(svc) => svc.call(req).await,
                Err(e) => match e {},
            };
            match outcome {
                Ok(inner) => inner.map_err(Into::into),
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
            let outcome = match service.ready().await {
                Ok(svc) => svc.call(req).await,
                Err(e) => match e {},
            };
            match outcome {
                Ok(inner) => inner.map_err(Into::into),
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

/// Define a prompt as a type rather than a closure.
///
/// The name and description are associated constants and the arguments come
/// from a method, so a prompt with state holds it in `self` rather than in an
/// `Arc` captured by a closure. Finish with
/// [`into_prompt`](Self::into_prompt), which produces the same [`Prompt`] the
/// builder does.
///
/// # Example
///
/// ```rust
/// use std::collections::HashMap;
/// use tower_mcp::{Content, GetPromptResult, McpPrompt, PromptArgument, PromptMessage, PromptRole, Result};
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
    /// The prompt name (must be unique within the router).
    const NAME: &'static str;
    /// A human-readable description of the prompt.
    const DESCRIPTION: &'static str;

    /// Define the arguments for this prompt
    fn arguments(&self) -> Vec<PromptArgument> {
        Vec::new()
    }

    /// Generate the prompt messages for the given arguments.
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
            handler: Some(Arc::new(McpPromptHandler { prompt })),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
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

        // The timeout is a genuine middleware failure (#1280), not the
        // handler's own error, so it propagates as an Err sanitized to
        // -32603 rather than becoming a successful prompt whose assistant
        // message carries the error text.
        let error = prompt.get(HashMap::new()).await.unwrap_err();
        assert!(error.to_string().contains("-32603"));
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

    #[tokio::test]
    async fn test_prompt_handler_with_arguments() {
        let prompt = PromptBuilder::new("greet")
            .description("Greeting prompt")
            .required_arg("name", "Person to greet")
            .optional_arg("style", "Greeting style")
            .handler(|args: HashMap<String, String>| async move {
                let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
                let style = args.get("style").map(|s| s.as_str()).unwrap_or("casual");
                let text = match style {
                    "formal" => format!("Good day, {name}."),
                    _ => format!("Hey {name}!"),
                };
                Ok(GetPromptResult::user_message(text))
            })
            .build();

        // Test with both arguments
        let mut args = HashMap::new();
        args.insert("name".to_string(), "Alice".to_string());
        args.insert("style".to_string(), "formal".to_string());
        let result = prompt.get(args).await.unwrap();
        assert_eq!(result.messages.len(), 1);

        // Test with required arg only
        let mut args = HashMap::new();
        args.insert("name".to_string(), "Bob".to_string());
        let result = prompt.get(args).await.unwrap();
        assert_eq!(result.messages.len(), 1);
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn test_mrtr_builder_preserves_input_required_outcome() {
        let prompt = PromptBuilder::new("continue")
            .mrtr_handler(|_ctx, _args| async move {
                Ok(RequestOutcome::input_required(
                    crate::protocol::InputRequiredResult::new().with_request_state("signed-state"),
                ))
            })
            .build();

        let outcome = prompt
            .get_outcome_with_context(RequestContext::new(RequestId::Number(1)), HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            outcome
                .as_input_required()
                .and_then(|result| result.request_state.as_deref()),
            Some("signed-state")
        );
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn mrtr_prompt_composes_middleware() {
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let prompt = PromptBuilder::new("layered_continue")
            .mrtr_handler(|_ctx, _args| async move {
                Ok(RequestOutcome::input_required(
                    crate::protocol::InputRequiredResult::new().with_request_state("layered-state"),
                ))
            })
            .layer(TimeoutLayer::new(Duration::from_secs(1)));

        let outcome = prompt
            .get_outcome_with_context(RequestContext::new(RequestId::Number(2)), HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            outcome
                .as_input_required()
                .and_then(|result| result.request_state.as_deref()),
            Some("layered-state")
        );
    }

    #[tokio::test]
    async fn test_prompt_definition_fields() {
        let prompt = PromptBuilder::new("test_prompt")
            .title("Test Prompt")
            .description("A test prompt")
            .required_arg("input", "The input")
            .optional_arg("format", "Output format")
            .handler(|_args: HashMap<String, String>| async move {
                Ok(GetPromptResult::user_message("test"))
            })
            .build();

        let def = prompt.definition();
        assert_eq!(def.name, "test_prompt");
        assert_eq!(def.title.as_deref(), Some("Test Prompt"));
        assert_eq!(def.description.as_deref(), Some("A test prompt"));
        assert_eq!(def.arguments.len(), 2);
        assert!(def.arguments[0].required);
        assert!(!def.arguments[1].required);
    }

    #[tokio::test]
    async fn test_prompt_with_context_handler() {
        let prompt = PromptBuilder::new("ctx_prompt")
            .description("Context-aware prompt")
            .handler_with_context(
                |ctx: RequestContext, args: HashMap<String, String>| async move {
                    let _ = ctx;
                    let name = args.get("name").map(|s| s.as_str()).unwrap_or("default");
                    Ok(GetPromptResult::user_message(format!("ctx: {name}")))
                },
            )
            .build();

        assert!(prompt.uses_context());

        let mut args = HashMap::new();
        args.insert("name".to_string(), "test".to_string());
        let ctx = RequestContext::new(RequestId::Number(1));
        let result: std::result::Result<GetPromptResult, Error> =
            prompt.get_with_context(ctx, args).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().messages.len(), 1);
    }

    #[tokio::test]
    async fn test_prompt_with_layer_catches_timeout() {
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let prompt = PromptBuilder::new("slow_prompt")
            .description("Will timeout")
            .handler(|_args: HashMap<String, String>| async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(GetPromptResult::user_message("too late"))
            })
            .layer(TimeoutLayer::new(Duration::from_millis(10)));

        // The prompt goes through ServiceHandler -> PromptCatchError. The
        // timeout is a genuine middleware failure (#1280), so it comes back
        // as a sanitized Err, not a successful GetPromptResult with the
        // error text folded into the content.
        let error = prompt.get(HashMap::new()).await.unwrap_err();
        assert!(error.to_string().contains("-32603"));
    }

    #[tokio::test]
    async fn test_prompt_clone() {
        let prompt = PromptBuilder::new("cloneable")
            .description("Can be cloned")
            .handler(|_args: HashMap<String, String>| async move {
                Ok(GetPromptResult::user_message("original"))
            })
            .build();

        let cloned = prompt.clone();
        assert_eq!(cloned.name, "cloneable");

        let result = cloned.get(HashMap::new()).await.unwrap();
        assert_eq!(result.messages.len(), 1);
    }
}
