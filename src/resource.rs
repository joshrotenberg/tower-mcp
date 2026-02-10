//! Resource definition and builder API
//!
//! Provides ergonomic ways to define MCP resources:
//!
//! 1. **Builder pattern** - Fluent API for defining resources
//! 2. **Trait-based** - Implement `McpResource` for full control
//! 3. **Resource templates** - Parameterized resources using URI templates (RFC 6570)
//!
//! ## Per-Resource Middleware
//!
//! Resources are implemented as Tower services internally, enabling middleware
//! composition via the `.layer()` method:
//!
//! ```rust
//! use std::time::Duration;
//! use tower::timeout::TimeoutLayer;
//! use tower_mcp::resource::ResourceBuilder;
//! use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
//!
//! let resource = ResourceBuilder::new("file:///large-file.txt")
//!     .name("Large File")
//!     .description("A large file that may take time to read")
//!     .handler(|| async {
//!         // Simulate slow read
//!         Ok(ReadResourceResult {
//!             contents: vec![ResourceContent {
//!                 uri: "file:///large-file.txt".to_string(),
//!                 mime_type: Some("text/plain".to_string()),
//!                 text: Some("content".to_string()),
//!                 blob: None,
//!             }],
//!         })
//!     })
//!     .layer(TimeoutLayer::new(Duration::from_secs(30)))
//!     .build();
//! ```
//!
//! # Resource Templates
//!
//! Resource templates allow servers to expose parameterized resources using URI templates.
//! When a client requests `resources/read` with a URI matching a template, the server
//! extracts the variables and passes them to the handler.
//!
//! ```rust
//! use tower_mcp::resource::ResourceTemplateBuilder;
//! use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
//! use std::collections::HashMap;
//!
//! let template = ResourceTemplateBuilder::new("file:///{path}")
//!     .name("Project Files")
//!     .description("Access files in the project directory")
//!     .handler(|uri: String, vars: HashMap<String, String>| async move {
//!         let path = vars.get("path").unwrap_or(&String::new()).clone();
//!         Ok(ReadResourceResult {
//!             contents: vec![ResourceContent {
//!                 uri,
//!                 mime_type: Some("text/plain".to_string()),
//!                 text: Some(format!("Contents of {}", path)),
//!                 blob: None,
//!             }],
//!         })
//!     });
//! ```

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::util::BoxCloneService;
use tower_service::Service;

use crate::context::RequestContext;
use crate::error::{Error, Result};
use crate::protocol::{
    ContentAnnotations, ReadResourceResult, ResourceContent, ResourceDefinition,
    ResourceTemplateDefinition, ToolIcon,
};

// =============================================================================
// Service Types for Per-Resource Middleware
// =============================================================================

/// Request type for resource services.
///
/// Contains the request context (for progress reporting, cancellation, etc.)
/// and the resource URI being read.
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

/// Catches errors from the inner service and converts them to error results.
///
/// This wrapper ensures that middleware errors (e.g., timeouts, rate limits)
/// and handler errors are converted to `Err(Error)` responses wrapped in
/// `Ok`, rather than propagating as Tower service errors.
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

impl<S> Service<ResourceRequest> for ResourceCatchError<S>
where
    S: Service<ResourceRequest, Response = ReadResourceResult> + Clone + Send + 'static,
    S::Error: fmt::Display + Send,
    S::Future: Send,
{
    type Response = ReadResourceResult;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<ReadResourceResult, Infallible>> + Send>>;

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

        Box::pin(async move {
            match fut.await {
                Ok(result) => Ok(result),
                Err(err) => {
                    // Return an error result with the error message
                    Ok(ReadResourceResult {
                        contents: vec![ResourceContent {
                            uri,
                            mime_type: Some("text/plain".to_string()),
                            text: Some(format!("Error reading resource: {}", err)),
                            blob: None,
                        }],
                    })
                }
            }
        })
    }
}

/// A boxed future for resource handlers
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Resource handler trait - the core abstraction for resource reading
pub trait ResourceHandler: Send + Sync {
    /// Read the resource contents
    fn read(&self) -> BoxFuture<'_, Result<ReadResourceResult>>;

    /// Read the resource with request context for progress/cancellation support
    ///
    /// The default implementation ignores the context and calls `read`.
    /// Override this to receive progress/cancellation context.
    fn read_with_context(&self, _ctx: RequestContext) -> BoxFuture<'_, Result<ReadResourceResult>> {
        self.read()
    }

    /// Returns true if this handler uses context (for optimization)
    fn uses_context(&self) -> bool {
        false
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

/// A complete resource definition with service-based execution.
///
/// Resources are implemented as Tower services internally, enabling middleware
/// composition via the builder's `.layer()` method. The service is wrapped
/// in [`ResourceCatchError`] to convert any errors (from handlers or middleware)
/// into error result responses.
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
    /// The boxed service that reads the resource
    service: BoxResourceService,
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
            service: self.service.clone(),
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

    /// Get the resource definition for resources/list
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
        }
    }

    /// Read the resource without context
    ///
    /// Creates a dummy request context. For full context support, use
    /// [`read_with_context`](Self::read_with_context).
    pub fn read(&self) -> BoxFuture<'static, ReadResourceResult> {
        let ctx = RequestContext::new(crate::protocol::RequestId::Number(0));
        self.read_with_context(ctx)
    }

    /// Read the resource with request context
    ///
    /// The context provides progress reporting, cancellation support, and
    /// access to client requests (for sampling, etc.).
    ///
    /// # Note
    ///
    /// This method returns `ReadResourceResult` directly (not `Result<ReadResourceResult>`).
    /// Any errors from the handler or middleware are converted to error responses
    /// in the result contents.
    pub fn read_with_context(&self, ctx: RequestContext) -> BoxFuture<'static, ReadResourceResult> {
        use tower::ServiceExt;
        let service = self.service.clone();
        let uri = self.uri.clone();
        Box::pin(async move {
            // ServiceExt::oneshot properly handles poll_ready before call
            // Service is Infallible, so unwrap is safe
            service
                .oneshot(ResourceRequest::new(ctx, uri))
                .await
                .unwrap()
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
            service,
        }
    }
}

// =============================================================================
// Builder API
// =============================================================================

/// Builder for creating resources with a fluent API
///
/// # Example
///
/// ```rust
/// use tower_mcp::resource::ResourceBuilder;
/// use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
///
/// let resource = ResourceBuilder::new("file:///config.json")
///     .name("Configuration")
///     .description("Application configuration file")
///     .mime_type("application/json")
///     .handler(|| async {
///         Ok(ReadResourceResult {
///             contents: vec![ResourceContent {
///                 uri: "file:///config.json".to_string(),
///                 mime_type: Some("application/json".to_string()),
///                 text: Some(r#"{"setting": "value"}"#.to_string()),
///                 blob: None,
///             }],
///         })
///     })
///     .build();
///
/// assert_eq!(resource.uri, "file:///config.json");
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

    /// Set the resource name (human-readable)
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set a human-readable title for the resource
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the resource description
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the MIME type of the resource
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

    /// Set the size of the resource in bytes
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
    ///                 }],
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

    /// Set a context-aware handler for reading the resource.
    ///
    /// The handler receives a `RequestContext` for progress reporting and
    /// cancellation checking.
    ///
    /// Returns a [`ResourceBuilderWithContextHandler`] that can be used to apply
    /// middleware layers via `.layer()` or build the resource directly via `.build()`.
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

    /// Create a static text resource (convenience method)
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
                    }],
                })
            }
        })
        .build()
    }

    /// Create a static JSON resource (convenience method)
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
                    }],
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
    ///             }],
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
            service,
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
            service,
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

    fn uses_context(&self) -> bool {
        true
    }
}

// =============================================================================
// Trait-based resource definition
// =============================================================================

/// Trait for defining resources with full control
///
/// Implement this trait when you need more control than the builder provides,
/// or when you want to define resources as standalone types.
///
/// # Example
///
/// ```rust
/// use tower_mcp::resource::McpResource;
/// use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
/// use tower_mcp::error::Result;
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
///             }],
///         })
///     }
/// }
///
/// let resource = ConfigResource { config: "{}".to_string() }.into_resource();
/// assert_eq!(resource.uri, "file:///config.json");
/// ```
pub trait McpResource: Send + Sync + 'static {
    const URI: &'static str;
    const NAME: &'static str;
    const DESCRIPTION: Option<&'static str> = None;
    const MIME_TYPE: Option<&'static str> = None;

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

/// Handler trait for resource templates
///
/// Unlike [`ResourceHandler`], template handlers receive the extracted
/// URI variables as a parameter.
pub trait ResourceTemplateHandler: Send + Sync {
    /// Read a resource with the given URI variables extracted from the template
    fn read(
        &self,
        uri: &str,
        variables: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<ReadResourceResult>>;
}

/// A parameterized resource template
///
/// Resource templates use URI template syntax (RFC 6570) to match multiple URIs
/// and extract variable values. This allows servers to expose dynamic resources
/// like file systems or database records.
///
/// # Example
///
/// ```rust
/// use tower_mcp::resource::ResourceTemplateBuilder;
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
///             }],
///         })
///     });
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
    /// Compiled regex for matching URIs
    pattern: regex::Regex,
    /// Variable names in order of appearance
    variables: Vec<String>,
    /// Handler for reading matched resources
    handler: Arc<dyn ResourceTemplateHandler>,
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
            pattern: self.pattern.clone(),
            variables: self.variables.clone(),
            handler: self.handler.clone(),
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

    /// Get the template definition for resources/templates/list
    pub fn definition(&self) -> ResourceTemplateDefinition {
        ResourceTemplateDefinition {
            uri_template: self.uri_template.clone(),
            name: self.name.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            mime_type: self.mime_type.clone(),
            icons: self.icons.clone(),
            annotations: self.annotations.clone(),
        }
    }

    /// Check if a URI matches this template and extract variables
    ///
    /// Returns `Some(HashMap)` with extracted variables if the URI matches,
    /// `None` if it doesn't match.
    pub fn match_uri(&self, uri: &str) -> Option<HashMap<String, String>> {
        self.pattern.captures(uri).map(|caps| {
            self.variables
                .iter()
                .enumerate()
                .filter_map(|(i, name)| {
                    caps.get(i + 1)
                        .map(|m| (name.clone(), m.as_str().to_string()))
                })
                .collect()
        })
    }

    /// Read a resource at the given URI using this template's handler
    ///
    /// # Arguments
    ///
    /// * `uri` - The actual URI being read
    /// * `variables` - Variables extracted from matching the URI against the template
    pub fn read(
        &self,
        uri: &str,
        variables: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<ReadResourceResult>> {
        self.handler.read(uri, variables)
    }
}

/// Builder for creating resource templates
///
/// # Example
///
/// ```rust
/// use tower_mcp::resource::ResourceTemplateBuilder;
/// use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
/// use std::collections::HashMap;
///
/// let template = ResourceTemplateBuilder::new("db://users/{id}")
///     .name("User Records")
///     .description("Access user records by ID")
///     .handler(|uri: String, vars: HashMap<String, String>| async move {
///         let id = vars.get("id").unwrap();
///         Ok(ReadResourceResult {
///             contents: vec![ResourceContent {
///                 uri,
///                 mime_type: Some("application/json".to_string()),
///                 text: Some(format!(r#"{{"id": "{}"}}"#, id)),
///                 blob: None,
///             }],
///         })
///     });
/// ```
pub struct ResourceTemplateBuilder {
    uri_template: String,
    name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    icons: Option<Vec<ToolIcon>>,
    annotations: Option<ContentAnnotations>,
}

impl ResourceTemplateBuilder {
    /// Create a new builder with the given URI template
    ///
    /// # URI Template Syntax
    ///
    /// Templates use RFC 6570 Level 1 syntax with simple variable expansion:
    /// - `{varname}` - Matches any non-slash characters
    ///
    /// # Examples
    ///
    /// - `file:///{path}` - Matches `file:///README.md`
    /// - `db://users/{id}` - Matches `db://users/123`
    /// - `api://v1/{resource}/{id}` - Matches `api://v1/posts/456`
    pub fn new(uri_template: impl Into<String>) -> Self {
        Self {
            uri_template: uri_template.into(),
            name: None,
            title: None,
            description: None,
            mime_type: None,
            icons: None,
            annotations: None,
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

    /// Set annotations (audience, priority hints) for this resource template
    pub fn annotations(mut self, annotations: ContentAnnotations) -> Self {
        self.annotations = Some(annotations);
        self
    }

    /// Set the handler function for reading template resources
    ///
    /// The handler receives:
    /// - `uri`: The full URI being read
    /// - `variables`: A map of variable names to their values extracted from the URI
    pub fn handler<F, Fut>(self, handler: F) -> ResourceTemplate
    where
        F: Fn(String, HashMap<String, String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ReadResourceResult>> + Send + 'static,
    {
        let (pattern, variables) = compile_uri_template(&self.uri_template);
        let name = self.name.unwrap_or_else(|| self.uri_template.clone());

        ResourceTemplate {
            uri_template: self.uri_template,
            name,
            title: self.title,
            description: self.description,
            mime_type: self.mime_type,
            icons: self.icons,
            annotations: self.annotations,
            pattern,
            variables,
            handler: Arc::new(FnTemplateHandler { handler }),
        }
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

/// Compile a URI template into a regex pattern and extract variable names
///
/// Supports RFC 6570 Level 1 (simple expansion):
/// - `{var}` matches any characters except `/`
/// - `{+var}` matches any characters including `/` (reserved expansion)
///
/// Returns the compiled regex and a list of variable names in order.
fn compile_uri_template(template: &str) -> (regex::Regex, Vec<String>) {
    let mut pattern = String::from("^");
    let mut variables = Vec::new();

    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
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

    // Compile the regex - panic if template is malformed
    let regex = regex::Regex::new(&pattern)
        .unwrap_or_else(|e| panic!("Invalid URI template '{}': {}", template, e));

    (regex, variables)
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
                    }],
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
                    }],
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
                    }],
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
                    }],
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
                    }],
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
                    }],
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
            handler: || async { Ok::<_, Error>(ReadResourceResult { contents: vec![] }) },
        };
        let service = ResourceHandlerService::new(handler);
        let catch_error = ResourceCatchError::new(service);
        let _clone = catch_error.clone();
    }

    #[test]
    fn test_resource_catch_error_debug() {
        let handler = FnHandler {
            handler: || async { Ok::<_, Error>(ReadResourceResult { contents: vec![] }) },
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
        let (regex, vars) = compile_uri_template("file:///{path}");
        assert_eq!(vars, vec!["path"]);
        assert!(regex.is_match("file:///README.md"));
        assert!(!regex.is_match("file:///foo/bar")); // no slashes in simple expansion
    }

    #[test]
    fn test_compile_uri_template_multiple_vars() {
        let (regex, vars) = compile_uri_template("api://v1/{resource}/{id}");
        assert_eq!(vars, vec!["resource", "id"]);
        assert!(regex.is_match("api://v1/users/123"));
        assert!(regex.is_match("api://v1/posts/abc"));
        assert!(!regex.is_match("api://v1/users")); // missing id
    }

    #[test]
    fn test_compile_uri_template_reserved_expansion() {
        let (regex, vars) = compile_uri_template("file:///{+path}");
        assert_eq!(vars, vec!["path"]);
        assert!(regex.is_match("file:///README.md"));
        assert!(regex.is_match("file:///foo/bar/baz.txt")); // slashes allowed
    }

    #[test]
    fn test_compile_uri_template_special_chars() {
        let (regex, vars) = compile_uri_template("http://example.com/api?query={q}");
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
                    }],
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
                    }],
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
                    }],
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
                    }],
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
                    }],
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
                    }],
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
}
