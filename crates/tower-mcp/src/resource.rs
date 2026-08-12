//! Resources: one fixed URI, or a URI template that routes
//!
//! A resource is content a client reads by URI. Which of the two kinds you
//! build depends on whether the URI is known before the request arrives:
//!
//! - [`Resource`], built with [`ResourceBuilder`], answers for exactly one
//!   URI and appears in `resources/list`.
//! - [`ResourceTemplate`], built with [`ResourceTemplateBuilder`], answers for
//!   a family of URIs described by an RFC 6570 template, appears in
//!   `resources/templates/list`, and hands its handler the variables it
//!   extracted from the request URI.
//!
//! A resource can also be written as a type rather than a closure by
//! implementing [`McpResource`].
//!
//! # How a read finds its handler
//!
//! [`McpRouter`](crate::McpRouter) answers `resources/read` by trying an exact
//! URI match against the registered resources first, then each registered
//! template in registration order, stopping at the first template that
//! matches. Registration order is the only tie-break, so register the narrower
//! pattern first: `db://users/{id}` before `db://{+rest}`, or the second one
//! swallows every read and the first never runs.
//!
//! # Handler forms
//!
//! | builder call | what the handler receives |
//! |---|---|
//! | [`ResourceBuilder::handler`] | nothing |
//! | [`ResourceBuilder::handler_with_context`] | a [`RequestContext`] for progress, cancellation, and client requests |
//! | [`ResourceTemplateBuilder::handler`] | the request URI and the extracted variables |
//!
//! Each returns a [`ReadResourceResult`]. For content already in memory,
//! [`ResourceBuilder::text`] and [`ResourceBuilder::json`] finish the builder
//! without a handler at all. With the `stateless` feature, `mrtr_handler`
//! accepts a handler that can suspend and ask the client for more input
//! (SEP-2322).
//!
//! # A failing resource handler is not a failed request
//!
//! Every [`Resource`] handler is wrapped in an error-catching service, because
//! a Tower stack composes only when the inner service cannot fail. An `Err`
//! from the handler, or from middleware such as a timeout, therefore reaches
//! the client as a successful read whose text is the error message, not as a
//! JSON-RPC error. [`ResourceTemplate`] handlers are not wrapped: their errors
//! propagate and the router reports them as JSON-RPC errors. See
//! [`Resource::read`] for a worked example of the difference.
//!
//! # Per-resource middleware
//!
//! Resources are Tower services internally, so any layer composes onto a
//! single resource through `.layer()`. This is the per-resource counterpart to
//! layering the whole router, and the place to put a bound that only one
//! expensive resource needs:
//!
//! ```rust
//! use std::time::Duration;
//! use tower::timeout::TimeoutLayer;
//! use tower_mcp::resource::ResourceBuilder;
//! use tower_mcp::protocol::ReadResourceResult;
//!
//! let resource = ResourceBuilder::new("file:///large-file.txt")
//!     .name("Large File")
//!     .description("A file large enough that a read can hang")
//!     .handler(|| async {
//!         Ok(ReadResourceResult::text("file:///large-file.txt", "content"))
//!     })
//!     .layer(TimeoutLayer::new(Duration::from_secs(30)))
//!     .build();
//! ```
//!
//! # URI templates
//!
//! A template is compiled once, at build time, into a matcher:
//!
//! - `{var}` matches any run of non-slash characters
//! - `{+var}` matches any characters at all, including `/`
//! - `{?a,b}` and `{&a,b}` declare optional query parameters (#1253)
//!
//! [`ResourceTemplate::match_uri`] documents the matching rules and shows each
//! of them running against real URIs.
//!
//! ```rust
//! use tower_mcp::resource::ResourceTemplateBuilder;
//! use tower_mcp::protocol::ReadResourceResult;
//! use std::collections::HashMap;
//!
//! let template = ResourceTemplateBuilder::new("file:///{+path}")
//!     .name("Project Files")
//!     .description("Any file under the project directory")
//!     .handler(|uri: String, vars: HashMap<String, String>| async move {
//!         let path = vars["path"].clone();
//!         Ok(ReadResourceResult::text(uri, format!("contents of {path}")))
//!     });
//!
//! // `{+path}` spans slashes, so nested paths reach the same handler.
//! let vars = template.match_uri("file:///src/lib.rs").unwrap();
//! assert_eq!(vars["path"], "src/lib.rs");
//! ```

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use serde_json::Value;

#[cfg(feature = "stateless")]
use tower::ServiceExt;
use tower::util::BoxCloneService;
use tower_service::Service;

#[cfg(feature = "stateless")]
use tokio::sync::Mutex;

use crate::context::RequestContext;
use crate::error::{Error, Result};
use crate::protocol::{
    ContentAnnotations, PromptArgument, ReadResourceResult, RequestOutcome, ResourceContent,
    ResourceDefinition, ResourceTemplateDefinition, ToolIcon,
};

// =============================================================================
// Service Types for Per-Resource Middleware
// =============================================================================

/// What a layer applied with `.layer()` sees for one read.
///
/// Middleware runs on this rather than on the handler's arguments, so a layer
/// can read the URI and the request context without knowing anything about the
/// handler behind it.
#[derive(Debug, Clone)]
pub struct ResourceRequest {
    /// Request context for progress reporting, cancellation, and client requests
    pub ctx: RequestContext,
    /// The URI of the resource being read
    pub uri: String,
}

impl ResourceRequest {
    /// Create a new resource request
    pub fn new(ctx: RequestContext, uri: String) -> Self {
        Self { ctx, uri }
    }
}

/// A boxed, cloneable resource service with `Error = Infallible`.
///
/// This is the internal service type that resources use. Middleware errors are
/// caught and converted to error results, so the service never fails at the Tower level.
pub type BoxResourceService = BoxCloneService<ResourceRequest, ReadResourceResult, Infallible>;

#[cfg(feature = "stateless")]
type BoxMrtrResourceService =
    BoxCloneService<ResourceRequest, RequestOutcome<ReadResourceResult>, Infallible>;

/// Catches errors from the inner service and converts them to error results.
///
/// This wrapper ensures that middleware errors (e.g., timeouts, rate limits)
/// and handler errors are converted to `Err(Error)` responses wrapped in
/// `Ok`, rather than propagating as Tower service errors.
#[doc(hidden)]
pub struct ResourceCatchError<S> {
    inner: S,
}

impl<S> ResourceCatchError<S> {
    /// Create a new `ResourceCatchError` wrapping the given service.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S: Clone> Clone for ResourceCatchError<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: fmt::Debug> fmt::Debug for ResourceCatchError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResourceCatchError")
            .field("inner", &self.inner)
            .finish()
    }
}

pin_project! {
    /// Future for [`ResourceCatchError`].
    #[doc(hidden)]
    pub struct ResourceCatchErrorFuture<F> {
        #[pin]
        inner: F,
        uri: Option<String>,
    }
}

impl<F, E> Future for ResourceCatchErrorFuture<F>
where
    F: Future<Output = std::result::Result<ReadResourceResult, E>>,
    E: fmt::Display,
{
    type Output = std::result::Result<ReadResourceResult, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(result)) => Poll::Ready(Ok(result)),
            Poll::Ready(Err(err)) => {
                let uri = this.uri.take().unwrap_or_default();
                Poll::Ready(Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("text/plain".to_string()),
                        text: Some(format!("Error reading resource: {}", err)),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                }))
            }
        }
    }
}

impl<S> Service<ResourceRequest> for ResourceCatchError<S>
where
    S: Service<ResourceRequest, Response = ReadResourceResult> + Clone + Send + 'static,
    S::Error: fmt::Display + Send,
    S::Future: Send,
{
    type Response = ReadResourceResult;
    type Error = Infallible;
    type Future = ResourceCatchErrorFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        // Map any readiness error to Infallible (we catch it on call)
        match self.inner.poll_ready(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(_)) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, req: ResourceRequest) -> Self::Future {
        let uri = req.uri.clone();
        let fut = self.inner.call(req);

        ResourceCatchErrorFuture {
            inner: fut,
            uri: Some(uri),
        }
    }
}

#[cfg(feature = "stateless")]
#[derive(Clone)]
struct MrtrResourceCatchError<S> {
    inner: S,
}

#[cfg(feature = "stateless")]
impl<S> MrtrResourceCatchError<S> {
    fn new(inner: S) -> Self {
        Self { inner }
    }
}

#[cfg(feature = "stateless")]
impl<S> Service<ResourceRequest> for MrtrResourceCatchError<S>
where
    S: Service<ResourceRequest, Response = RequestOutcome<ReadResourceResult>>
        + Clone
        + Send
        + 'static,
    S::Error: fmt::Display + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = RequestOutcome<ReadResourceResult>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        match self.inner.poll_ready(cx) {
            Poll::Ready(Ok(())) | Poll::Ready(Err(_)) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, req: ResourceRequest) -> Self::Future {
        let uri = req.uri.clone();
        let future = self.inner.call(req);
        Box::pin(async move {
            Ok(match future.await {
                Ok(outcome) => outcome,
                Err(error) => RequestOutcome::Complete(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("text/plain".to_string()),
                        text: Some(format!("Error reading resource: {error}")),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                }),
            })
        })
    }
}

/// A boxed future for resource handlers
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The shape a resource handler is adapted to internally.
///
/// [`ResourceBuilder`] wraps closures in an implementation of this trait, and
/// the router reads every resource through it. No public constructor accepts
/// an implementation, so a resource written as a type rather than a closure
/// implements [`McpResource`] instead.
pub(crate) trait ResourceHandler: Send + Sync {
    /// Read the resource contents
    fn read(&self) -> BoxFuture<'_, Result<ReadResourceResult>>;

    /// Read the resource with request context for progress/cancellation support
    ///
    /// The default implementation ignores the context and calls `read`.
    /// Override this to receive progress/cancellation context.
    fn read_with_context(&self, _ctx: RequestContext) -> BoxFuture<'_, Result<ReadResourceResult>> {
        self.read()
    }
}

/// Resource handler that may return an SEP-2322 input-required continuation.
#[cfg(feature = "stateless")]
pub(crate) trait MrtrResourceHandler: Send + Sync {
    /// Read a resource attempt with continuation values in the context.
    fn read(
        &self,
        ctx: RequestContext,
    ) -> BoxFuture<'_, Result<RequestOutcome<ReadResourceResult>>>;
}

#[cfg(feature = "stateless")]
struct MrtrResourceHandlerService<H> {
    handler: Arc<H>,
}

#[cfg(feature = "stateless")]
impl<H> MrtrResourceHandlerService<H> {
    fn new(handler: H) -> Self {
        Self {
            handler: Arc::new(handler),
        }
    }
}

#[cfg(feature = "stateless")]
impl<H> Clone for MrtrResourceHandlerService<H> {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
        }
    }
}

#[cfg(feature = "stateless")]
impl<H> Service<ResourceRequest> for MrtrResourceHandlerService<H>
where
    H: MrtrResourceHandler + 'static,
{
    type Response = RequestOutcome<ReadResourceResult>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: ResourceRequest) -> Self::Future {
        let handler = self.handler.clone();
        Box::pin(async move { handler.read(req.ctx).await })
    }
}

#[cfg(feature = "stateless")]
struct ServiceMrtrResourceHandler {
    service: Mutex<BoxMrtrResourceService>,
    uri: String,
}

#[cfg(feature = "stateless")]
impl MrtrResourceHandler for ServiceMrtrResourceHandler {
    fn read(
        &self,
        ctx: RequestContext,
    ) -> BoxFuture<'_, Result<RequestOutcome<ReadResourceResult>>> {
        Box::pin(async move {
            let request = ResourceRequest::new(ctx, self.uri.clone());
            let mut service = self.service.lock().await.clone();
            let outcome = service
                .ready()
                .await
                .expect("MRTR resource service is infallible")
                .call(request)
                .await
                .expect("MRTR resource service is infallible");
            Ok(outcome)
        })
    }
}

/// Adapts a `ResourceHandler` to a Tower `Service<ResourceRequest>`.
///
/// This is an internal adapter that bridges the handler abstraction to the
/// service abstraction, enabling middleware composition.
struct ResourceHandlerService<H> {
    handler: Arc<H>,
}

impl<H> ResourceHandlerService<H> {
    fn new(handler: H) -> Self {
        Self {
            handler: Arc::new(handler),
        }
    }
}

impl<H> Clone for ResourceHandlerService<H> {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
        }
    }
}

impl<H> fmt::Debug for ResourceHandlerService<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResourceHandlerService")
            .finish_non_exhaustive()
    }
}

impl<H> Service<ResourceRequest> for ResourceHandlerService<H>
where
    H: ResourceHandler + 'static,
{
    type Response = ReadResourceResult;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<ReadResourceResult, Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: ResourceRequest) -> Self::Future {
        let handler = self.handler.clone();
        Box::pin(async move { handler.read_with_context(req.ctx).await })
    }
}

/// A resource that is ready to register.
///
/// Produced by [`ResourceBuilder`] or [`McpResource::into_resource`] and
/// consumed by [`McpRouter::resource`](crate::McpRouter::resource). The public
/// fields are what `resources/list` advertises; behind them is a Tower service,
/// which is what lets `.layer()` compose middleware onto a single resource.
///
/// That service is wrapped so it cannot fail, so handler and middleware errors
/// come back as content rather than as errors. [`read`](Self::read) shows what
/// that looks like.
pub struct Resource {
    /// Resource URI
    pub uri: String,
    /// Human-readable name
    pub name: String,
    /// Human-readable title for display purposes
    pub title: Option<String>,
    /// Optional description
    pub description: Option<String>,
    /// Optional MIME type
    pub mime_type: Option<String>,
    /// Optional icons for display in user interfaces
    pub icons: Option<Vec<ToolIcon>>,
    /// Optional size in bytes
    pub size: Option<u64>,
    /// Optional annotations (audience, priority hints)
    pub annotations: Option<ContentAnnotations>,
    /// Validated protocol metadata included in `resources/list`.
    pub meta: Option<Value>,
    /// The boxed service that reads the resource
    service: Option<BoxResourceService>,
    #[cfg(feature = "stateless")]
    mrtr_handler: Option<Arc<dyn MrtrResourceHandler>>,
}

impl Clone for Resource {
    fn clone(&self) -> Self {
        Self {
            uri: self.uri.clone(),
            name: self.name.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            mime_type: self.mime_type.clone(),
            icons: self.icons.clone(),
            size: self.size,
            annotations: self.annotations.clone(),
            meta: self.meta.clone(),
            service: self.service.clone(),
            #[cfg(feature = "stateless")]
            mrtr_handler: self.mrtr_handler.clone(),
        }
    }
}

impl std::fmt::Debug for Resource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resource")
            .field("uri", &self.uri)
            .field("name", &self.name)
            .field("title", &self.title)
            .field("description", &self.description)
            .field("mime_type", &self.mime_type)
            .field("icons", &self.icons)
            .field("size", &self.size)
            .field("annotations", &self.annotations)
            .field("meta", &self.meta)
            .finish_non_exhaustive()
    }
}

// SAFETY: BoxCloneService is Send + Sync (tower provides unsafe impl Sync),
// and all other fields in Resource are Send + Sync.
unsafe impl Send for Resource {}
unsafe impl Sync for Resource {}

impl Resource {
    /// Create a new resource builder
    pub fn builder(uri: impl Into<String>) -> ResourceBuilder {
        ResourceBuilder::new(uri)
    }

    /// The entry this resource contributes to `resources/list`.
    pub fn definition(&self) -> ResourceDefinition {
        ResourceDefinition {
            uri: self.uri.clone(),
            name: self.name.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            mime_type: self.mime_type.clone(),
            icons: self.icons.clone(),
            size: self.size,
            annotations: self.annotations.clone(),
            meta: self.meta.clone(),
        }
    }

    /// Attach `_meta` to what `resources/list` publishes for this resource.
    ///
    /// The value is checked here rather than at serialization time, so a key
    /// that does not fit the spec's name grammar is rejected while there is
    /// still a caller to tell.
    ///
    /// ```rust
    /// use serde_json::json;
    /// use tower_mcp::resource::ResourceBuilder;
    ///
    /// let resource = ResourceBuilder::new("docs://usage")
    ///     .name("Usage")
    ///     .text("...")
    ///     .with_meta(json!({ "example.com/generated-at": "2026-08-10" }))
    ///     .expect("valid _meta key");
    ///
    /// assert!(
    ///     resource
    ///         .clone()
    ///         .with_meta(json!({ "/leading-slash": true }))
    ///         .is_err()
    /// );
    /// ```
    pub fn with_meta(
        mut self,
        meta: Value,
    ) -> std::result::Result<Self, crate::protocol::MetaValidationError> {
        crate::protocol::validate_meta_object(&meta)?;
        self.meta = Some(meta);
        Ok(self)
    }

    /// Read the resource with a placeholder request context.
    ///
    /// Progress reporting, cancellation, and client requests are inert on the
    /// placeholder context, so this is for tests and for handlers built with
    /// [`ResourceBuilder::handler`], which never see the context anyway. The
    /// transports call [`read_with_context`](Self::read_with_context).
    ///
    /// There is no `Result` here because a handler error has already been
    /// turned into the value returned. That is the cost of letting middleware
    /// compose onto a resource, and it means a failed read still looks like a
    /// read: the error text arrives as content.
    ///
    /// ```rust
    /// use tower_mcp::error::Error;
    /// use tower_mcp::resource::ResourceBuilder;
    ///
    /// # tokio_test::block_on(async {
    /// let resource = ResourceBuilder::new("db://users")
    ///     .name("Users")
    ///     .handler(|| async { Err(Error::internal("database is offline")) })
    ///     .build();
    ///
    /// let result = resource.read().await;
    /// let text = result.contents[0].text.as_deref().unwrap();
    /// assert!(text.contains("database is offline"), "{text}");
    /// # });
    /// ```
    ///
    /// A [`ResourceTemplate`] handler behaves the other way round: its errors
    /// stay errors, and [`ResourceTemplate::read`] returns a `Result`.
    pub fn read(&self) -> BoxFuture<'static, ReadResourceResult> {
        let ctx = RequestContext::new(crate::protocol::RequestId::Number(0));
        self.read_with_context(ctx)
    }

    /// Read the resource with the request context the transport built.
    ///
    /// This is the path the router takes. The context carries the request id,
    /// progress reporting, the cancellation token, and the client requester
    /// used for sampling and elicitation. Handlers registered through
    /// [`ResourceBuilder::handler`] ignore it; those registered through
    /// [`ResourceBuilder::handler_with_context`] receive it.
    ///
    /// Errors become content rather than a `Result`, as described on
    /// [`read`](Self::read). A handler that suspends for client input is
    /// reported the same way, since this signature has nowhere to put a
    /// continuation; use [`read_outcome_with_context`](Self::read_outcome_with_context)
    /// for that.
    pub fn read_with_context(&self, ctx: RequestContext) -> BoxFuture<'static, ReadResourceResult> {
        let resource = self.clone();
        let uri = self.uri.clone();
        Box::pin(async move {
            match resource.read_outcome_with_context(ctx).await {
                Ok(RequestOutcome::Complete(result)) => result,
                Ok(RequestOutcome::InputRequired(_)) => ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("text/plain".into()),
                        text: Some(
                            "resource requires additional client input; use read_outcome_with_context"
                                .into(),
                        ),
                        blob: None,
                        meta: None,
                    }],
                    ..ReadResourceResult::default()
                },
                Err(error) => ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("text/plain".into()),
                        text: Some(error.to_string()),
                        blob: None,
                        meta: None,
                    }],
                    ..ReadResourceResult::default()
                },
            }
        })
    }

    /// Read the resource while preserving an SEP-2322 continuation.
    ///
    /// The router calls this so that a handler which needs more input from the
    /// client can say so, instead of having that outcome flattened into
    /// content by [`read_with_context`](Self::read_with_context).
    pub fn read_outcome_with_context(
        &self,
        ctx: RequestContext,
    ) -> BoxFuture<'static, Result<RequestOutcome<ReadResourceResult>>> {
        use tower::ServiceExt;
        #[cfg(feature = "stateless")]
        if let Some(handler) = self.mrtr_handler.clone() {
            return Box::pin(async move { handler.read(ctx).await });
        }
        let service = self
            .service
            .clone()
            .expect("resource must have a complete or MRTR handler");
        let uri = self.uri.clone();
        Box::pin(async move {
            let result = service
                .oneshot(ResourceRequest::new(ctx, uri))
                .await
                .unwrap();
            Ok(RequestOutcome::Complete(result))
        })
    }

    /// Create a resource from a handler (internal helper)
    #[allow(clippy::too_many_arguments)]
    fn from_handler<H: ResourceHandler + 'static>(
        uri: String,
        name: String,
        title: Option<String>,
        description: Option<String>,
        mime_type: Option<String>,
        icons: Option<Vec<ToolIcon>>,
        size: Option<u64>,
        annotations: Option<ContentAnnotations>,
        handler: H,
    ) -> Self {
        let handler_service = ResourceHandlerService::new(handler);
        let catch_error = ResourceCatchError::new(handler_service);
        let service = BoxCloneService::new(catch_error);

        Self {
            uri,
            name,
            title,
            description,
            mime_type,
            icons,
            size,
            annotations,
            meta: None,
            service: Some(service),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
        }
    }

    #[cfg(feature = "stateless")]
    #[allow(clippy::too_many_arguments)]
    fn from_mrtr_handler<H: MrtrResourceHandler + 'static>(
        uri: String,
        name: String,
        title: Option<String>,
        description: Option<String>,
        mime_type: Option<String>,
        icons: Option<Vec<ToolIcon>>,
        size: Option<u64>,
        annotations: Option<ContentAnnotations>,
        handler: H,
    ) -> Self {
        Self {
            uri,
            name,
            title,
            description,
            mime_type,
            icons,
            size,
            annotations,
            meta: None,
            service: None,
            mrtr_handler: Some(Arc::new(handler)),
        }
    }
}

// =============================================================================
// Builder API
// =============================================================================

/// Builder for a resource served at one fixed URI.
///
/// The URI is the identity clients read by; the name is what a user sees in a
/// picker, and defaults to the URI when it is not set. Finish the chain with a
/// handler and [`build`](ResourceBuilderWithHandler::build), or with
/// [`text`](Self::text) or [`json`](Self::json) for content already in memory,
/// then register the result with
/// [`McpRouter::resource`](crate::McpRouter::resource).
///
/// # Example
///
/// ```rust
/// use tower_mcp::protocol::ReadResourceResult;
/// use tower_mcp::resource::ResourceBuilder;
///
/// # tokio_test::block_on(async {
/// let resource = ResourceBuilder::new("file:///config.json")
///     .name("Configuration")
///     .description("Application configuration file")
///     .mime_type("application/json")
///     .handler(|| async {
///         Ok(ReadResourceResult::text_with_mime(
///             "file:///config.json",
///             r#"{"setting": "value"}"#,
///             "application/json",
///         ))
///     })
///     .build();
///
/// assert_eq!(resource.uri, "file:///config.json");
///
/// let result = resource.read().await;
/// assert_eq!(
///     result.contents[0].text.as_deref(),
///     Some(r#"{"setting": "value"}"#)
/// );
/// # });
/// ```
pub struct ResourceBuilder {
    uri: String,
    name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    size: Option<u64>,
    annotations: Option<ContentAnnotations>,
}

impl ResourceBuilder {
    /// Start a resource at this URI.
    ///
    /// The URI is the key the router registers under, so registering a second
    /// resource with the same URI replaces the first rather than failing.
    /// [`McpRouter::try_merge`](crate::McpRouter::try_merge) is the way to
    /// find that out instead of discovering it in production.
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: None,
            title: None,
            description: None,
            mime_type: None,
            icons: None,
            size: None,
            annotations: None,
        }
    }

    /// Set the name clients list this resource under.
    ///
    /// Defaults to the URI when unset, which is legal and unhelpful in a
    /// picker.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the display title, which a client prefers over the name.
    ///
    /// Worth setting when the name has to stay stable for other reasons and
    /// is not what a person should read.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the resource description
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Declare the MIME type in `resources/list`.
    ///
    /// A handler sets the type on each content item it returns, and nothing
    /// reconciles the two, so a resource that declares one type and returns
    /// another is simply lying to clients. [`text`](Self::text) and
    /// [`json`](Self::json) cannot drift this way because they build the
    /// content themselves.
    pub fn mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }

    /// Add an icon for the resource
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

    /// Advertise a size in bytes, so a client can decide before reading.
    ///
    /// A hint only: nothing compares it with what the handler returns.
    pub fn size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// Set annotations (audience, priority hints) for this resource
    pub fn annotations(mut self, annotations: ContentAnnotations) -> Self {
        self.annotations = Some(annotations);
        self
    }

    /// Set the handler function for reading the resource.
    ///
    /// Returns a [`ResourceBuilderWithHandler`] that can be used to apply
    /// middleware layers via `.layer()` or build the resource directly via `.build()`.
    ///
    /// # Sharing State
    ///
    /// Capture an [`Arc`] in the closure to share state across handler
    /// invocations or with other parts of your application:
    ///
    /// ```rust
    /// use std::sync::Arc;
    /// use tokio::sync::RwLock;
    /// use tower_mcp::resource::ResourceBuilder;
    /// use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
    ///
    /// let db = Arc::new(RwLock::new(vec!["initial".to_string()]));
    ///
    /// let db_clone = Arc::clone(&db);
    /// let resource = ResourceBuilder::new("app://entries")
    ///     .name("Entries")
    ///     .handler(move || {
    ///         let db = Arc::clone(&db_clone);
    ///         async move {
    ///             let entries = db.read().await;
    ///             Ok(ReadResourceResult {
    ///                 contents: vec![ResourceContent {
    ///                     uri: "app://entries".to_string(),
    ///                     mime_type: Some("text/plain".to_string()),
    ///                     text: Some(entries.join("\n")),
    ///                     blob: None,
    ///                     meta: None,
    ///                 }],
    ///                 meta: None,
    ///                 ..Default::default()
    ///             })
    ///         }
    ///     })
    ///     .build();
    /// ```
    ///
    /// [`Arc`]: std::sync::Arc
    pub fn handler<F, Fut>(self, handler: F) -> ResourceBuilderWithHandler<F>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
    {
        ResourceBuilderWithHandler {
            uri: self.uri,
            name: self.name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            size: self.size,
            annotations: self.annotations,
            handler,
        }
    }

    /// Set a handler that receives the [`RequestContext`].
    ///
    /// Take this form when the read runs long enough to report progress
    /// against, has to notice cancellation, or wants to ask the client
    /// something. Everything else can use [`handler`](Self::handler), which
    /// skips the context entirely.
    ///
    /// Reporting progress is unconditional in the handler and free when
    /// unwanted: a client that sent no progress token receives nothing.
    ///
    /// ```rust
    /// use tower_mcp::context::RequestContext;
    /// use tower_mcp::error::Error;
    /// use tower_mcp::protocol::ReadResourceResult;
    /// use tower_mcp::resource::ResourceBuilder;
    ///
    /// let resource = ResourceBuilder::new("logs://today")
    ///     .name("Today's logs")
    ///     .handler_with_context(|ctx: RequestContext| async move {
    ///         let mut pages = Vec::new();
    ///         for page in 0..4 {
    ///             if ctx.is_cancelled() {
    ///                 return Err(Error::internal("read cancelled"));
    ///             }
    ///             ctx.report_progress(f64::from(page), Some(4.0), Some("reading"))
    ///                 .await;
    ///             pages.push(format!("page {page}"));
    ///         }
    ///         Ok(ReadResourceResult::text("logs://today", pages.join("\n")))
    ///     })
    ///     .build();
    ///
    /// # tokio_test::block_on(async {
    /// let result = resource.read().await;
    /// let text = result.contents[0].text.as_deref().unwrap();
    /// assert!(text.contains("page 3"), "{text}");
    /// # });
    /// ```
    ///
    /// Returns a builder that still accepts `.layer()` before `.build()`.
    pub fn handler_with_context<F, Fut>(self, handler: F) -> ResourceBuilderWithContextHandler<F>
    where
        F: Fn(RequestContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
    {
        ResourceBuilderWithContextHandler {
            uri: self.uri,
            name: self.name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            size: self.size,
            annotations: self.annotations,
            handler,
        }
    }

    /// Set an SEP-2322 resource handler that may return input-required.
    #[cfg(feature = "stateless")]
    pub fn mrtr_handler<F, Fut>(self, handler: F) -> ResourceBuilderWithMrtrHandler<F>
    where
        F: Fn(RequestContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<RequestOutcome<ReadResourceResult>>> + Send + 'static,
    {
        ResourceBuilderWithMrtrHandler {
            uri: self.uri,
            name: self.name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            size: self.size,
            annotations: self.annotations,
            handler,
        }
    }

    /// Finish the builder by serving a fixed string.
    ///
    /// The content is captured once, so this is for text that does not change
    /// while the server runs: instructions, a licence, a schema. Anything read
    /// from disk or a database on demand needs [`handler`](Self::handler).
    ///
    /// ```rust
    /// use tower_mcp::resource::ResourceBuilder;
    ///
    /// # tokio_test::block_on(async {
    /// let resource = ResourceBuilder::new("docs://usage")
    ///     .name("Usage")
    ///     .mime_type("text/markdown")
    ///     .text("# Usage\n\nStart the server, then call `tools/list`.");
    ///
    /// let result = resource.read().await;
    /// assert_eq!(
    ///     result.contents[0].mime_type.as_deref(),
    ///     Some("text/markdown")
    /// );
    /// # });
    /// ```
    pub fn text(self, content: impl Into<String>) -> Resource {
        let uri = self.uri.clone();
        let content = content.into();
        let mime_type = self.mime_type.clone();

        self.handler(move || {
            let uri = uri.clone();
            let content = content.clone();
            let mime_type = mime_type.clone();
            async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type,
                        text: Some(content),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            }
        })
        .build()
    }

    /// Finish the builder by serving a fixed JSON value.
    ///
    /// The value is serialized once, at build time, and the MIME type is set
    /// to `application/json` whatever [`mime_type`](Self::mime_type) said
    /// earlier, so the declared type cannot drift from the body.
    ///
    /// ```rust
    /// use serde_json::json;
    /// use tower_mcp::resource::ResourceBuilder;
    ///
    /// # tokio_test::block_on(async {
    /// let resource = ResourceBuilder::new("config://limits")
    ///     .name("Limits")
    ///     .json(json!({ "max_connections": 100 }));
    ///
    /// let result = resource.read().await;
    /// assert_eq!(
    ///     result.contents[0].mime_type.as_deref(),
    ///     Some("application/json")
    /// );
    /// # });
    /// ```
    pub fn json(mut self, value: serde_json::Value) -> Resource {
        let uri = self.uri.clone();
        self.mime_type = Some("application/json".to_string());
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string());

        self.handler(move || {
            let uri = uri.clone();
            let text = text.clone();
            async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("application/json".to_string()),
                        text: Some(text),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            }
        })
        .build()
    }
}

/// Builder state after handler is specified.
///
/// This builder allows applying middleware layers via `.layer()` or building
/// the resource directly via `.build()`.
pub struct ResourceBuilderWithHandler<F> {
    uri: String,
    name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    size: Option<u64>,
    annotations: Option<ContentAnnotations>,
    handler: F,
}

/// Builder state after an MRTR handler is specified.
///
/// The multi-round-trip form of [`ResourceBuilderWithHandler`], reached by
/// [`ResourceBuilder::mrtr_handler`]. Same choices from here: apply
/// middleware with `.layer()`, or finish with `.build()`.
#[cfg(feature = "stateless")]
pub struct ResourceBuilderWithMrtrHandler<F> {
    uri: String,
    name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    size: Option<u64>,
    annotations: Option<ContentAnnotations>,
    handler: F,
}

/// Builder state after an MRTR handler and at least one layer.
///
/// Reached by calling `.layer()` on a
/// [`ResourceBuilderWithMrtrHandler`]. Further `.layer()` calls stack onto
/// `L`, outermost last, and `.build()` finishes.
#[cfg(feature = "stateless")]
pub struct ResourceBuilderWithMrtrLayer<F, L> {
    uri: String,
    name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    size: Option<u64>,
    annotations: Option<ContentAnnotations>,
    handler: F,
    layer: L,
}

#[cfg(feature = "stateless")]
impl<F, Fut> ResourceBuilderWithMrtrHandler<F>
where
    F: Fn(RequestContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RequestOutcome<ReadResourceResult>>> + Send + 'static,
{
    /// Build the resource with an SEP-2322-aware handler.
    pub fn build(self) -> Resource {
        let name = self.name.unwrap_or_else(|| self.uri.clone());

        Resource::from_mrtr_handler(
            self.uri,
            name,
            self.title,
            self.description,
            self.mime_type,
            self.icons,
            self.size,
            self.annotations,
            MrtrContextHandler {
                handler: self.handler,
            },
        )
    }

    /// Apply a Tower layer to every attempt at this MRTR-capable resource.
    ///
    /// Each retry is an independent request, so the layer runs once per
    /// round. Middleware failures become complete resource error results,
    /// matching non-MRTR resource middleware.
    pub fn layer<L>(self, layer: L) -> ResourceBuilderWithMrtrLayer<F, L> {
        ResourceBuilderWithMrtrLayer {
            uri: self.uri,
            name: self.name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            size: self.size,
            annotations: self.annotations,
            handler: self.handler,
            layer,
        }
    }
}

#[cfg(feature = "stateless")]
#[allow(private_bounds)]
impl<F, Fut, L> ResourceBuilderWithMrtrLayer<F, L>
where
    F: Fn(RequestContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RequestOutcome<ReadResourceResult>>> + Send + 'static,
    L: tower::Layer<MrtrResourceHandlerService<MrtrContextHandler<F>>>
        + Clone
        + Send
        + Sync
        + 'static,
    L::Service: Service<ResourceRequest, Response = RequestOutcome<ReadResourceResult>>
        + Clone
        + Send
        + 'static,
    <L::Service as Service<ResourceRequest>>::Error: fmt::Display + Send + 'static,
    <L::Service as Service<ResourceRequest>>::Future: Send + 'static,
{
    /// Build the MRTR resource with the applied layer(s).
    pub fn build(self) -> Resource {
        let name = self.name.unwrap_or_else(|| self.uri.clone());
        let handler = MrtrContextHandler {
            handler: self.handler,
        };
        let service = MrtrResourceHandlerService::new(handler);
        let service = self.layer.layer(service);
        let service = BoxCloneService::new(MrtrResourceCatchError::new(service));

        Resource {
            uri: self.uri.clone(),
            name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            size: self.size,
            annotations: self.annotations,
            meta: None,
            service: None,
            mrtr_handler: Some(Arc::new(ServiceMrtrResourceHandler {
                service: Mutex::new(service),
                uri: self.uri,
            })),
        }
    }

    /// Apply an additional Tower layer.
    pub fn layer<L2>(
        self,
        layer: L2,
    ) -> ResourceBuilderWithMrtrLayer<F, tower::layer::util::Stack<L2, L>> {
        ResourceBuilderWithMrtrLayer {
            uri: self.uri,
            name: self.name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            size: self.size,
            annotations: self.annotations,
            handler: self.handler,
            layer: tower::layer::util::Stack::new(layer, self.layer),
        }
    }
}

impl<F, Fut> ResourceBuilderWithHandler<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
{
    /// Build the resource without any middleware layers.
    pub fn build(self) -> Resource {
        let name = self.name.unwrap_or_else(|| self.uri.clone());

        Resource::from_handler(
            self.uri,
            name,
            self.title,
            self.description,
            self.mime_type,
            self.icons,
            self.size,
            self.annotations,
            FnHandler {
                handler: self.handler,
            },
        )
    }

    /// Apply a Tower layer (middleware) to this resource.
    ///
    /// The layer wraps the resource's handler service, enabling functionality like
    /// timeouts, rate limiting, and metrics collection at the per-resource level.
    ///
    /// # Example
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use tower::timeout::TimeoutLayer;
    /// use tower_mcp::resource::ResourceBuilder;
    /// use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
    ///
    /// let resource = ResourceBuilder::new("file:///slow.txt")
    ///     .name("Slow Resource")
    ///     .handler(|| async {
    ///         Ok(ReadResourceResult {
    ///             contents: vec![ResourceContent {
    ///                 uri: "file:///slow.txt".to_string(),
    ///                 mime_type: Some("text/plain".to_string()),
    ///                 text: Some("content".to_string()),
    ///                 blob: None,
    ///                 meta: None,
    ///             }],
    ///             meta: None,
    ///             ..Default::default()
    ///         })
    ///     })
    ///     .layer(TimeoutLayer::new(Duration::from_secs(30)))
    ///     .build();
    /// ```
    pub fn layer<L>(self, layer: L) -> ResourceBuilderWithLayer<F, L> {
        ResourceBuilderWithLayer {
            uri: self.uri,
            name: self.name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            size: self.size,
            annotations: self.annotations,
            handler: self.handler,
            layer,
        }
    }
}

/// Builder state after a layer has been applied to the handler.
///
/// This builder allows chaining additional layers and building the final resource.
pub struct ResourceBuilderWithLayer<F, L> {
    uri: String,
    name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    size: Option<u64>,
    annotations: Option<ContentAnnotations>,
    handler: F,
    layer: L,
}

// Allow private_bounds because these internal types (ResourceHandlerService, FnHandler, etc.)
// are implementation details that users don't interact with directly.
#[allow(private_bounds)]
impl<F, Fut, L> ResourceBuilderWithLayer<F, L>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
    L: tower::Layer<ResourceHandlerService<FnHandler<F>>> + Clone + Send + Sync + 'static,
    L::Service: Service<ResourceRequest, Response = ReadResourceResult> + Clone + Send + 'static,
    <L::Service as Service<ResourceRequest>>::Error: fmt::Display + Send,
    <L::Service as Service<ResourceRequest>>::Future: Send,
{
    /// Build the resource with the applied layer(s).
    pub fn build(self) -> Resource {
        let name = self.name.unwrap_or_else(|| self.uri.clone());

        let handler_service = ResourceHandlerService::new(FnHandler {
            handler: self.handler,
        });
        let layered = self.layer.layer(handler_service);
        let catch_error = ResourceCatchError::new(layered);
        let service = BoxCloneService::new(catch_error);

        Resource {
            uri: self.uri,
            name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            size: self.size,
            annotations: self.annotations,
            meta: None,
            service: Some(service),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
        }
    }

    /// Apply an additional Tower layer (middleware).
    ///
    /// Layers are applied in order, with earlier layers wrapping later ones.
    /// This means the first layer added is the outermost middleware.
    pub fn layer<L2>(
        self,
        layer: L2,
    ) -> ResourceBuilderWithLayer<F, tower::layer::util::Stack<L2, L>> {
        ResourceBuilderWithLayer {
            uri: self.uri,
            name: self.name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            size: self.size,
            annotations: self.annotations,
            handler: self.handler,
            layer: tower::layer::util::Stack::new(layer, self.layer),
        }
    }
}

/// Builder state after context-aware handler is specified.
pub struct ResourceBuilderWithContextHandler<F> {
    uri: String,
    name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    size: Option<u64>,
    annotations: Option<ContentAnnotations>,
    handler: F,
}

impl<F, Fut> ResourceBuilderWithContextHandler<F>
where
    F: Fn(RequestContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
{
    /// Build the resource without any middleware layers.
    pub fn build(self) -> Resource {
        let name = self.name.unwrap_or_else(|| self.uri.clone());

        Resource::from_handler(
            self.uri,
            name,
            self.title,
            self.description,
            self.mime_type,
            self.icons,
            self.size,
            self.annotations,
            ContextAwareHandler {
                handler: self.handler,
            },
        )
    }

    /// Apply a Tower layer (middleware) to this resource.
    ///
    /// Works the same as [`ResourceBuilderWithHandler::layer`].
    pub fn layer<L>(self, layer: L) -> ResourceBuilderWithContextLayer<F, L> {
        ResourceBuilderWithContextLayer {
            uri: self.uri,
            name: self.name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            size: self.size,
            annotations: self.annotations,
            handler: self.handler,
            layer,
        }
    }
}

/// Builder state after a layer has been applied to a context-aware handler.
pub struct ResourceBuilderWithContextLayer<F, L> {
    uri: String,
    name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    size: Option<u64>,
    annotations: Option<ContentAnnotations>,
    handler: F,
    layer: L,
}

// Allow private_bounds because these internal types are implementation details.
#[allow(private_bounds)]
impl<F, Fut, L> ResourceBuilderWithContextLayer<F, L>
where
    F: Fn(RequestContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
    L: tower::Layer<ResourceHandlerService<ContextAwareHandler<F>>> + Clone + Send + Sync + 'static,
    L::Service: Service<ResourceRequest, Response = ReadResourceResult> + Clone + Send + 'static,
    <L::Service as Service<ResourceRequest>>::Error: fmt::Display + Send,
    <L::Service as Service<ResourceRequest>>::Future: Send,
{
    /// Build the resource with the applied layer(s).
    pub fn build(self) -> Resource {
        let name = self.name.unwrap_or_else(|| self.uri.clone());

        let handler_service = ResourceHandlerService::new(ContextAwareHandler {
            handler: self.handler,
        });
        let layered = self.layer.layer(handler_service);
        let catch_error = ResourceCatchError::new(layered);
        let service = BoxCloneService::new(catch_error);

        Resource {
            uri: self.uri,
            name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            size: self.size,
            annotations: self.annotations,
            meta: None,
            service: Some(service),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
        }
    }

    /// Apply an additional Tower layer (middleware).
    pub fn layer<L2>(
        self,
        layer: L2,
    ) -> ResourceBuilderWithContextLayer<F, tower::layer::util::Stack<L2, L>> {
        ResourceBuilderWithContextLayer {
            uri: self.uri,
            name: self.name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            size: self.size,
            annotations: self.annotations,
            handler: self.handler,
            layer: tower::layer::util::Stack::new(layer, self.layer),
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

impl<F, Fut> ResourceHandler for FnHandler<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
{
    fn read(&self) -> BoxFuture<'_, Result<ReadResourceResult>> {
        Box::pin((self.handler)())
    }
}

/// Handler that receives request context
struct ContextAwareHandler<F> {
    handler: F,
}

impl<F, Fut> ResourceHandler for ContextAwareHandler<F>
where
    F: Fn(RequestContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
{
    fn read(&self) -> BoxFuture<'_, Result<ReadResourceResult>> {
        let ctx = RequestContext::new(crate::protocol::RequestId::Number(0));
        self.read_with_context(ctx)
    }

    fn read_with_context(&self, ctx: RequestContext) -> BoxFuture<'_, Result<ReadResourceResult>> {
        Box::pin((self.handler)(ctx))
    }
}

#[cfg(feature = "stateless")]
struct MrtrContextHandler<F> {
    handler: F,
}

#[cfg(feature = "stateless")]
impl<F, Fut> MrtrResourceHandler for MrtrContextHandler<F>
where
    F: Fn(RequestContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RequestOutcome<ReadResourceResult>>> + Send + 'static,
{
    fn read(
        &self,
        ctx: RequestContext,
    ) -> BoxFuture<'_, Result<RequestOutcome<ReadResourceResult>>> {
        Box::pin((self.handler)(ctx))
    }
}

// =============================================================================
// Trait-based resource definition
// =============================================================================

/// Define a resource as a type rather than a closure.
///
/// The URI, name, description, and MIME type are associated constants, so they
/// are decided at compile time and the type owns whatever state the read needs.
/// A resource whose identity is only known at runtime belongs in
/// [`ResourceBuilder`] instead. Finish with
/// [`into_resource`](Self::into_resource), which produces the same [`Resource`]
/// the builder does.
///
/// # Example
///
/// ```rust
/// use tower_mcp::{McpResource, ReadResourceResult, ResourceContent, Result};
///
/// struct ConfigResource {
///     config: String,
/// }
///
/// impl McpResource for ConfigResource {
///     const URI: &'static str = "file:///config.json";
///     const NAME: &'static str = "Configuration";
///     const DESCRIPTION: Option<&'static str> = Some("Application configuration");
///     const MIME_TYPE: Option<&'static str> = Some("application/json");
///
///     async fn read(&self) -> Result<ReadResourceResult> {
///         Ok(ReadResourceResult {
///             contents: vec![ResourceContent {
///                 uri: Self::URI.to_string(),
///                 mime_type: Self::MIME_TYPE.map(|s| s.to_string()),
///                 text: Some(self.config.clone()),
///                 blob: None,
///                 meta: None,
///             }],
///             meta: None,
///             ..Default::default()
///         })
///     }
/// }
///
/// let resource = ConfigResource { config: "{}".to_string() }.into_resource();
/// assert_eq!(resource.uri, "file:///config.json");
/// ```
pub trait McpResource: Send + Sync + 'static {
    /// The resource URI.
    const URI: &'static str;
    /// The resource name.
    const NAME: &'static str;
    /// Optional human-readable description.
    const DESCRIPTION: Option<&'static str> = None;
    /// Optional MIME type for the resource content.
    const MIME_TYPE: Option<&'static str> = None;

    /// Read the resource content.
    fn read(&self) -> impl Future<Output = Result<ReadResourceResult>> + Send;

    /// Convert to a Resource instance
    fn into_resource(self) -> Resource
    where
        Self: Sized,
    {
        let resource = Arc::new(self);
        Resource::from_handler(
            Self::URI.to_string(),
            Self::NAME.to_string(),
            None,
            Self::DESCRIPTION.map(|s| s.to_string()),
            Self::MIME_TYPE.map(|s| s.to_string()),
            None,
            None,
            None,
            McpResourceHandler { resource },
        )
    }
}

/// Wrapper to make McpResource implement ResourceHandler
struct McpResourceHandler<T: McpResource> {
    resource: Arc<T>,
}

impl<T: McpResource> ResourceHandler for McpResourceHandler<T> {
    fn read(&self) -> BoxFuture<'_, Result<ReadResourceResult>> {
        let resource = self.resource.clone();
        Box::pin(async move { resource.read().await })
    }
}

// =============================================================================
// Resource Templates
// =============================================================================

/// The shape a resource-template handler is adapted to internally.
///
/// Unlike [`ResourceHandler`], a template handler is told which URI matched
/// and what was extracted from it, because one handler serves many URIs.
/// Templates are built from closures through
/// [`ResourceTemplateBuilder::handler`]; no public constructor accepts an
/// implementation of this trait.
pub(crate) trait ResourceTemplateHandler: Send + Sync {
    /// Read a resource with the given URI variables extracted from the template
    fn read(
        &self,
        uri: &str,
        variables: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<ReadResourceResult>>;
}

/// Resource-template handler that may return an SEP-2322 continuation.
#[cfg(feature = "stateless")]
pub(crate) trait MrtrResourceTemplateHandler: Send + Sync {
    /// Read a matched template resource with retry values in the context.
    fn read(
        &self,
        ctx: RequestContext,
        uri: &str,
        variables: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<RequestOutcome<ReadResourceResult>>>;
}

/// A resource whose URI is a pattern rather than a constant.
///
/// The pattern is compiled once, when the handler is attached, and every
/// `resources/read` that does not name a registered [`Resource`] is offered to
/// each template in registration order. The first match wins, and the
/// variables it extracted are passed to the handler together with the URI that
/// produced them, so one handler serves a whole family of URIs: a filesystem
/// subtree, a table of records, a paged listing.
///
/// [`match_uri`](Self::match_uri) documents the matching rules and shows them
/// running.
///
/// # Example
///
/// ```rust
/// use std::collections::HashMap;
/// use serde_json::json;
/// use tower_mcp::protocol::ReadResourceResult;
/// use tower_mcp::resource::ResourceTemplateBuilder;
///
/// let template = ResourceTemplateBuilder::new("db://users/{id}")
///     .name("User Records")
///     .description("One user record per id")
///     .handler(|uri: String, vars: HashMap<String, String>| async move {
///         let id = vars["id"].clone();
///         Ok(ReadResourceResult::json(uri, &json!({ "id": id })))
///     });
///
/// # tokio_test::block_on(async {
/// let vars = template.match_uri("db://users/42").expect("42 is one segment");
/// assert_eq!(vars["id"], "42");
///
/// let result = template.read("db://users/42", vars).await.unwrap();
/// assert!(result.contents[0].text.as_deref().unwrap().contains("42"));
/// # });
///
/// // `{id}` stops at a slash, so a deeper URI is left for another template.
/// assert!(template.match_uri("db://users/42/posts").is_none());
/// ```
pub struct ResourceTemplate {
    /// The URI template pattern (e.g., `file:///{path}`)
    pub uri_template: String,
    /// Human-readable name
    pub name: String,
    /// Human-readable title for display purposes
    pub title: Option<String>,
    /// Optional description
    pub description: Option<String>,
    /// Optional MIME type hint
    pub mime_type: Option<String>,
    /// Optional icons for display in user interfaces
    pub icons: Option<Vec<ToolIcon>>,
    /// Optional annotations (audience, priority hints)
    pub annotations: Option<ContentAnnotations>,
    /// Arguments this template accepts for URI expansion, as declared by
    /// [`ResourceTemplateBuilder::argument`]. Empty unless declared (#1282).
    pub arguments: Vec<PromptArgument>,
    /// Protocol-level `_meta`, set by [`with_meta`](Self::with_meta).
    meta: Option<Value>,
    /// Compiled regex for matching URIs
    pattern: regex::Regex,
    /// Variables declared by a form-style query expression, empty when the
    /// template has none (#1253).
    query_variables: Vec<String>,
    /// Variable names in order of appearance
    variables: Vec<String>,
    /// Handler for reading matched resources
    handler: Option<Arc<dyn ResourceTemplateHandler>>,
    #[cfg(feature = "stateless")]
    mrtr_handler: Option<Arc<dyn MrtrResourceTemplateHandler>>,
}

impl Clone for ResourceTemplate {
    fn clone(&self) -> Self {
        Self {
            uri_template: self.uri_template.clone(),
            name: self.name.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            mime_type: self.mime_type.clone(),
            icons: self.icons.clone(),
            annotations: self.annotations.clone(),
            arguments: self.arguments.clone(),
            meta: self.meta.clone(),
            pattern: self.pattern.clone(),
            query_variables: self.query_variables.clone(),
            variables: self.variables.clone(),
            handler: self.handler.clone(),
            #[cfg(feature = "stateless")]
            mrtr_handler: self.mrtr_handler.clone(),
        }
    }
}

impl std::fmt::Debug for ResourceTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceTemplate")
            .field("uri_template", &self.uri_template)
            .field("name", &self.name)
            .field("title", &self.title)
            .field("description", &self.description)
            .field("mime_type", &self.mime_type)
            .field("icons", &self.icons)
            .field("variables", &self.variables)
            .finish_non_exhaustive()
    }
}

impl ResourceTemplate {
    /// Create a new resource template builder
    pub fn builder(uri_template: impl Into<String>) -> ResourceTemplateBuilder {
        ResourceTemplateBuilder::new(uri_template)
    }

    /// The entry this template contributes to `resources/templates/list`.
    ///
    /// It advertises the pattern rather than deriving arguments from it: a
    /// client expands the pattern itself, and `arguments` carries only what
    /// [`ResourceTemplateBuilder::argument`] declared, which is nothing
    /// unless the author said otherwise (#1282).
    pub fn definition(&self) -> ResourceTemplateDefinition {
        ResourceTemplateDefinition {
            uri_template: self.uri_template.clone(),
            name: self.name.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            mime_type: self.mime_type.clone(),
            icons: self.icons.clone(),
            annotations: self.annotations.clone(),
            arguments: self.arguments.clone(),
            meta: self.meta.clone(),
        }
    }

    /// Attach `_meta` to what `resources/templates/list` publishes for this
    /// template.
    ///
    /// The counterpart of [`Resource::with_meta`], and validated the same
    /// way: keys must satisfy the protocol's `_meta` rules, so a bad key is
    /// rejected here rather than serialized onto the wire (#1282).
    ///
    /// ```rust
    /// use serde_json::json;
    /// use tower_mcp::ResourceTemplate;
    ///
    /// # fn template() -> ResourceTemplate {
    /// ResourceTemplate::builder("file:///{path}")
    ///     .name("files")
    ///     .handler(|_uri: String, _vars: std::collections::HashMap<String, String>| async move {
    ///         Ok(tower_mcp::protocol::ReadResourceResult::text("file:///a", ""))
    ///     })
    /// # }
    /// let tagged = template()
    ///     .with_meta(json!({ "example.com/audience": "internal" }))
    ///     .expect("a valid _meta key");
    /// assert!(tagged.definition().meta.is_some());
    ///
    /// // A leading slash is not a valid key, and is refused rather than sent.
    /// assert!(template().with_meta(json!({ "/nope": true })).is_err());
    /// ```
    pub fn with_meta(
        mut self,
        meta: Value,
    ) -> std::result::Result<Self, crate::protocol::MetaValidationError> {
        crate::protocol::validate_meta_object(&meta)?;
        self.meta = Some(meta);
        Ok(self)
    }

    /// Match a URI against this template and extract its variables.
    ///
    /// `None` means this template does not serve that URI, which is the
    /// router's signal to try the next one. The whole URI must match, not a
    /// prefix of it.
    ///
    /// # Path expansion
    ///
    /// `{var}` matches a run of non-slash characters and `{+var}` matches
    /// anything at all. That single difference decides whether a template
    /// routes one path segment or an entire subtree:
    ///
    /// ```rust
    /// use std::collections::HashMap;
    /// use tower_mcp::protocol::ReadResourceResult;
    /// use tower_mcp::resource::{ResourceTemplate, ResourceTemplateBuilder};
    ///
    /// fn template(pattern: &str) -> ResourceTemplate {
    ///     ResourceTemplateBuilder::new(pattern).name("example").handler(
    ///         |uri: String, _vars: HashMap<String, String>| async move {
    ///             Ok(ReadResourceResult::text(uri, "body"))
    ///         },
    ///     )
    /// }
    ///
    /// let segment = template("db://users/{id}");
    /// assert_eq!(segment.match_uri("db://users/42").unwrap()["id"], "42");
    /// assert!(segment.match_uri("db://users/42/posts").is_none());
    ///
    /// let subtree = template("file:///{+path}");
    /// assert_eq!(
    ///     subtree.match_uri("file:///src/lib.rs").unwrap()["path"],
    ///     "src/lib.rs"
    /// );
    ///
    /// // Both forms require something to capture: neither matches empty.
    /// assert!(segment.match_uri("db://users/").is_none());
    /// ```
    ///
    /// # Query expansion
    ///
    /// `{?a,b}`, and the `{&a,b}` continuation form, declare query parameters
    /// (#1253). They are matched against the URI's query string rather than
    /// compiled into the pattern, because a regex cannot reasonably express
    /// "any subset of these, in any order".
    ///
    /// RFC 6570 defines expansion rather than matching, so the routing policy
    /// is this crate's:
    ///
    /// - every declared variable is optional, and the bare URI still matches
    /// - order is not significant
    /// - present with no value is an empty string, and distinct from absent
    /// - undeclared keys are ignored rather than rejected, so a caller
    ///   appending a tracking parameter cannot break routing
    /// - the first occurrence of a repeated key wins
    /// - values are percent-decoded and `+` reads as a space; a malformed
    ///   escape is left as written rather than dropped, so a bad value still
    ///   reaches the handler instead of vanishing
    ///
    /// Path captures are handed over exactly as matched. That asymmetry is
    /// deliberate: query strings are form-encoded by convention, and path
    /// captures were never decoded, so decoding them now would silently change
    /// what existing handlers receive.
    ///
    /// ```rust
    /// # use std::collections::HashMap;
    /// # use tower_mcp::protocol::ReadResourceResult;
    /// # use tower_mcp::resource::{ResourceTemplate, ResourceTemplateBuilder};
    /// # fn template(pattern: &str) -> ResourceTemplate {
    /// #     ResourceTemplateBuilder::new(pattern).name("example").handler(
    /// #         |uri: String, _vars: HashMap<String, String>| async move {
    /// #             Ok(ReadResourceResult::text(uri, "body"))
    /// #         },
    /// #     )
    /// # }
    /// let threads = template("agent://threads{?cursor,limit}");
    ///
    /// // Nothing supplied still routes here, with no variables.
    /// assert!(threads.match_uri("agent://threads").unwrap().is_empty());
    ///
    /// // A subset in any order, each variable under its own name.
    /// let vars = threads.match_uri("agent://threads?limit=20").unwrap();
    /// assert_eq!(vars["limit"], "20");
    /// assert!(!vars.contains_key("cursor"));
    ///
    /// // Present-but-empty is a different fact from absent.
    /// let vars = threads.match_uri("agent://threads?cursor=").unwrap();
    /// assert_eq!(vars["cursor"], "");
    ///
    /// // An undeclared key is ignored; a repeated key keeps the first.
    /// let vars = threads
    ///     .match_uri("agent://threads?utm=ad&cursor=one&cursor=two")
    ///     .unwrap();
    /// assert_eq!(vars["cursor"], "one");
    /// assert!(!vars.contains_key("utm"));
    ///
    /// // Query values are decoded; the path capture beside them is not.
    /// let files = template("file:///{name}{?rev}");
    /// let vars = files.match_uri("file:///a%20b?rev=a+b").unwrap();
    /// assert_eq!(vars["name"], "a%20b");
    /// assert_eq!(vars["rev"], "a b");
    ///
    /// // A template that declares no query expression never splits on `?`,
    /// // so a literal `?` in the pattern keeps matching literally.
    /// let literal = template("http://example.com/api?query={q}");
    /// assert_eq!(
    ///     literal.match_uri("http://example.com/api?query=hello").unwrap()["q"],
    ///     "hello"
    /// );
    /// ```
    pub fn match_uri(&self, uri: &str) -> Option<HashMap<String, String>> {
        // Only a template that declares a query expression splits the URI.
        // Without one the pattern matches the whole string, including any
        // literal `?`, exactly as it did before query support existed.
        let (path, query) = if self.query_variables.is_empty() {
            (uri, None)
        } else {
            match uri.split_once('?') {
                Some((path, query)) => (path, Some(query)),
                None => (uri, None),
            }
        };

        let mut matched: HashMap<String, String> = self.pattern.captures(path).map(|caps| {
            self.variables
                .iter()
                .enumerate()
                .filter_map(|(i, name)| {
                    caps.get(i + 1)
                        .map(|m| (name.clone(), m.as_str().to_string()))
                })
                .collect()
        })?;

        if let Some(query) = query {
            matched.extend(extract_query_variables(query, &self.query_variables));
        }
        Some(matched)
    }

    /// Read one URI through this template's handler.
    ///
    /// `variables` is what [`match_uri`](Self::match_uri) returned for `uri`.
    /// Nothing re-derives or checks it here, so a map taken from a different
    /// URI is passed to the handler as given.
    ///
    /// Unlike [`Resource::read`], this returns a `Result`. Template handlers
    /// are not wrapped in an error-catching service, so an `Err` stays an
    /// error and the router turns it into a JSON-RPC error rather than into
    /// resource content.
    ///
    /// A template built with `mrtr_handler` has no plain handler to call and
    /// reports that as an error here; use
    /// [`read_outcome_with_context`](Self::read_outcome_with_context).
    pub fn read(
        &self,
        uri: &str,
        variables: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<ReadResourceResult>> {
        match &self.handler {
            Some(handler) => handler.read(uri, variables),
            None => Box::pin(async {
                Err(Error::invalid_params(
                    "MRTR resource template requires read_outcome_with_context",
                ))
            }),
        }
    }

    /// Read a matched resource while preserving an SEP-2322 continuation.
    pub fn read_outcome_with_context(
        &self,
        ctx: RequestContext,
        uri: &str,
        variables: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<RequestOutcome<ReadResourceResult>>> {
        let _ = &ctx;
        #[cfg(feature = "stateless")]
        if let Some(handler) = &self.mrtr_handler {
            return handler.read(ctx, uri, variables);
        }
        match &self.handler {
            Some(handler) => {
                let handler = handler.clone();
                let uri = uri.to_string();
                Box::pin(async move {
                    handler
                        .read(&uri, variables)
                        .await
                        .map(RequestOutcome::Complete)
                })
            }
            None => Box::pin(async {
                Err(Error::invalid_params(
                    "resource template has neither a complete nor MRTR handler",
                ))
            }),
        }
    }
}

/// Builder for a [`ResourceTemplate`].
///
/// The chain ends at [`handler`](Self::handler), which compiles the pattern
/// and returns the template itself; there is no separate `build` step. The
/// name defaults to the pattern when it is not set.
///
/// # Example
///
/// A paged listing, where the page cursor is a declared query variable and the
/// handler has to cope with its absence because every query variable is
/// optional:
///
/// ```rust
/// use std::collections::HashMap;
/// use tower_mcp::protocol::ReadResourceResult;
/// use tower_mcp::resource::ResourceTemplateBuilder;
///
/// let template = ResourceTemplateBuilder::new("agent://threads{?cursor,limit}")
///     .name("Threads")
///     .description("Conversation threads, newest first")
///     .mime_type("application/json")
///     .handler(|uri: String, vars: HashMap<String, String>| async move {
///         let cursor = vars.get("cursor").map(String::as_str).unwrap_or("start");
///         let limit: usize = vars
///             .get("limit")
///             .and_then(|value| value.parse().ok())
///             .unwrap_or(50);
///         Ok(ReadResourceResult::text(
///             uri,
///             format!("{limit} threads from {cursor}"),
///         ))
///     });
///
/// # tokio_test::block_on(async {
/// let vars = template.match_uri("agent://threads").unwrap();
/// let result = template.read("agent://threads", vars).await.unwrap();
/// assert_eq!(
///     result.contents[0].text.as_deref(),
///     Some("50 threads from start")
/// );
/// # });
/// ```
pub struct ResourceTemplateBuilder {
    uri_template: String,
    name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ContentAnnotations>,
    arguments: Vec<PromptArgument>,
}

impl ResourceTemplateBuilder {
    /// Start a template from an RFC 6570 URI pattern.
    ///
    /// The pattern is not compiled until a handler is attached, so a mistake
    /// in it surfaces at [`handler`](Self::handler) (a panic) or
    /// [`try_handler`](Self::try_handler) (an error), not here.
    ///
    /// # Supported expansions
    ///
    /// | form | matches | example |
    /// |---|---|---|
    /// | `{var}` | any run of non-slash characters | `db://users/{id}` matches `db://users/123` |
    /// | `{+var}` | any characters, slashes included | `file:///{+path}` matches `file:///src/lib.rs` |
    /// | `{?a,b}`, `{&a,b}` | optional query parameters (#1253) | `agent://threads{?cursor,limit}` matches `agent://threads` and `agent://threads?limit=20` |
    ///
    /// Everything else in the pattern is literal, including a `?` in a
    /// template that declares no query expression.
    /// [`ResourceTemplate::match_uri`] documents the matching rules in full,
    /// with the query routing policy this crate settled on where RFC 6570
    /// defines expansion but not matching.
    ///
    /// # Rejected at build time
    ///
    /// A query expression describes the query string, so nothing can follow
    /// it: trailing text, a second query expression, and an empty variable
    /// name are all reported rather than left to mismatch silently at
    /// request time.
    pub fn new(uri_template: impl Into<String>) -> Self {
        Self {
            uri_template: uri_template.into(),
            name: None,
            title: None,
            description: None,
            mime_type: None,
            icons: None,
            annotations: None,
            arguments: Vec::new(),
        }
    }

    /// Set the human-readable name for this template
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set a human-readable title for the template
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the description for this template
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the MIME type hint for resources from this template
    pub fn mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }

    /// Add an icon for the template
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

    /// Set annotations (audience, priority hints) for this resource template
    pub fn annotations(mut self, annotations: ContentAnnotations) -> Self {
        self.annotations = Some(annotations);
        self
    }

    /// Declare an argument this template accepts for URI expansion.
    ///
    /// Arguments are advertised on `resources/templates/list`, and are for the
    /// client's benefit rather than the router's: matching a URI still comes
    /// from the compiled pattern, so declaring an argument neither adds a
    /// variable nor validates one. Nothing is derived from the pattern
    /// automatically, because a variable name alone carries no description and
    /// no honest answer about whether it is required (#1282).
    ///
    /// ```rust
    /// use tower_mcp::ResourceTemplate;
    ///
    /// let template = ResourceTemplate::builder("db://{table}{?limit}")
    ///     .name("rows")
    ///     .argument("table", Some("Table to read from"), true)
    ///     .argument("limit", Some("Maximum rows to return"), false)
    ///     .handler(|_uri: String, _vars: std::collections::HashMap<String, String>| async move {
    ///         Ok(tower_mcp::protocol::ReadResourceResult::text("db://t", "[]"))
    ///     });
    ///
    /// let definition = template.definition();
    /// assert_eq!(definition.arguments.len(), 2);
    /// assert!(definition.arguments[0].required);
    /// assert!(!definition.arguments[1].required);
    /// ```
    pub fn argument(
        mut self,
        name: impl Into<String>,
        description: Option<impl Into<String>>,
        required: bool,
    ) -> Self {
        self.arguments.push(PromptArgument {
            name: name.into(),
            description: description.map(Into::into),
            required,
        });
        self
    }

    /// Attach the handler, compile the pattern, and return the template.
    ///
    /// The handler is called with the URI that was requested and the variables
    /// [`ResourceTemplate::match_uri`] pulled out of it. Only variables that
    /// actually matched are present: path variables always are, query
    /// variables only when the request supplied them.
    ///
    /// ```rust
    /// use std::collections::HashMap;
    /// use tower_mcp::protocol::ReadResourceResult;
    /// use tower_mcp::resource::ResourceTemplateBuilder;
    ///
    /// let template = ResourceTemplateBuilder::new("api://v1/{collection}/{id}")
    ///     .name("API records")
    ///     .handler(|uri: String, vars: HashMap<String, String>| async move {
    ///         let body = format!("{} #{}", vars["collection"], vars["id"]);
    ///         Ok(ReadResourceResult::text(uri, body))
    ///     });
    ///
    /// # tokio_test::block_on(async {
    /// let uri = "api://v1/posts/456";
    /// let vars = template.match_uri(uri).unwrap();
    /// let result = template.read(uri, vars).await.unwrap();
    /// assert_eq!(result.contents[0].text.as_deref(), Some("posts #456"));
    /// # });
    /// ```
    ///
    /// Indexing the map, as above, is safe for a path variable the pattern
    /// declares and a panic for anything else. Use `get` for query variables.
    ///
    /// # Panics
    ///
    /// Panics if the URI template is not a valid pattern. That is the right
    /// trade for a template written as a literal, since it fails on the first
    /// run rather than on the first request. For a pattern that comes from
    /// configuration or user input, use [`try_handler`](Self::try_handler).
    pub fn handler<F, Fut>(self, handler: F) -> ResourceTemplate
    where
        F: Fn(String, HashMap<String, String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
    {
        self.try_handler(handler).unwrap_or_else(|e| {
            panic!("Invalid URI template: {e}");
        })
    }

    /// The fallible form of [`handler`](Self::handler).
    ///
    /// Reach for this when the pattern is not a literal in the source: a
    /// plugin manifest, a config file, an argument. A bad pattern then
    /// disables one template with a reportable error instead of taking down
    /// the server that loaded it.
    ///
    /// ```rust
    /// use std::collections::HashMap;
    /// use tower_mcp::protocol::ReadResourceResult;
    /// use tower_mcp::resource::ResourceTemplateBuilder;
    ///
    /// fn load(pattern: &str) -> Result<(), String> {
    ///     ResourceTemplateBuilder::new(pattern)
    ///         .name("configured")
    ///         .try_handler(|uri: String, _vars: HashMap<String, String>| async move {
    ///             Ok(ReadResourceResult::text(uri, "body"))
    ///         })
    ///         .map(|_template| ())
    ///         .map_err(|error| error.to_string())
    /// }
    ///
    /// assert!(load("agent://threads{?cursor}").is_ok());
    ///
    /// // A query expression describes the end of the URI, so nothing may
    /// // follow it.
    /// let error = load("agent://threads{?cursor}/latest").unwrap_err();
    /// assert!(error.contains("after its query expression"), "{error}");
    /// ```
    pub fn try_handler<F, Fut>(self, handler: F) -> std::result::Result<ResourceTemplate, Error>
    where
        F: Fn(String, HashMap<String, String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
    {
        let CompiledTemplate {
            pattern,
            variables,
            query_variables,
        } = compile_uri_template(&self.uri_template)?;
        let name = self.name.unwrap_or_else(|| self.uri_template.clone());

        Ok(ResourceTemplate {
            uri_template: self.uri_template,
            name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            annotations: self.annotations,
            arguments: self.arguments,
            meta: None,
            pattern,
            query_variables,
            variables,
            handler: Some(Arc::new(FnTemplateHandler { handler })),
            #[cfg(feature = "stateless")]
            mrtr_handler: None,
        })
    }

    /// Set an SEP-2322-aware template handler that may require client input.
    ///
    /// # Panics
    ///
    /// Panics if the URI template produces an invalid regex pattern. Use
    /// [`try_mrtr_handler`](Self::try_mrtr_handler) for a fallible variant.
    #[cfg(feature = "stateless")]
    pub fn mrtr_handler<F, Fut>(self, handler: F) -> ResourceTemplate
    where
        F: Fn(RequestContext, String, HashMap<String, String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<RequestOutcome<ReadResourceResult>>> + Send + 'static,
    {
        self.try_mrtr_handler(handler)
            .unwrap_or_else(|error| panic!("Invalid URI template: {error}"))
    }

    /// Fallible variant of [`mrtr_handler`](Self::mrtr_handler).
    #[cfg(feature = "stateless")]
    pub fn try_mrtr_handler<F, Fut>(
        self,
        handler: F,
    ) -> std::result::Result<ResourceTemplate, Error>
    where
        F: Fn(RequestContext, String, HashMap<String, String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<RequestOutcome<ReadResourceResult>>> + Send + 'static,
    {
        let CompiledTemplate {
            pattern,
            variables,
            query_variables,
        } = compile_uri_template(&self.uri_template)?;
        let name = self.name.unwrap_or_else(|| self.uri_template.clone());

        Ok(ResourceTemplate {
            uri_template: self.uri_template,
            name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            annotations: self.annotations,
            arguments: self.arguments,
            meta: None,
            pattern,
            query_variables,
            variables,
            handler: None,
            mrtr_handler: Some(Arc::new(MrtrFnTemplateHandler { handler })),
        })
    }
}

/// Handler wrapping a function for templates
struct FnTemplateHandler<F> {
    handler: F,
}

impl<F, Fut> ResourceTemplateHandler for FnTemplateHandler<F>
where
    F: Fn(String, HashMap<String, String>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
{
    fn read(
        &self,
        uri: &str,
        variables: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<ReadResourceResult>> {
        let uri = uri.to_string();
        Box::pin((self.handler)(uri, variables))
    }
}

#[cfg(feature = "stateless")]
struct MrtrFnTemplateHandler<F> {
    handler: F,
}

#[cfg(feature = "stateless")]
impl<F, Fut> MrtrResourceTemplateHandler for MrtrFnTemplateHandler<F>
where
    F: Fn(RequestContext, String, HashMap<String, String>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RequestOutcome<ReadResourceResult>>> + Send + 'static,
{
    fn read(
        &self,
        ctx: RequestContext,
        uri: &str,
        variables: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<RequestOutcome<ReadResourceResult>>> {
        Box::pin((self.handler)(ctx, uri.to_string(), variables))
    }
}

/// A URI template compiled for matching.
struct CompiledTemplate {
    /// Matches the part of a URI before any query string.
    pattern: regex::Regex,
    /// Path variables, in capture order.
    variables: Vec<String>,
    /// Variables declared by a form-style query expression, if any.
    ///
    /// Empty for a template without one, which is what keeps such templates
    /// matching a literal `?` exactly as before.
    query_variables: Vec<String>,
}

/// Compile a URI template into a regex pattern and extract variable names.
///
/// Supports:
/// - `{var}`, simple expansion, matching any characters except `/`
/// - `{+var}`, reserved expansion, matching any characters
/// - `{?a,b}` and `{&a,b}`, form-style query expansion (RFC 6570 section
///   3.2.8), matched against the URI's query string rather than the regex
///
/// A query expression must be the last thing in the template, since it
/// describes the query string and nothing can follow that (#1253).
fn compile_uri_template(template: &str) -> std::result::Result<CompiledTemplate, Error> {
    let mut pattern = String::from("^");
    let mut variables = Vec::new();
    let mut query_variables: Vec<String> = Vec::new();

    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            // Form-style query expansion describes the query string, so it is
            // matched separately rather than compiled into the regex.
            if matches!(chars.peek(), Some('?') | Some('&')) {
                chars.next();
                let body: String = chars.by_ref().take_while(|&c| c != '}').collect();
                if !query_variables.is_empty() {
                    return Err(Error::Internal(format!(
                        "URI template '{template}' declares more than one query expression"
                    )));
                }
                for name in body.split(',') {
                    let name = name.trim();
                    if name.is_empty() {
                        return Err(Error::Internal(format!(
                            "URI template '{template}' has an empty query variable name"
                        )));
                    }
                    query_variables.push(name.to_string());
                }
                if chars.peek().is_some() {
                    return Err(Error::Internal(format!(
                        "URI template '{template}' has text after its query expression, \
                         which describes the end of the URI"
                    )));
                }
                continue;
            }

            // Check for + prefix (reserved expansion)
            let is_reserved = chars.peek() == Some(&'+');
            if is_reserved {
                chars.next();
            }

            // Collect variable name
            let var_name: String = chars.by_ref().take_while(|&c| c != '}').collect();
            variables.push(var_name);

            // Choose pattern based on expansion type
            if is_reserved {
                // Reserved expansion - match anything
                pattern.push_str("(.+)");
            } else {
                // Simple expansion - match non-slash characters
                pattern.push_str("([^/]+)");
            }
        } else {
            // Escape regex special characters
            match c {
                '.' | '+' | '*' | '?' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
                | '\\' => {
                    pattern.push('\\');
                    pattern.push(c);
                }
                _ => pattern.push(c),
            }
        }
    }

    pattern.push('$');

    let regex = regex::Regex::new(&pattern)
        .map_err(|e| Error::Internal(format!("Invalid URI template '{}': {}", template, e)))?;

    Ok(CompiledTemplate {
        pattern: regex,
        variables,
        query_variables,
    })
}

/// Percent-decode a query component, treating `+` as a space.
///
/// Kept local rather than pulling in a decoder: resource templates are core,
/// and the crate's only percent-encoding dependency is optional and enabled
/// by an unrelated feature.
///
/// A malformed escape is left as written rather than dropped, so a bad value
/// still reaches the handler instead of silently vanishing from the map.
fn decode_query_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            // Decoded from the byte array rather than by slicing the
            // string. `&value[i + 1..i + 3]` panics when either index lands
            // inside a multibyte character, which a client can trigger with
            // something as ordinary as `?q=%a\u{e9}`. Indexing bytes cannot
            // cross a boundary, and any byte that is not an ASCII hex digit
            // fails `to_digit` and falls through to the malformed path.
            b'%' if i + 2 < bytes.len() => {
                let high = (bytes[i + 1] as char).to_digit(16);
                let low = (bytes[i + 2] as char).to_digit(16);
                match (high, low) {
                    (Some(high), Some(low)) => {
                        out.push((high * 16 + low) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_string())
}

/// Pull the declared variables out of a URI's query string.
///
/// Routing policy, which RFC 6570 does not specify for matching:
///
/// - every declared variable is optional, so a URI with none still matches
/// - order does not matter
/// - a variable present with no value maps to an empty string, which is
///   distinct from being absent
/// - keys that were not declared are ignored rather than rejected, so an
///   added tracking parameter does not break routing
/// - the first occurrence of a repeated key wins
fn extract_query_variables(query: &str, declared: &[String]) -> HashMap<String, String> {
    let mut found = HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (raw_key, raw_value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        let key = decode_query_component(raw_key);
        if !declared.contains(&key) {
            continue;
        }
        found
            .entry(key)
            .or_insert_with(|| decode_query_component(raw_value));
    }
    found
}

#[cfg(test)]
mod query_expansion_tests {
    use super::*;

    fn template(pattern: &str) -> ResourceTemplate {
        ResourceTemplateBuilder::new(pattern).name("t").handler(
            |uri: String, _vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: None,
                        text: Some("body".into()),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            },
        )
    }

    /// #1282: a template could not carry the metadata a plain resource can,
    /// and `definition()` emitted `arguments: []` unconditionally.
    ///
    /// The clone is the point of the assertion. `ResourceTemplate` implements
    /// `Clone` by hand because of the `Arc<dyn ..>` handler, so a new field
    /// that is not added there is dropped by every router that stores one.
    #[test]
    fn arguments_and_meta_reach_the_definition_and_survive_a_clone() {
        let template = ResourceTemplateBuilder::new("db://{table}{?limit}")
            .name("rows")
            .argument("table", Some("Table to read"), true)
            .argument("limit", None::<String>, false)
            .handler(|uri: String, _vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult::text(uri, "[]"))
            })
            .with_meta(serde_json::json!({ "example.com/tier": "internal" }))
            .expect("a valid _meta key");

        for (label, definition) in [
            ("original", template.definition()),
            ("clone", template.clone().definition()),
        ] {
            assert_eq!(definition.arguments.len(), 2, "{label} lost its arguments");
            assert_eq!(definition.arguments[0].name, "table");
            assert_eq!(
                definition.arguments[0].description.as_deref(),
                Some("Table to read")
            );
            assert!(definition.arguments[0].required);
            assert!(!definition.arguments[1].required);
            assert!(definition.arguments[1].description.is_none());
            assert_eq!(
                definition
                    .meta
                    .as_ref()
                    .and_then(|m| m.get("example.com/tier")),
                Some(&serde_json::json!("internal")),
                "{label} lost its _meta"
            );
        }
    }

    /// A template that declares nothing still advertises nothing, so the wire
    /// output is unchanged for every server that has not opted in.
    #[test]
    fn a_template_without_declared_arguments_advertises_none() {
        let definition = template("db://users/{id}").definition();
        assert!(definition.arguments.is_empty());
        assert!(definition.meta.is_none());
    }

    /// #1253: `{?cursor,limit}` was read as one variable literally named
    /// "?cursor,limit", with a required capture, so the base URI did not
    /// match at all.
    #[test]
    fn the_base_uri_matches_when_no_query_variable_is_present() {
        let t = template("agent://threads{?cursor,limit}");
        let vars = t.match_uri("agent://threads").expect("base URI must match");
        assert!(
            vars.is_empty(),
            "no query variables were supplied: {vars:?}"
        );
    }

    #[test]
    fn each_query_variable_arrives_under_its_own_name() {
        let t = template("agent://threads{?cursor,limit}");
        let vars = t
            .match_uri("agent://threads?cursor=abc&limit=20")
            .expect("must match");
        assert_eq!(vars.get("cursor").map(String::as_str), Some("abc"));
        assert_eq!(vars.get("limit").map(String::as_str), Some("20"));
    }

    /// Every declared variable is optional and order is not significant,
    /// which is the routing policy RFC 6570 leaves open.
    #[test]
    fn any_subset_in_any_order_matches() {
        let t = template("codex://threads{?cursor,limit,cwd}");
        let vars = t
            .match_uri("codex://threads?cwd=%2Ftmp&cursor=x")
            .expect("must match");
        assert_eq!(vars.get("cursor").map(String::as_str), Some("x"));
        assert_eq!(vars.get("cwd").map(String::as_str), Some("/tmp"));
        assert!(!vars.contains_key("limit"), "absent stays absent: {vars:?}");
    }

    #[test]
    fn query_expansion_composes_with_path_variables() {
        let t = template("claude://projects/{project_key}/sessions{?cursor}");
        let vars = t
            .match_uri("claude://projects/abc/sessions?cursor=n2")
            .expect("must match");
        assert_eq!(vars.get("project_key").map(String::as_str), Some("abc"));
        assert_eq!(vars.get("cursor").map(String::as_str), Some("n2"));

        let bare = t
            .match_uri("claude://projects/abc/sessions")
            .expect("must match without a query");
        assert_eq!(bare.get("project_key").map(String::as_str), Some("abc"));
        assert!(!bare.contains_key("cursor"));
    }

    /// Present-but-empty is a different fact from absent, and a handler that
    /// distinguishes them should be able to.
    #[test]
    fn an_empty_value_is_present_not_absent() {
        let t = template("agent://threads{?cursor}");
        let vars = t.match_uri("agent://threads?cursor=").expect("must match");
        assert_eq!(vars.get("cursor").map(String::as_str), Some(""));
    }

    /// Routing must not break because a caller appended something we never
    /// declared, and a repeated key resolves predictably.
    #[test]
    fn undeclared_keys_are_ignored_and_the_first_duplicate_wins() {
        let t = template("agent://threads{?cursor}");
        let vars = t
            .match_uri("agent://threads?utm=x&cursor=first&cursor=second")
            .expect("must match");
        assert_eq!(vars.get("cursor").map(String::as_str), Some("first"));
        assert!(!vars.contains_key("utm"));
    }

    #[test]
    fn values_are_percent_decoded_and_plus_is_a_space() {
        let t = template("agent://search{?q}");
        let vars = t
            .match_uri("agent://search?q=a%20b+c%2Fd")
            .expect("must match");
        assert_eq!(vars.get("q").map(String::as_str), Some("a b c/d"));
    }

    /// A percent sign next to a multibyte character used to panic: the
    /// decoder sliced the string by byte offset after checking only length,
    /// so either end of the slice could land inside a character. Any client
    /// could reach it through a query expression, because even an undeclared
    /// key is decoded before it is discarded.
    #[test]
    fn percent_decoding_survives_multibyte_input() {
        let t = template("agent://search{?q}");
        for (uri, expected) in [
            ("agent://search?q=%a\u{e9}", "%a\u{e9}"),
            ("agent://search?q=a%9\u{e9}", "a%9\u{e9}"),
            ("agent://search?q=\u{e9}%\u{e9}", "\u{e9}%\u{e9}"),
            ("agent://search?q=%\u{1f600}", "%\u{1f600}"),
        ] {
            let vars = t
                .match_uri(uri)
                .unwrap_or_else(|| panic!("must match: {uri}"));
            assert_eq!(
                vars.get("q").map(String::as_str),
                Some(expected),
                "for {uri}"
            );
        }
    }

    /// An undeclared key is decoded before it is discarded, so it is just as
    /// reachable as a declared one.
    #[test]
    fn an_undeclared_key_with_multibyte_input_is_also_safe() {
        let t = template("agent://search{?q}");
        let vars = t
            .match_uri("agent://search?%a\u{e9}=x&q=fine")
            .expect("must match");
        assert_eq!(vars.get("q").map(String::as_str), Some("fine"));
    }

    /// A percent at or near the end has no two digits to read.
    #[test]
    fn a_truncated_escape_is_left_alone() {
        let t = template("agent://search{?q}");
        for (uri, expected) in [
            ("agent://search?q=%", "%"),
            ("agent://search?q=%2", "%2"),
            ("agent://search?q=a%", "a%"),
            ("agent://search?q=%zz", "%zz"),
        ] {
            let vars = t.match_uri(uri).expect("must match");
            assert_eq!(
                vars.get("q").map(String::as_str),
                Some(expected),
                "for {uri}"
            );
        }
    }

    /// A malformed escape reaches the handler as written rather than
    /// disappearing, so the caller can see what they sent.
    #[test]
    fn a_malformed_escape_is_left_alone() {
        let t = template("agent://search{?q}");
        let vars = t.match_uri("agent://search?q=100%").expect("must match");
        assert_eq!(vars.get("q").map(String::as_str), Some("100%"));
    }

    /// The `&` continuation form is the same expansion.
    #[test]
    fn the_continuation_operator_is_supported() {
        let t = template("agent://threads{&cursor}");
        let vars = t.match_uri("agent://threads?cursor=z").expect("must match");
        assert_eq!(vars.get("cursor").map(String::as_str), Some("z"));
    }

    /// A template without a query expression keeps matching a literal `?`
    /// exactly as before, which is what stops this being a breaking change.
    #[test]
    fn templates_without_a_query_expression_are_unchanged() {
        let compiled = compile_uri_template("http://example.com/api?query={q}").unwrap();
        assert!(compiled.query_variables.is_empty());
        assert_eq!(compiled.variables, vec!["q".to_string()]);

        let t = template("http://example.com/api?query={q}");
        let vars = t
            .match_uri("http://example.com/api?query=hello")
            .expect("must match");
        assert_eq!(vars.get("q").map(String::as_str), Some("hello"));
    }

    #[test]
    fn a_query_expression_must_end_the_template() {
        assert!(compile_uri_template("agent://x{?a}/more").is_err());
        assert!(compile_uri_template("agent://x{?a}{?b}").is_err());
        assert!(compile_uri_template("agent://x{?}").is_err());
        assert!(compile_uri_template("agent://x{?a,}").is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tower::timeout::TimeoutLayer;

    #[tokio::test]
    async fn test_builder_resource() {
        let resource = ResourceBuilder::new("file:///test.txt")
            .name("Test File")
            .description("A test file")
            .text("Hello, World!");

        assert_eq!(resource.uri, "file:///test.txt");
        assert_eq!(resource.name, "Test File");
        assert_eq!(resource.description.as_deref(), Some("A test file"));

        let result = resource.read().await;
        assert_eq!(result.contents.len(), 1);
        assert_eq!(result.contents[0].text.as_deref(), Some("Hello, World!"));
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn test_mrtr_builder_preserves_input_required_outcome() {
        let resource = ResourceBuilder::new("test://continue")
            .mrtr_handler(|_ctx| async move {
                Ok(RequestOutcome::input_required(
                    crate::protocol::InputRequiredResult::new().with_request_state("signed-state"),
                ))
            })
            .build();

        let outcome = resource
            .read_outcome_with_context(RequestContext::new(crate::protocol::RequestId::Number(1)))
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
    async fn mrtr_resource_composes_middleware() {
        let resource = ResourceBuilder::new("test://layered-continue")
            .mrtr_handler(|_ctx| async move {
                Ok(RequestOutcome::input_required(
                    crate::protocol::InputRequiredResult::new().with_request_state("layered-state"),
                ))
            })
            .layer(TimeoutLayer::new(Duration::from_secs(1)))
            .build();

        let outcome = resource
            .read_outcome_with_context(RequestContext::new(crate::protocol::RequestId::Number(2)))
            .await
            .unwrap();
        assert_eq!(
            outcome
                .as_input_required()
                .and_then(|result| result.request_state.as_deref()),
            Some("layered-state")
        );
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn test_mrtr_template_preserves_context_and_input_required_outcome() {
        let template = ResourceTemplateBuilder::new("test://items/{id}").mrtr_handler(
            |ctx, uri, variables| async move {
                assert_eq!(ctx.request_state(), Some("prior-state"));
                assert_eq!(uri, "test://items/42");
                assert_eq!(variables.get("id").map(String::as_str), Some("42"));
                Ok(RequestOutcome::input_required(
                    crate::protocol::InputRequiredResult::new().with_request_state("next-state"),
                ))
            },
        );
        let variables = template.match_uri("test://items/42").unwrap();
        let mut ctx = RequestContext::new(crate::protocol::RequestId::Number(1));
        ctx.extensions_mut().insert(crate::mrtr::MrtrRequest::new(
            None,
            Some("prior-state".into()),
        ));

        let outcome = template
            .read_outcome_with_context(ctx, "test://items/42", variables)
            .await
            .unwrap();
        assert_eq!(
            outcome
                .as_input_required()
                .and_then(|result| result.request_state.as_deref()),
            Some("next-state")
        );
    }

    #[tokio::test]
    async fn test_json_resource() {
        let resource = ResourceBuilder::new("file:///config.json")
            .name("Config")
            .json(serde_json::json!({"key": "value"}));

        assert_eq!(resource.mime_type.as_deref(), Some("application/json"));

        let result = resource.read().await;
        assert!(result.contents[0].text.as_ref().unwrap().contains("key"));
    }

    #[tokio::test]
    async fn test_handler_resource() {
        let resource = ResourceBuilder::new("memory://counter")
            .name("Counter")
            .handler(|| async {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "memory://counter".to_string(),
                        mime_type: Some("text/plain".to_string()),
                        text: Some("42".to_string()),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            })
            .build();

        let result = resource.read().await;
        assert_eq!(result.contents[0].text.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn test_handler_resource_with_layer() {
        let resource = ResourceBuilder::new("file:///with-timeout.txt")
            .name("Resource with Timeout")
            .handler(|| async {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "file:///with-timeout.txt".to_string(),
                        mime_type: Some("text/plain".to_string()),
                        text: Some("content".to_string()),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            })
            .layer(TimeoutLayer::new(Duration::from_secs(30)))
            .build();

        let result = resource.read().await;
        assert_eq!(result.contents[0].text.as_deref(), Some("content"));
    }

    #[tokio::test]
    async fn test_handler_resource_with_timeout_error() {
        let resource = ResourceBuilder::new("file:///slow.txt")
            .name("Slow Resource")
            .handler(|| async {
                // Sleep much longer than timeout to ensure timeout fires reliably in CI
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "file:///slow.txt".to_string(),
                        mime_type: Some("text/plain".to_string()),
                        text: Some("content".to_string()),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            })
            .layer(TimeoutLayer::new(Duration::from_millis(50)))
            .build();

        let result = resource.read().await;
        // Timeout error should be caught and converted to error content
        assert!(
            result.contents[0]
                .text
                .as_ref()
                .unwrap()
                .contains("Error reading resource")
        );
    }

    #[tokio::test]
    async fn test_context_aware_handler() {
        let resource = ResourceBuilder::new("file:///ctx.txt")
            .name("Context Resource")
            .handler_with_context(|_ctx: RequestContext| async {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "file:///ctx.txt".to_string(),
                        mime_type: Some("text/plain".to_string()),
                        text: Some("context aware".to_string()),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            })
            .build();

        let result = resource.read().await;
        assert_eq!(result.contents[0].text.as_deref(), Some("context aware"));
    }

    #[tokio::test]
    async fn test_context_aware_handler_with_layer() {
        let resource = ResourceBuilder::new("file:///ctx-layer.txt")
            .name("Context Resource with Layer")
            .handler_with_context(|_ctx: RequestContext| async {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "file:///ctx-layer.txt".to_string(),
                        mime_type: Some("text/plain".to_string()),
                        text: Some("context with layer".to_string()),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            })
            .layer(TimeoutLayer::new(Duration::from_secs(30)))
            .build();

        let result = resource.read().await;
        assert_eq!(
            result.contents[0].text.as_deref(),
            Some("context with layer")
        );
    }

    #[tokio::test]
    async fn test_trait_resource() {
        struct TestResource;

        impl McpResource for TestResource {
            const URI: &'static str = "test://resource";
            const NAME: &'static str = "Test";
            const DESCRIPTION: Option<&'static str> = Some("A test resource");
            const MIME_TYPE: Option<&'static str> = Some("text/plain");

            async fn read(&self) -> Result<ReadResourceResult> {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: Self::URI.to_string(),
                        mime_type: Self::MIME_TYPE.map(|s| s.to_string()),
                        text: Some("test content".to_string()),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            }
        }

        let resource = TestResource.into_resource();
        assert_eq!(resource.uri, "test://resource");
        assert_eq!(resource.name, "Test");

        let result = resource.read().await;
        assert_eq!(result.contents[0].text.as_deref(), Some("test content"));
    }

    #[test]
    fn test_resource_definition() {
        let resource = ResourceBuilder::new("file:///test.txt")
            .name("Test")
            .description("Description")
            .mime_type("text/plain")
            .text("content");

        let def = resource.definition();
        assert_eq!(def.uri, "file:///test.txt");
        assert_eq!(def.name, "Test");
        assert_eq!(def.description.as_deref(), Some("Description"));
        assert_eq!(def.mime_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn test_resource_request_new() {
        let ctx = RequestContext::new(crate::protocol::RequestId::Number(1));
        let req = ResourceRequest::new(ctx, "file:///test.txt".to_string());
        assert_eq!(req.uri, "file:///test.txt");
    }

    #[test]
    fn test_resource_catch_error_clone() {
        let handler = FnHandler {
            handler: || async {
                Ok::<_, Error>(ReadResourceResult {
                    contents: vec![],
                    meta: None,
                    ..Default::default()
                })
            },
        };
        let service = ResourceHandlerService::new(handler);
        let catch_error = ResourceCatchError::new(service);
        let _clone = catch_error.clone();
    }

    #[test]
    fn test_resource_catch_error_debug() {
        let handler = FnHandler {
            handler: || async {
                Ok::<_, Error>(ReadResourceResult {
                    contents: vec![],
                    meta: None,
                    ..Default::default()
                })
            },
        };
        let service = ResourceHandlerService::new(handler);
        let catch_error = ResourceCatchError::new(service);
        let debug = format!("{:?}", catch_error);
        assert!(debug.contains("ResourceCatchError"));
    }

    // =========================================================================
    // Resource Template Tests
    // =========================================================================

    #[test]
    fn test_compile_uri_template_simple() {
        let CompiledTemplate {
            pattern: regex,
            variables: vars,
            ..
        } = compile_uri_template("file:///{path}").unwrap();
        assert_eq!(vars, vec!["path"]);
        assert!(regex.is_match("file:///README.md"));
        assert!(!regex.is_match("file:///foo/bar")); // no slashes in simple expansion
    }

    #[test]
    fn test_compile_uri_template_multiple_vars() {
        let CompiledTemplate {
            pattern: regex,
            variables: vars,
            ..
        } = compile_uri_template("api://v1/{resource}/{id}").unwrap();
        assert_eq!(vars, vec!["resource", "id"]);
        assert!(regex.is_match("api://v1/users/123"));
        assert!(regex.is_match("api://v1/posts/abc"));
        assert!(!regex.is_match("api://v1/users")); // missing id
    }

    #[test]
    fn test_compile_uri_template_reserved_expansion() {
        let CompiledTemplate {
            pattern: regex,
            variables: vars,
            ..
        } = compile_uri_template("file:///{+path}").unwrap();
        assert_eq!(vars, vec!["path"]);
        assert!(regex.is_match("file:///README.md"));
        assert!(regex.is_match("file:///foo/bar/baz.txt")); // slashes allowed
    }

    #[test]
    fn test_compile_uri_template_special_chars() {
        let CompiledTemplate {
            pattern: regex,
            variables: vars,
            ..
        } = compile_uri_template("http://example.com/api?query={q}").unwrap();
        assert_eq!(vars, vec!["q"]);
        assert!(regex.is_match("http://example.com/api?query=hello"));
    }

    #[test]
    fn test_resource_template_match_uri() {
        let template = ResourceTemplateBuilder::new("db://users/{id}")
            .name("User Records")
            .handler(|uri: String, vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: None,
                        text: Some(format!("User {}", vars.get("id").unwrap())),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            });

        // Test matching
        let vars = template.match_uri("db://users/123").unwrap();
        assert_eq!(vars.get("id"), Some(&"123".to_string()));

        // Test non-matching
        assert!(template.match_uri("db://posts/123").is_none());
        assert!(template.match_uri("db://users").is_none());
    }

    #[test]
    fn test_resource_template_match_multiple_vars() {
        let template = ResourceTemplateBuilder::new("api://{version}/{resource}/{id}")
            .name("API Resources")
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

        let vars = template.match_uri("api://v2/users/abc-123").unwrap();
        assert_eq!(vars.get("version"), Some(&"v2".to_string()));
        assert_eq!(vars.get("resource"), Some(&"users".to_string()));
        assert_eq!(vars.get("id"), Some(&"abc-123".to_string()));
    }

    #[tokio::test]
    async fn test_resource_template_read() {
        let template = ResourceTemplateBuilder::new("file:///{path}")
            .name("Files")
            .mime_type("text/plain")
            .handler(|uri: String, vars: HashMap<String, String>| async move {
                let path = vars.get("path").unwrap().clone();
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("text/plain".to_string()),
                        text: Some(format!("Contents of {}", path)),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            });

        let vars = template.match_uri("file:///README.md").unwrap();
        let result = template.read("file:///README.md", vars).await.unwrap();

        assert_eq!(result.contents.len(), 1);
        assert_eq!(result.contents[0].uri, "file:///README.md");
        assert_eq!(
            result.contents[0].text.as_deref(),
            Some("Contents of README.md")
        );
    }

    #[test]
    fn test_resource_template_definition() {
        let template = ResourceTemplateBuilder::new("db://records/{id}")
            .name("Database Records")
            .description("Access database records by ID")
            .mime_type("application/json")
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

        let def = template.definition();
        assert_eq!(def.uri_template, "db://records/{id}");
        assert_eq!(def.name, "Database Records");
        assert_eq!(
            def.description.as_deref(),
            Some("Access database records by ID")
        );
        assert_eq!(def.mime_type.as_deref(), Some("application/json"));
    }

    #[test]
    fn test_resource_template_reserved_path() {
        let template = ResourceTemplateBuilder::new("file:///{+path}")
            .name("Files with subpaths")
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

        // Reserved expansion should match slashes
        let vars = template.match_uri("file:///src/lib/utils.rs").unwrap();
        assert_eq!(vars.get("path"), Some(&"src/lib/utils.rs".to_string()));
    }

    #[test]
    fn test_resource_annotations() {
        use crate::protocol::{ContentAnnotations, ContentRole};

        let annotations = ContentAnnotations {
            audience: Some(vec![ContentRole::User]),
            priority: Some(0.8),
            last_modified: None,
        };

        let resource = ResourceBuilder::new("file:///important.txt")
            .name("Important File")
            .annotations(annotations.clone())
            .text("content");

        let def = resource.definition();
        assert!(def.annotations.is_some());
        let ann = def.annotations.unwrap();
        assert_eq!(ann.priority, Some(0.8));
        assert_eq!(ann.audience.unwrap(), vec![ContentRole::User]);
    }

    #[test]
    fn test_resource_template_annotations() {
        use crate::protocol::{ContentAnnotations, ContentRole};

        let annotations = ContentAnnotations {
            audience: Some(vec![ContentRole::Assistant]),
            priority: Some(0.5),
            last_modified: None,
        };

        let template = ResourceTemplateBuilder::new("db://users/{id}")
            .name("Users")
            .annotations(annotations)
            .handler(|uri: String, _vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: None,
                        text: Some("data".to_string()),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            });

        let def = template.definition();
        assert!(def.annotations.is_some());
        let ann = def.annotations.unwrap();
        assert_eq!(ann.priority, Some(0.5));
        assert_eq!(ann.audience.unwrap(), vec![ContentRole::Assistant]);
    }

    #[test]
    fn test_resource_no_annotations_by_default() {
        let resource = ResourceBuilder::new("file:///test.txt")
            .name("Test")
            .text("content");

        let def = resource.definition();
        assert!(def.annotations.is_none());
    }

    #[test]
    fn test_try_handler_success() {
        let result = ResourceTemplateBuilder::new("db://users/{id}")
            .name("Users")
            .try_handler(|uri: String, _vars: HashMap<String, String>| async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: None,
                        text: Some("ok".to_string()),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                    ..Default::default()
                })
            });

        assert!(result.is_ok());
        let template = result.unwrap();
        assert_eq!(template.uri_template, "db://users/{id}");
    }

    #[test]
    fn test_compile_uri_template_returns_result() {
        // Valid templates should succeed
        assert!(compile_uri_template("file:///{path}").is_ok());
        assert!(compile_uri_template("api://v1/{resource}/{id}").is_ok());
        assert!(compile_uri_template("file:///{+path}").is_ok());
        assert!(compile_uri_template("no-vars").is_ok());
        assert!(compile_uri_template("").is_ok());
    }
}
