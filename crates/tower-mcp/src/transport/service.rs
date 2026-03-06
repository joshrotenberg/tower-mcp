//! Service types for transport-level middleware support
//!
//! This module provides the types needed to apply tower middleware layers
//! to MCP request processing within HTTP and WebSocket transports.
//!
//! The key type is `ServiceFactory`, a function that takes an [`McpRouter`]
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
#[cfg(any(feature = "http", feature = "websocket"))]
use std::sync::Arc;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;

use tower::util::BoxCloneService;
use tower_service::Service;

use crate::error::JsonRpcError;
use crate::protocol::{McpRequest, RequestId};
#[cfg(any(feature = "http", feature = "websocket"))]
use crate::router::McpRouter;
use crate::router::{RouterRequest, RouterResponse, ToolAnnotationsMap};

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
/// The default factory (from `identity_factory`) returns the router as-is.
/// When `.layer()` is called on a transport, the factory wraps the router
/// with the given middleware and a [`CatchError`] adapter.
#[cfg(any(feature = "http", feature = "websocket"))]
pub(crate) type ServiceFactory = Arc<dyn Fn(McpRouter) -> McpBoxService + Send + Sync>;

/// Create a `ServiceFactory` that returns the router unchanged.
///
/// This is the default factory used by transports when no `.layer()` is applied.
/// Tool annotations are still injected into request extensions.
#[cfg(any(feature = "http", feature = "websocket"))]
pub(crate) fn identity_factory() -> ServiceFactory {
    Arc::new(|router: McpRouter| {
        let annotations = router.tool_annotations_map();
        BoxCloneService::new(InjectAnnotations::new(router, annotations))
    })
}

/// A service wrapper that injects [`ToolAnnotationsMap`] into request
/// extensions for `tools/call` requests.
///
/// This allows middleware to inspect tool annotations (e.g., `read_only_hint`,
/// `destructive_hint`) without needing direct access to the router.
/// Transports apply this wrapper automatically.
#[derive(Clone)]
pub struct InjectAnnotations<S> {
    inner: S,
    annotations: ToolAnnotationsMap,
}

impl<S> InjectAnnotations<S> {
    /// Create a new `InjectAnnotations` wrapping the given service.
    pub fn new(inner: S, annotations: ToolAnnotationsMap) -> Self {
        Self { inner, annotations }
    }
}

impl<S: fmt::Debug> fmt::Debug for InjectAnnotations<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InjectAnnotations")
            .field("inner", &self.inner)
            .finish()
    }
}

impl<S> Service<RouterRequest> for InjectAnnotations<S>
where
    S: Service<RouterRequest, Response = RouterResponse>,
{
    type Response = RouterResponse;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: RouterRequest) -> Self::Future {
        if matches!(&req.inner, McpRequest::CallTool(_)) {
            req.extensions.insert(self.annotations.clone());
        }
        self.inner.call(req)
    }
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
    use std::sync::Arc;

    use super::*;
    use crate::protocol::{CallToolParams, CallToolResult, RequestId, ToolAnnotations};
    use crate::router::McpRouter;

    #[test]
    #[cfg(any(feature = "http", feature = "websocket"))]
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

    #[tokio::test]
    async fn test_inject_annotations_for_call_tool() {
        use crate::{CallToolResult, ToolBuilder};

        let tool = ToolBuilder::new("read_data")
            .description("Read some data")
            .annotations(ToolAnnotations {
                read_only_hint: true,
                destructive_hint: false,
                ..Default::default()
            })
            .handler(|_: serde_json::Value| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let router = McpRouter::new().server_info("test", "1.0.0").tool(tool);
        let annotations = router.tool_annotations_map();
        let mut service = InjectAnnotations::new(router, annotations);

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "read_data".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: crate::router::Extensions::new(),
        };

        // Verify the service processes the request (we can't inspect extensions
        // after call, but we test the map is built correctly below)
        let result = Service::call(&mut service, req).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_inject_annotations_skips_non_call_tool() {
        let router = McpRouter::new().server_info("test", "1.0.0");
        let annotations = router.tool_annotations_map();
        let mut service = InjectAnnotations::new(router, annotations);

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::Ping,
            extensions: crate::router::Extensions::new(),
        };

        let result = Service::call(&mut service, req).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_tool_annotations_map_methods() {
        use crate::ToolBuilder;

        let read_tool = ToolBuilder::new("reader")
            .description("Read-only tool")
            .annotations(ToolAnnotations {
                read_only_hint: true,
                destructive_hint: false,
                idempotent_hint: true,
                ..Default::default()
            })
            .handler(|_: serde_json::Value| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let write_tool = ToolBuilder::new("writer")
            .description("Destructive tool")
            .annotations(ToolAnnotations {
                read_only_hint: false,
                destructive_hint: true,
                idempotent_hint: false,
                ..Default::default()
            })
            .handler(|_: serde_json::Value| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let plain_tool = ToolBuilder::new("plain")
            .description("No annotations")
            .handler(|_: serde_json::Value| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let router = McpRouter::new()
            .server_info("test", "1.0.0")
            .tool(read_tool)
            .tool(write_tool)
            .tool(plain_tool);

        let map = router.tool_annotations_map();

        // read-only tool
        assert!(map.is_read_only("reader"));
        assert!(!map.is_destructive("reader"));
        assert!(map.is_idempotent("reader"));

        // destructive tool
        assert!(!map.is_read_only("writer"));
        assert!(map.is_destructive("writer"));
        assert!(!map.is_idempotent("writer"));

        // tool without annotations: not in map, defaults apply
        assert!(!map.is_read_only("plain"));
        assert!(map.is_destructive("plain")); // default is true
        assert!(!map.is_idempotent("plain"));

        // nonexistent tool: same defaults as no annotations
        assert!(!map.is_read_only("nonexistent"));
        assert!(map.is_destructive("nonexistent"));
        assert!(!map.is_idempotent("nonexistent"));

        // get() returns None for plain and nonexistent
        assert!(map.get("reader").is_some());
        assert!(map.get("writer").is_some());
        assert!(map.get("plain").is_none());
        assert!(map.get("nonexistent").is_none());
    }

    #[tokio::test]
    async fn test_annotations_visible_in_middleware() {
        use crate::ToolBuilder;
        use crate::router::ToolAnnotationsMap;
        use std::sync::atomic::{AtomicBool, Ordering};

        // A minimal middleware that checks for annotations in extensions.
        #[derive(Clone)]
        struct CheckAnnotations<S> {
            inner: S,
            found: Arc<AtomicBool>,
        }

        impl<S> Service<RouterRequest> for CheckAnnotations<S>
        where
            S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>,
        {
            type Response = RouterResponse;
            type Error = Infallible;
            type Future = S::Future;

            fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                self.inner.poll_ready(cx)
            }

            fn call(&mut self, req: RouterRequest) -> Self::Future {
                if let Some(map) = req.extensions.get::<ToolAnnotationsMap>()
                    && map.is_read_only("reader")
                {
                    self.found.store(true, Ordering::SeqCst);
                }
                self.inner.call(req)
            }
        }

        let tool = ToolBuilder::new("reader")
            .description("A read-only tool")
            .annotations(ToolAnnotations {
                read_only_hint: true,
                ..Default::default()
            })
            .handler(|_: serde_json::Value| async move { Ok(CallToolResult::text("ok")) })
            .build();

        let router = McpRouter::new().server_info("test", "1.0.0").tool(tool);
        let annotations = router.tool_annotations_map();
        let found = Arc::new(AtomicBool::new(false));

        // InjectAnnotations is outer (runs first, injects into extensions),
        // then CheckAnnotations sees the enriched request.
        let inner = CheckAnnotations {
            inner: router,
            found: found.clone(),
        };
        let mut service = InjectAnnotations::new(inner, annotations);

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "reader".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: crate::router::Extensions::new(),
        };

        let result = Service::call(&mut service, req).await;
        assert!(result.is_ok());
        assert!(
            found.load(Ordering::SeqCst),
            "Middleware should see annotations in extensions"
        );
    }
}
