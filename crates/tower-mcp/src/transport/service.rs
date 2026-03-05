//! Service types for transport-level middleware support
//!
//! This module provides the types needed to apply tower middleware layers
//! to MCP request processing within HTTP and WebSocket transports.
//!
//! The key type is [`ServiceFactory`], a function that takes an [`McpRouter`]
//! and produces a boxed, middleware-wrapped service. Transports store this
//! factory and use it when creating sessions.
//!
//! [`CatchError`] is a wrapper that converts middleware errors (e.g., timeouts)
//! into [`RouterResponse`] errors, preserving the `Error = Infallible` contract
//! that [`JsonRpcService`] requires.
//!
//! [`McpRouter`]: crate::router::McpRouter
//! [`RouterResponse`]: crate::router::RouterResponse
//! [`JsonRpcService`]: crate::jsonrpc::JsonRpcService

use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;

use tower::util::BoxCloneService;
use tower_service::Service;

use crate::error::JsonRpcError;
use crate::protocol::RequestId;
use crate::router::{McpRouter, RouterRequest, RouterResponse};

/// A boxed, cloneable MCP service with `Error = Infallible`.
///
/// This is the service type that transports use internally after applying
/// middleware layers. It wraps any `Service<RouterRequest>` implementation
/// so that [`JsonRpcService`](crate::jsonrpc::JsonRpcService) can consume it
/// without knowing the concrete middleware stack.
pub type McpBoxService = BoxCloneService<RouterRequest, RouterResponse, Infallible>;

/// A factory function that produces a [`McpBoxService`] from an [`McpRouter`].
///
/// Transports store this factory and call it when creating new sessions.
/// The default factory (from [`identity_factory`]) returns the router as-is.
/// When `.layer()` is called on a transport, the factory wraps the router
/// with the given middleware and a [`CatchError`] adapter.
pub type ServiceFactory = Arc<dyn Fn(McpRouter) -> McpBoxService + Send + Sync>;

/// Create a [`ServiceFactory`] that returns the router unchanged.
///
/// This is the default factory used by transports when no `.layer()` is applied.
pub fn identity_factory() -> ServiceFactory {
    Arc::new(|router: McpRouter| BoxCloneService::new(router))
}

/// A service wrapper that catches errors from middleware and converts them
/// into [`RouterResponse`] error values, maintaining the `Error = Infallible`
/// contract required by [`JsonRpcService`](crate::jsonrpc::JsonRpcService).
///
/// When a middleware layer (e.g., `TimeoutLayer`) produces an error, this
/// wrapper converts it into a JSON-RPC internal error response using the
/// request ID from the original request. This allows error information to
/// flow through the normal response path rather than requiring special
/// error handling at the transport level.
pub struct CatchError<S> {
    inner: S,
}

impl<S> CatchError<S> {
    /// Create a new `CatchError` wrapping the given service.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S: Clone> Clone for CatchError<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: fmt::Debug> fmt::Debug for CatchError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CatchError")
            .field("inner", &self.inner)
            .finish()
    }
}

pin_project! {
    /// Future for [`CatchError`].
    pub struct CatchErrorFuture<F> {
        #[pin]
        inner: F,
        request_id: Option<RequestId>,
    }
}

impl<F, E> Future for CatchErrorFuture<F>
where
    F: Future<Output = Result<RouterResponse, E>>,
    E: fmt::Display,
{
    type Output = Result<RouterResponse, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(response)) => Poll::Ready(Ok(response)),
            Poll::Ready(Err(err)) => {
                let request_id = this.request_id.take().unwrap_or(RequestId::Number(0));
                Poll::Ready(Ok(RouterResponse {
                    id: request_id,
                    inner: Err(JsonRpcError::internal_error(err.to_string())),
                }))
            }
        }
    }
}

impl<S> Service<RouterRequest> for CatchError<S>
where
    S: Service<RouterRequest, Response = RouterResponse> + Clone + Send + 'static,
    S::Error: fmt::Display + Send,
    S::Future: Send,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = CatchErrorFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(|_| unreachable!())
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        // Capture the request ID before passing the request to the inner service.
        // We need this to build a proper JSON-RPC error response if the middleware fails.
        let request_id = req.id.clone();
        let fut = self.inner.call(req);

        CatchErrorFuture {
            inner: fut,
            request_id: Some(request_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RequestId;

    #[test]
    fn test_identity_factory_produces_service() {
        let router = McpRouter::new().server_info("test", "1.0.0");
        let factory = identity_factory();
        let _service = factory(router);
    }

    #[tokio::test]
    async fn test_catch_error_passes_through_success() {
        let router = McpRouter::new().server_info("test", "1.0.0");
        let mut service = CatchError::new(router);

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: crate::protocol::McpRequest::Ping,
            extensions: crate::router::Extensions::new(),
        };

        let result = Service::call(&mut service, req).await;
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.inner.is_ok());
    }

    #[test]
    fn test_catch_error_clone() {
        let router = McpRouter::new().server_info("test", "1.0.0");
        let service = CatchError::new(router);
        let _clone = service.clone();
    }

    #[test]
    fn test_catch_error_debug() {
        let router = McpRouter::new().server_info("test", "1.0.0");
        let service = CatchError::new(router);
        let debug = format!("{:?}", service);
        assert!(debug.contains("CatchError"));
    }
}
