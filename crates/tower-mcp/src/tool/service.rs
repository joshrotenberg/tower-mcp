//! The tower plumbing a tool is wrapped in.
//!
//! Two concerns, both of which have to sit between the router and a handler
//! rather than inside either: turning a middleware error into a tool-level
//! error result, and running a guard before the inner service sees the call.

use super::*;

/// Catches errors from the inner service and converts them to `CallToolResult::error()`.
///
/// This wrapper ensures that middleware errors (e.g., timeouts, rate limits)
/// and handler errors are converted to tool-level error responses with
/// `is_error: true`, rather than propagating as Tower service errors.
#[doc(hidden)]
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

pin_project! {
    /// Future for [`ToolCatchError`].
    #[doc(hidden)]
    pub struct ToolCatchErrorFuture<F> {
        #[pin]
        inner: F,
    }
}

impl<F, E> Future for ToolCatchErrorFuture<F>
where
    F: Future<Output = std::result::Result<CallToolResult, E>>,
    E: fmt::Display,
{
    type Output = std::result::Result<CallToolResult, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().inner.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(result)) => Poll::Ready(Ok(result)),
            Poll::Ready(Err(err)) => Poll::Ready(Ok(CallToolResult::error(err.to_string()))),
        }
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
    type Future = ToolCatchErrorFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        // Map any readiness error to Infallible (we catch it on call)
        match self.inner.poll_ready(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(_)) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, req: ToolRequest) -> Self::Future {
        ToolCatchErrorFuture {
            inner: self.inner.call(req),
        }
    }
}

/// Catches errors from an MRTR-capable tool service.
///
/// Per-tool middleware has the same error semantics for complete and MRTR
/// handlers: middleware and handler failures become complete tool error
/// results, while input-required outcomes pass through unchanged.
#[cfg(feature = "stateless")]
#[derive(Clone)]
pub(super) struct MrtrToolCatchError<S> {
    inner: S,
}

#[cfg(feature = "stateless")]
impl<S> MrtrToolCatchError<S> {
    pub(super) fn new(inner: S) -> Self {
        Self { inner }
    }
}

#[cfg(feature = "stateless")]
impl<S> Service<ToolRequest> for MrtrToolCatchError<S>
where
    S: Service<ToolRequest, Response = RequestOutcome<CallToolResult>> + Clone + Send + 'static,
    S::Error: fmt::Display + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = RequestOutcome<CallToolResult>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        match self.inner.poll_ready(cx) {
            Poll::Ready(Ok(())) | Poll::Ready(Err(_)) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, req: ToolRequest) -> Self::Future {
        let future = self.inner.call(req);
        Box::pin(async move {
            Ok(match future.await {
                Ok(outcome) => outcome,
                Err(error) => RequestOutcome::Complete(CallToolResult::error(error.to_string())),
            })
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
#[doc(hidden)]
#[derive(Clone)]
pub struct GuardService<G, S> {
    pub(super) guard: G,
    pub(super) inner: S,
}

impl<G, S, R> Service<ToolRequest> for GuardService<G, S>
where
    G: Fn(&ToolRequest) -> std::result::Result<(), String> + Clone + Send + Sync + 'static,
    S: Service<ToolRequest, Response = R> + Clone + Send + 'static,
    S::Error: Into<Error> + Send,
    S::Future: Send,
    R: Send + 'static,
{
    type Response = R;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = std::result::Result<R, Error>> + Send>>;

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
